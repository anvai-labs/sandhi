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
const secret = 'dashboard-test-admin';
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
  await observeUntil((text) => text.includes('Authentication required') && !text.includes('gpt-mock'));
  await act('Admin token', 'fill', reference);
  await act('Use token', 'click');
  await observeUntil((text) => text.includes('gpt-mock') && text.includes('group:dashboard'));
  await act('Clear token', 'click');
  await observeUntil((text) => text.includes('Authentication required') && !text.includes('gpt-mock'));
  await assert.rejects(service.navigate(sessionId, pageId, { url: 'http://127.0.0.1:1/' }));
  await service.closeSession(sessionId);
  console.log('AgentBrowser smoke passed: locked, secret-ref auth, usage, clear, egress denial');
} finally {
  await service.shutdown();
}
