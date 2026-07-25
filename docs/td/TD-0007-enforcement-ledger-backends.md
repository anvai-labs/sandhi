# TD-0007: Enforcement-ledger backends — the contract, the conformance suite, and choosing the shared/HA store

Status: Draft (proposed)
Date: 2026-07-23
Implements: [ADR-0005](../adr/0005-enforcement-correctness-reservation-ledger-observe-enforce-split.md) D2/D5/D6, Phase 3 ("durable/shared ledger behind the Phase-0 trait")

> **One-line thesis.** The enforcement ledger's backend is a *swappable implementation behind one
> contract* (`EnforcementLedger`/`LedgerView`). This TD (a) freezes that contract as an executable
> **conformance suite**, and (b) scores the candidate shared/HA backends against it — Redis, etcd,
> Postgres, and a *proposed* proximaDB "ledger modality" — so the choice is made on evidence, not
> reflex. Sandhi commits to **nothing** until a backend passes the suite.

## Goal

ADR-0005 D5 draws the seam: *"The ledger abstraction lives in `sandhi-core`; durability/atomicity/TTL
live behind it,"* with the swap chain **in-memory → SQLite (single-node) → Redis (HA)**. Steps 0–2
and the SQLite single-node durable arm have shipped (#54, #58, #62, #63). What remains (ADR-0005
Phase 3) is the **shared/HA** backend — the one that holds a cap *across proxy replicas*, not just
across a restart.

The open question the team raised: **do we need Redis, or can proximaDB serve this as a dedicated
modality?** This TD answers it by first making the requirement precise (you cannot judge a store
against a vibe), then measuring each candidate — including proximaDB *as it exists today* — against
that requirement.

**Non-goal:** shipping a specific backend now. Single-node SQLite already satisfies every deployment
that runs **one** proxy process. The shared backend is needed **only** when Sandhi runs ≥2 proxy
replicas sharing one budget scope. This TD is the design gate that must clear *before* that code.

## The requirement — what any ledger backend must satisfy

This is not "a database that can store numbers." The enforcement ledger runs **twice per model call**
(reserve on ingress, settle on completion), **inline on the egress hot path**, and a budget cap is a
**correctness boundary**, not a metric. From ADR-0005 D2/D3/D6, the load-bearing properties are:

