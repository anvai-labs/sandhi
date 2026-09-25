# Attempt accounting and durable evidence

Status: W05 in progress. W05a and W05b are integrated. W05b remains
opt-in/non-authoritative; W05c–e are pending.
Date: 2026-09-08. Tracker: [TD-0026](../td/TD-0026-gateway-product-evolution.md).

## Work backward from reconciliation

An operator must be able to explain why admitted/settled usage differs from logical activity,
which measurements are incomplete, and which evidence reached a downstream consumer. A receipt
for a database charge cannot prove that a provider executed a request, or how many retries ran.
Preserve that distinction before designing dashboards or pricing integrations.

| Identity / fact | Meaning | Deduplication rule |
|---|---|---|
| Logical operation | A caller's operation within an authenticated key/idempotency window | Preserve existing logical `UsageEvent` behavior; a caller key is not proof of exactly-once execution |
| Gateway execution | One admitted ingress invocation, including repeats of a logical operation | Each invocation remains distinct even if its logical usage event is suppressed |
| Physical attempt | One dispatch at the provider transport boundary, including retries/fallbacks | Immutable attempt ID; each actual dispatch counted separately |
| Reservation | Held admission capacity, currently per execution/scope | Lease ID is local to its ledger/shard, not globally unique |
| Settlement receipt | Immutable evidence that a specific lease settled a specific neutral charge | Opaque persistent receipt ID; same lease replay cannot replace the charge |
| Export delivery | One delivery of immutable evidence, possibly repeated | Receiver deduplicates by evidence ID; sender acknowledgement alone cannot prove receiver commit |

Current evidence: `RequestAccounting::finalize` settles each ingress execution, but suppresses
logical events for repeated idempotency keys. `ResilientProvider` emits a final attempt count;
failed intermediate attempts have no individual measurement record. Transparent transport does
not retry. A zero ledger charge after absent usage is not evidence of zero provider consumption.
Legacy TTL reclaim deletes abandoned leases; tracked intents now retain unresolved
liability through expiry. HTTP adoption remains separate.

## Delivery substeps

