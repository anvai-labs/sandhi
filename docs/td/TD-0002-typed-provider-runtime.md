# TD-0002: Typed provider runtime

- Status: accepted; implementation in progress
- Contract owner: `sandhi-core`
- Runtime owner: `sandhi-providers`
- First consumer: Victor
- Started: 2026-07-21

## Decision

Sandhi is the authoritative provider boundary. Hosts submit a versioned neutral chat request and
consume a versioned response or stream event. Sandhi owns provider capabilities, wire codecs,
transport, retries, structured errors, and source-of-truth usage metering. Victor owns agent
orchestration, prompt construction, tool selection and execution, conversation repair, UX,
credential acquisition, and pricing display.

```text
Victor -- typed Python FFI --\
HTTP clients -- ingress codec ----> ProviderRuntime --> provider codec --> upstream
Rust hosts -------------------/             |
                                      usage + errors
```

The Python FFI and HTTP gateway are two front doors to the same Rust runtime. They must not grow
separate provider factories, schemas, codecs, retry policy, or usage parsing.

## Compatibility policy

The neutral contract is narrow and versioned, not a generated copy of every provider SDK.
Provider-only fields travel in namespaced `extensions`; unrecognized response state is retained
there when needed for lossless replay. Additive fields are permitted within v1. Breaking semantic
changes require v2. Sandhi has no external users yet, so 0.1.2 ships only the clean typed runtime;
provider-native FFI and duplicated host/proxy factories are removed before release rather than
carried as compatibility debt.

All six Chat Completions roles are represented: `developer`, `system`, `user`, `assistant`,
`tool`, and legacy `function`. Tool messages require `tool_call_id`; legacy function messages
require `name`. Assistant content may be absent when tool calls are present. Content parts cover
text, image URL, input audio, and file inputs. Sandhi validates linkage and shape; Victor repairs
agent history before submitting it.

