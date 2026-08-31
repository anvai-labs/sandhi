import assert from "node:assert/strict";
import { createServer } from "node:http";
import { test } from "node:test";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { readFileSync, rmSync } from "node:fs";

import { Gateway, parseUsage, wireContractVersion } from "../index.js";
import { ProviderRuntime } from "../sandhi.js";

function localServer(responses) {
  return new Promise((resolve) => {
    let calls = 0;
    let lastHeaders = {};
    const server = createServer((request, response) => {
      lastHeaders = request.headers;
      const item = responses[calls++];
      response.writeHead(item.status ?? 200, {
        "content-type": item.contentType,
        "content-length": Buffer.byteLength(item.body),
      });
      response.end(item.body);
    });
    server.listen(0, "127.0.0.1", () => {
      resolve({
        origin: `http://127.0.0.1:${server.address().port}`,
        baseUrl: `http://127.0.0.1:${server.address().port}/v1`,
        calls: () => calls,
        lastHeaders: () => lastHeaders,
        close: () => new Promise((done) => server.close(done)),
      });
    });
  });
}

test("persistent typed provider completes and streams neutral documents", async () => {
  const server = await localServer([
    {
      contentType: "application/json",
      body: JSON.stringify({
        id: "r1",
        model: "gpt-test",
        choices: [{ message: { content: "hello" }, finish_reason: "stop" }],
        usage: {
          prompt_tokens: 10,
          completion_tokens: 3,
          prompt_tokens_details: { cached_tokens: 4 },
        },
      }),
    },
    {
      contentType: "text/event-stream",
      body:
        'data: {"id":"r2","model":"gpt-test","choices":[{"delta":{"content":"he"},"finish_reason":null}]}\n\n' +
        'data: {"id":"r2","model":"gpt-test","choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":4}}}\n\n' +
        "data: [DONE]\n\n",
    },
  ]);
  try {
    const runtime = new ProviderRuntime();
    const provider = runtime.openaiCompat(
      "openai",
      server.baseUrl,
      "key",
      undefined,
      0,
    );
    const request = JSON.stringify({
      schema_version: "1",
      model: "gpt-test",
      messages: [{ role: "user", content: "hi" }],
    });
    const response = JSON.parse(await provider.completeJson(request));
    assert.equal(response.output.content, "hello");
    assert.equal(response.usage.tokens_in, 6);

    const events = [];
    for await (const event of provider.streamJson(request)) events.push(JSON.parse(event));
    assert.deepEqual(events.map((event) => event.event), [
      "response_start",
      "text_delta",
      "usage",
      "finish",
    ]);
    assert.equal(server.calls(), 2);
  } finally {
    await server.close();
  }
});

test("Anthropic bearer auth is explicit across the typed binding", async () => {
  const server = await localServer([
    {
      contentType: "application/json",
      body: JSON.stringify({
        id: "msg_1",
        type: "message",
        role: "assistant",
        model: "claude-test",
        content: [{ type: "text", text: "ok" }],
        stop_reason: "end_turn",
        usage: { input_tokens: 2, output_tokens: 1 },
      }),
    },
  ]);
  try {
    const provider = new ProviderRuntime().provider(
      "anthropic",
      "claude-test",
      "oauth-token",
      server.origin,
      undefined,
      0,
      undefined,
      undefined,
      "bearer",
    );
    const response = JSON.parse(
      await provider.completeJson(
        JSON.stringify({
          model: "claude-test",
          messages: [{ role: "user", content: "hi" }],
          max_output_tokens: 16,
        }),
      ),
    );
    assert.equal(response.output.content, "ok");
    assert.equal(server.lastHeaders().authorization, "Bearer oauth-token");
    assert.equal(server.lastHeaders()["x-api-key"], undefined);
  } finally {
    await server.close();
  }
});

