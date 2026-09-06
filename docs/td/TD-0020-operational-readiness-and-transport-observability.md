# TD-0020: Operational readiness and transport observability — you cannot operate what you cannot see

- **Status:** **In progress**, updated 2026-09-06. P1/W06a is integrated through PR #232;
  W06c offline recovery is merged through PR #233 with green post-merge CI (C01d in TD-0026).
  W06d workload automation is integrated through PR #234 with green post-merge CI; actual-user acceptance remains open.
  P2 is partial via TD-0014's shipped connection,
  stream, and connection-shed signals plus integrated W06b buffer visibility;
  the remaining P2–P5 scope is open. Owns gaps
  **G15, G16, G17, G18, G27**.
- **Relates to:** [TD-0011](TD-0011-first-party-observability.md) (the metric registry and its D2
  bounded-label discipline, which this extends), [TD-0014](TD-0014-data-plane-resource-safety.md)
  (whose bounds need matching G16 instrumentation),
  [TD-0015](TD-0015-performance-baseline-and-fault-injection.md) (the offline counterpart to these
  runtime gauges), [ADR-0004](../adr/0004-two-plane-proxy-and-enforcement-boundary.md) D4 (the gate
  these endpoints inherit).

## Why this exists

TD-0011 built a good metric registry. Every counter and histogram in it describes **one settled
model call**: token dimensions, duration, TTFT, denials, rate-limits, reclaims, overshoot
(`sandhi-proxy/src/metrics.rs:193-272`). That is the right first layer, and it is the only layer.

At proposal time there was **no metric for anything that was not a completed call**. TD-0014 has
since shipped open-connection/open-stream gauges and a connection-shed counter. Drain duration, file
descriptors, queue depth, admission wait time, and upstream pool size remain open here.
At proposal time that made every resource bound in TD-0014 both broken and **invisible**. G02, G03,
and G19 have since shipped with connection/stream/connection-shed signals. G04 and the remaining
drain, queue, file-descriptor, and pool visibility stay open here; those controls cannot be tuned
honestly until their gauges exist.

The readiness half is a smaller but sharper defect. There is exactly one health route,
`/healthz` (`lib.rs:211,573`), returning a static string. It has no relationship to shutdown state.
During graceful drain the process stops accepting new connections
(`lib.rs:301-347`) while `/healthz` keeps reporting healthy — so a load balancer keeps routing to a
socket that is no longer accepting, for as long as its health-check interval. Sandhi *has* a careful
drain implementation, and then does not tell anyone it is draining.

Two adjacent items complete the picture. **G17:** each `ProviderHandle` builds **two** independent
`reqwest::Client`s — one for the typed adapter, one for the raw forwarder
(`sandhi-providers/src/typed.rs:481-487`, both via `default_client()` at
`sandhi-providers/src/lib.rs:66-71`) — each with its own pool at reqwest defaults, meaning unbounded
idle connections per host and 2N pools for N credentials. **G27:** DNS is whatever `reqwest` defaults
to, with no cache control and no visibility; a stall is bounded only by `connect_timeout(10s)`.

## First principles

1. **A bound without a gauge is untunable.** Every limit added anywhere in this project ships with
   the metric that shows how close traffic is to it. TD-0014 D6 states the same rule from the other
   direction.
2. **Liveness and readiness are different questions.** *Am I alive* and *should you send me traffic*
   diverge during exactly the window where getting it wrong costs the most.
3. **Bounded cardinality, without exception.** TD-0011 D2's rule holds: no subject, group, session,
   virtual key, request id, budget scope, or IP address in a label. Gauges are per-process
   aggregates, not per-tenant breakdowns.
4. **Observe the resource, not the proxy for it.** "Requests in flight" is not "streams open" is not
   "connections established." Conflating them is what let G02 hide.
5. **Load shedding is an observability feature first.** You cannot decide when to shed without
   measuring queue delay, so G18 follows G16 rather than preceding it.

## Non-goals

- **No new export protocol.** Prometheus text at `/metrics` and the optional OTLP path (TD-0011 P3)
  are the surfaces; this TD adds instruments to them, not transports.
- **No per-tenant gauges.** Per-subject visibility lives in the usage aggregate (TD-0009), which is
  bounded by an explicit cap with an overflow bucket. Repeating it as labels would reintroduce the
  cardinality problem TD-0011 D2 solved.
- **No custom DNS resolver.** G27 is about configuration and visibility, not about owning resolution
  (ADR-0006 D5).
- **No distributed tracing changes.** TD-0011 P3 owns spans.