| ID | Deliverable | Gate / state |
|---|---|---|
| W05a | Atomic settlement receipt/outbox storage, immutable IDs, bounded claims and acknowledgement, rollback/reopen/concurrency tests | Integrated; 14 focused tests; not wired to proxy or network exporter |
| W05b | Transport-owned attempt lifecycle and neutral draft contract | Integrated through PR #246 after clean adversarial review and green exact-head/post-merge CI; remains opt-in diagnostics only, while downstream accounting review still gates authoritative use and external release |
| W05c | Connect admission, settlement and evidence without bypassing correctness | Partial foundation: owned library settlement outcomes and canonical terminal-observation receipt linkage; authoritative proxy mode, physical-attempt linkage, logical dedup separation and no-lease policy remain pending |
| W05d | Unknown-liability and late-settlement recovery | Partial storage foundation: atomic admission intent (#308), expiry protection and immutable terminal usage snapshots (#310), followed by canonical stored-charge settlement and read-only bounded recovery inventory. Amendments, authoritative HTTP ownership and idempotent recovery remain pending; coordinate TD-0024 retention |
| W05e | Receiver contract, exporter and operator evidence | Pending: receiver idempotency, authenticated/scoped transport, retry/backoff, backlog/freshness UX, safe retention, multi-shard cursor/migration and real consumer review |

W05 completes only after all substeps and joint contract gates have evidence. No code in W05a
renames a gateway invocation as a physical attempt or reinterprets historical logical events.

## W05a storage contract

The new storage API is deliberately separate from `settle_durable`: existing proxy call sites
do not silently acquire a new accounting mode. A receipt records only ledger-owned scope,
reservation ID, committed neutral charge and settlement time. It is an internal Rust/SQLite
record, not a published cross-repository event schema, usage-category export or price input.
There are no prompt, response, URL, raw header, credential or idempotency-key payload fields.
Scope is still caller-supplied metadata and must not contain secrets.

Settlement and outbox insertion must share one `BEGIN IMMEDIATE` transaction on the owning
ledger connection (`WAL`, `synchronous=FULL`). Exact replays return the original receipt;
conflicting amounts, wrong scopes, missing/reclaimed leases and legacy settlements without
receipts are explicit non-success outcomes. No fabricated backfill closes historical gaps.
SQLite signed-integer bounds are checked before writes.

Receipt IDs are generated once and persisted; never derive export identity from a per-shard
autoincrement number, a secret or a caller idempotency key. Claims are bounded, expire, and use
fresh fencing tokens so an expired worker cannot acknowledge a reissued delivery. Acknowledged
rows retain an immutable tombstone; acknowledgement does not delete accounting evidence.
At-least-once delivery still requires receiver-side deduplication. No exporter or network write
is enabled by these primitives.

The implementation is `sandhi-store::ledger::evidence`: `settle_with_evidence_durable`,
`claim_settlements_durable` and `acknowledge_settlement_durable`, with scope-routed settlement
and explicit-shard delivery wrappers on `ShardedLedger`. Claims accept batches of 1–1000 and
leases of 1–3600 seconds using a trusted local timestamp, never a remote request timestamp.
Clock rollback can delay reclaim; clock jumps can cause duplicate delivery, so receiver
deduplication is mandatory. Acknowledgement returns false for unknown, stale, expired **or
already acknowledged** claims; callers must not interpret false as permission to resend a
previously committed charge. Replay settlement still returns the original receipt after ack.
The additive `budget_settlement_outbox` table is empty for legacy callers. No automatic pruning
or backpressure policy exists yet; do not enable this in a production hot path before W05c/e.

Sharding must route the new operation by the lease scope and validate that scope against the
stored row. Polling/acknowledgement require an explicit local shard; a shard index is an internal
location, never a tenant credential or a stable cross-topology cursor. An unsupported migration
of a legacy single-file source containing receipts fails before creating destination files or
changing source rows, including when all receipts were acknowledged. This guard is not a general
topology manager: existing multi-shard N→M configuration changes are not detected or supported
by this new primitive. Keep topology fixed; automatic outbox retention and topology changes
with evidence remain W05e/TD-0024 gates. Migration/restore requires quiesced writers.

## W05a verification

`cargo test -p sandhi-store ledger::evidence --quiet` passes 14 tests (including a child-process
helper). Tests pin exact replay/conflicting charges, signed bounds/zero charges, missing and
legacy leases, wrong scopes, insert/update rejection, all-or-nothing claim batches, independent
SQLite connections settling/claiming concurrently, reopen durability, expired-worker fencing,
ack tombstones, shard-local lease-ID collisions and refusal to migrate evidence-bearing sources.

An isolated child exits without running destructors before/after commit. The pre-commit child
stages the two database facts directly; insertion-trigger tests separately cover the production
API's rollback path. This proves process-exit/reopen behavior, not a power-loss, disk-full,
fsync-failure or full gateway pre-dispatch crash drill. Those remain explicit integration and
operational acceptance gates, not inferred from SQLite transaction tests.

## Durable admission intent (W05d storage foundation)

`SqliteLedger::reserve_with_intent_durable` is an opt-in library primitive, not an
HTTP activation switch. It uses the same admission calculation as `reserve_durable`
and atomically commits the reservation plus one ledger-generated execution ID.
The ID is distinct from logical idempotency and is not authorization to retry
inference. `IntentAdmission::Denied` preserves the existing budget denial contract;
capacity exhaustion, invalid input, entropy failure and storage failure are explicit
errors. A failed intent insertion rolls back admission.

Callers set a finite retained-record ceiling (1–100,000 per ledger); the count and
insert share the write transaction. The ceiling includes settled identities.
There is no pruning or automatic capacity recovery. Expired intent-backed leases
remain reserved through **both** opportunistic admission reclaim and periodic
reclaim until explicitly settled. Expiry does not establish zero provider usage.
Legacy reservations without intents retain their previous expiry behavior.
The common admission calculation now checks SQLite's signed ceiling bound and
compares aggregate spend in widened arithmetic, avoiding negative/wrapped ceilings.

`intent_durable` reads an original execution/reservation association from the same
ledger after reopen. Keep topology fixed: legacy-to-sharded migration rejects a
source containing intents before creating target files. The initial admission
increment had no terminal observation; the next storage slice is documented below.
There is still no sharded admission wrapper, unresolved-intent enumerator, worker,
receipt/attempt linkage, exporter or retention policy. Existing HTTP
finalization is unchanged and remains outside durable-settlement acceptance.
Older writers do not understand protected intents: do not downgrade or share this
ledger with an older writer once intents exist. Preserve the database and use an
explicitly reviewed migration/recovery plan.
Do not enable this on production traffic until the subsequent owner/recovery and
bounded-failure policies are implemented and reviewed.

Tests extend the existing evidence owner for rollback, concurrent capacity limits,
reopen, both expiry routes, late settlement, signed bounds and migration refusal.
The existing child-process crash owner also covers committed and uncommitted
reservation/intent pairs. These are process-exit tests, not power-loss guarantees.
No duplicate receipt or proxy-level database suite was introduced.

## Durable terminal usage observation (W05d storage foundation)

`SqliteLedger::record_terminal_durable(scope, execution_id, usage)` persists one
immutable, versioned `UsageV2` snapshot for an existing execution intent. Reservation
identity and scope come from that intent within the immediate transaction. A caller
cannot attach evidence to a different reservation by supplying its numeric ID.
`terminal_durable` reads the snapshot after reopen; matching scope is not caller
authorization. These are library APIs, not an HTTP activation or recovery worker.

The first successful write returns `ObservationOutcome::Recorded`; an exact replay
returns `AlreadyRecorded` with the original timestamp and frozen charge. A changed
snapshot conflicts, including equal-total changes to categories, completeness,
measurement basis or outcome. First observation after settlement is rejected; exact
replay of already stored evidence remains possible after settlement. No observation
operation updates spend, releases a reservation or retries inference.

Final/partial usage freezes the existing canonical `sandhi_core::billable` result
once; unavailable usage freezes **None**, never a measured zero. Cache categories,
reasoning inclusion, audio/prediction counters, completeness, basis and latency
provenance remain available in the full snapshot. Partial/estimated usage does not
become complete/provider-reported by being persisted. On read, charge is retrieved
from storage rather than recalculated using a later formula. `UsageV2.outcome`
remains opaque execution-level metadata, not proof of a physical adapter send or
provider receipt; the providers' diagnostic `AttemptOutcome` is not duplicated here.

Outcome and upstream-request-ID metadata are limited to 256 and 1,024 UTF-8 bytes;
serialized snapshots are capped at 16 KiB. Oversized values, invalid cache metadata
that serde would otherwise omit, and a charge outside SQLite's signed integer
range are rejected. Reads reject unsupported versions, oversized/corrupt snapshots
and unknown fields instead of silently dropping evidence. Future `UsageV2` or
serialization changes must preserve a versioned reader or provide an explicit
migration for valid v1 snapshots; do not reinterpret old evidence with a new shape.
One row per retained
intent inherits admission's bounded record count. There is no automatic pruning.

**Activation limits:** an unavailable or partial snapshot is immutable too. A later
refinement requires a separately reviewed amendment contract; do not overwrite or
infer a zero charge to release capacity. Intent-backed unknown liability remains
reserved after expiry. Current-version caller-charge APIs reject tracked reservations;
use the canonical settlement API below with one compatible owner and unchanged
ledger topology. Older binaries can still bypass this guard: do not mix them with
tracked writers or roll back to them while tracked work exists. Bounded recovery,
authoritative HTTP ownership, retention/export and broad lifecycle acceptance remain
subsequent gates. The lossy W05b diagnostic channel is not an authoritative input.

Verification extends the existing evidence suite for full-snapshot reopen, replay
and conflict, concurrent winners, failed/ignored insert rollback, unavailable versus
zero, malformed input and corrupt reads. The existing child-process fixture covers
committed and uncommitted observations; these are process-exit tests, not power-loss
acceptance. No separate ledger or proxy test suite was introduced.

## Canonical terminal settlement (W05c/d storage foundation)

`SqliteLedger::settle_terminal_durable(scope, execution_id)` reads the retained
intent and immutable observation inside one immediate transaction, then settles
using its **stored** charge. It accepts no caller charge and never reruns the
billable calculation. The same receipt-writing transaction helper serves existing
untracked receipt callers; no second receipt ledger or charge derivation is added.

Only `Final` + `ProviderReported` snapshots settle, including explicit zero.
Unavailable, partial and estimated usage return `UnresolvedObservation` and retain
the entire reservation through expiry. Missing snapshots return `MissingObservation`;
missing intents, corrupt data and wrong scopes also reject without releasing
liability. Amendment and remaining-liability rules are still needed before other
usage states can settle automatically. A provider-reported basis is a stored contract
fact, not independent proof that the provider's usage or executed cache reuse is accurate.

`Committed` records the settlement and receipt together. Concurrent or later replay,
including after reopen or receipt acknowledgement, returns `AlreadyCommitted` with
the original receipt. A historical receipt with a conflicting charge is preserved
and rejected, never rewritten. Scope matching remains a storage guard rather than
caller authorization; an eventual HTTP owner must supply authenticated scope.

This deliberately tightens the **opt-in tracked** contract: `settle_with_evidence_durable`
returns `TrackedSettlementRequired` for a tracked reservation even when the supplied
charge happens to match; legacy `settle_durable` returns a SQLite constraint error.
Both guards run within the write transaction. The legacy void `EnforcementLedger::settle`
cannot report that error but cannot update the tracked reservation; it is unsuitable
for an authoritative owner. Existing untracked outcomes and receipts stay unchanged.
Migration helpers and raw database access are trusted maintenance surfaces, not
alternative runtime settlement APIs.

The evidence owner covers frozen-charge use, bypass attempts before/after observation
and settlement, unresolved versus measured-zero liability, corrupt/missing observations,
rollback on ignored/failed receipt insertion, concurrent replay, reopen and acknowledged
replay. The existing child-process fixture also exits after canonical settlement and
verifies the committed pair on reopen. These are process-exit tests, not power-loss
acceptance. Existing receipt tests still own shared transaction failure behavior;
no duplicate test suite was introduced.

This library increment does not activate HTTP settlement, a recovery worker,
physical-attempt correlation, exporter, sharded intent admission or topology migration.
Keep unresolved snapshots and historical receipts; a compatible binary/owner is required
for rollback. Full lifecycle and mixed-team C5 acceptance remain open.

## Bounded terminal recovery inventory (W05d storage foundation)

`SqliteLedger::recovery_page_durable(scope, cursor, limit)` accepts a page limit
of 1–100 and returns at most that many records (no matching records yields an empty
terminal page). The first page
freezes the highest existing tracked reservation ID in that scope. Subsequent pages
use that upper bound and the last returned reservation ID; there is no second
identifier derivation or mutable-state filter before the page limit. Untracked
reservations and other scopes do not consume the page. New admissions beyond the
upper bound wait for the next sweep. Each page has one short read transaction;
state can change between pages, and a fresh sweep is required for late observations
on previously visited records.

Records distinguish missing terminal observations, unresolved usage, observations
ready for canonical settlement, and already-settled executions with a consistent
receipt, including acknowledged receipts. Eligibility uses the canonical settlement
predicate: final provider-reported usage, including measured zero. Inventory never
recomputes the stored charge, changes spend/liability, claims work, retries inference
or releases reservations. A returned candidate is not a lock: a future recovery
owner must call `settle_terminal_durable`, which revalidates and returns the original
receipt if another owner already committed it.

Malformed snapshots, orphaned intent/observation bindings and inconsistent receipts
fail explicitly. An orphan's scope is unknown, so its presence blocks any scoped
inventory without disclosing record details. Settled state, actual charge, frozen
charge, receipt scope/identity/charge and timestamp must agree. Canonical terminal
settlement uses the same receipt-consistency check. Historical settlement without
a receipt is an error, not a repaired or silently omitted row. No mutation is
performed when reporting these errors.

The opaque in-process cursor belongs to the original ledger and fixed topology;
it is not portable across database replacement, restore or sharding changes. Scope
matching is not caller authorization. Page completion is neither a whole-database
integrity certificate nor proof of no remaining recovery work. Returned records and
lookahead allocation are bounded; SQLite execution latency is not thereby bounded.
There is no HTTP activation, recovery worker, durable scheduler, observation amendment,
exporter or retention policy in this increment. Older writers still require the
compatibility restrictions above.

The existing evidence suite owns pagination tests: interleaved scopes, frozen upper
bounds with concurrent admissions/state updates, reopened/acknowledged receipts,
invalid cursor/input limits, corrupt bindings/snapshots/receipts, and read-only spend,
liability and row-change conservation. The existing completeness/basis matrix is
extended to verify inventory classification rather than duplicated.

## Proposed physical-attempt state machine (W05b–d)

### Owned settlement transition (W05c foundation)

`sandhi_proxy::settlement::PendingSettlement` freezes a caller-provided execution
request ID, reservation and cache-inclusive `billable(UsageV2)` once. Its consuming
`try_commit` delegates to W05a's existing scope-routed atomic receipt transaction;
it introduces no second ledger, charge derivation or receipt identity. `Committed`
means that transaction returned a new or replayed receipt, not that an event reached
the dashboard or a consumer. Logical deduplication is a separate operation.

`Unresolved` returns the original owned attempt and a structured reason: busy or
poisoned proxy mutex, missing reservation, volatile ledger, unavailable usage, or
the store's evidence error. Unknown usage is not converted to a measured zero.
Partial measured usage retains its observed charge. A caller can retry the same
frozen settlement; this API never retries inference. The proxy mutex is acquired
without waiting, but SQLite's configured busy timeout still applies. There is no
end-to-end settlement deadline or guaranteed commit.

Retries must use the original admission ledger and unchanged shard topology.
Reservation IDs are ledger-local; this primitive does not authenticate request IDs
or bind them to a database identity. A poisoned internal evidence-shard mutex now
returns `EvidenceError::ShardPoisoned` for settlement, claim and acknowledgement,
instead of unwinding through the consuming settlement transition.

The primitive is library-only: existing HTTP finalization and defaults are unchanged.
Unresolved ownership is volatile, not a durable recovery queue; dropping it or
crashing can still lose the observation. W05d's pre-dispatch intent, reclaim/late
settlement and recovery contract remains required, as does W05e's retention policy
before enabling receipt creation on production traffic. No standalone activation
switch, background retry worker, exporter or automatic pruning is added.

The existing proxy ledger test owner covers frozen-charge retry under contention,
reclaimed-lease propagation, and explicit unknown/unleased/volatile/poisoned outcomes.
The store's transaction/replay/rollback suite remains the single owner for W05a's
database contract; it is not duplicated in proxy tests.

### Remaining durable lifecycle

Persist admission/dispatch intent before a potentially billable send. A crash between intent and
send is uncertain, not proven execution. Record transport dispatch and terminal observation
separately: success/error/cancelled is orthogonal to measured/estimated/unavailable and
final/partial. Preserve usage categories and reasoning inclusion from W03. Provider request IDs
are diagnostic evidence, not automatically trustworthy deduplication keys.

An interrupted or abandoned attempt becomes unresolved liability. Releasing capacity under an
explicit policy must not erase that fact. Later measured evidence appends an idempotent amendment
linked to the original attempt/receipt; it must not mutate previously exported facts. The lease
TTL, recovery window, liability policy and retention tombstones need a single reviewed design.
Retries need their own identities and coverage, including failure before headers and after bytes.

### W05b diagnostic contract implemented for review

`AttemptContext::channel` creates an execution-scoped sender/receiver pair with a caller-selected
capacity of 1–65,536. The sender is Sandhi-owned and uses only non-blocking `try_send`; consumer
code never runs on the provider task. A full or disconnected channel increments
`dropped_observations()`. Delivery is therefore best-effort, not an audit guarantee. Each guard
attempts at most one terminal emission, but either dispatch or terminal observations may be
dropped. The receiver must be drained outside the request task. No persistence worker is supplied.

An attempt ID combines 128 bits from operating-system randomness with a checked monotonic `u64`
ordinal. Separate contexts remain collision-resistant when a caller execution ID repeats, and
ordinal exhaustion disables observation rather than wrapping. Execution/provider/model/request-ID
labels are bounded and control characters are rejected or replaced. Bodies, raw headers,
credentials, attribution authority and prices never enter the observation shape.

`Dispatch` means the adapter has begun the HTTP transport invocation; it does not prove that bytes
left the host, the provider received them, or the provider billed them. A terminal observation
records success, provider rejection, transport error, timeout, cancellation or incomplete stream,
independently from final/partial/unavailable neutral usage. Status and bounded provider request ID
are retained after response headers, including cancellation, idle timeout, mid-stream failure and
response-decode failure. `StreamChunk::terminal` is the only completion signal; an empty data chunk
can be nonterminal and cannot hide a later error.

The built-in `ProviderHandle` factories expose `complete_with_attempts` and
`stream_with_attempts`, which carry the context through the typed codec and resilience decorator
to each physical adapter invocation. Host-owned `ChatProvider` implementations and `FnProvider`
fail the observed path explicitly unless they implement it. `RawForwarder` names the narrower
`with_metered_attempt_context` scope honestly: only `forward_metered` and
`forward_stream_metered` observe attempts; the unmetered escape hatches do not. W05b does not wire
this channel into the proxy request path. That authoritative connection, failure policy and
receipt linkage remain W05c–e work, so these diagnostics cannot yet justify billing or budget
enforcement.

### W05b verification

The exact local branch passes 618 workspace tests with all features, plus strict all-target
Clippy, formatting and diff checks. Four credential-dependent live tests are intentionally
ignored. Line coverage is 86.41% workspace-wide and 93.51% for the attempt module, above the 75%
gate. TDD regressions first reproduced nested complete/stream-setup/idle timeouts being mislabeled
as cancellation and control-character expansion exceeding metadata byte budgets. Linked timeout
scopes preserve ancestor causes without contaminating retry or concurrent sibling scopes, and
sanitization now accounts for replacement-character UTF-8 width. Fresh adversarial review found
no remaining findings. PR #246 merged the verified implementation as `bc4b120`; exact-head CI
`34229915141` and post-merge CI `34230690137` passed on public hosted runners. The first PR-head
run exposed stale path-dependency metadata in the separate Python and Node lockfiles. Refreshing
only those entries made all three locked advisory scans pass before the successful rerun. The
authorized admin override bypassed only the missing approving review, never a failed or pending
check.

A future external schema should contain opaque execution/attempt/evidence IDs, authenticated
attribution, destination/provider/model facts, optional policy and credential revisions, neutral
usage categories, measurement provenance and outcome. Revisions not observed by Sandhi stay
absent; do not fill them with invented defaults. No pricing or identity-provider authority moves
into core. Schema generation remains Rust-owned after the contract review.

## Co-design boundaries

The accounting consumer reviews logical versus physical totals, unknowns, amendments, delivery
deduplication and price-effective-time needs before export is enabled. SentinelPass supplies
credential-generation evidence only after SP2; its lookup is not a model attempt. AgentBrowser
supplies browser action evidence only after AB04's correlation/retention contract; a browser
action is not a token charge. Opaque run IDs correlate facts but never authorize cross-tenant reads.
No sibling changes, external issue/PR, consumer sign-off or live service test is implied.


## Owned tracked terminal settlement (W05c integration foundation)

`PendingSettlement::tracked(request_id, intent, usage)` owns the complete immutable
`UsageV2` and the existing execution intent. `try_commit` validates the full binding
against the original ledger (expiry uses its persisted whole-second precision),
records terminal usage, and calls canonical stored-charge
settlement. It does not calculate a second charge. `charge()` remains the legacy
caller-charge accessor and returns `None` for tracked attempts; the committed receipt
is the authoritative charge result.

An unresolved result returns the owner, including its original usage and identity.
`observation_recorded()` distinguishes an observation not yet confirmed stored from
one whose record/replay succeeded but whose settlement remains unresolved. The latter
is not a settlement acknowledgement. Exact replay revalidates the immutable stored
observation before attempting settlement again. Unknown usage is recorded and retains
liability; a caller must not replace it with invented zero usage or retry inference.

This bridge supports one file-backed ledger only. Volatile ledgers (including SQLite
memory/temporary databases), multiple shards, missing intents, incorrect reservation
metadata and poisoned locks fail explicitly before recording an observation. Use the
original database and fixed topology; scope matching is not authentication. The bridge
is an opt-in library API and does not add tracked admission to the HTTP request path.

The existing settlement-owner tests cover proxy-lock contention, separate injected
observation and receipt failures, retained ownership, reopen/replay, invalid bindings,
unsupported ledgers and unknown liability. The existing shard-poison test covers the
new forwarding methods. Canonical charge eligibility, atomicity and corruption remain
owned by the store tests, without a duplicated matrix.

The outer mutex attempt does not wait, but SQLite and the inner shard mutex can wait.
The existing 120-second buffered transport bound is not an established end-to-end
settlement deadline. Before HTTP activation, finish proven-never-dispatched liability
handling, typed finalization results, bounded blocking-job ownership, shutdown/restart
recovery, retention/export and lifecycle acceptance. Old writers and incompatible
rollback remain unsafe for tracked reservations.