test("Responses protocol stays item-shaped across the typed binding", async () => {
  const server = await localServer([
    {
      contentType: "application/json",
      body: JSON.stringify({
        id: "resp_1",
        model: "gpt-test",
        status: "completed",
        output: [
          {
            type: "message",
            role: "assistant",
            content: [{ type: "output_text", text: "ok" }],
          },
        ],
        usage: {
          input_tokens: 12,
          output_tokens: 3,
          input_tokens_details: { cached_tokens: 2 },
          output_tokens_details: { reasoning_tokens: 1 },
        },
      }),
    },
  ]);
  try {
    const provider = new ProviderRuntime().provider(
      "openai",
      "gpt-test",
      "oauth-token",
      server.baseUrl,
      JSON.stringify({ originator: "victor" }),
      0,
      undefined,
      undefined,
      undefined,
      "responses",
    );
    const response = JSON.parse(
      await provider.completeJson(
        JSON.stringify({
          model: "gpt-test",
          messages: [{ role: "user", content: "hi" }],
        }),
      ),
    );
    assert.equal(response.output.content, "ok");
    assert.equal(response.usage.tokens_in, 10);
    assert.equal(response.usage.cache_read_tokens, 2);
    assert.equal(response.usage.reasoning_tokens, 1);
    assert.equal(server.lastHeaders().authorization, "Bearer oauth-token");
    assert.equal(server.lastHeaders().originator, "victor");
  } finally {
    await server.close();
  }
});

test("ChatGPT Responses profile aggregates its required upstream stream", async () => {
  const server = await localServer([
    {
      contentType: "text/event-stream",
      body:
        'data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-test"}}\n\n' +
        'data: {"type":"response.output_text.delta","delta":"ok"}\n\n' +
        'data: {"type":"response.completed","response":{"status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1}}}\n\n',
    },
  ]);
  try {
    const provider = new ProviderRuntime().provider(
      "openai",
      "gpt-test",
      "oauth-token",
      server.origin,
      undefined,
      0,
      undefined,
      undefined,
      undefined,
      "chatgpt_responses",
    );
    const response = JSON.parse(
      await provider.completeJson(
        JSON.stringify({
          model: "gpt-test",
          messages: [
            { role: "developer", content: "be precise" },
            { role: "user", content: "hi" },
          ],
        }),
      ),
    );
    assert.equal(response.output.content, "ok");
    assert.equal(response.usage.tokens_in, 4);
  } finally {
    await server.close();
  }
});

test("raw provider transport exports are absent", async () => {
  const module = await import("../sandhi.js");
  for (const name of ["complete", "stream", "registerProvider", "ByteStream"])
    assert.equal(module[name], undefined);
});

test("catalog surface serves curated model data (TD-0004)", async () => {
  const { providerDescriptorJson, providerModelsJson } = await import("../sandhi.js");
  const descriptor = JSON.parse(providerDescriptorJson("anthropic"));
  assert.equal(descriptor.slug, "anthropic");
  const models = JSON.parse(providerModelsJson("anthropic"));
  assert.ok(models.some((m) => m.id === "claude-fable-5"));
  const fable = models.find((m) => m.id === "claude-fable-5");
  assert.equal(fable.max_input_tokens, 1000000);
  // Facts only — the catalog carries no pricing (measure-vs-price line held).
  assert.equal(fable.price, undefined);
  // Seeded compat vendor lineups resolve through aliases too.
  const grok = JSON.parse(providerModelsJson("grok"));
  assert.ok(grok.some((m) => m.id === "grok-4"));
  // Aggregators stay empty (dynamic hosting catalogs — live discovery).
  assert.deepEqual(JSON.parse(providerModelsJson("openrouter")), []);
  // Self-hosted InferFlux: model ids are operator config server-side, never vendor facts.
  assert.deepEqual(JSON.parse(providerModelsJson("inferflux")), []);
  assert.throws(() => providerModelsJson("unknown-provider"));
});

test("usage parsing and metering retain cache attribution", () => {
  assert.equal(wireContractVersion(), "1");
  const usage = parseUsage(
    "openai",
    JSON.stringify({
      usage: {
        prompt_tokens: 100,
        completion_tokens: 20,
        prompt_tokens_details: { cached_tokens: 60 },
      },
    }),
  );
  assert.equal(usage.tokensIn, 40);
  assert.equal(usage.cacheReadTokens, 60);

  const gateway = new Gateway();
  gateway.addVirtualKey("vk", "alice", "platform", "openai");
  gateway.setBudget("group:platform", 1000);
  const event = gateway.meterTokens("vk", "openai", "m", 40, 20, 0, 60, "s1");
  assert.equal(event.subjectId, "alice");
  assert.equal(event.sessionId, "s1");
  // D4 billable counts the cache split: 40 fresh in + 60 cache-read + 20 out = 120.
  assert.equal(gateway.spent("group:platform"), 120);
});

