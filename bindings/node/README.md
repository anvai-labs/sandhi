# @anvai-labs/sandhi

Node binding for [**Sandhi**](https://github.com/anvai-labs/sandhi) — *the metering layer for
AI agents*. The Rust core, in-process via napi-rs: **virtual keys, budgets, and neutral
usage-event metering** with zero network hop. Mirrors the Python `sandhi-gateway` API.

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

`meter()` parses usage **at the source** (same Rust parsers as the proxy), attributes it to the
virtual key's subject/group, records the budget, emits the neutral event, and returns it.
Unknown key or bad JSON → throws.

Apache-2.0. Depends only on `sandhi-core` — no HTTP transport in the addon.