| # | Property | ADR-0005 | Why a naive KV store fails it |
|---|----------|----------|-------------------------------|
| **C1** | **Atomic conditional admit** — check `spent + reserved + ceiling ≤ limit` **and** write the lease as **one linearizable operation**. | D2 ("atomic conditional reserve in the store"), D3 ("linearizable for `Block`") | A SELECT-then-UPDATE from the client races: two replicas both read stale, both admit, cap overshoots. The check **must** run *inside* the store under its write lock. |
| **C2** | **TTL leases** — a reservation is `{id, expires_at}`; an unsettled lease is reclaimed at expiry so a crashed replica cannot **leak capacity forever**. Reclaim must be **timed/active**, not "on next read." | D2 ("lease, not counter") | Lazy/read-time expiry leaves a dead lease holding the cap until someone happens to query the scope — a crashed node silently wedges the budget. |
| **C3** | **Idempotent settle-by-id** — `settle(reservation_id, actual)` is a `reserved → settled` state transition guarded so a **repeat is a no-op** (at-least-once delivery, failover replay). | D2 ("idempotent settle") | Delta arithmetic on a shared counter double-counts under retry. |
| **C4** | **Linearizable per scope for `Block`; eventual is acceptable for `Warn`.** | D3, D6 | A hard cap that admits on a stale read is not a cap. A soft cap may lag. |
| **C5** | **Sub-ms p99 point-write, hot path.** Small records, high churn (one row per in-flight call), low cardinality of scopes. | D6 ("never block the async runtime"), Consequences | An engine tuned for large records / scan / search pays the wrong cost per op. |
| **C6** | **Per-tier fail policy** — on a backend error, `Block` fails **closed**, `Warn` fails **open**; never block the runtime waiting on the ledger. | D6 | A shared store *will* have blips; the policy is defined at the caller (already implemented in `ProxyLedger`, #63). |

**These six are the spec.** Everything below is "which store satisfies them, at what cost."

## The conformance suite (executable, backend-agnostic)

The in-memory (`sandhi-core/src/ledger.rs`) and SQLite (`sandhi-store/src/ledger.rs`) ledgers already
carry these tests; Phase 3 promotes them to a **shared `#[cfg(test)]` conformance harness** any
backend impl runs. A candidate is "done" when it is green on all of:

1. `ceiling_reservation_prevents_overshoot` — near-full cap admits nothing more; the C1 invariant
   `spent + reserved ≤ limit` holds at every step.
2. `concurrent_reservations_cannot_oversubscribe` — **run with real concurrency** for a shared
   backend: N threads/clients race `reserve`; the sum admitted never breaches the cap (C1/C4).
3. `settle_is_idempotent_under_repeat` — a thrice-replayed `settle` counts once (C3).
4. `expired_lease_is_reclaimed_no_capacity_leak` — a lease past its TTL frees the cap **without a
   read touching the scope** (C2 — this is the test lazy-expiry stores fail).
5. `settle_after_reclaim_is_a_noop_and_loses_the_usage` — documents the TTL tradeoff (C2/C3).
6. `warn_policy_admits_over_cap_but_still_tracks_spend` — soft cap never denies, still accrues
   spend for alerts (C4/C6).
7. `daily/monthly_window_excludes_prior_window_spend` — calendar-aligned windows survive a
   reconnect (D5).
8. **New for shared backends:** `partition_or_failover_does_not_double_admit` — under a simulated
   leader change / network partition, no two replicas admit against the same headroom (C4).

A backend that cannot express test 2 or 4 **natively** (i.e. only by the caller doing
read-then-write, or by relying on read-time expiry) does not satisfy the contract — full stop.

## Candidate backends, scored against C1–C6

| Backend | C1 atomic admit | C2 timed TTL | C3 idem settle | C4 linearizable | C5 sub-ms | HA / shared | Verdict |
|---|---|---|---|---|---|---|---|
| **In-memory** (`InMemoryLedger`) | ✅ (process lock) | ✅ (sweep) | ✅ | single-process only | ✅ | ❌ not shared | Dev / fallback (shipped) |
| **SQLite** (`SqliteLedger`) | ✅ (`BEGIN IMMEDIATE`) | ✅ (opportunistic + sweep) | ✅ (`WHERE settled=0`) | ✅ single-node | ✅ | ❌ **durable ≠ shared** | **Shipped, single-node only** |
| **Redis** | ✅ (Lua token-bucket, one round-trip) | ✅ (native key TTL, timed) | ✅ (Lua CAS on a settle flag) | ✅ per-key (single primary) | ✅ in-memory, sub-ms | ✅ well-trodden | **Strong default** |
| **etcd** | ✅ (MVCC `Txn(compare→then)`) | ✅ (**native Lease API — a literal match for C2**) | ✅ (txn on a revision) | ✅ **linearizable by design (Raft)** | ⚠️ higher than Redis; fine at budget QPS | ✅ Raft, first-class | **Cleanest conceptual fit** |
| **Postgres** | ✅ (`UPDATE … WHERE spent+reserved+? ≤ limit`) | ✅ (a `reclaim` job / partial index) | ✅ (row state) | ✅ | ⚠️ ms-class, connection-bound | ✅ mature, but heavy | Fine if PG is already in the stack |
| **proximaDB (today)** | ❌ **`conditional_write=false`** | ⚠️ TTL is **read-time/lazy** | ❌ no CAS/txn on records | ⚠️ single-node default; HA experimental | ❓ no point-write figure | ⚠️ `cluster` feature experimental | **Not a fit as-is** (see below) |
| **proximaDB (proposed modality)** | *would need* exposed CAS / atomic reserve-op | *would need* timed eviction | *would need* txn/CAS | *would need* non-experimental Raft | *would need* sub-ms point write | ✅ if the modality ships | **Coherent roadmap bet, gated on the suite** |

### Why etcd is called out over Redis

etcd's **Lease API is C2 verbatim**: `LeaseGrant(ttl)` → attach keys → keys auto-expire on lease
death, with `KeepAlive` as the heartbeat — exactly ADR-0005 D2's "`Reservation{id, expires_at}` + a
sweeper." Its `Txn(compare → then → else)` gives C1 natively without a Lua script, and it is
**linearizable by construction** (C4) rather than by single-primary convention. Redis wins on raw
throughput and ubiquity; etcd wins on *matching the lease model exactly* and on a stronger default
consistency posture. For a **low-QPS, low-cardinality, correctness-critical** ledger, that trade
favors etcd. Both are ~200 lines behind the trait. **Recommend prototyping etcd first, Redis as the
throughput fallback.**

## proximaDB: fit analysis

proximaDB is a **single-node vector/multi-model (vector · graph · document · observability) database,
SQL-first over pgwire, with an internal-only KV primitive.** Measured against C1–C6 *as it ships
today*:

- **C1 — fails.** `WriteContractHealth.conditional_write = false`; no CAS, no predicate/conditional
  write, no atomic counter; pgwire is autocommit-only (`BEGIN` → SQLSTATE `0A000`), gRPC is
  single-statement. The record proto carries a `version` field but the `if_match`/`if_none_match`
  enforcement is **not wired**. A Sandhi reserve would be forced into the exact SELECT-then-UPDATE
  race C1 forbids.
- **C2 — wrong shape.** Native record TTL exists (`valid_to_ns`) but is enforced by **read-time
  filtering (lazy expiry)**, not timed eviction. A crashed replica's lease would hold the cap until
  a read touched the scope — precisely the capacity-leak C2 exists to prevent.
- **C3 — no primitive.** No record-level CAS or idempotent state transition exposed.
- **C4 — not today.** Ships single-node; Raft + Quorum/One/All replication exist in-tree but are
  **feature-gated (`cluster`) and marked Experimental.** You must not put an unproven consistency
  layer *under a hard budget cap*.
- **C5 — unknown.** Documented latency is vector-search p99 (~8–15 ms, self-labeled non-SLA); no
  point-write figure. The write path (WAL → memtable → flush → object-store block formats) is built
  for large records + ANN search, not millions of tiny hot counters.

**But the internal machinery for a real modality already exists** and is simply unexposed: a
create-only CAS (`put_if_absent` at the object-store/lease layer), a 2PC transaction manager with
Serializable isolation, native TTL, and a Raft stack. A **dedicated transactional-KV / "ledger"
modality** — exposing an atomic conditional-write or a reserve-lease op over small keyed records,
with *timed* TTL and non-experimental per-key linearizability — is therefore **architecturally
coherent for proximaDB itself**, and would serve rate-limiting, quotas, feature-flags, and session
workloads generally, not just Sandhi.

That is a **proximaDB-side product bet**, not a Sandhi coupling. The correct sequence:

```
proximaDB ships the ledger modality
        → the modality passes THIS conformance suite (C1–C6, tests 1–8)
        → Sandhi adds a thin `ProximaLedger` arm behind `EnforcementLedger`
```

At no point does Sandhi depend on proximaDB's experimental path. If the modality ships and passes,
Sandhi gains a first-party dogfooding backend "for free"; if it doesn't, etcd/Redis is unaffected.

## Decision

1. **Keep `EnforcementLedger`/`LedgerView` the sole seam.** No backend name appears above the trait.
2. **Promote the ledger tests to a shared conformance harness** (C1–C6, tests 1–8), runnable by any
   impl, with a **concurrent** variant of tests 2 and 8 for shared backends.
3. **When multi-node HA is actually required, prototype etcd first** (lease API = C2 verbatim,
   linearizable `Txn` = C1), Redis as the throughput fallback. Postgres only if it is already a hard
   dependency of the deployment.
4. **Treat a proximaDB ledger modality as a candidate impl, gated on the suite** — specced and owned
   in the proximaDB repo, accepted into Sandhi only after it is green on C1–C6. Not on the critical
   path.
5. **Do not durabilize-shared prematurely.** Single-node SQLite (shipped) is the correct answer for
   every one-replica deployment; this TD unblocks the multi-replica case without forcing it.

## Consequences

- **Positive:** the "which store?" question is now answerable by running a test suite, not by
  argument. proximaDB, Redis, etcd, and Postgres are peers behind one contract; the dogfooding path
  (proximaDB) is open but never load-bearing on an unproven layer.
- **Cost:** the conformance harness must be written to be backend-parametric (a small trait-object or
  generic test rig) and must include a genuinely concurrent oversubscription test — harder than the
  single-threaded in-memory tests, but it is exactly the property that matters.
- **Boundary preserved:** neutral tokens only; no dollars/SKU/tier reaches any backend. The measure-
  vs-price line is unaffected by where the ledger is stored.

## References

- [ADR-0005](../adr/0005-enforcement-correctness-reservation-ledger-observe-enforce-split.md) —
  D2 (lease + atomic reserve), D3 (observe/enforce split), D5 (the seam; "single-node SQLite is
  durable but *not* shared"), D6 (per-tier fail policy). This TD is the Phase-3 elaboration.
- [ADR-0004](../adr/0004-two-plane-proxy-and-enforcement-boundary.md) D3 — the original "durable,
  atomic, shared ledger" that ADR-0005 D5 split into durable (SQLite) vs. shared (Redis/etc.).
- `sandhi-core/src/ledger.rs` (trait + `InMemoryLedger`), `sandhi-store/src/ledger.rs`
  (`SqliteLedger`), `sandhi-proxy/src/ledger.rs` (`ProxyLedger` — the swap point + C6 fail policy).
- Prior art: Redis Lua token-bucket; etcd Lease + MVCC `Txn`; Envoy ratelimit RLS (central decision
  + fail-open shadow).
