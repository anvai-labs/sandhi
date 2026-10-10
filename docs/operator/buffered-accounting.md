# Owned buffered accounting

!!! warning "Opt-in accounting; lifecycle acceptance remains separate"
    `SANDHI_BUFFERED_ACCOUNTING=tracked` enables durable buffered HTTP accounting.
    It is included in the [0.12.0 release scope](../releases/v0.12.0.md); check that
    page for publication status. A package release does not establish deployment
    acceptance. Unset or `off` preserves the existing path. Any
    other value fails startup.

Keep the existing authentication, TLS, credential and `SANDHI_STORE` configuration.
This switch grants no permissions and changes no provider credentials.

## Follow the evidence

```mermaid
flowchart TD
    A[Authorized request] --> B[Persist intent and request ID]
    B --> C[Authorize one provider dispatch]
    C --> D[Observe qualified usage]
    D --> E[Retain usage in process memory]
    E --> F[Persist terminal observation]
    F --> G[Commit accounting receipt]
    E -. storage failure .-> H[Retry retained accounting work]
    H --> F
    E -. process dies before persistence .-> I[Unknown liability remains held]
    E -. best effort .-> J[Metrics and usage event]
```

| Evidence | What it proves | What it does not prove |
|---|---|---|
| Request ID and prepared intent | One correlated admission is durable | The provider executed or charged |
| Usage in memory or metrics | The gateway observed reported usage | Persistence or committed spend |
| Durable terminal observation | The original usage snapshot survived storage | Settlement has committed |
| Accounting receipt | The original ledger committed accounting | Atomic telemetry delivery or external billing |

A disconnected client does not cancel owned admission, the provider call or
settlement. Recovery never repeats inference or re-emits usage events.
Usage is retained before telemetry callbacks; a failing sink cannot discard the
snapshot needed for accounting recovery.

## Supported activation

| Surface | Required in tracked mode | Refused before admission |
|---|---|---|
| Ledger | One file-backed ledger, fixed topology | Volatile or sharded ledgers fail startup |
| Ingress | Buffered OpenAI Chat or Anthropic Messages | Other dialects and streaming |
| Upstream | Built-in, retry-free OpenAI-compatible transport | Custom transports; expiring credential handles without raw transport |
| Policy | Explicit output limit and Block policy | Warn, fail-open, retry and logical-idempotency combinations |
| Processing plane | Existing transparent or translated transport | No silent downgrade to legacy accounting |

## Bounds and configuration

| Bound | Standalone value | Configuration / meaning |
|---|---|---|
| Retained owners | 64 | One slot spans the detached HTTP operation |
| Each accounting wait | 2 seconds | A timed-out wait does not cancel or repeat a possibly committed transition |
| Transport deadline | 120 seconds | Existing global/endpoint/model buffered policy may override, up to 600 seconds |
| HTTP wait | Transport + three accounting waits | A longer client timeout cannot extend it |
| Retained intent capacity | 100,000 | Retention and migration are separate work |

Embedders can call `settlement::buffered::enable(&Arc<ProxyState>, Config::new(...))`
before serving. Configuration is immutable after enable. The library validates
capacity 1–1024, accounting waits up to 30 seconds and transport deadlines up to
600 seconds. See [buffered deadline policy](buffered-deadlines.md).

## Interpret a response

| Outcome | Client result | Required interpretation |
|---|---|---|
| Final qualified usage and committed settlement | Normal provider result | Consult the receipt for committed accounting |
| Missing/malformed usage, transport failure, accounting timeout or failed settlement | Correlated 502 | Upstream may have completed; **do not automatically retry** |
| Explicit reported zero | May settle zero | Absence of usage is not reported zero |
| Client disconnect | Client has no confirmed result | Gateway ownership continues; inspect evidence |

The client may not receive completed model output when accounting is unresolved.
A 502 is not proof the model failed.

