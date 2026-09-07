// Optional sibling integration: real AgentBrowser service + Chromium, no REST listener.
// Only disposable fixture data may enter this harness. No vault, cookies or artifacts.
import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const sibling = process.env.SANDHI_AGENTBROWSER_ROOT;
const origin = new URL(process.env.SANDHI_SMOKE_ORIGIN).origin;
assert.equal(new URL(origin).hostname, '127.0.0.1');
const moduleAt = (name) => import(pathToFileURL(resolve(sibling, `packages/${name}/dist/index.js`)));
const [{ AgentBrowserService }, { PlaywrightChromiumEngine }, { NetworkPolicy }, { SecretManager }] =
  await Promise.all(['api', 'engine-playwright', 'policy', 'core'].map(moduleAt));

// The production service blocks loopback. This test-only policy permits exactly
// the disposable fixture origin (including port), not arbitrary private services.
class FixturePolicy extends NetworkPolicy {
  async checkRequest(request) {
    assert.equal(new URL(request.url).origin, origin, 'non-fixture egress denied');
    await super.checkRequest(request);
  }
}
const secret = process.env.SANDHI_SMOKE_ADMIN_TOKEN || 'dashboard-test-admin';
const expectedScope = process.env.SANDHI_SMOKE_EXPECTED_SCOPE || 'group:dashboard';
const expectedJson = process.env.SANDHI_SMOKE_EXPECTED_EVIDENCE;
assert.ok(expectedJson === undefined || expectedJson.length <= 16384, 'fixture evidence too large');
const expectedEvidence = expectedJson === undefined ? null : JSON.parse(expectedJson);
const reference = 'vault://sandhi-smoke/admin';
const service = new AgentBrowserService({
  engine: new PlaywrightChromiumEngine(),
  networkPolicy: new FixturePolicy({ blockMetadata: true, maxRedirects: 0 }),
  secretManager: new SecretManager({ [reference]: secret }),
});
try {
  const { sessionId } = await service.createSession({
    tenantId: 'sandhi-smoke', headless: true, ttlMs: 60000, idleTimeoutMs: 30000,
    allowedHosts: ['127.0.0.1'], allowDownloads: false,
  });
  const { pageId } = await service.createPage(sessionId);
  await service.navigate(sessionId, pageId, { url: `${origin}/dashboard`, waitUntil: 'networkidle' });
  async function observeUntil(predicate) {
    const deadline = Date.now() + 10000;
    do {
      const observation = await service.observe(sessionId, pageId, { mode: 'content' });
      const serialized = JSON.stringify(observation);
      assert.ok(!serialized.includes(secret), 'registered secret leaked into observation');
      assert.equal(observation.untrustedContent, true);
      if (predicate(serialized)) return;
      await new Promise((done) => setTimeout(done, 100));
    } while (Date.now() < deadline);
    assert.fail('expected dashboard state was not observed');
  }
  async function act(label, action, value) {
    const snapshot = await service.getSnapshot(sessionId, pageId);
    const matches = snapshot.fields.filter((field) => field.label === label);
    assert.equal(matches.length, 1, `expected one accessible target: ${label}`);
    const result = await service.executePlan(sessionId, pageId, [{
      action, target: { ref: matches[0].ref }, ...(value === undefined ? {} : { value }),
    }]);
    assert.equal(result.ok, true, JSON.stringify(result));
    assert.equal(result.completed, 1);
  }
  async function assertRestoredNumbers() {
    if (expectedEvidence === null) return;
    const observation = await service.observe(sessionId, pageId, { mode: 'content' });
    const extracted = await service.extract(sessionId, pageId, { format: 'tables' });
    const accessible = await service.observe(sessionId, pageId, { mode: 'accessibility', maxElements: 1000 });
    assert.ok(!JSON.stringify([observation, extracted, accessible]).includes(secret),
      'registered secret leaked into numeric evidence');
    assert.equal(observation.untrustedContent, true);
    assert.equal(observation.truncated, false);
    assert.equal(accessible.untrustedContent, true);
    assert.equal(accessible.truncated, false);
    let stage = 'cards';
    try {
      // Content observations retain block boundaries: each card is its value followed
      // by its label. Restrict matching to Overview, before the Attribution heading.
      const text = observation.text;
      assert.ok(Array.isArray(text));
      const start = text.indexOf('Overview');
      const end = text.indexOf('Attribution');
      assert.ok(start >= 0 && end > start);
      const cards = text.slice(start + 1, end);
      assert.match(cards.shift(), /^Loaded at .+\. Use Refresh to update\.$/);
      const number = (value) => {
        assert.ok(Number.isSafeInteger(value) && value >= 0);
        return value.toLocaleString('en-US');
      };
      const labels = [['calls', 'calls'], ['tokens in', 'tokens_in'],
        ['tokens out', 'tokens_out'], ['cache read', 'cache_read_tokens'],
        ['billable', 'billable_tokens']];
      assert.deepEqual(cards.slice(0, 10), labels.flatMap(([label, key]) =>
        [number(expectedEvidence.total[key]), label]));
      // Card association is DOM-text evidence only: the supported accessibility
      // observation omits their static text. Visibility is independently proved
      // below for the numeric attribution/budget rows, not these cards.

      // Associate the four attribution tables with their ordered headings, then
      // compare complete keyed rows and numeric columns, never page-wide totals.
      stage = 'attribution';
      const headings = ['By user (subject)', 'By team (group)', 'By provider', 'By model'];
      assert.deepEqual(text.filter((line) => headings.includes(line)), headings);
      const tables = extracted.data;
      assert.ok(Array.isArray(tables));
      const usageHeaders = ['key', 'calls', 'in', 'out', 'cache write', 'cache read', 'billable', 'latency'];
      const usage = tables.filter((table) => JSON.stringify(table.headers) === JSON.stringify(usageHeaders));
      assert.equal(usage.length, 4);
      const dimensions = ['by_subject', 'by_group', 'by_provider', 'by_model'];
      const fields = ['calls', 'tokens_in', 'tokens_out', 'cache_creation_tokens', 'cache_read_tokens', 'billable_tokens'];
      for (const [index, dimension] of dimensions.entries()) {
        const wanted = expectedEvidence[dimension].map((row) =>
          [row.key, ...fields.map((field) => number(row[field]))]);
        assert.ok(wanted.length > 0 && wanted.length <= 64);
        assert.ok(usage[index].rows.every((row) => row.length === 8));
        const headingStart = text.indexOf(headings[index]);
        const headingEnd = index < 3 ? text.indexOf(headings[index + 1])
          : text.findIndex((line) => line.startsWith('Declarative config '));
        assert.ok(headingStart >= 0 && headingEnd > headingStart);
        assert.deepEqual(text.slice(headingStart + 1, headingEnd),
          [usageHeaders.join(' '), ...usage[index].rows.map((row) => row.join(' '))]);
        const byKey = (a, b) => a[0].localeCompare(b[0]);
        assert.deepEqual(usage[index].rows.map((row) => row.slice(0, 7)).sort(byKey), wanted.sort(byKey));
      }
      stage = 'budgets';
      const budgets = tables.filter((table) => JSON.stringify(table.headers) === JSON.stringify(
        ['scope', 'spent / limit (tokens)', 'utilization', 'window', 'policy']));
      assert.equal(budgets.length, 1);
      const wantedBudgets = expectedEvidence.budgets.map((row) =>
        [row.scope, `${number(row.spent)} / ${number(row.limit_tokens)}`, '', row.window, row.policy]);
      const byScope = (a, b) => a[0].localeCompare(b[0]);
      assert.deepEqual(budgets[0].rows.sort(byScope), wantedBudgets.sort(byScope));
      // HTML extraction also includes hidden DOM. Accessibility rows/cells must
      // independently expose these exact values; hidden tables cannot certify UI.
      stage = 'visibility';
      const visibleRows = accessible.elements.filter((element) => element.role === 'row' && element.visible);
      const wantedRows = [...usage, budgets[0]].flatMap((table) => [table.headers, ...table.rows])
        .map((row) => row.filter((cell) => cell !== '').join(' '));
      for (const row of new Set(wantedRows)) {
        assert.equal(visibleRows.filter((element) => element.name === row).length,
          wantedRows.filter((candidate) => candidate === row).length);
      }
    } catch {
      // Do not print extracted operator state on failure, even for this synthetic fixture.
      assert.fail(`restored dashboard numeric evidence mismatch: ${stage}`);
    }
  }
  await observeUntil((text) => text.includes('Authentication required') && !text.includes('gpt-mock'));
  await act('Admin token', 'fill', reference);
  await act('Use token', 'click');
  await observeUntil((text) => text.includes('gpt-mock') && text.includes(expectedScope));
  await assertRestoredNumbers();
  await act('Refresh', 'click');
  await observeUntil((text) => text.includes('gpt-mock') && text.includes(expectedScope));
  await assertRestoredNumbers();
  await act('Clear token', 'click');
  await observeUntil((text) => text.includes('Authentication required') && !text.includes('gpt-mock'));
  await assert.rejects(service.navigate(sessionId, pageId, { url: 'http://127.0.0.1:1/' }));
  await service.closeSession(sessionId);
  console.log('AgentBrowser smoke passed: locked, secret-ref auth, usage, clear, egress denial');
} finally {
  await service.shutdown();
}
