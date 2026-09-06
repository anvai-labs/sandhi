# Gateway product and implementation review

Date: 2026-09-04. Sandhi baseline: `ed1781e`.
Delivery tracker: [TD-0026](../td/TD-0026-gateway-product-evolution.md).

Implementation follow-up (2026-09-04): W01 fixes F01 and F07's dashboard construction/storage
issues and F06's dashboard read-error suppression. Browser, database-fault and real AgentBrowser
smoke tests passed. F06's durable evidence/outbox work remains W05; scoped management remains
W07. Findings and source line references below describe the original baseline, not the updated
working tree. See the delivery tracker for current state and exact verification commands.

Implementation follow-up (2026-09-05): W02 fixes F02's swallowed budget commit failure, serializes
commit/live publication, rejects invalid budget intent, and reports failed/partial config and
inline-alert outcomes. Fault/restart/concurrency and browser/CLI tests pass. Cross-store atomicity
and revision conflict detection are not claimed; the latter remains W08. The finding below is
retained as baseline evidence.

Implementation follow-up (2026-09-05): W03 fixes F04 for new measurements with explicit reasoning
inclusion across parsers, events, aggregates, SQL, bindings and settlement; it also corrects
Gemini completion ordering so translated Responses clients receive terminal usage. Historical
ambiguous rows retain legacy arithmetic and are not advertised as repaired. F03's guarantee
claim is corrected: admission remains an estimate and multiple in-flight calls may overshoot;
no strict-cap eligibility is claimed. See the [metering and budget contract](../product/metering-and-budget-guarantees.md)
and W03's recorded test evidence. Proven model/category bounds remain future work.

Implementation follow-up (2026-09-05): W04 fixes F08's native runtime boundary with bounded
worker execution and offloaded serialized handlers, tested against a disposable daemon through
the real admin endpoint. Read-only reference onboarding, explicit local capabilities, typed
failures, redaction and exact-label restart recovery are implemented. Local revocation does
not imply broker/provider revocation; F09's cached credential lifetime remains W09, not resolved
by this slice. See the [broker contract](../product/broker-integration-contract.md). Live broker,
Windows and joint security certification remain separate gates.

Implementation follow-up (2026-09-05): W05a adds atomic settlement-receipt/outbox storage with
conflict detection and bounded, fenced delivery claims. Store tests cover rollback, process
exit/reopen, independent writers, acknowledgement tombstones and migration refusal. This is a
foundation for F05/F06, not their closure: the proxy does not call the new API yet, physical
attempt capture and unresolved liability remain absent, and no external exporter is enabled.
See [W05a–e gates](../product/attempt-accounting-and-evidence.md).

## Assessment

Sandhi has a credible usage gateway foundation: four SDK-facing ingress dialects, transparent
and translated forwarding, virtual keys, authoritative attribution, durable single-node leases,
cache-aware measurement, stream finalization, rate limiting, a usable operator API/CLI, and
Prometheus/OTLP support. Extending this foundation is preferable to replacing its crate boundaries.

The principal product gap is confidence: an operator must know whether data is complete, a control
was actually applied, a credential remains authorized, and a budget is exact or estimated. Some
existing implementation paths undermine that confidence even when their happy-path tests pass.
Resolve these before promoting fleet-wide governance or expanding transport breadth.

## Evidence and limits

Reviewed README, architecture records and the TD index; selected active designs; core usage,
budget and event contracts; proxy admission, dashboard, operator API, ledger and telemetry paths;
provider resilience; store buffering and vault integration; related test suites and CI workflows.
This is a targeted product/architecture/code review, not an exhaustive line-by-line security audit.

Verification performed:

- `cargo test --workspace`: passed, exit 0. The initial offline attempt failed because the local
  registry only knew protocol 0.8.0; online resolution succeeded without a manifest or lockfile edit.
