# TD-0016: The enforcement throughput ceiling — making a durable cap fast enough to keep

- **Status:** Draft (proposed), 2026-08-31. **Blocked on [TD-0015](TD-0015-performance-baseline-and-fault-injection.md) P1**
  for its central premise. Owns gaps **G07, G08, G24, G25, G26**.
- **Relates to:** [ADR-0005](../adr/0005-enforcement-correctness-reservation-ledger-observe-enforce-split.md)
  (the lease model this must not weaken), [TD-0007](TD-0007-enforcement-ledger-backends.md) (the
  C1–C6 contract and the shared-backend selection this feeds), [ADR-0006](../adr/0006-layer-boundary-and-protocol-scope.md)
  (whose verdict rests on G07 being the real ceiling), [ADR-0004](../adr/0004-two-plane-proxy-and-enforcement-boundary.md) D1
  (the plane selection G26 interacts with).

## Why this exists

ADR-0005 got enforcement *correct*: reserve a ceiling, settle by lease id, reclaim on TTL, decide
fail-open/closed per tier. The correctness argument is the strongest in the repository and none of
it is in question here.

What was never examined is the **cost of that correctness on the hot path**. On the durable arm
(`SANDHI_STORE` set) every model call takes two fully-durable, globally-serialized SQLite write
transactions — including for scopes with no configured limit, since `reserve_durable` writes the
lease row regardless (`sandhi-store/src/ledger.rs:215-220`):

```
ProxyState.ledger : Mutex<ProxyLedger>       sandhi-proxy/src/lib.rs:86
SqliteLedger      : ONE Connection           sandhi-store/src/ledger.rs:48
synchronous = FULL (fsync per commit)        sandhi-store/src/lib.rs:59-61
BEGIN IMMEDIATE per reserve                  sandhi-store/src/ledger.rs:165-167
reserve → spawn_blocking                     sandhi-proxy/src/lib.rs:1969
settle  → block_in_place                     sandhi-proxy/src/lib.rs:1997-2007, 2155-2159
```

Three multipliers compound: `synchronous=FULL` means an `fsync` per commit; a single `Connection`
means no write parallelism; and one `Mutex` means no concurrency even in the parts that would not
contend at the storage layer. *Inference:* this caps a budgeted deployment in the low thousands of
requests per second, serialized — and it is invisible today because nothing measures it (TD-0015).

The related structural limit is **G08**: `SANDHI_REPLICA_COUNT != 1` is a startup `assert!`
(`sandhi-proxy/src/main.rs::validate_replica_topology`). That assert is *good engineering* — it
refuses to silently multiply limits — and it is also the reason Sandhi has no HA story and cannot
do a rolling deploy without a window where enforcement is absent or duplicated.

## First principles

1. **Correctness is not negotiable; the *implementation* of correctness is.** Every change here is
   judged by the TD-0007 C1–C6 suite staying green. A faster ledger that admits over a cap is not a
   faster ledger.
2. **`fsync` is a batching problem, not a durability trade.** Group commit gives N callers one
   `fsync` while every one of them still observes a durable commit before being admitted. Dropping
   to `synchronous=NORMAL` would be trading correctness for speed; group commit is not.
3. **Distinguish the three multipliers.** `fsync` frequency, connection parallelism, and lock
   granularity are separate problems with separate fixes and separate risks. Attacking them as one
   "make the ledger fast" project is how the correctness argument gets lost.
4. **A cap is per scope; the ledger is not.** Contention between *different* scopes is pure
   accident — no correctness argument requires it. That is the cheapest structural win available.
