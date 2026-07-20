// End-to-end tests for the @anvai-labs/sandhi Node binding.
// Requires the addon to be built first: `npm run build:debug` (or `build`).
import assert from "node:assert/strict";
import { test } from "node:test";

import { createServer } from "node:http";

import { Gateway, parseUsage, wireContractVersion } from "../index.js";
// The transport surface is exercised through the sandhi.js entry so `for await` (Symbol.asyncIterator) works.
import { complete, stream, registerProvider } from "../sandhi.js";

// Start a throwaway localhost HTTP server that replies with `bodyStr` (+ content-type). Returns the
// base URL and a close() — no network needed to exercise the transport.
function localServer(bodyStr, contentType) {
  return new Promise((resolve) => {
    const srv = createServer((req, res) => {
      res.writeHead(200, { "content-type": contentType, "content-length": Buffer.byteLength(bodyStr) });
      res.end(bodyStr);
    });
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      resolve({ base: `http://127.0.0.1:${port}/v1`, close: () => new Promise((r) => srv.close(r)) });
    });
  });
}

test("complete — async transport parses usage at the source", async () => {
  const resp = JSON.stringify({
    choices: [{ index: 0, message: { role: "assistant", content: "hi" }, finish_reason: "stop" }],
    usage: { prompt_tokens: 100, completion_tokens: 20, prompt_tokens_details: { cached_tokens: 60 } },
  });
  const srv = await localServer(resp, "application/json");
  try {
    const body = JSON.stringify({ model: "gpt-4o", messages: [{ role: "user", content: "hi" }] });
    const out = await complete("openai", "gpt-4o", srv.base, "sk-test", body, "sess-1");
    assert.equal(out.status, 200);
    assert.equal(out.usage.tokensIn, 40); // 100 - 60 cached (fresh-only)
    assert.equal(out.usage.cacheReadTokens, 60);
    assert.equal(out.usage.tokensOut, 20);
    assert.equal(JSON.parse(out.body).choices[0].message.content, "hi");
  } finally {
    await srv.close();
  }
});

test("stream — async iterator forwards bytes verbatim + finalizes usage", async () => {
  const sse =
    'data: {"choices":[{"delta":{"content":"he"}}]}\n\n' +
    'data: {"choices":[{"delta":{"content":"llo"}}]}\n\n' +
    'data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":4}}}\n\n' +
    "data: [DONE]\n\n";
  const srv = await localServer(sse, "text/event-stream");
  try {
    const body = JSON.stringify({ model: "gpt-4o", messages: [{ role: "user", content: "hi" }] });
    const s = await stream("openai", "gpt-4o", srv.base, "sk", body, "s1");
    let forwarded = Buffer.alloc(0);
    let usage = null;
    for await (const chunk of s) {
      forwarded = Buffer.concat([forwarded, chunk.data]);
      if (chunk.usage != null) usage = chunk.usage;
    }
    const text = forwarded.toString("utf8");
    assert.ok(text.includes("he") && text.includes("llo") && text.includes("[DONE]"));
    assert.ok(usage != null);
    assert.equal(usage.tokensIn, 6); // 10 - 4 cached
    assert.equal(usage.tokensOut, 5);
    assert.equal(usage.cacheReadTokens, 4);
  } finally {
    await srv.close();
  }
});

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

test("registerProvider — host-language escape hatch serves complete()", async () => {
  // A custom provider that owns its own (here trivial) transport and self-reports usage — served
  // through complete() without a Rust adapter. Parity with the Python binding's register_provider.
  registerProvider("mycustom", async (model, bodyJson, sessionId) => {
    const req = JSON.parse(bodyJson);
    return {
      status: 200,
      body: JSON.stringify({ model, echoed: req, session: sessionId }),
      usage: { tokensIn: 7, tokensOut: 3, cacheCreationTokens: 0, cacheReadTokens: 2 },
    };
  });

  const body = JSON.stringify({ messages: [{ role: "user", content: "hi" }] });
  const out = await complete("mycustom", "custom-model-x", "http://unused", "k", body, "s9");
  assert.equal(out.status, 200);
  assert.equal(out.usage.tokensIn, 7);
  assert.equal(out.usage.tokensOut, 3);
  assert.equal(out.usage.cacheReadTokens, 2);
  const parsed = JSON.parse(out.body);
  assert.equal(parsed.model, "custom-model-x");
  assert.equal(parsed.session, "s9");
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
