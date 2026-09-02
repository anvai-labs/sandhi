# @anvai-labs/sandhi

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

`completeJson` accepts an optional second JSON-string argument containing per-call wire headers;
`streamJson` accepts the same argument and returns an async iterable. Transport-owned headers and
credentials cannot be overridden. Invalid documents fail before network I/O; provider failures
throw `SandhiProviderError` with a structured `ProviderErrorV1` payload.

Apache-2.0. The transport surface links `sandhi-providers` (async HTTP stack) into the addon.
