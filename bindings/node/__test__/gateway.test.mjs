// End-to-end tests for the @anvai-labs/sandhi Node binding.
// Requires the addon to be built first: `npm run build:debug` (or `build`).
import assert from "node:assert/strict";
import { test } from "node:test";

import { Gateway, parseUsage, wireContractVersion } from "../index.js";

test("wireContractVersion", () => {
  assert.equal(wireContractVersion(), "1");
});

test("parseUsage — openai cache split", () => {
  const resp = JSON.stringify({
    usage: { prompt_tokens: 100, completion_tokens: 20, prompt_tokens_details: { cached_tokens: 60 } },
  });
  const u = parseUsage("openai", resp);
  assert.equal(u.tokensIn, 40); // 100 - 60 cached
  assert.equal(u.cacheReadTokens, 60);
  assert.equal(u.tokensOut, 20);
  assert.equal(u.cacheCreationTokens, 0);
});

test("parseUsage — anthropic direct split", () => {
  const resp = JSON.stringify({
    usage: { input_tokens: 12, output_tokens: 5, cache_creation_input_tokens: 3, cache_read_input_tokens: 7 },
  });
  const u = parseUsage("anthropic", resp);
  assert.deepEqual(
    { i: u.tokensIn, o: u.tokensOut, cc: u.cacheCreationTokens, cr: u.cacheReadTokens },
    { i: 12, o: 5, cc: 3, cr: 7 },
  );
});

test("gateway meters, attributes, and budgets", () => {
  const gw = new Gateway();
  gw.addVirtualKey("vk_alice", "alice", "platform", "anthropic");
  gw.setBudget("group:platform", 1000);

  const resp = JSON.stringify({
    usage: { input_tokens: 220, output_tokens: 80, cache_creation_input_tokens: 0, cache_read_input_tokens: 40 },
  });
  const ev = gw.meter("vk_alice", "anthropic", "claude-x", resp, "conv_7");

  assert.equal(ev.subjectId, "alice");
  assert.equal(ev.groupId, "platform");
  assert.equal(ev.virtualKeyId, "vk_alice");
  assert.equal(ev.sessionId, "conv_7");
  assert.equal(ev.provider, "anthropic");
  assert.equal(ev.backend, "external");
  assert.equal(ev.schemaVersion, "1");
  assert.equal(ev.tokensIn, 220);
  assert.equal(ev.cacheReadTokens, 40);
  assert.ok(ev.gpuSeconds == null); // napi maps Rust None → undefined

  // billable = 220 + 80 = 300 recorded on the group scope
  assert.equal(gw.spent("group:platform"), 300);
  assert.equal(gw.events().length, 1);
  assert.equal(gw.checkBudget("group:platform", 800), false); // 300 + 800 > 1000
  assert.equal(gw.checkBudget("group:platform", 700), true);
});

test("unknown virtual key throws", () => {
  const gw = new Gateway();
  assert.throws(() => gw.meter("vk_nope", "openai", "m", JSON.stringify({ usage: {} })));
});

test("meterTokens bypasses parsing (escape hatch)", () => {
  const gw = new Gateway();
  gw.addVirtualKey("vk", "s", "g", "x");
  const ev = gw.meterTokens("vk", "custom-provider", "m", 11, 7, 0, 2, "sess");
  assert.equal(ev.tokensIn, 11);
  assert.equal(ev.tokensOut, 7);
  assert.equal(ev.cacheReadTokens, 2);
  assert.equal(ev.provider, "custom-provider");
  assert.equal(ev.sessionId, "sess");
  assert.equal(gw.spent("group:g"), 18); // 11 + 7
});