- `cargo test -p sandhi-proxy --features sentinelpass-ipc --quiet`: passed; 172 tests across five
  test binaries. This verifies the feature build and existing tests, not a live daemon lifecycle.
- No browser automation, live provider billing experiment, multi-replica fault test, or SentinelPass
  daemon test was run. Browser and fault regressions below are implementation acceptance work.
- SentinelPass source was inspected remotely at `00d1e7de09e3d360954240be7588dd7c73ac317e`;
  the requested sibling directory was absent. See the [co-design](../upstream/sentinelpass-gateway-codesign.md).

Evidence labels: **Confirmed** means the cited code directly demonstrates the behavior;
**Gap** means an absent capability or documented limitation; **Validate** means a plausible failure
whose runtime trigger still needs a focused reproduction. P1 means lead the next correctness
delivery; P2 means needed for the target product; P3 means evidence-gated expansion.

## Findings and corrective designs

| ID | Priority / evidence | Finding, trigger, and consequence | Implementation and acceptance |
|---|---|---|---|
| F01 | P1 / Confirmed | Dashboard `loadUsage`, `loadKeys`, `loadBudgets`, and `loadAlerts` call bare `fetch`, while `require_dashboard_access` requires the admin bearer when configured. Entering the token enables mutations but does not authenticate those reads. Non-success reads render empty/zero panels. Evidence: `crates/sandhi-proxy/src/lib.rs:1315`, `:1621`, `:1671`, `:1732`, `:1789`, `:1827`; API auth is correctly tested in `tests/operator.rs:1142`. | One authenticated HTTP wrapper for reads and writes; reload all protected panels after authentication changes; explicit locked, forbidden, unavailable, empty and stale states. Browser test: configured token → visible seeded usage; wrong token → authentication state, never zero usage. |
| F02 | P1 / Confirmed | `ProxyLedger::set_budget` logs durable failure and returns `()`. `apply_budget` still updates the in-memory spec and the API returns success. A failed persistence write can make the displayed policy differ from enforcement. Evidence: `crates/sandhi-proxy/src/ledger.rs:114`, `operator.rs:552`, `:742`. | Propagate typed errors; commit durable state before publishing a new live revision; validate policy/window values rather than defaulting malformed values. Inject a store write failure: API fails, revision and effective cap remain unchanged across restart. Apply the same rule to config application. |
| F03 | P1 / Confirmed | `input_estimate` uses bytes/4. `RequestAccounting` explicitly permits measured settlement above the reservation. The earlier admission comment says a hard cap cannot be overshot, contradicting that behavior. Concurrent underestimated calls compound the excess. Evidence: `lib.rs:2335`, `:2990`, `:3262`, `:3270`. | Publish guarantee classes; provider/model-aware input bounds and explicit output/reasoning bounds for strict admission. Unsupported bounds cannot be called strict. Keep real settlement above estimates, expose overshoot and uncertain liabilities. Test multilingual, tools, media, reasoning and concurrent near-cap calls. |
| F04 | P1 / Confirmed | `billable_parts` infers separate reasoning solely from `reasoning_tokens > tokens_out`. Gemini parsing keeps candidate and thought counts separately. Its existing fixture has input 100, output 40, reasoning 25; the arithmetic counts 140 despite storing 25 separate thought tokens. Evidence: `sandhi-core/src/event.rs:43`, `usage.rs:213`, `:344`. | Replace magnitude inference with explicit provider normalization or a versioned inclusion semantic; preserve category provenance. Test separate reasoning below, equal to and above output, plus folded-provider cases, through parser → event → aggregate → ledger. Define treatment of historical ambiguous rows before migration. |
| F05 | P1 / Confirmed | Idempotent replay settles each physical request's lease, then suppresses the duplicate logical usage event and returns before usage metrics/alerts. Thus physical enforcement spend can exceed exported logical usage. An `attempts` count already exists, but is not an attempt ledger. Evidence: `lib.rs:3010`, `:3040`; `sandhi-proxy/src/ledger.rs:79`; `sandhi-core/src/event.rs` `attempts`. | Preserve existing logical event semantics; introduce durable per-attempt measurement and reconciliation linked to logical request ID. Report logical activity separately from measured physical consumption. Repeated idempotency key with two billable responses must expose both attempts without double-counting one delivered logical result. |
| F06 | P1 / Confirmed | Usage/keys/alerts reads suppress storage errors using `.ok()` or `unwrap_or_default()`. The dashboard converts absent totals to zero. `BufferedSink` deliberately drops on a full/closed queue; observation is independent of enforcement. Evidence: `lib.rs:1323`, `:1354`, `:1412`, `:1671`; `sandhi-core/src/sink.rs` `record_drop`, `Sink::emit`. | Typed unavailable/partial responses and data freshness; queue depth/drop/oldest-event metrics. Durable outbox for financially authoritative export, separate from optional best-effort telemetry. Inject full queue, store outage, crash and replay; never report complete totals for missing records. |
| F07 | P1 / Confirmed unsafe construction; exploitability to validate | `keysView` inserts provider/label strings into inline JavaScript handlers; `esc` escapes HTML but not the single-quoted JavaScript context. Admin-controlled credential metadata can therefore break handler syntax. A bearer is kept in `sessionStorage`, increasing consequences of any script injection. Evidence: `lib.rs:1611`, `:1621`, `:1695`. | Move to DOM text nodes and bound event listeners; remove inline handlers, adopt CSP and prohibit secrets in URLs/logs. Browser test punctuation and script-like metadata as inert text. Do not characterize this as unauthenticated compromise without proving the metadata write path. |
| F08 | P1 / Validate | Native IPC calls `Runtime::block_on` behind the synchronous `Vault` trait. The async `add_key` handler calls `vault.set` directly. With a readable IPC token, the native backend may enter a nested Tokio runtime and panic; the same bridge also needs bounded execution. Evidence: `sandhi-store/src/vault.rs:214`, `:224`; `operator.rs:324`. | Reproduce using a fake daemon through the real admin endpoint, then use an async vault boundary or bounded dedicated worker; construction/drop must also be runtime-safe. Cover lookup, save, lock, timeout and shutdown in IPC-enabled CI. |
| F09 | P1 / Gap, confirmed cache behavior | Provider handles contain resolved secret material and are cached on registration/rehydration. There is no grant invalidation subscription or secret lease expiry in the reviewed adapter. Revoking a secret-read grant does not retract cached credentials. Evidence: `operator.rs:346`; `main.rs` provider rehydration; `sandhi-store/src/vault.rs:177`. | Define credential generations, validity, revalidation and invalidation; distinguish broker grant revoke, gateway key revoke and provider-key revoke. Prove a bounded cutoff for new dispatch with the co-design lifecycle tests. |
| F10 | P2 / Gap | `budget_scope` selects one explicit, group, or key scope. Store and operator specs carry one window per scope; rate limits are per-process RPM. These do not implement simultaneous organization/project/team/key/run caps or global fleet limits. Evidence: `lib.rs:3484`; `operator.rs:135`; `ratelimit.rs`; TD-0007. | Atomic reservation over all applicable scopes/windows; RPM, TPM and concurrency controls with explicit deployment guarantees. Multi-scope failure rolls back every reservation; two replicas cannot each spend a whole shared cap. |
| F11 | P2 / Gap | Admin API uses one all-powerful bearer; dashboard shares that authority. Public reads without an admin token are documented behavior, not an auth bypass. No scoped operator role or policy-mutation audit surface is present in the reviewed router. Evidence: `operator.rs:184`; `lib.rs:599`, `:1315`; SECURITY.md. | Scoped management credentials and service identities, tenant-authoritative query filters and append-only mutation records. Enterprise identity provisioning stays downstream; local authorization is enforced here. Make public/development posture explicit in setup. |
| F12 | P2 / Confirmed | Only static `/healthz` exists; no `/readyz` route. Draining, backlogs and credential degradation are not presented as a coherent operator state. Evidence: `lib.rs:607`, `:866`, `:1249`; TD-0020. | Drain-aware readiness, bounded queue/admission metrics and an operations view. Ledger outage remains per-policy admission behavior, as TD-0020 resolves; do not evict every warn-policy request via readiness. |
| F13 | P2 / Gap | Reservation history and total-window SUM work grow with lifetime admissions; retention/rollup work is already TD-0024. Provider/funnel co-edit risks are already TD-0025. | Reuse those designs after reconciling stale assumptions. Longevity benchmarks, crash-safe rollups and a plane-differential harness precede structural refactoring. |
| F14 | P2 / Gap | Retries/timeouts/circuit breaking exist, but governed destination selection, fallback policy, residency-aware routes and route simulation are not established product contracts. `CircuitBreaker::allow` permits callers after cooldown without claiming a single half-open probe despite its comment. Evidence: `sandhi-providers/src/resilience.rs:69`; TD-0005. | Pin breaker state transitions with a concurrent probe test; add eligible destinations and deterministic routing only after attempt accounting. Bound retry amplification; prohibit automatic retry after response bytes or unconfirmed upstream execution. |
| F15 | P2 / Gap | No integrated service-policy/guardrail lifecycle or content-retention contract is defined. A provider base URL and webhook are privileged egress configuration, so delegated administration adds an SSRF boundary. Existing best-effort webhook delivery is not a durable incident channel. Evidence: `operator.rs:898`; `config.rs`; TD-0005/0019. | Egress policy with explicit self-hosted exceptions, redirect and address revalidation; bounded guardrail hooks, timeouts, disclosure and content retention controls; signed/redacted alerts with bounded retry/dead-letter handling. Fault/abuse tests precede delegated configuration. |
| F16 | P2 / Confirmed design drift | TD-0005 retains stale references to missing durability and inactive rate limits, an `allow: bool` sketch despite its own corrective note, and ambiguous SDK enforcement claims. TD-0024 assumes idempotency/schema work has not landed. Dashboard text still says reads need no token. | Reconcile these sections before implementing their phases; link accepted boundaries and current evidence. Publish a truthful capability/guarantee matrix and keep planned capabilities out of shipped feature claims. |

