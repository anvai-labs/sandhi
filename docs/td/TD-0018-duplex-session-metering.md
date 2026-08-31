# TD-0018: Duplex session metering — one lease, many usage events

- **Status:** Draft (proposed), 2026-08-31. **Spike-gated: D1 must pass before any of P2–P4 is
  scheduled.** Depends on [TD-0017](TD-0017-transport-security-and-ingress-protocol-breadth.md).
  Owns gaps **G11, G12**.
- **Relates to:** [ADR-0006](../adr/0006-layer-boundary-and-protocol-scope.md) D3 (which names the
  metered session, not the transport, as the abstraction to generalize),
  [ADR-0005](../adr/0005-enforcement-correctness-reservation-ledger-observe-enforce-split.md) (the
  lease model this must extend without weakening), [TD-0013](TD-0013-streaming-usage-fidelity.md)
  (partial settlement on interruption, whose reasoning generalises directly),
  [TD-0010](TD-0010-ingress-dialect-parity.md) (dialect parity, which a session protocol joins).

## Why this exists

Sandhi's accounting is built on an assumption that is true of every path it serves today and false
of the next class of traffic: **one request produces one lease and one usage event.**

`RequestAccounting` (`sandhi-proxy/src/lib.rs:2010-2220`) holds one `Option<Reservation>`, one
`Option<UsageV2>`, and one `outcome`, and emits exactly one event in `finalize()`. `Drop` guarantees
that finalization happens (`:2216-2220`) — which is genuinely good design, and is precisely why the
structure is load-bearing enough that extending it needs a design document.

Realtime voice, multimodal sessions, and duplex agent/tool traffic do not fit. A voice session is one
long-lived connection carrying many turns, each with its own token usage, over minutes. Modelling it
as one request means one usage event for a session that should produce dozens, one reservation
ceiling for a total nobody can estimate in advance, and no way to stop a session mid-way when a
budget is exhausted.

The transport gap is real but shallow: no route uses WebSocket and `axum`'s `ws` feature is off,
though `axum::serve` already calls `serve_connection_with_upgrades`
(`axum-0.7.9/src/serve.rs:256,424`), so HTTP/1.1 Upgrade is wired at the connection level. **The
transport is a feature flag and a handler. The accounting model is the actual project.** ADR-0006 D3
records this explicitly, and it is why this TD is about sessions rather than about sockets.

## First principles

1. **The invariant to preserve is metering, not request shape.** "Every model call is counted and
   attributed" must survive a change in wire framing. If it cannot, the framing is not adopted.
2. **A session is a sequence of billable units, not a single big one.** Reserving once for a whole
   session either over-reserves (blocking legitimate traffic) or under-reserves (the ADR-0005 D1
   overshoot bug at session scale).
3. **Enforcement must be able to act mid-session.** A budget that can only be checked at session
   start is not a budget for a session that runs for ten minutes.
4. **Interruption is the normal case, not the exception.** TD-0013 established this for streams:
   settle what accrued, never release to zero. A duplex session interrupts more often, not less.
5. **Prove the model before building the transport.** The WebSocket handler is the easy half and
   would happily be written against an accounting model that does not work.

## Non-goals

- **No WebRTC.** Realtime voice over WebRTC involves SRTP, ICE, and a media stack — a different
  project, and one ADR-0006 D5's reasoning would likely refuse.
- **No audio processing, transcoding, or VAD.** Sandhi meters and enforces; it does not touch media.
- **No new dialect surface in the spike.** D1 proves the accounting model with the simplest possible
  framing. Provider-specific realtime protocols come after, under TD-0010's parity discipline.
- **No session affinity or reconnection semantics** in this TD. A dropped session settles and ends;
  resumption is a later question and probably a different one.

## Decisions

**D1 — A disposable spike decides whether this TD proceeds.** Build a throwaway branch with axum's
`ws` feature, one echo-shaped `/v1/realtime` route, and a session that opens a lease, meters N
simulated turns, and settles on close. Success criteria, all required:

- N turns produce N usage events, each attributed to the same `session_id`, `subject_id`, `group_id`.
- A budget exhausted at turn K refuses turn K+1 and closes the session with a dialect-shaped reason,
  while the K completed turns remain correctly settled.