This role set was rechecked against OpenAI's live Chat Completions OpenAPI on 2026-07-22. The
current role hierarchy is `developer`, `system`, `user`, `assistant`, and `tool`; the older
`function` message/parameters remain compatibility-only and are deprecated in favor of tools.
OpenAI's Responses API is intentionally not modeled as extra chat roles: it uses typed Items such
as `message`, `reasoning`, `function_call`, and `function_call_output`. Sandhi therefore implements
Responses as the distinct `OpenaiResponses` endpoint family mapped into this neutral contract,
not as a seventh Chat Completions role. Sources: OpenAI `POST /v1/chat/completions` and
`POST /v1/responses` OpenAPI plus the official
[`Chat Completions → Responses` migration guide](https://developers.openai.com/api/docs/guides/migrate-to-responses#2-map-messages-to-items).

Authentication is part of protocol selection, not an opaque string attached after routing.
Anthropic Messages supports explicit `api_key` (`x-api-key`) and `bearer` (`Authorization`)
schemes, and must emit exactly one credential header. OpenAI API keys use Chat Completions bearer
auth; ChatGPT subscription OAuth selects the distinct Responses family. Unknown or incompatible
auth schemes fail before network I/O.

The Responses family has two explicit profiles. `responses` is the public API and supports both
complete and stream calls. `chatgpt_responses` is the subscription/Codex backend profile: its base
URL has no `/v1`, it requires non-empty instructions plus item-array input, forces `store=false`
and upstream SSE, and aggregates that typed event stream when a host asks for `complete`. Victor
also forwards the optional `ChatGPT-Account-ID` workspace header extracted by its credential
layer. These are protocol facts, not token-string or URL heuristics.

## Normative v1 surface

- `ChatRequestV1`: model, typed messages, tools/tool choice, generation controls, response format,
  session/attribution metadata, and namespaced extensions.
- `ChatResponseV1`: id/model, typed assistant output, finish reason, `UsageV2`, and extensions.
- `ChatStreamEventV1`: response start, text/reasoning/refusal deltas, tool-call start/argument/end,
  usage, finish, and structured error. Tool call index and id are never discarded.
- `ProviderErrorV1`: stable code, message, retryability, optional HTTP status/provider/request id,
  and details.
- `ProviderDescriptorV1` / `ModelDescriptorV1`: endpoint family, aliases, capabilities, limits,
  defaults, and model routing.
- `UsageV2`: v1 fresh/cache totals plus optional modality, reasoning, prediction, completeness,
  attempts, outcome, and upstream request id.

## Runtime and front doors

`ProviderRuntime` creates persistent, shareable provider handles. A handle owns the HTTP pool and
resilience state; calls do not rebuild adapters. The Python shape is:

```python
runtime = sandhi_gateway.ProviderRuntime()
provider = runtime.openai_compat("openrouter", base_url, api_key, headers_json="{}")
# Responses is deliberately explicit; auth tokens and URLs never guess the wire protocol.
responses = runtime.openai_responses("openai", base_url, bearer_token, headers_json="{}")
response_json = await provider.complete_json(request_json)
async for event_json in provider.stream_json(request_json):
    ...
```

The JSON methods are an ABI-stable bridge for typed v1 documents, not provider-native JSON. A
generated Python model layer may wrap them without changing Rust. The proxy will decode OpenAI
`/v1/chat/completions` and Anthropic `/v1/messages` ingress into the same contract; native
passthrough is allowed only when ingress and upstream dialects match.

## Provider migration ledger

Legend: schema = descriptor/model facts; codec = typed request/response/stream; host = Victor uses
typed path; proxy = normalized gateway route.

| Family | Providers | Schema | Codec | Host | Proxy |
|---|---|---:|---:|---:|---:|
| OpenAI-compatible wave 1 | OpenAI, Together, Fireworks, OpenRouter, xAI, Mistral | [x] | [x] | [x] | [x] |
| OpenAI-compatible specialized | DeepSeek, Moonshot, ZAI, Groq, Cerebras, Qwen | [x] | [x] | [x] | [x] |
| Native typed | Anthropic, Gemini, Ollama, Cohere | [x] | [x] | Anthropic/Gemini/Ollama [x]; Cohere n/a | OpenAI/Anthropic ingress [x] |
| OpenAI-compatible local | llama.cpp, vLLM, LM Studio | explicit endpoint | [x] | [x] | [x] |
| OpenAI Responses | OpenAI API / ChatGPT subscription OAuth | explicit family | complete + stream [x] | FFI [x] | ingress pending |
| Explicit Victor-native protocols | Azure, Hugging Face, Vertex, Bedrock, Replicate | outside 0.1.2 | outside 0.1.2 | explicit | n/a |
| Host extensions | MLX and private providers | outside 0.1.2 | outside 0.1.2 | explicit | n/a |

The `0.1.2` admitted set is the first four rows. Azure deployment URLs/API-version headers,
Hugging Face model-path endpoints, Vertex/Bedrock credential signing, Replicate prediction jobs,
and in-process MLX are different protocols rather than OpenAI-compatible aliases. They must get
typed codecs or remain explicitly outside Sandhi; they may not be smuggled through the generic
OpenAI family.

## Implementation ledger

- [x] Persist boundary, compatibility policy, migration matrix, and gates.
- [x] Define canonical chat messages, content parts, tools, responses, stream events, errors, and
  usage v2 in `sandhi-core`.
- [x] Generate and check JSON Schema plus Python/TypeScript model facades from the Rust contract.
- [x] Move the provider factory into one reusable `ProviderRuntime` used by all bindings/proxy.
- [x] Add persistent Python runtime/provider handles.
- [x] Implement typed OpenAI-compatible complete codec.
- [x] Implement chunk-boundary-safe typed OpenAI-compatible stream codec.
- [x] Prove the persistent typed handle with a temporary flag-gated Victor bridge. This is test
  scaffolding only and must be deleted when the direct typed consumer lands.
- [x] Route Victor wave-1, specialized admitted, and OpenAI-compatible local providers through
  typed complete and stream handles.
- [x] Add Anthropic/Gemini/Cohere/Ollama typed codecs.
- [x] Add a distinct OpenAI Responses item/event codec, source-of-truth usage parser, explicit
  Python/Node protocol selector, and Victor subscription-OAuth FFI path.
- [x] Make OpenAI and Anthropic proxy ingress explicit and normalize through the runtime.
- [x] Add atomic budget reservation/reconciliation using usage completeness and attempt outcome.
- [x] Delete provider-native FFI functions and duplicate binding factories before 0.1.2.
- [x] Move canonical provider endpoints, aliases, capabilities, model routes, and provider header
  names out of Victor and into Sandhi descriptors.
- [x] Delete the admitted OpenAI-compatible cloud providers' bypassed HTTP/SSE methods and
  rewrite their old direct-wire tests as typed boundary/policy tests.
- [x] Extract the remaining admitted native/local host policy, then delete their bypassed direct
  transport methods and tests from Victor.
- [x] Decide and enforce the 0.1.2 support boundary for Azure, Hugging Face, Vertex, Bedrock,
  Replicate, and MLX; do not publish with an ambiguous fallback.

Victor enforces that boundary in `sandhi_transport.resolve_transport_class`: every Victor-owned
provider must resolve to a Sandhi typed transport or appear in the explicit native-only alias set.
An unclassified future provider fails closed. Third-party provider classes remain extensible.

### Remaining Victor deletion ledger

Transport selection is already fail-closed, so these Python methods are bypassed in normal
registry construction. They are still source duplication and must be removed only after their
non-transport responsibilities have a named home:

| Victor class | Keep/extract in Victor | Move/delete after parity |
|---|---|---|
| `OpenAIProvider` | OAuth acquisition/refresh, account-id extraction, model capability/context policy | SDK client plus Chat Completions complete/stream/parser/error wire methods |
| `AnthropicProvider` | OAuth acquisition/refresh and agent cache-placement intent | SDK client, Messages request/response/stream wire methods; express cache intent neutrally |
| `GoogleProvider` | Google credential acquisition and model capability/context policy | SDK client and Gemini generate/stream codecs |
| `OllamaProvider` | discovery, lifecycle, local model/context policy | `/api/chat` complete/stream parsing |
| `LMStudioProvider`, `VLLMProvider`, `LlamaCppProvider` | health/discovery/lifecycle and local model policy | `/chat/completions` HTTP/SSE execution |

The deletion gate is structural: an admitted Victor provider may retain discovery or credential
I/O, but its `chat`/`stream` call graph must terminate at a Sandhi typed handle and it must not own
a provider generation client. Direct instantiation tests must be rewritten against the resolved
Sandhi variant before deleting each old method set.

## Acceptance gates

1. Every role and content part round-trips through serde and rejects invalid role linkage.
2. Complete and stream parity fixtures cover text, refusal, parallel tool calls, finish reasons,
   usage/cache splits, unknown fields, and arbitrary byte chunk boundaries.
3. One persistent handle demonstrably reuses its adapter/client and circuit breaker.
4. Victor native-vs-Sandhi parity tests pass before enabling a provider wave.
5. FFI and proxy return the same structured error and usage documents for the same fixture.
6. No provider/model wire fact is duplicated in Victor after its migration checkbox is enabled.
7. No provider-native FFI or Victor demotion path remains in the 0.1.2 artifact.
8. API-key and subscription-OAuth modes select an explicit, tested protocol/auth scheme; no OAuth
   token is silently sent using the API-key header or wrong endpoint family.

## Release policy

The implementation rows above are internal milestones, not external package releases. The last
published version is 0.1.1 and there are no external users. Complete the entire admitted TD-0002
scope, delete the superseded raw surfaces, pass every release gate, and publish it once as 0.1.2
across crates.io, PyPI, and npm. Do not publish partial stepping stones.