## Decisions

**D1 — `/readyz`, distinct from `/healthz`, and drain-aware.** `/healthz` keeps its current meaning
(the process is alive) and its current behaviour. `/readyz` reports whether the process should
receive new traffic: `200` normally, `503` from the moment the shutdown signal fires, throughout the
drain. Rejected: making `/healthz` drain-aware — some orchestrators restart on a failing liveness
probe, which would kill a draining process mid-stream and settle every in-flight call as `Partial`.

`/readyz` reflects **drain state only** — never ledger or upstream health (see R1). A readiness
probe that failed on ledger error would override ADR-0005 D6's per-tier fail policy for every tier
at once, turning a per-scope enforcement decision into a process-wide availability one.

**D2 — A transport gauge set, all bounded-cardinality process aggregates.** Connections currently
established; connections accepted (counter); streams currently open; requests waiting on the
admission semaphore; admission wait time (histogram); file descriptors in use; upstream pool
connections per provider slug (bounded by the catalog, per TD-0011 D2); usage-sink and alert-writer
queue depth and drop counts — the latter two exist as internal counters
(`BufferedSink::dropped_events`, `BufferedAlertStore::dropped_updates`), are logged once on a failed
drain (`main.rs:309,317`), and are **on no metrics surface at all**, so an operator cannot see a
buffer filling until it is too late to act.

**D3 — The gauges are gated exactly like `/metrics` is today.** `/readyz` is ungated — a load
balancer cannot present an admin bearer, and readiness leaks nothing. Everything else follows the
existing ADR-0004 D4 gate rather than inventing a second policy.

**D4 — One shared HTTP client per upstream host, not two per handle.** The typed adapter and the raw
forwarder for one `ProviderHandle` share a client and therefore a pool. Explicit
`pool_max_idle_per_host` and `pool_idle_timeout`, with documented defaults, replace reqwest's
unbounded default. Rejected: one process-wide client — different upstreams legitimately need
different timeout and auth-header configuration, and pooling across tenants that hold *different
credentials to the same host* is a cross-tenant coupling nobody asked for.

**D5 — Load shedding on measured admission delay (G18).** When admission wait time exceeds a
threshold, refuse new work with a dialect-shaped `503` and `Retry-After` rather than queueing
unboundedly. Reuses TD-0012 D4's rendering path exactly; there is no new error vocabulary. Follows
D2 because the threshold is meaningless without the measurement.

**D6 — DNS gets a bound and a gauge, not an implementation (G27).** Expose resolver timeout
configuration and a counter for resolution failures and slow resolutions. If `reqwest`'s default
`GaiResolver` proves inadequate under measurement, swapping to a caching resolver is a dependency
decision made against evidence — not a resolver Sandhi writes.

**D7 — Drain reports progress.** Graceful shutdown logs, at intervals, how many streams remain and
how long is left of the grace period, and emits a final drain-duration metric. Today, a drain that
hits its deadline logs one line (`lib.rs:341-343`) and an operator has no way to know whether it was
about to finish or nowhere close.

## Phases

