# Owned buffered accounting

`SANDHI_BUFFERED_ACCOUNTING=tracked` enables the first durable HTTP accounting
mode. Unset or `off` preserves the existing path. Other values fail startup.
Keep existing authentication, TLS, credential and `SANDHI_STORE` configuration;
this switch does not grant permissions or change provider credentials.

The first activation is intentionally limited:

- One file-backed ledger with fixed topology. Volatile or sharded ledgers fail startup.
- Buffered OpenAI Chat or Anthropic Messages ingress, through a built-in,
  retry-free OpenAI-compatible upstream. Both transparent and translated paths use
  the existing transport. Other dialects, streaming, custom transports and expiring
  credential handles without a raw transport are refused before admission.
- An explicit output limit, Block policy, and no logical idempotency key. Warn,
  fail-open and retry combinations are refused, not silently downgraded.

Standalone bounds are 64 retained owners, two seconds per accounting wait and a
120-second default transport deadline. Existing global/route/model buffered
policies override the transport deadline, up to 600 seconds in tracked mode. A
longer HTTP client timeout does not extend it. Embedders can call
`settlement::buffered::enable(&Arc<ProxyState>, Config::new(...))` before serving;
configuration is immutable after enable. The library validates capacity 1–1024,
accounting waits up to 30 seconds and transport deadlines up to 600 seconds.
The HTTP wait is bounded by transport plus three accounting waits. A timeout
never cancels or repeats a possibly committed accounting transition.

Each admitted tracked request gets one generated Sandhi ID, committed with its prepared intent
in the same SQLite transaction. Responses carry `x-sandhi-request-id`; tracked
usage events use that same ID, keeping an available upstream request ID separate.
Response-body completion IDs are not substituted for HTTP request IDs. Origin
header availability remains transport-dependent; the typed provider response
currently does not preserve success response headers. An absent origin ID does
not invalidate the gateway/intent/receipt correlation.

A disconnected HTTP client does not cancel the owned admission, provider call or
settlement. Once usage is available it is retained before telemetry callbacks.
Missing or malformed usage, transport failure, accounting timeout and failed
settlement return a correlated 502 stating that accounting is unresolved and the
upstream may have completed. **Do not automatically retry that response.** Explicit
reported zero can settle; absence cannot. A 502 is not proof the model failed.
The client may not receive the completed model output if accounting is unresolved.

Recovery runs once per second on the original ledger. A slice visits at most eight
batches, each with at most 32 execution records and one retained terminal retry;
the one-second elapsed check occurs between batches. These are record/iteration
bounds, not a hard SQLite latency bound. Recovery advances scope and execution
cursors, including unbudgeted and removed-key scopes. Only authoritative final
usage can settle; uncertain dispatches and unavailable observations remain held.
No model calls or usage events are replayed. In-process terminal snapshots can be
retried after storage failure; a process crash before their persistence still
requires reconciliation, because RAM is not durable evidence.

Shutdown uses the original grace deadline and process watchdog. After admitted
workers drain, a fresh bounded inventory checks durable uncertainty as well as
retained owners. Unresolved rows, an incomplete inventory or timeout produce an
incomplete shutdown (exit 124), even with zero active workers. A large backlog may
require more recovery before shutdown; the final check does not extend grace.

Receipts establish committed accounting. Usage events, metrics and tracing are
best-effort observations and may be absent after a telemetry failure; they are
not an atomic receipt-delivery outbox. Tracked token counters can increase before
terminal usage or settlement persists; a success-labelled observation does not
prove a successful client response or committed spend. Threshold alerts are not fired by this
tracked path. Retained intent capacity remains bounded at 100,000; retention,
observation amendments, network export and multi-shard migration are separate
work. Do not restore older writers over a database containing tracked evidence.

This mode does not establish complete streaming lifecycle, executed-cache reuse,
tokenizer correctness, actual-member C5 acceptance, or a released deployment.
See [the owning contract](../product/attempt-accounting-and-evidence.md).


The local SDK recovery drills now exercise actual SIGKILL and restart with the same
hashed binary: unknown dispatch keeps liability and incomplete shutdown; persisted
final usage recovers one receipt after a synthetic receipt-write failure. A second
restart preserves spend and receipt identity. See the
[acceptance scope and remaining gates](../product/attempt-accounting-and-evidence.md#tracked-buffered-crashrestart-acceptance).
Additional contention drills witness gateway usage before terminal persistence:
survival retries retained usage into one receipt, while death before persistence
retains unknown liability. Neither branch repeats the provider request.
These synthetic HTTP/token-mode checks do not establish tracked TLS/OIDC, streaming
or deployed-provider acceptance.