5. **Do not durabilize first** (ADR-0005's build order). The dual applies: do not distribute first
   either. Single-node throughput is the cheaper problem and must be solved before the shared
   backend inherits it.

## Non-goals

- **No change to the lease model.** Reserve-a-ceiling, settle-by-id, TTL reclaim, per-tier fail
  policy: all preserved exactly.
- **No backend selection.** Choosing Redis/etcd/Postgres/proximaDB is TD-0007's decision against its
  own conformance suite. This TD makes the single-node arm fast and hands TD-0007 a measured
  baseline to beat.
- **No relaxation of `synchronous=FULL`.** Explicitly rejected in D2.

## Decisions

**D1 — Nothing lands before TD-0015 P1 publishes the ledger benchmark.** The premise that the commit
path dominates is an inference. If P1 falsifies it, this TD is rescoped and ADR-0006 is revised.
This is the ADR-0005 "pressure-test before code" discipline applied to its own successor.

**D2 — Group commit, not weaker durability.** Concurrent reservations batch into one transaction and
one `fsync`; each caller is released only after the commit that includes it. Rejected:
`synchronous=NORMAL` for the ledger — ADR-0005 C2/C3 require a cap/lease commit to survive power
loss, and `sandhi-store/src/lib.rs:55-61` already documents `NORMAL` as correct for the *best-effort
sink* and wrong for the ledger. Trading that away would be reversing a decision that was made for
the right reason.

**D3 — Shard the ledger by scope.** Replace the single `Mutex<ProxyLedger>` with per-scope
partitioning, so two tenants never serialize against each other. The atomic conditional admit
(ADR-0005 C1) is *already* per-scope inside SQLite's `BEGIN IMMEDIATE` — the global mutex is
protecting nothing that needs protecting across scopes. This is the change with the best
correctness-risk-to-benefit ratio and should be attempted first. It also directly serves
[TD-0014](TD-0014-data-plane-resource-safety.md) D5's per-tenant isolation goal, which explicitly
deferred the ledger lock to this TD.

**D4 — A connection pool behind the ledger, not one connection.** With WAL already enabled
(`apply_durable_pragmas`), readers do not block the writer. Reserve and settle need write access;
`spent()` and `limit()` do not. Splitting read paths onto pooled read connections removes them from
the write lock entirely.

**D5 — Revisit G08 with evidence, not by assumption.** `BEGIN IMMEDIATE` plus a 5s `busy_timeout`
may *already* satisfy C1 across processes sharing one SQLite file. Run the TD-0007 conformance
suite with two processes against one file before assuming a shared backend is required. If it
passes, the assert can relax to a *documented, tested* multi-process-single-host topology — which is
a real deployment shape (rolling restart on one host) that is currently forbidden. If it fails, that
is a concrete input to TD-0007.

**D6 — Fix `input_estimate` (G24) as a correctness item, not an optimisation.** It is `bytes/4`
(`lib.rs:2359-2373`) and self-documented as undercounting CJK. It is one addend of the reservation
ceiling — `input_estimate + effective_max` (`lib.rs:2376-2377`) — and the function's own comment is
explicit that "the *output* side, not this, is the load-bearing part" (`:2356-2358`). So an
undercount here is the smaller of the two error terms, but it still means a cap is enforced one call
later than intended — the exact class of defect
ADR-0005 exists to remove. The `sandhi_settle_overshoot_total` counter (TD-0013 D6) already measures
the symptom; this closes the cause. A byte-class-aware estimator is likely sufficient; a real
tokenizer is a dependency this project should not take lightly.

**D7 — Make the two ledger arms behaviourally identical (G25).** The in-memory arm stores a `Warn`
scope as *uncapped* while the durable arm enforces `Warn` natively (`proxy/src/ledger.rs:70-80`).
The divergence is documented, which is better than hidden, and it still means dev and production
enforce differently. Resolution: promote TD-0007's conformance suite to a shared harness both arms
run — which TD-0007 already specifies and which nothing has yet done.

**D8 — Document G26 as an accepted trade, or fix it; do not leave it undescribed.** A `Block`-capped
scope with unbounded output is forced off the transparent plane so the output bound can be injected
(`lib.rs:1653`). The enforcement reasoning is correct. The *consequence* — that capped tenants
lose byte-exact forwarding and therefore prompt-cache fidelity, which is a real cost paid by exactly
the cost-sensitive tenants who set caps — is stated nowhere an operator would see. Either inject the
bound into the raw body (breaking envelope fidelity in a *different* way, and needing its own
argument) or document the trade in the operator guide. Silence is the only unacceptable option.

## Phases

| Phase | Scope | Acceptance (the failing test to write first) |
|---|---|---|
| **P0** | D1 — measure (executed as TD-0015 P1) | A published number for reserve+settle throughput and its breakdown across the three multipliers. **Gate:** if the commit path is not dominant, stop and rescope |
| **P1** ✅ (structural; measured caveat) | D3 — shard by scope | Two scopes reserve concurrently with no cross-scope serialization (assert measured parallelism, not just correctness); the full C1–C6 suite green; single-scope behaviour bit-identical to today |
| **P2** | D4 — read/write connection split | `spent()`/`limit()` no longer contend with `reserve`/`settle`; dashboard aggregate queries under load do not raise admission p99 beyond a recorded bound |
| **P3** | D2 — group commit | N concurrent reservations complete in one `fsync`; **every** admitted caller observes a durable commit before admission (assert by crash-injection: kill after admit, restart, lease is present); C1–C6 green |
| **P4** | D6 + D7 — estimator and arm parity | A CJK prompt reserves ≥ the provider's measured input tokens, and `sandhi_settle_overshoot_total` stays at zero across the corpus; the shared conformance harness passes identically against both ledger arms |
| **P5** | D5 + D8 — topology and the plane trade | Two processes, one store, N racing reservations: the cap is never breached, or the failure is characterised and handed to TD-0007. G26's trade is documented in `docs/operator/proxy-guide.adoc` or removed |

P1 is the best first move: highest benefit, lowest correctness risk, and it unblocks TD-0014 D5.
P3 carries the most risk and should land last among the performance phases, behind crash-injection
tests.

## Pressure test

1. **"Group commit adds latency to a single request to help a busy one."** Yes — bounded by the
   batch window, which should be small (tens of microseconds) and adaptive. At low load the batch is
   one and the behaviour is unchanged. The alternative for a *contended* ledger is worse latency for
   everyone, since they queue on the mutex anyway.
2. **"Sharding the ledger breaks the atomic admit."** It does not, and this is the crux: C1's
   atomicity is *per scope* and lives inside SQLite's `BEGIN IMMEDIATE`. The global mutex adds
   cross-scope serialization that no invariant requires. P1's acceptance asserts exactly this by
   running the full conformance suite unchanged.
3. **"You are optimising before you know it matters."** Precisely why D1 exists as a hard gate and
   P0 is a stop-or-continue decision point. This TD is written so it can be *cancelled* by its own
   first phase.
4. **"Relaxing the replica assert risks silently multiplying limits — the exact thing it prevents."**
   D5 relaxes it only for a topology that has *passed the conformance suite under real concurrency*,
   and only for multi-process-single-file, not multi-host. If the suite fails, the assert stays and
   the result feeds TD-0007. The current state — a forbidden topology that might actually be safe —
   is also a cost, just an unmeasured one.
5. **"G24 is a P3; why is it in a throughput TD?"** Because it shares the phase-4 test harness with
   D7 and because it is not really a throughput item — it is the accuracy of the ceiling, which is
   this TD's subject matter. Splitting it into its own document would be one more coordination
   point for a two-day change.
6. **"D8 is a documentation task masquerading as engineering."** The engineering question is whether
   the bound can be injected into the raw body without breaking envelope fidelity. The documentation
   is the fallback if the answer is no. Leaving a cost-sensitive tenant silently downgraded to the
   slower, cache-missing plane is a product defect regardless of which resolution wins.

## Resolved

**R1 — The relaxed replica assert is expressed as a topology *enum*, never a replica count.**
"Multiple processes, one host, one file" is defensible if the conformance suite passes it; "multiple
hosts over a network filesystem" is not, and SQLite's own documentation is explicit that locking is
unreliable there. A numeric `SANDHI_REPLICA_COUNT=3` cannot distinguish the two, so the knob must
not be numeric — the unsafe topology has to be **inexpressible**, not merely discouraged.

**R2 — `input_estimate` becomes script-aware, not model-aware.** `ModelDescriptorV1`
(`crates/sandhi-core/src/chat.rs:414-430`) carries `max_input_tokens`, `max_output_tokens`,
`default_temperature` and `capabilities` — but **no bytes-per-token ratio**. It does carry a
free-form `extensions: BTreeMap<String, Value>` (`:428-429`), so a ratio could be smuggled in
without a schema change; that makes this a *governance* argument rather than a "new field" one.
Either way a per-model ratio becomes catalog data somebody must curate and version under TD-0004,
for an estimator whose documented defect (CJK undercount, `sandhi-proxy/src/lib.rs:2359-2373`) a
script-aware ratio already fixes at zero coupling. Revisit only if `sandhi_settle_overshoot_total`
stays non-zero after the change — which is the point of having that counter.

**D9 — The reservation *output* ceiling should be model-aware, and that is free.** The inverse of
R2, discovered while resolving it: `DEFAULT_OUTPUT_CEILING` (`sandhi-proxy/src/lib.rs:64`) is a flat
4096 applied to any capped scope whose client left output unbounded, while the catalog already
carries the real per-model `max_output_tokens`. Using it needs **no new catalog data** and no new
governance — only a lookup. A flat 4096 is simultaneously too low for a long-form model (it silently
truncates a capped tenant's output) and too high for a small one (it over-reserves and refuses
admissible calls). Lands as P6.

## Phase addendum

| Phase | Scope | Acceptance |
|---|---|---|
| **P6** | D9 — model-aware output ceiling | A capped scope on a long-context model reserves against that model's real `max_output_tokens`, not 4096; a model absent from the catalog falls back to the flat default; no capped call is truncated below the model's own limit |

## Still open

- **What batch window does group commit use, and is it adaptive?** Gated on P0's measurement. A
  fixed window penalises low load, an adaptive one is another tuning surface to get wrong, and
  neither can be chosen before the commit cost is known.
- **Does sharding by scope degrade when nearly all traffic is one scope?** Gated on P1, which must
  *measure* the single-large-tenant case rather than assume it is acceptable. The likely answer is
  that it degrades to today's behaviour, which would be fine — but "likely" is how the P1 line-budget
  defect happened.
