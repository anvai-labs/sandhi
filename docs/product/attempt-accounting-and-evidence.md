# Attempt accounting and durable evidence

Status: W05 in progress. W05a is integrated; W05b is locally verified and awaiting remote
integration. It remains opt-in/non-authoritative.
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
TTL reclaim currently deletes abandoned leases; W05a does not change that behavior.

## Delivery substeps

| ID | Deliverable | Gate / state |
|---|---|---|
| W05a | Atomic settlement receipt/outbox storage, immutable IDs, bounded claims and acknowledgement, rollback/reopen/concurrency tests | Integrated; 14 focused tests; not wired to proxy or network exporter |
| W05b | Transport-owned attempt lifecycle and neutral draft contract | Locally verified: implementation, TDD regressions and clean adversarial review cover adapter/plane/retry paths, pre-dispatch rejection, nested timeout/cancellation lineage, correlation, bounded metadata and non-final usage; remote PR/CI still gate integration and downstream accounting review still gates external release |
| W05c | Connect admission, settlement and evidence without bypassing correctness | Pending: opt-in authoritative mode, per-shard colocation, failure policy, receipt/attempt linkage, logical dedup separation and all no-lease paths |
| W05d | Unknown-liability and late-settlement recovery | Pending: durable pre-dispatch intent; crash windows, stale leases, late observations, amendments and idempotent recovery; coordinate TD-0024 retention |
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

## Proposed physical-attempt state machine (W05b–d)

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
no remaining findings. This is local implementation evidence; remote PR review, exact-head CI,
merge and post-merge CI remain pending.

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