// ---------------------------------------------------------------------------
// ProviderRuntime.provider() dispatch + the direct openaiResponses factory.
// Handle construction is pure (no network); this covers every provider branch
// and the typed-provider getter.
// ---------------------------------------------------------------------------

test("provider() factory dispatches named backends and exposes the slug getter", () => {
  const runtime = new ProviderRuntime();
  for (const [name, provider, model] of [
    ["anthropic", "anthropic", "claude-test"],
    ["claude alias", "claude", "claude-test"],
    ["gemini", "gemini", "gemini-1.5-pro"],
    ["google alias", "google", "gemini-1.5-pro"],
    ["cohere", "cohere", "command-r"],
    ["ollama", "ollama", "llama3"],
  ]) {
    // Args: provider, model, api_key, base_url, headers_json, max_retries.
    const handle = runtime.provider(provider, model, "key", undefined, undefined, 0);
    assert.equal(typeof handle.provider, "string", `${name} exposed a slug`);
    assert.ok(handle.provider.length > 0, `${name} slug non-empty`);
  }

  // api_key (default) auth scheme on Anthropic — explicit api_key spelling + default both ok.
  assert.ok(
    runtime.provider("anthropic", "claude-test", "k", undefined, undefined, 0, undefined, undefined, "api_key").provider,
  );
  assert.ok(runtime.provider("anthropic", "claude-test", "k", undefined, undefined, 0).provider);
});

test("provider() routes the openai-compat + responses escape hatches", () => {
  const runtime = new ProviderRuntime();
  // Unknown provider WITH a base_url → openai_compat escape hatch.
  const custom = runtime.provider("acme", "m", "key", "https://example.test/v1", undefined, 0);
  assert.equal(custom.provider, "acme");

  // Known catalog provider WITHOUT a base_url → known_openai_compat resolves the spec.
  const known = runtime.provider("deepseek", "deepseek-chat", "key", undefined, undefined, 0);
  assert.equal(known.provider, "deepseek");

  // Self-hosted InferFlux is a first-class catalog slug (ADR-0008): no base_url needed, the
  // spec's default (http://127.0.0.1:8080/v1) resolves.
  const inferflux = runtime.provider("inferflux", "llama3-8b", "local-key", undefined, undefined, 0);
  assert.equal(inferflux.provider, "inferflux");

  // openaiResponses() direct factory (Responses API bearer form).
  const responses = runtime.openaiResponses("openai", "https://example.test/v1", "token", undefined, 0);
  assert.equal(responses.provider, "openai");

  // Responses protocol via provider() resolves a known catalog provider's base_url when omitted.
  const viaProvider = runtime.provider(
    "deepseek",
    "deepseek-chat",
    "key",
    undefined,
    undefined,
    0,
    undefined,
    undefined,
    undefined,
    "responses",
  );
  assert.equal(viaProvider.provider, "deepseek");
});

test("provider() rejects invalid dispatch inputs at the FFI seam", () => {
  const runtime = new ProviderRuntime();
  // Unsupported auth_scheme value on Anthropic. Args: provider,model,key,base_url,headers,retries,,,auth_scheme.
  assert.throws(
    () => runtime.provider("anthropic", "m", "k", undefined, undefined, 0, undefined, undefined, "bogus"),
    /auth_scheme/,
  );
  // bearer for a non-scheme family is a NO-OP (its default IS Bearer) — accepted,
  // not rejected (TD-0008 rule 5). Only a contradictory scheme is an error.
  const bearerNoop = runtime.provider("openai", "m", "k", "https://e.test/v1", undefined, 0, undefined, undefined, "bearer");
  assert.equal(bearerNoop.provider, "openai");
  assert.throws(
    () => runtime.provider("openai", "m", "k", "https://e.test/v1", undefined, 0, undefined, undefined, "api_key"),
    /Anthropic/,
  );
  // Unsupported protocol value.
  assert.throws(
    () => runtime.provider("openai", "m", "k", undefined, undefined, 0, undefined, undefined, undefined, "bogus"),
    /protocol/,
  );
  // Responses protocol + unknown provider + no base_url.
  assert.throws(
    () => runtime.provider("acme", "m", "k", undefined, undefined, 0, undefined, undefined, undefined, "responses"),
    /baseUrl/,
  );
  // Unknown catalog provider without a base_url under chat_completions → typed provider error.
  assert.throws(() => runtime.provider("acme", "m", "k", undefined, undefined, 0), /unknown catalog provider|acme/);
  // Malformed headers JSON.
  assert.throws(() => runtime.openaiCompat("openai", "https://e.test/v1", "k", "not-json", 0), /headers/);
});

