// SandhiProviderError rewrap contract (TD-0008 P2/C): typed payloads become the
// class; binding-internal failures pass through untouched.
import assert from "node:assert/strict";
import { test } from "node:test";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const sandhi = require("../sandhi.js");

test("SandhiProviderError exposes typed fields from a ProviderErrorV1 payload", () => {
  const payload = {
    code: "upstream_error",
    message: "upstream status 400: bad tool pairing",
    retryable: false,
    http_status: 400,
    provider: "moonshot",
    request_id: "req_abc",
    details: { upstream_body: "{\"error\":\"bad tool pairing\"}" },
  };
  const err = new sandhi.SandhiProviderError(payload, JSON.stringify(payload));
  assert.equal(err.name, "SandhiProviderError");
  assert.equal(err.code, "upstream_error");
  assert.equal(err.httpStatus, 400);
  assert.equal(err.retryable, false);
  assert.equal(err.requestId, "req_abc");
  assert.ok(err instanceof Error);
  assert.equal(err.details.upstream_body, "{\"error\":\"bad tool pairing\"}");
});

test("async iterator rewraps typed error messages and passes others through", async () => {
  const typedJson = JSON.stringify({ code: "rate_limited", message: "rate limited (429)", retryable: true, http_status: 429 });
  const makeStream = (error) => {
    const stream = Object.create(sandhi.TypedEventStream.prototype);
    stream.read = async () => { throw error; };
    return stream;
  };

  await assert.rejects(
    async () => { for await (const _ of makeStream(new Error(typedJson))) { /* drain */ } },
    (err) => err instanceof sandhi.SandhiProviderError && err.code === "rate_limited" && err.retryable === true
  );

  const plain = new Error("segfault in binding");
  await assert.rejects(
    async () => { for await (const _ of makeStream(plain)) { /* drain */ } },
    (err) => err === plain && !(err instanceof sandhi.SandhiProviderError)
  );
});