| Phase | Scope | Acceptance (the failing test to write first) |
|---|---|---|
| **P1** | D1 + D7 — `/readyz` and drain visibility | After the shutdown signal, `/readyz` returns 503 while in-flight streams still complete normally and `/healthz` still returns 200; drain duration is recorded and remaining-stream progress is logged |
| **P2** (partial ✅) | D2 — the transport gauge set. **Shipped early via TD-0014 P2/P3** (#169, #181): `sandhi_streams_open`, `sandhi_connections_open`, and `sandhi_connections_shed_total`. Still open here: drain duration, admission-wait histogram, FD gauge, queue depths, and pool size | Opening N concurrent SSE streams moves the open-streams gauge to N and back to 0 after drain; `BufferedSink` drop counts appear at `/metrics`; every new label is drawn from the closed set (assert against TD-0011 D2's bounded-label test) |
| **P3** | D4 — pool consolidation | One `ProviderHandle` creates one client; registering K credentials against one host stays within a recorded FD budget; existing per-adapter timeout and auth behaviour is unchanged (assert via the existing provider test suites) |
| **P4** | D5 — load shedding | Under an offered load exceeding admission capacity, the proxy returns dialect-shaped 503 + `Retry-After` instead of growing the queue; a shed request consumes no lease and emits no usage event (the TD-0012 D5 property, restated for shedding) |
| **P5** | D6 — DNS bounds and counters | A stalled resolver produces a bounded, counted, observable failure rather than a request that waits on `connect_timeout` |

P1 and P2 are small and unblock TD-0014's verification, so they are the natural first pair. P2 is a
prerequisite for taking TD-0014 P4 (per-tenant bulkheads) seriously, since fairness cannot be tuned
blind.

## Pressure test

1. **"Kubernetes already handles this with `preStop` hooks and termination grace."** Only if the
   readiness probe actually flips, which requires the endpoint this TD adds. A `preStop` sleep is the
   workaround people reach for *because* `/readyz` is missing, and it guesses at a duration that D7's
   metric would tell them.
2. **"More gauges means more cardinality risk."** Every instrument in D2 is a process-level aggregate
   with either no labels or one drawn from the catalog-bounded provider slug. The cardinality risk is
   per-*tenant* labels, which §Non-goals explicitly refuses and which TD-0011 D2 already forbids.
3. **"Sharing a client between the typed and raw paths couples two planes that were separated
   deliberately."** They are separated at the *semantic* layer — ADR-0004 D1 is about byte fidelity
   and translation, not about socket ownership. Two connection pools to the same host with the same
   credential is an accident of construction, not a design boundary, and it doubles idle FDs.
4. **"Load shedding will reject traffic that would have succeeded."** By design, and it is strictly
   better than the current behaviour of accepting unboundedly and failing everyone slowly. D5's
   threshold is on *measured* delay rather than a guess, which is why it follows D2.
5. **"`/readyz` is trivial — why is it in a design document?"** The endpoint is trivial; deciding
   that `/healthz` must *not* change is the part worth writing down, because the obvious
   implementation is to make the existing endpoint drain-aware and that causes restart loops.
6. **"G27 is speculative — nobody has reported a DNS problem."** Nobody could: a DNS stall currently
   presents as a request that took ten seconds and then failed, indistinguishable from a slow
   upstream. D6 is mostly about making the failure *distinguishable*, which is a precondition for
   anyone reporting it.

## Resolved

**R1 — `/readyz` must NOT fail when the ledger backend is unavailable.** The tempting answer is that
a proxy which cannot enforce should not receive traffic. It is wrong, and importantly so: ADR-0005
D6 makes the fail policy **per-tier and deliberate** — a `Block` scope fails closed, a `Warn` scope
fails open. A readiness probe that pulled the whole process out of rotation on ledger error would
override that policy for every tier at once, silently converting a per-scope *enforcement* decision
into a process-wide *availability* decision. Surface ledger health as a metric and let D6 keep
owning admission. Recorded as an amendment to D1 rather than only here, because it is a decision
about what `/readyz` means, not a footnote.

**R2 — A Linux-only FD gauge with a documented gap, not an abstraction over two syscalls.**
`/proc/self/fd` is Linux-only and macOS needs a different call. The gauge exists to answer "are we
approaching the FD limit" in production, and production is Linux. A portability shim would add a
platform abstraction to serve a development environment that does not need the number.

**R3 — Drain progress is both a log line and a metric.** They serve different people at different
times: the log serves the operator watching a deploy in real time, the metric serves the dashboard
and the post-hoc question "how long do our drains actually take?" (which is also TD-0015's
graceful-drain measurement). Neither is redundant.

## Still open

- **W06 readiness reachability (2026-09-05):** the current shutdown path stops accepting and
  signals connection graceful shutdown immediately. A router-only `/readyz` flag test would
  not prove that fresh network probes can receive `503` during drain. Before P1, define a
  bounded probe-reachable quiesce phase or separate probe listener, reject new model/admin
  mutation work before dispatch, and test HTTP/TLS, queued admissions and in-flight SSE.
  Also define the deadline honestly: current alert and usage drains each receive a separate
  grace after listener drain. P1 cannot claim an end-to-end shutdown deadline without changing
  and testing those phases. Buffer visibility does not close this gap.

## W06b buffer visibility slice (2026-09-05)

Implemented and locally verified in `feat/operational-buffer-visibility`, separate from
checkpoint PR #230; not merged or released.
Core `BufferedSink` and proxy `BufferedAlertStore` expose sender-free observer snapshots;
the registry reads bookkeeping only, with no database I/O or worker messages at scrape time.
The binary attaches observers after constructing its metric registry. `/metrics` keeps its
existing authorization; fixed `buffer="usage"|"alerts"` is the only added dimension.

`sandhi_buffer_configured` distinguishes absence from an idle queue. For configured buffers,
`sandhi_buffer_capacity`, `sandhi_buffer_queued`, `sandhi_buffer_in_flight` and
`sandhi_buffer_dropped_total` distinguish channel capacity, accepted data awaiting callback
start, an active callback and rejected enqueue attempts plus queued items abandoned after a
worker panic. The panicking callback's persistence result is unknown and is not counted as
a queue drop. Logical queued capacity is enforced under the same bookkeeping lock as enqueue;
close also uses that lock so new data cannot enter behind the shutdown message. This preserves
`queued <= capacity` even when the receiver has dequeued but not yet claimed an item.
Control messages are excluded from
queued/in-flight but share channel slots. A snapshot is coherent for one buffer, not a
transactional snapshot across both writers. Observers contain no sender and do not extend
channel lifetime. Counters reset on process restart; configured is not a worker-health signal.

These are best-effort observation queues, **not W05 authoritative outbox backlog**. Completed
callbacks are not confirmed durable writes. SQL insertion failures and in-memory ring evictions
have separate existing counters but are not exported by this slice. Alert write failures/missing
rules, oldest age, freshness, OTLP parity and durable incident delivery remain follow-ups. P2
still owns admission waits, file descriptors and pool instrumentation; P1 readiness is unchanged.

Verification: full workspace tests passed, including the native-feature coverage run at
**87.26% line coverage**. Deterministic blocked-writer tests cover active versus queued work,
overflow, callback panic/abandoned queue, observer lifetime and concurrent admission/close.
Three metric tests cover honest disabled samples, observer integration and contiguous Prometheus
family grouping. Two real-binary HTTP tests passed for explicit capacities, disabled buffers and
the unchanged authentication gate. All-target clippy passed with the native feature; formatting,
binding facade drift and diff checks passed. No public JSON schema or binding contract changed.
The full SDK/dashboard/broker suite with AgentBrowser passed **95 tests, 1 skipped** (Google SDK
unavailable locally); all-target clippy also passed without the native feature. C01b in TD-0026
tracks integration separately; no merge or release is implied by local verification.

## W06a execution gates (2026-09-06; integrated)

This slice covers the whole shutdown path, not just a readiness flag. These are acceptance
gates; C01c in TD-0026 tracks verification and integration:

1. Define a lifecycle/cutoff shared by the listener and request admission. Keep fresh HTTP/TLS
   probes reachable during a bounded quiesce phase; `/readyz` returns 503 while `/healthz`
   retains 200. Readiness remains drain-only, not a new ledger/provider-health policy.
2. Reject new model requests and admin mutations before dispatch after the cutoff, including
   requests already waiting on admission/body reads. Pin the race between cutoff and permit
   acquisition; merely checking middleware once is insufficient. Existing admitted streams
   may finish within the remaining deadline. Define cutoff as dispatch authorization's
   linearization point: already-authorized work may send bytes later. A literal network-byte
   cutoff would require transport-level gating. Own reservation guards across detached work
   so pre-dispatch cancellation cannot orphan leases; track admitted mutations until completion.
3. Own one monotonic deadline across quiesce, connection cancellation, both writer drains,
   runtime cleanup and telemetry shutdown. `RequestAccounting::Drop` calls synchronous
   settlement, the aborted-task join loop is currently unbounded, and detached blocking
   admission/broker work may outlive its request. `#[tokio::main]` runtime drop and
   `OtelGuard::Drop` can also wait beyond the listener grace. Explicitly bound or disclose
   those phases; do not claim a process-wide bound from `close(remaining)` alone.
4. Before shipping, test real HTTP/TLS fresh probes, keep-alive requests, stalled handshakes,
   queued admissions, in-flight SSE, locked SQLite settlement, blocked writer callbacks and
   telemetry shutdown. Assert no post-cutoff dispatch, truthful unfinished/uncertain work,
   and an externally measured process-exit bound. Do not relabel abandoned work as persisted.
   Include saturated connection/per-IP limits: a bounded probe allowance or a qualified
   reachability contract is required, not removal of existing transport protections.

Listener ownership is in `serve_router_listener_with_shutdown`; application admission and
request accounting are in `lib.rs`; binary/runtime/writer sequencing is in `main.rs`; telemetry
cleanup is in `otel.rs`. Library embedders must retain runtime ownership and receive an honest
bounded shutdown result rather than a process-wide termination side effect. W05 continues to
own authoritative unknown liability and durable settlement evidence.

### Implemented contract

`Lifecycle` serializes cutoff and operation admission. AI requests recheck after body extraction
and authorize dispatch only after owned reservation acquisition. Lost blocking-task results
retain reservation rollback ownership. Admin mutation guards outlive detached broker work;
config children regate individually and retain partial commit results.

`/readyz` is ungated, non-cacheable and drain-only; `/healthz` remains liveness. The same-port
quiesce window defaults to 1000 ms (`SANDHI_SHUTDOWN_QUIESCE_MS`), bounded by one quarter of
the original deadline's remaining grace. HTTP/TLS fresh and keep-alive probes are reachable
only during that window and **subject to existing global/per-IP caps**. No reserved probe
capacity or reachability after listener close is promised. Requests already authorized before
cutoff may still send upstream bytes; this is not transport-byte fencing.

The library reports `TimedOut` when cancellation/operation cleanup is unfinished and never
terminates its host. Synchronous cleanup can outlive cancellation; runtime ownership stays
with the embedder. The binary owns a separate watchdog, armed before shutdown logging, which
enforces the same monotonic deadline through accounting, writer closure, OTLP destruction and
explicit Tokio teardown. Incomplete cleanup exits 124 without claiming settlement or flush;
normal completion exits 0. The hard-exit path performs no potentially blocking logging.

Prometheus adds fixed, unlabeled readiness, active-operation and elapsed-shutdown gauges under
the existing metrics authorization. Active operations are not durable commits or all live
connections. This does not complete the remaining P2 gauges, pool/DNS work, W06c recovery or
W06d workload/user acceptance. See the operator guide for probe migration and timeout handling.

### Verification

Native workspace tests passed with 87.76% line coverage; OTLP-feature proxy tests and strict
default/native+OTLP clippy passed. Full SDK/dashboard/broker plus real AgentBrowser smoke:
113 passed, 1 skipped (Google SDK absent locally). Eight `test_shutdown.py` cases exercise
actual SIGTERM over HTTP/TLS, including fresh/keep-alive probes, held SSE, queued/slow-body
cutoff, saturated transport limits and hung/locked-SQLite exit 124. Rust tests cover detached
reservation rollback, admin/config guards, metrics authorization, stalled TLS, blocked cleanup
and runtime-teardown watchdog subprocesses. This is not a live external-collector or production
workload certification. Adversarial review's trailing OTLP span-drop guard race was fixed by
placing the operation guard last; re-review found no remaining blocker. C01c owns PR/CI evidence.

## W06c recovery acceptance (2026-09-06; integrated and verified)

The [recovery runbook](../operator/recovery-drill.md) and disposable SDK fixtures rehearse
single-file and fixed two-shard restart/restore, exact attribution and continued settlement,
real SIGKILL lease preservation and held-capacity rejection, WAL-aware standalone snapshots,
invalid/overlapping archive rejection and historical revocation reconciliation in quarantine.
Native/plain synthetic broker cases distinguish retained metadata from actual secret authority;
AgentBrowser checks the restored, authenticated dashboard without seeding new usage.

The broader review also reproduced configured ledger-open failure bypassing a persisted zero
hard cap through a fresh memory ledger. W06c therefore requires startup failure before binding
when a configured database cannot initialize; only absent storage configuration selects memory.
Empty/non-UTF-8 settings, `:memory:` and SQLite `file:` URIs are rejected by the binary.
External broker resolution remains separate. Initialization is not an atomic migration of all
components, and missing shards can still be created: restore completeness preflight is required.

These are initial single-node drills, not online cross-file backup, production RTO/RPO, live broker
certification, automatic revocation reconciliation or authoritative unknown-consumption recovery.
W06d separately owns workload evidence and actual-user acceptance. C01d tracks review and CI.

Independent review closed source/archive overlap and exact serialized-manifest size findings;
the new startup regression first demonstrated upstream dispatch despite a persisted zero cap,
then passed after the fix. All 17 startup failure/healthy-mode cases passed independent re-review.
Default/native workspace and OTLP proxy tests and strict clippy passed; native line coverage was
87.77%. Final combined SDK/browser and remote CI evidence belongs to C01d.

W06d's [integrated workload record](../product/m1-acceptance.md) adds a 36-phase synthetic baseline
and a 227-test combined SDK/browser regression result. This is not full TD-0015 certification or
a production latency/capacity promise. Actual-user acceptance still gates M1/main promotion.

## Remaining pool decision

**What is the right default `pool_max_idle_per_host`?** Gated on
  [TD-0015](TD-0015-performance-baseline-and-fault-injection.md) R5. Too low and every request pays
  a handshake; too high and idle FDs accumulate exactly as they do today under reqwest's unbounded
  default. This is a measurement, not a preference.