// ---------------------------------------------------------------------------
// TypedEventStream.read() pull API + the upstream-error propagation path.
// ---------------------------------------------------------------------------

test("TypedEventStream.read() drains a healthy stream to exhaustion", async () => {
  const server = await localServer([
    {
      contentType: "text/event-stream",
      body:
        'data: {"id":"r2","model":"gpt-test","choices":[{"delta":{"content":"he"},"finish_reason":null}]}\n\n' +
        'data: {"id":"r2","model":"gpt-test","choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}\n\n' +
        "data: [DONE]\n\n",
    },
  ]);
  try {
    const provider = new ProviderRuntime().openaiCompat("openai", server.baseUrl, "key", undefined, 0);
    const stream = provider.streamJson(
      JSON.stringify({ model: "gpt-test", messages: [{ role: "user", content: "hi" }] }),
    );
    const events = [];
    while (true) {
      const chunk = await stream.read();
      if (chunk === null || chunk === undefined) break;
      events.push(JSON.parse(chunk));
    }
    assert.deepEqual(events.map((e) => e.event), ["response_start", "text_delta", "usage", "finish"]);
  } finally {
    await server.close();
  }
});

test("streamJson surfaces an upstream error via read()", async () => {
  const server = await localServer([
    { status: 500, contentType: "application/json", body: JSON.stringify({ error: "boom" }) },
  ]);
  try {
    const provider = new ProviderRuntime().openaiCompat("openai", server.baseUrl, "key", undefined, 0);
    const stream = provider.streamJson(
      JSON.stringify({ model: "gpt-test", messages: [{ role: "user", content: "hi" }] }),
    );
    await assert.rejects(() => stream.read(), /status|500|boom|error/i);
  } finally {
    await server.close();
  }
});

test("completeJson surfaces an upstream HTTP error as a typed descriptor", async () => {
  const server = await localServer([
    { status: 500, contentType: "application/json", body: JSON.stringify({ error: "boom" }) },
  ]);
  try {
    const provider = new ProviderRuntime().openaiCompat("openai", server.baseUrl, "key", undefined, 0);
    await assert.rejects(
      () => provider.completeJson(JSON.stringify({ model: "gpt-test", messages: [{ role: "user", content: "hi" }] })),
      /status|500|boom|error/i,
    );
  } finally {
    await server.close();
  }
});

// ---------------------------------------------------------------------------
// Gateway: meter() (parse-driven), events(), checkBudget(), the JSONL sink,
// group-less scope, and every built-in usage parser via parseUsage.
// ---------------------------------------------------------------------------

