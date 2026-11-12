# @anvailabs/sandhi

Node binding for [**Sandhi**](https://github.com/anvai-labs/sandhi) — *the metering layer for
AI agents*. The Rust core runs in-process through napi-rs. It exposes **metering** (virtual keys,
budgets, neutral usage-event emission — zero network hop) and a typed persistent provider runtime.

```bash
# The npm package is not published yet; build the binding from a source checkout.
cd bindings/node
npm ci
npm run build
npm test
```

```js
import { Gateway, parseUsage } from "./sandhi.js";

const gw = new Gateway("usage.jsonl");                 // events append as JSONL (+ in-memory)
gw.addVirtualKey("vk_alice", "alice", "platform", "anthropic");
gw.setBudget("group:platform", 1_000_000);

// ... you make your own provider call and get the raw response JSON ...
const event = gw.meter("vk_alice", "anthropic", "claude-x", responseJson, "conv_7");
// event.tokensIn, event.cacheReadTokens, event.subjectId, ...
gw.spent("group:platform");                            // budget recorded
gw.checkBudget("group:platform", 5000);                // true/false

parseUsage("openai", responseJson);                    // { tokensIn, tokensOut, cache* }
```

### Custom / unknown providers (host escape hatch)

For a provider Sandhi doesn't natively parse, do your own parsing and pass the counts:

```js
gw.meterTokens("vk_alice", "myprovider", "model", tokensIn, tokensOut);
```

`meter()` parses usage **at the source** (same Rust parsers as the proxy), attributes it to the
virtual key's subject/group, records the budget, emits the neutral event, and returns it.
Unknown key or bad JSON → throws.

### Usage snapshots (in-process aggregation)

```js
const rows = JSON.parse(gw.usageSnapshotJson("subject"));   // busiest subject first
rows[0].billable_tokens;                                    // what budgets enforce on
JSON.parse(gw.usageSnapshotJson("total"))[0];               // one grand-total row
JSON.parse(gw.usageSnapshotJson("session", 256));           // bound distinct keys to 256
```

Folds the events recorded so far into
[`usage-aggregate.v1`](https://github.com/anvai-labs/sandhi/blob/main/schemas/usage-aggregate.v1.schema.json)
rows for one dimension — `subject` (`user`), `group`, `provider`, `model`, `key`
(`virtual_key`), `session`, or `total` — using the same fold the reverse proxy, the `sandhi` CLI,
and the dashboard read. Neutral units only, never dollars. The rows are the schema'd contract
shape (snake_case), not napi camelCase. The optional second argument caps distinct keys (default
1024); everything past it folds into a single `"(overflow)"` row, so a long-lived process loses
per-key detail but never the sum. Unknown dimension → throws.

### Provider transport (in-process)

Reuse a provider handle so its HTTP pool, retry policy, timeouts, and circuit breaker survive across
calls. Inputs and outputs are serialized Sandhi chat-contract v1 documents rather than
provider-native JSON:

```js
import { ProviderRuntime } from "./sandhi.js";

const runtime = new ProviderRuntime();
const provider = runtime.provider("openrouter", "openai/gpt-4o", API_KEY);
const request = JSON.stringify({
  schema_version: "1",
  model: "openai/gpt-4o",
  messages: [{ role: "user", content: "hello" }],
});

const response = JSON.parse(await provider.completeJson(request));
for await (const eventJson of provider.streamJson(request)) {
  const event = JSON.parse(eventJson); // response_start, text_delta, usage, finish, ...
}
```

At contract minor 12, `include_native_stream: true` adds `native_frame` events for
Chat, Messages, Responses and Gemini handles. Default is false; other families
reject true. Render native frames or canonical content, and meter canonical usage
once. Opaque provider state remains bound to its compatible backend/account.

`runtime.provider()` accepts explicit `protocol` and the final optional
`endpointProfile` argument independently of the provider label. Its existing
positional arguments are preserved:

```js
const provider = runtime.provider(
  "my-coding-backend", "my-model", API_KEY,
  "https://api.z.ai/api/coding/paas/v4", // baseUrl
  undefined, 0, 30, 10,               // headersJson, maxRetries, timeout, idle
  "bearer", "chat_completions", 10,   // authScheme, protocol, setup timeout
  "openai_terminal_chunk_usage",      // endpointProfile
);
```

Profiles default to `standard`; `anthropic_implicit_cache` is an explicit Messages
convention. `responses` and `gemini_generate_content` select those wire families.
Profiles describe the immediate source, stay out of model request bodies and grant
no credential authority. See [shared endpoint profiles](../../docs/adr/0039-shared-embedded-endpoint-profiles.md)
for family constraints and the remaining gateway second-hop qualification gate.

Prepared 0.17.0 buffered calls share strict source usage qualification with
accounted calls for Chat, Messages, Responses and Gemini. Codex uses the qualified
SSE aggregation path. Missing/invalid counters and malformed known tool fields
produce nonretryable `output_projection_error`, preserving the distinction from
actual network errors. See [buffered qualification](../../docs/adr/0040-shared-buffered-source-qualification.md)
for native/portable behavior and remaining gates.

Minor 13 adds optional `cache_intent`, for example `{mode:"suppress_gateway"}`.
Suppression preserves native controls and adds no gateway markers; it cannot turn
off provider-internal caching. Direct unconfigured handles accept preserve/suppress
and refuse undeclared automatic capabilities before HTTP. Registered gateway
backends can declare exact model/lifetime support; the internal compiled plan is
excluded from JSON and schema. See the [backend guide](../../docs/operator/backends.md).

`completeJson` accepts an optional second JSON-string argument containing per-call wire headers;
`streamJson` accepts the same argument and returns an async iterable. Transport-owned headers and
credentials cannot be overridden. Invalid documents fail before network I/O; provider failures
throw `SandhiProviderError` with a structured `ProviderErrorV1` payload.

Canonical streams may emit accounting-only `usage` events before content or after a
numeric usage snapshot. When `usage.completeness` is `"unavailable"` and a valid
`cache_read_observation` is present, merge that metadata without adding its counters or replacing the last
numeric snapshot/completeness. It is not a second final usage verdict. These events
preserve evidence for consumers that stop reading after content; transparent and
translated provider wire responses are unchanged.

Apache-2.0. The transport surface links `sandhi-providers` (async HTTP stack) into the addon.


### Connect the binding to a Sandhi gateway

Append `peerRole="sandhi_gateway_v1"` to `ProviderRuntime.provider` and select the
client protocol independently of the backend. The positional options remain
`baseUrl, headersJson, maxRetries, timeoutSecs, streamIdleTimeoutSecs, authScheme,
protocol, streamSetupTimeoutSecs, endpointProfile, peerRole`.

```js
const provider = runtime.provider(
  "agent-client", "logical-model", virtualKey, "https://gateway.example/v1",
  undefined, 0, 60, 30, "bearer", "chat_completions", 10,
  undefined, "sandhi_gateway_v1",
);
```

Use the root base for Messages, `/v1` for Chat/Responses and `/v1beta` for Gemini.
HTTPS or loopback HTTP, explicit client protocol and zero retries are required;
backend endpoint profiles and Codex account/protocol options refuse on this peer.
Typed `cache_intent` is forwarded for one server policy compilation without local
cache markers or invented backend capabilities. Native/portable fidelity remains
explicit. Counts metadata describes the immediate wire convention, not upstream
provenance or committed spend; consult source evidence/receipts after translation.
See the [gateway peer contract](../../docs/adr/0041-explicit-gateway-peer-contract.md).
