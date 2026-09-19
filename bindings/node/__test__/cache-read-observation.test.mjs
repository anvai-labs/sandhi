import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { Gateway, parseUsage, chatContractSchemaJson } from "../sandhi.js";

const corpus = JSON.parse(readFileSync(new URL("../../fixtures/cache-read-observation-parity.json", import.meta.url), "utf8"));

for (const item of corpus.manual) {
  test(`manual cache observation: ${item.name}`, () => {
    const gateway = new Gateway();
    gateway.addVirtualKey("vk");
    const event = gateway.meterTokens("vk", "custom", "model", 10, 2,
      undefined, undefined, undefined, undefined, item.observation);
    assert.deepEqual(event.cacheReadObservation ?? null, item.expected);
    assert.equal(event.cacheReadTokens, 0);
    assert.equal(event.tokensIn, 10);
    assert.equal(gateway.spent("vk:vk"), 12);
    assert.deepEqual(gateway.events()[0].cacheReadObservation ?? null, item.expected);
    const row = JSON.parse(gateway.usageSnapshotJson("total"))[0];
    const status = item.expected?.status ?? "unknown";
    assert.deepEqual(row.cache_read_coverage, Object.fromEntries(
      ["reported", "absent", "malformed", "unsupported", "unknown"].map(key => [key, Number(key === status)]),
    ));
  });
}

for (const item of corpus.parsed) {
  test(`parsed cache observation: ${item.name}`, () => {
    const response = JSON.stringify(item.response);
    const expected = { status: item.status, source: "origin_usage" };
    const parsed = parseUsage(item.provider, response);
    assert.equal(parsed.cacheReadTokens, item.cached);
    assert.deepEqual(parsed.cacheReadObservation, expected);
    const gateway = new Gateway();
    gateway.addVirtualKey("vk");
    const event = gateway.meter("vk", item.provider, "model", response);
    assert.equal(event.cacheReadTokens, item.cached);
    assert.deepEqual(event.cacheReadObservation, expected);
    const row = JSON.parse(gateway.usageSnapshotJson("total"))[0];
    assert.equal(row.cache_read_coverage[item.status], 1);
    assert.equal(row.calls, 1);
  });
}

test("cache observation and coverage schema additions are optional", () => {
  const schema = JSON.parse(chatContractSchemaJson("usage.v2"));
  assert.ok(schema.properties.cache_read_observation);
  assert.ok(!(schema.required ?? []).includes("cache_read_observation"));
  const aggregate = JSON.parse(chatContractSchemaJson("usage-aggregate.v1"));
  assert.ok(aggregate.properties.cache_read_coverage);
  assert.ok(!(aggregate.required ?? []).includes("cache_read_coverage"));
});
