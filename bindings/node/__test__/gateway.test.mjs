import assert from "node:assert/strict";
import { createServer } from "node:http";
import { test } from "node:test";

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
  assert.equal(gateway.spent("group:platform"), 60);
});