Each admitted request gets one generated Sandhi ID, committed atomically with its
prepared intent. Responses carry `x-sandhi-request-id`; tracked usage events use
that same ID and preserve an available origin ID separately. The ID is not an
execution ID, idempotency key or replay authority. Old admissions retain absent
correlation. Response-body completion IDs are not substituted for HTTP request IDs.
The typed provider response currently does not preserve success response headers;
an absent origin ID does not invalidate the gateway/intent/receipt join.

## Recover and shut down

| Situation | Behavior |
|---|---|
| Process survives a storage failure | Retry the retained, immutable terminal snapshot |
| Durable final usage survives a restart | Recover settlement through the original ledger |
| Process dies before terminal persistence | Keep liability unresolved; RAM cannot establish durable evidence |
| Recovery sweep | Once per second; at most eight batches, 32 execution records and one retained terminal retry per batch |
| Sweep progress | Advance scope/execution cursors, including unbudgeted and removed-key scopes |
| Shutdown | Drain workers, then check fresh bounded durable inventory plus retained owners |
| Unknown rows, incomplete inventory or timeout | Incomplete shutdown, exit 124 |

The one-second recovery check happens between batches. These are iteration/record
bounds, **not a hard SQLite latency bound**. Recovery settles only authoritative
final usage; unavailable observations stay held. Shutdown keeps the original grace
deadline and watchdog; a large backlog may need more recovery before shutdown.
Do not restore older writers over a database containing tracked evidence.

## Accepted source evidence and remaining gates

| Gate | Source evidence | Scope |
|---|---|---|
| Unknown dispatch after SIGKILL | Liability and incomplete shutdown survive restart | [#339](https://github.com/anvai-labs/sandhi/pull/339) |
| Persisted final usage | One receipt recovered; another restart preserves spend and identity | #339, synthetic receipt-write failure |
| Terminal publication contention | Retained retry commits once if process survives; death preserves unknown liability | [#340](https://github.com/anvai-labs/sandhi/pull/340) |
| Tracked buffered TLS/OIDC | CA-verified shutdown and OIDC accounting/denial joins; existing browser role matrix | [Synthetic source acceptance](../product/attempt-accounting-and-evidence.md#tracked-buffered-tlsoidc-source-acceptance) |
| OIDC crash/restart | Unknown liability retains its bound scope; final usage settles once after inference-grant revocation; new dispatch is denied without accounting changes | Same two recovery cases parameterized over token and TLS/OIDC modes |
| Released binary with deployed OIDC policy | 0.12.0 isolated TLS candidate: both Qwen routes and ZAI reconcile three receipts / 51 tokens | [Qualification and limits](../product/attempt-accounting-and-evidence.md#isolated-released-binary-oidc-qualification-2026-10-10); managed gateway unchanged |
| Managed tracked OIDC and streaming lifecycle | Pending | Separate acceptance gates |
| Strict streamed usage qualification | Core event qualifier shares buffered counter validation; normal null usage remains unavailable | [Source prerequisite](../product/attempt-accounting-and-evidence.md#openai-chat-stream-usage-qualification-w05c-prerequisite); no transport wiring or tracked streaming activation |
| Executed cache reuse, tokenizer correctness and actual-member C5 | Pending | Source drills cannot establish these |
| Released tracked-mode deployment | Pending | Existing 0.12.0 default-mode deployment does not qualify tracked lifecycle |

The restart drills use the same copied/hashed binary and local synthetic provider
responses. The two original SIGKILL drills run with token compatibility and
CA-verified TLS/OIDC using the disposable authority; terminal-publication contention
remains a separate token-mode drill. Each successful drill observes one origin request.
See the [full acceptance record](../product/attempt-accounting-and-evidence.md#tracked-buffered-crashrestart-acceptance).

!!! note "Telemetry is not a settlement outbox"
    Events, metrics and traces are best effort. Counters can increase before
    persistence, and a success-labelled observation does not prove a successful
    client response. The event may be absent after a sink failure. Tracked threshold
    alerts, atomic telemetry export, observation amendments and multi-shard migration
    remain separate work.
