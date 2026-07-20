# @anvai-labs/sandhi

Node binding for [**Sandhi**](https://github.com/anvai-labs/sandhi) — *the metering layer for
AI agents*. The Rust core, in-process via napi-rs. Two surfaces: **metering** (virtual keys,
budgets, neutral usage-event emission — zero network hop) and **provider transport**
(`complete` / `stream` through the shared Rust adapters, usage parsed at the source). Mirrors
the Python `sandhi-gateway` API.

```bash
npm install @anvai-labs/sandhi
```

```js
import { Gateway, parseUsage, wireContractVersion } from "@anvai-labs/sandhi";

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

(A stored `registerParser(provider, callback)` — like the Python binding — is a fast-follow.)

`meter()` parses usage **at the source** (same Rust parsers as the proxy), attributes it to the
virtual key's subject/group, records the budget, emits the neutral event, and returns it.
Unknown key or bad JSON → throws.

### Provider transport (in-process)

Forward a provider call through Sandhi's Rust transport in-process — usage is parsed at the
source, so metering trust is single-sourced. `complete()` returns a promise; `stream()` returns a
`ByteStream` that is `for await`-able (bytes forwarded verbatim, usage finalized on the last chunk):

```js
import { complete, stream } from "@anvai-labs/sandhi";

const res = await complete("openai", "gpt-4o", "https://api.openai.com/v1", KEY, bodyJson, "sess-1");
// res.status, res.body (JSON string), res.usage.tokensIn ...

for await (const chunk of await stream("openai", "gpt-4o", BASE, KEY, bodyJson, "sess-1")) {
  process.stdout.write(chunk.data);        // raw upstream bytes
  if (chunk.usage) record(chunk.usage);    // finalized on the terminal chunk
}
```

#### Custom providers through transport (`registerProvider`)

For a provider Sandhi's built-in adapters don't cover, register a JS async handler that owns its own
transport — it then rides `complete()` with the same usage metering (ADR-0047 D10 escape hatch,
parity with the Python binding):

```js
import { registerProvider, complete } from "@anvai-labs/sandhi";

registerProvider("myllm", async (model, bodyJson, sessionId) => {
  const res = await myFetch(model, bodyJson);            // your own transport
  return { status: 200, body: JSON.stringify(res), usage: { tokensIn, tokensOut } };
});
const out = await complete("myllm", "model", base, key, bodyJson, "sess-1");
```

(Streaming for custom providers is not supported — they run through `complete()`.)

Apache-2.0. The transport surface links `sandhi-providers` (async HTTP stack) into the addon.