test("Gateway.meter parses, attributes, records budget, and lists events", () => {
  const gateway = new Gateway();
  gateway.addVirtualKey("vk_alice", "alice", "platform", "anthropic");
  gateway.setBudget("group:platform", 1000);

  const event = gateway.meter(
    "vk_alice",
    "anthropic",
    "claude-x",
    JSON.stringify({
      usage: {
        input_tokens: 100,
        output_tokens: 20,
        cache_creation_input_tokens: 5,
        cache_read_input_tokens: 10,
      },
    }),
    "conv_1",
    "router",
  );
  assert.equal(event.subjectId, "alice");
  assert.equal(event.groupId, "platform");
  assert.equal(event.sessionId, "conv_1");
  assert.equal(event.route, "router");
  assert.equal(event.provider, "anthropic");
  assert.equal(event.usageCompleteness, "final");
  // 100 fresh in + 5 cache-write + 10 cache-read + 20 out (D4); narrow read 120.
  assert.equal(gateway.spent("group:platform"), 135);

  // Within budget → true; over budget → false. (135 spent of 1000 → 865 remaining.)
  assert.equal(gateway.checkBudget("group:platform", 864), true);
  assert.equal(gateway.checkBudget("group:platform", 866), false);

  // events() returns the in-memory buffer.
  const listed = gateway.events();
  assert.equal(listed.length, 1);
  assert.equal(listed[0].requestId, event.requestId);

  // Unknown virtual key throws.
  assert.throws(() => gateway.meter("ghost", "openai", "m", JSON.stringify({})), /unknown virtual key/);
  // Bad JSON throws.
  assert.throws(() => gateway.meter("vk_alice", "openai", "m", "{not json"), /valid JSON/);
});

test("Gateway budgets are 64-bit (no u32 truncation/overflow)", () => {
  // Regression: set_budget/check_budget/spent were u32, silently wrapping past ~4.29G tokens
  // (an enforcement bypass absent on the Python binding/core). They are now i64 (a JS safe
  // integer, range to ~9e15), so a budget above the old u32 ceiling survives intact.
  const gateway = new Gateway();
  gateway.addVirtualKey("vk", "alice", "platform", "openai");
  // 5e9 > u32::MAX (4_294_967_295): a u32 budget would truncate this to ~705M.
  gateway.setBudget("group:platform", 5_000_000_000);
  // 5e9 is within the 5e9 cap; 5e9 + 1 is not — no truncation either way.
  assert.equal(gateway.checkBudget("group:platform", 5_000_000_000), true);
  assert.equal(gateway.checkBudget("group:platform", 5_000_000_001), false);
  // spent round-trips the i64; meter a small amount and confirm the value is exact.
  gateway.meterTokens("vk", "openai", "m", 10, 5, 0, 0, "s");
  assert.equal(gateway.spent("group:platform"), 15);
});

test("Gateway meters a group-less virtual key against the vk:* scope and writes a JSONL sink", () => {
  const sink = join(tmpdir(), `sandhi-sink-${process.pid}-${Date.now()}.jsonl`);
  rmSync(sink, { force: true });
  const gateway = new Gateway(sink);
  gateway.addVirtualKey("vk_solo", "solo", undefined, "openai");
  const event = gateway.meterTokens("vk_solo", "openai", "gpt-test", 10, 5, 0, 0, "s");
  assert.ok(!event.groupId, "group-less key has no groupId");
  assert.equal(event.subjectId, "solo");
  // A group-less key records against the vk:* scope (no group budget set → no enforcement).
  assert.equal(gateway.spent("vk:vk_solo"), 15);
  // JSONL sink got exactly one line.
  const lines = readFileSync(sink, "utf8").trim().split("\n");
  assert.equal(lines.length, 1);
  assert.equal(JSON.parse(lines[0]).subject_id, "solo");
  rmSync(sink, { force: true });
});

// ---------------------------------------------------------------------------
// Gateway.usageSnapshotJson (TD-0009 P2): the in-process aggregate surface.
// The corpus is shared with the Python suite so the two cannot drift.
// ---------------------------------------------------------------------------

function parityCorpus() {
  const path = new URL("../../fixtures/usage-snapshot-parity.json", import.meta.url);
  return JSON.parse(readFileSync(path, "utf8"));
}

function drive(gateway, corpus) {
  for (const key of corpus.virtual_keys) {
    gateway.addVirtualKey(key.id, key.subject, key.group ?? undefined, key.upstream);
  }
  for (const call of corpus.calls) {
    gateway.meterTokens(
      call.virtual_key,
      call.provider,
      call.model,
      call.tokens_in,
      call.tokens_out,
      call.cache_creation_tokens,
      call.cache_read_tokens,
      call.session_id ?? undefined,
    );
  }
}

