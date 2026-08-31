# TD-0020: Operational readiness and transport observability — you cannot operate what you cannot see

- **Status:** Draft (proposed), 2026-08-31. Owns gaps **G15, G16, G17, G18, G27**.
- **Relates to:** [TD-0011](TD-0011-first-party-observability.md) (the metric registry and its D2
  bounded-label discipline, which this extends), [TD-0014](TD-0014-data-plane-resource-safety.md)
  (whose every bound is unobservable until G16 lands — build them in parallel),
  [TD-0015](TD-0015-performance-baseline-and-fault-injection.md) (the offline counterpart to these
  runtime gauges), [ADR-0004](../adr/0004-two-plane-proxy-and-enforcement-boundary.md) D4 (the gate
  these endpoints inherit).

## Why this exists

TD-0011 built a good metric registry. Every counter and histogram in it describes **one settled
model call**: token dimensions, duration, TTFT, denials, rate-limits, reclaims, overshoot
(`sandhi-proxy/src/metrics.rs:193-272`). That is the right first layer, and it is the only layer.

There is **no metric for anything that is not a completed call**: not open connections, not in-flight
streams, not file descriptors, not queue depth, not admission wait time, not upstream pool size.
Which means every resource bound in TD-0014 — the concurrency limit that does not limit streams
(G02), the absent connection cap (G03), per-tenant contention (G04), pre-auth connection abuse (G19)
— is not merely broken but **invisible**. An operator cannot see the problem, cannot see a fix land,
and cannot tune the knob afterwards.

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

**D2 — A transport gauge set, all bounded-cardinality process aggregates.** Connections currently
established; connections accepted (counter); streams currently open; requests waiting on the
admission semaphore; admission wait time (histogram); file descriptors in use; upstream pool
connections per provider slug (bounded by the catalog, per TD-0011 D2); usage-sink and alert-writer
queue depth and drop counts — the latter two exist as internal counters
(`BufferedSink::dropped_events`, `BufferedAlertStore::dropped_updates`) and are **exported nowhere**.

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
| **P2** | D2 — the transport gauge set | Opening N concurrent SSE streams moves the open-streams gauge to N and back to 0 after drain; `BufferedSink` drop counts appear at `/metrics`; every new label is drawn from the closed set (assert against TD-0011 D2's bounded-label test) |
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

## Open questions

- Should `/readyz` also fail when the ledger backend is unavailable? Arguably yes — a proxy that
  cannot enforce should arguably not receive traffic. But ADR-0005 D6's fail-open/closed policy is
  per-tier and deliberate, and a readiness probe that overrides it would silently change enforcement
  semantics into an availability decision.
- Is FD count portable enough to gauge? `/proc/self/fd` is Linux-only; macOS needs a different call.
  Possibly a Linux-only gauge with a documented gap, rather than an abstraction over two syscalls.
- What is the right default `pool_max_idle_per_host`? Too low and every request pays a handshake; too
  high and idle FDs accumulate exactly as they do today. This wants a TD-0015 measurement.
- Should drain progress be a metric, a log line, or both? Both is easiest to justify: the log serves
  the operator watching a deploy, the metric serves the dashboard afterwards.