- An abrupt RST mid-turn settles the accrued partial for that turn (TD-0013's rule) and leaks no
  lease.
- The `Drop`-guaranteed finalization property survives — no path exits without settling.

If any fails, this TD stops and the finding is recorded. **A spike that reports "the lease model
needs a rewrite" is a successful spike**, and a far better outcome than discovering it in P3.

**D2 — Extend the accounting model to `MeteredSession`; do not fork it.** Per ADR-0006 D3:

```rust
pub trait MeteredSession: Send {
    fn scope(&self) -> &BudgetScope;
    /// Reserve a ceiling for the NEXT unit of work in this session.
    fn reserve(&mut self, ceiling: u64) -> Admission;
    /// Settle one unit. Idempotent by lease id. Called many times per session.
    fn settle(&mut self, lease: LeaseId, actual: u64);
    /// Terminal. Runs on Drop; safe to call after any abrupt end.
    fn finalize(&mut self, outcome: Outcome);
}
```

Today's `RequestAccounting` becomes the degenerate one-unit implementation of this trait, so the
unary and SSE paths keep their exact current behaviour and their existing tests. Rejected: a parallel
accounting path for sessions — two implementations of "count every call correctly" is how the
`billable()` divergence that ADR-0005 D4 had to unify happened in the first place.

**D3 — Per-turn leases, not one session lease.** Each turn reserves its own ceiling and settles its
own actual. This preserves ADR-0005 D1 exactly (a ceiling is only ever estimable for one unit) and
gives D4 its enforcement point for free. Rejected: a session-scoped lease with incremental
decrements — that reintroduces the reserve-then-reconcile softness ADR-0005 replaced.

**D4 — Enforcement acts between turns, and the refusal is in-band.** A turn whose ceiling would
breach a `Block` cap is refused and the session is closed with a protocol-appropriate reason. A
`Warn` cap admits and alerts, matching the existing policy semantics exactly. There is no new policy
vocabulary here — that is deliberate.

**D5 — One session, one connection, one `session_id`, preserved end to end.** ADR-0001's rule that
`session_id` is never flattened becomes structural rather than conventional. Every usage event from
one session carries it, so the run cost tree (ADR-0005 D7) works for voice without modification.

**D6 — The session is bounded like every other resource.** Maximum concurrent sessions, maximum
session duration, and an idle timeout, all counted against TD-0014's per-tenant bulkheads. A
long-lived duplex connection is the single most expensive resource Sandhi can hold; it does not get
to be the only unbounded one (TD-0014's whole subject).

## Phases

| Phase | Scope | Acceptance (the failing test to write first) |
|---|---|---|
| **P0** | D1 — the spike | All four D1 criteria pass on a throwaway branch, or this TD stops with the finding recorded here |
| **P1** | D2 — `MeteredSession`, with the existing paths as its one-unit case | Every existing proxy and operator test passes unchanged against the refactored accounting; the `Drop` finalization tests (`tests/proxy.rs:797`, `:1613`, `:1690`) are untouched and green |
| **P2** | D3 + D5 — per-turn leases and session identity | A 10-turn session produces 10 usage events sharing one `session_id`; `GET /admin/usage/run/{run_id}` renders the session as a cost tree; each turn settles independently and idempotently |
| **P3** | D4 — mid-session enforcement | A budget exhausted at turn K refuses K+1 and closes the session in-band; turns 1..K remain settled and attributed; a `Warn` cap admits and fires the threshold alert |
| **P4** | D6 + the WebSocket ingress proper | Session count, duration, and idle timeouts are enforced and observable (TD-0020 gauges); an abrupt RST at every stage of the turn lifecycle leaks no lease, no permit, and no task |

P1 is a pure refactor with zero behaviour change and is worth landing on its own merits — it is the
one phase that improves the code even if voice never ships.

## Pressure test

1. **"You are designing for a use case with no customer yet."** Which is why P0 is a disposable spike
   with an explicit stop condition, and why P1 stands alone as a refactor that pays for itself. No
   transport work is scheduled until the model is proven.
2. **"Per-turn leases mean a durable write per turn — that is the TD-0016 problem, multiplied."**
   Correct, and it makes TD-0016 a hard prerequisite for P2 in practice. A voice session at several
   turns per second against a ledger doing an `fsync` per reserve is not viable. Noted here so the
   sequencing is not discovered late.
3. **"WebSocket means the request-body limit and the concurrency limit no longer apply."** Precisely
   why D6 exists and why TD-0014 is a prerequisite rather than a parallel effort. A session that
   escapes every existing bound would be the largest resource-safety regression the project could
   ship.
4. **"Just meter voice at the provider's session boundary and emit one event at the end."** That
   loses mid-session enforcement (D4), loses per-turn attribution, and settles nothing if the session
   is interrupted — reintroducing exactly the metering-evasion hole TD-0013 closed for streams.
5. **"`MeteredSession` will make the simple unary path harder to read."** A real risk. P1's
   acceptance is that every existing test passes *unchanged*, and if the one-unit implementation is
   not obviously as clear as today's `RequestAccounting`, the refactor is wrong and should be
   reverted rather than argued for.
6. **"HTTP/2 streams would serve duplex without WebSocket."** Possibly, and if TD-0017 P0 finds h2
   ingress already works, a bidirectional h2 stream is a legitimate alternative framing to evaluate
   in P0. The accounting model in D2–D5 is framing-independent by design, which is the point.

## Resolved

**R1 — An idle session holds no budget headroom, because D3 already settled it.** This was recorded
as an open question, but per-turn leases (D3) mean a lease exists only for the turn in flight;
between turns there is nothing held. The question presupposed a session-scoped lease, which D3
explicitly rejected. An internal inconsistency in this TD, not a design choice — resolved, not
answered.

**R2 — A session needs no new query surface.** Verified against `RunCostTreeV1`
(`crates/sandhi-core/src/stats.rs:206-215`): the tree is a `run_id`, a total, and roots of nodes
keyed by `(step_id, parent_id)`. A session is therefore just a run whose steps are turns, and D5's
`session_id` preservation is sufficient. `GET /admin/usage/run/{run_id}` renders a voice session as
a cost tree with no change at all.

## Still open — product decisions, not engineering ones

Both need an owner outside this document. Neither blocks anything today, because the TD is already
spike-gated on P0.

- **What is a billable "turn" for a realtime protocol that interleaves audio, text and tool calls
  continuously?** The unit boundary may be provider-defined rather than protocol-defined, which
  would push per-family logic into the session layer — the coupling this TD most wants to avoid.
  Worth noting the shape of a good answer: the turn is most likely *the unit at which the provider
  itself reports usage*, since Sandhi's measurement can be no finer than its source. That is a
  hypothesis for P0 to test, not a decision.
- **If P0 stops this TD, what is offered to a customer asking for voice?** Probably "meter it
  out-of-band from the provider's own usage reporting" — which is a different product with a
  different accuracy claim, and should be named as such rather than improvised under pressure.