test("usageSnapshotJson matches the cross-language parity corpus", () => {
  // Byte-identical to the fixture the Python suite asserts against (TD-0009 D6/P2).
  const corpus = parityCorpus();
  const gateway = new Gateway();
  drive(gateway, corpus);

  for (const [dimension, expected] of Object.entries(corpus.expected_json)) {
    assert.equal(gateway.usageSnapshotJson(dimension), expected, dimension);
  }

  // Aliases resolve through the one core parser, so `key`/`virtualKey` and `subject`/`user`
  // cannot mean different things here than at /admin/usage?by=.
  assert.equal(gateway.usageSnapshotJson("virtual_key"), corpus.expected_json.key);
  assert.equal(gateway.usageSnapshotJson("user"), corpus.expected_json.subject);
  assert.equal(gateway.usageSnapshotJson(" TOTAL "), corpus.expected_json.total);

  // Neutral units only (ADR-0001): no dollars anywhere in the aggregate.
  const rows = JSON.parse(gateway.usageSnapshotJson("subject"));
  assert.equal(rows[0].key, "bob"); // ranked by billable tokens, busiest first
  assert.ok(rows.every((r) => !("cost" in r) && !("price" in r)));
  // No call reported a duration, so latency is absent rather than zeroed.
  assert.ok(rows.every((r) => !("latency" in r)));

  assert.throws(() => gateway.usageSnapshotJson("cost"), /unknown usage dimension/);
});

test("usageSnapshotJson cap folds overflow and keeps the total exact", () => {
  // TD-0009 D2: the cap loses per-key detail, never the sum.
  const corpus = parityCorpus().overflow;
  const key = corpus.virtual_key;
  const gateway = new Gateway();
  gateway.addVirtualKey(key.id, key.subject, key.group ?? undefined, key.upstream);
  for (const session of corpus.sessions) {
    gateway.meterTokens(
      key.id,
      corpus.provider,
      corpus.model,
      corpus.tokens_in,
      corpus.tokens_out,
      0,
      0,
      session,
    );
  }

  const capped = gateway.usageSnapshotJson("session", corpus.cap);
  assert.equal(capped, corpus.expected_session_json);
  const rows = JSON.parse(capped);
  // cap distinct keys survive; everything after them lands in one honest bucket.
  assert.equal(rows.length, corpus.cap + 1);
  const overflow = rows.find((r) => r.key === "(overflow)");
  assert.equal(overflow.calls, corpus.sessions.length - corpus.cap);

  // The sum is exact under eviction: rows still add up to the grand total.
  assert.equal(gateway.usageSnapshotJson("total"), corpus.expected_total_json);
  const total = JSON.parse(gateway.usageSnapshotJson("total"));
  // The payload is the serde JSON of UsageAggregateV1 — snake_case, not the napi camelCase
  // objects `events()` returns; the schema'd contract shape crosses the seam unchanged.
  assert.equal(total[0].billable_tokens, corpus.expected_total_billable);
  assert.equal(
    rows.reduce((sum, r) => sum + r.billable_tokens, 0),
    corpus.expected_total_billable,
  );
  assert.equal(
    rows.reduce((sum, r) => sum + r.calls, 0),
    corpus.sessions.length,
  );

  // Uncapped, every session keeps its own row.
  const uncapped = JSON.parse(gateway.usageSnapshotJson("session"));
  assert.deepEqual(uncapped.map((r) => r.key), corpus.sessions);
  assert.ok(uncapped.every((r) => r.key !== "(overflow)"));
});

test("parseUsage exercises every built-in provider parser", () => {
  // Anthropic-style.
  const anthropic = parseUsage(
    "anthropic",
    JSON.stringify({ usage: { input_tokens: 7, output_tokens: 3 } }),
  );
  assert.equal(anthropic.tokensIn, 7);
  assert.equal(anthropic.tokensOut, 3);

  // The remaining parsers are selected by slug; missing fields default to zero via unwrap_or_default,
  // so a minimal body still exercises each match arm.
  for (const provider of ["gemini", "cohere", "ollama", "bedrock", "openai_responses", "responses"]) {
    const got = parseUsage(provider, JSON.stringify({}));
    assert.equal(got.tokensIn, 0);
    assert.equal(got.tokensOut, 0);
  }

  // Invalid JSON throws.
  assert.throws(() => parseUsage("openai", "{nope"), /valid JSON/);
});