## Capability disposition

| User need | Current foundation | Target disposition |
|---|---|---|
| Connect a model safely | SDK dialects, catalog, key registration, CLI | Guided credential reference → connection check → scoped key → first attributed request |
| Explain a failed call | Dialect-shaped errors, outcome, request IDs, OTLP | Caller-safe reason and next action; operator timeline joining admission, attempts and credential generation |
| Control usage and money | Neutral units, one-scope lease ledger, alerts | Hierarchical units and accurate export here; price-versioned monetary reporting/admission downstream |
| Operate production | TLS, bounds, drain, metrics, SQLite | Readiness, SLOs, retention, restore drills, durable export and safe config rollback |
| Govern teams | Key attribution and exact model allowlists | Scoped management plus policy distribution; identity lifecycle in the control plane |
| Manage secrets | OS keyring and SentinelPass IPC/CLI | Broker-aware onboarding, bounded credential validity, shared posture and lifecycle evidence |
| Improve efficiency | Prompt-cache fidelity, measured cache split | Explainable routes and cache effectiveness; quality-gated optimization, no speculative semantic cache |
| Broaden workloads | Four HTTP dialects; additional upstreams | Named user demand plus modality/protocol admission evidence for embeddings, duplex, MCP or A2A |

## Required review decisions

Adopt the [product specification](../product/gateway-vision-and-requirements.md) as a proposal,
sequence correctness work before expansion, and retain the existing measurement/pricing boundary
unless explicitly revised. Do not declare stronger budget, audit, tenant or revocation guarantees
until the tests associated with the relevant delivery slice pass.
