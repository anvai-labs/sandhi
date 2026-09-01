# TD-0009: Usage-visibility surfaces — one aggregate, many transports

- **Status:** **Complete** (2026-09-01). Revised from the Proposed draft after a first-principles
  pass: the draft prescribed three local patches, which reproduced the very defect it was written
  to fix. See *What the first draft got wrong*.
- **Relates to:** ADR-0001 (wire contract, measure-vs-price), TD-0002 (typed runtime,
  additive-only contract policy), TD-0003 (operator surface: CLI, admin API, dashboard),
  TD-0008 (Victor–Sandhi co-design boundary)
- **Companion changes:** #68 (latency + reasoning-token fields), #61 (transparent plane),
  #78 (one billable definition — the defect this TD generalizes)

## First principles

1. **There is exactly one atom: the `UsageEvent`,** emitted once per logical call at the point of
   measurement. Sandhi's job ends there.
2. **Everything else is a view over that atom** — a total, a ranking, a live counter, a dashboard
   row. A view is a *fold*, not a new source of truth.
3. **A fold defined more than once is a fold that will disagree with itself.** This is not
   theoretical: #78 fixed three implementations of "billable" that had drifted into a 2×
   discrepancy between what the ledger charged and what the dashboard displayed.
4. **Therefore the fold belongs in one place, and its output is a boundary object** — versioned
   and schema'd like `UsageEvent` itself, because it crosses into the dashboard, the CLI, both
   language bindings, and eventually the AnvaiOps control plane.
5. **A progress signal is not a measurement.** Anything emitted mid-call is a hint about work in
   flight; the meter is the terminal event. Conflating them double-counts.

## What the first draft got wrong

The Proposed draft named three gaps and proposed three fixes: extend the store's `Bucket`, add a
separate in-core aggregator for the bindings, and emit incremental usage events. Two of those are
local patches at the wrong boundary:

- **`Bucket` is not a store type.** It is *the aggregate view*, and the store is merely one place
  it can be computed from. Extending a store-private struct (which #78 did, correctly for its
  scope) leaves the bindings with no aggregate at all and the dashboard coding against a shape
  that exists only inside `sandhi-store`.
- **"A core aggregator *and* the SQL queries, proven equal by test"** accepted two implementations
  and proposed to police the gap. The equality test is still necessary (SQL stays as an index over
  large histories), but the *type* and the *semantics* must be singular, with SQL demoted to an
  optimization of a fold defined elsewhere — not a peer definition.

Principle 3 was in the draft and the draft still violated it. Corrected below.

## Why this exists

Sandhi measures well and shows almost none of it. Three gaps, each verified against
`develop` at `f7dadf4`:

1. **The in-process path has no stats surface at all.** No binding links `sandhi-store`
   (`grep -l sandhi-store bindings/*/Cargo.toml` → nothing), by the deliberate decision that
   keeps SQLite out of the wheels. Bindings accept a `sink_path` and emit raw events; there is
   no aggregation, no query, no UI. The `sandhi` CLI cannot fill the hole either — its `usage`
   subcommand is a thin HTTP client to `/admin/usage` (18 base-url/http references in
   `cli.rs`), so it is meaningless without a running proxy. **Victor consumes Sandhi
   in-process**, so the primary consumer is the one with no visibility.

2. **The newest metering fields are write-only.** #68 added `duration_ms`,
   `time_to_first_token_ms`, and `reasoning_tokens` to `UsageEvent`, the SQLite schema, the
   `INSERT`, and an idempotent migration. Nothing reads them back: `Bucket` — the only
   aggregate shape — carries `key/calls/tokens_in/tokens_out/cache_*` and no latency or
   reasoning field, and no query, CLI flag, or dashboard panel references them. We measure
   latency and discard it at the query layer.

3. **Live usage is unwired though the contract already allows it.**
   `ChatStreamEventV1::Usage` exists (`sandhi-core/src/chat.rs`). Victor's live readout is
   `estimated_content_tokens += len(chunk.content) / 4` — a chars/4 estimate — with
   authoritative numbers arriving only at the end. TD-0008's scorecard tracks consumer
   decisions for `reasoning_delta` and `refusal_delta`; **`Usage` has no row at all**, which is
   precisely the silent-gap pattern that TD exists to prevent.

## Non-goals

- **No dollars, anywhere in these surfaces.** Not in the dashboard, not in `sandhi usage`, not
  in the live counter. Pricing is downstream (ADR-0001). This is not hypothetical: the README
  advertised a "local cost display — from a community price table" until #71 removed it. A
  running cost is the most natural-feeling feature here and the one that breaks the product.
- First-party **OTel/Prometheus export** — a real gap, but a different shape (push, long-lived
  collectors, cardinality governance). Separate TD.
- **Admin/dashboard authentication** — tracked in SECURITY.md as unauthed-by-design for
  self-host. Richer stats raise the stakes; it does not change this design.
- Per-minute **rate-limit enforcement** — orthogonal, still open.

## Decisions

**D1 — The aggregate is a versioned contract type, not a struct in whichever crate happened to
need it first.** `UsageAggregateV1` lives in `sandhi-core` beside `UsageEvent`, derives
`JsonSchema`, is exported through `contract_schema_documents()` as
`schemas/usage-aggregate.v1.schema.json`, and is carried into the generated Python/TypeScript
facades. It crosses into the dashboard, the CLI, both bindings, and (later) the AnvaiOps control
plane — that is the definition of a boundary object, and boundary objects in this repo are
schema'd, digest-pinned, and drift-gated. `sandhi-store::Bucket` becomes a re-export of it, so
there is one shape end to end.

**D1a — The fold lives with the type; SQL is an index over it, not a second definition.**
`UsageAggregator` in core folds `&UsageEvent → UsageAggregateV1` and is the semantics. The store
keeps SQL because scanning a million rows through Rust is the wrong tool, but SQL is now an
*optimization of a defined fold* — if the two disagree, SQL is wrong by construction. Rejected:
linking `sandhi-store` into the bindings (SQLite's C build plus megabytes per abi3/napi wheel,
and it dissolves the crate boundary that exists for exactly this reason). Deferred: an optional
`sandhi-gateway[store]` extra for durable local history (P4), only on demand.

**D2 — Bounded cardinality, with an honest overflow.** `subject_id` and `session_id` are
unbounded in a long-lived process, so an unbounded map is a memory leak. Per-dimension
capacity (default 1024 keys, configurable), and everything beyond it folds into a single
`"(overflow)"` bucket. The failure mode is **losing per-key detail, never losing the sum** —
totals stay exact under eviction. Tested explicitly, not documented and hoped for.

**D3 — Exact where it enforces, honest where it estimates.** Token and call counts are exact in
both transports — budgets are enforced on them. Latency is summarised as a `LatencySummary`
carrying `samples`, `p50_ms`, `p95_ms`, and the type says in its schema that percentiles are
approximate: SQLite has no native percentile, a t-digest is not worth a dependency for an
operator panel, and a percentile over a bounded recent-N sample is genuinely useful for "is this
model slow for this team". Carrying `samples` in the payload means a consumer can see the
approximation's weight instead of inferring confidence it does not have. The rule: **approximate
what informs, never what enforces.**

**D4 — Incremental `Usage` stream events are progress signals, never metering events.** The
invariant "one `UsageEvent` per logical call, assembled by the caller" does not move. Emit
`ChatStreamEventV1::Usage` mid-stream **only where the provider actually reports it**
(Anthropic's `message_delta` carries cumulative `output_tokens`); for providers that report
terminally only (OpenAI `include_usage`), emit nothing extra — no synthetic deltas. Add an
optional `basis` discriminator (`provider_reported` | `partial`) so a consumer can tell an
authoritative running count from an in-flight one. Additive within v1 per TD-0002; requires
regenerating the schemas and binding facades, which `codegen-drift` enforces.

**D5 — Victor consumes it under TD-0008's operating rule 1.** A consumer-decision row for
`Usage`, then: use authoritative deltas for the live readout where present, keep chars/4 as an
explicitly-labelled estimate otherwise (`1,203 tok` vs `~1,200 tok est.`), and reconcile
against the terminal event. The UI must never show an estimate as if it were counted — the
whole point of Sandhi is that the meter is trustworthy.

**D6 — One semantics, one type, N transports — equality proven, not assumed.** The core fold is
the definition. Every transport (in-process snapshot, SQL, HTTP) must produce the same
`UsageAggregateV1` for the same event sequence, proven by a conformance test over a shared
fixture corpus (the TD-0001 pattern). SQL is the transport most likely to drift, because it
re-expresses the fold in another language; its test is therefore not optional.

**D7 — Co-design: the aggregate is a consumer contract, so consumers get a decision row.** Under
TD-0008 operating rule 1, shipping `UsageAggregateV1` obliges a recorded decision from each
consumer rather than a producer-side capability nobody consumes — the exact failure that TD
documents for `reasoning_delta`. Victor's decision: render per-session and per-run totals from
its own in-process snapshot (P2) instead of the chars/4 estimate, and reconcile against the
terminal event. Node/Python parity is required at the same commit, not as a follow-up, because
parity-as-follow-up is what produced TD-0008 P4.

## Phases

| Phase | Scope | Acceptance |
|---|---|---|
| **P1** | The boundary: `UsageAggregateV1` + `UsageAggregator` fold in core, schema + facades, `sandhi-store::Bucket` re-exports it, SQL fills it incl. latency/reasoning read-back, `sandhi usage` + dashboard render it | Core fold and SQL produce identical aggregates over a shared corpus; `codegen-drift` green with the new schema; latency carries `samples` |
| **P2** | Lib parity: `usage_snapshot_json()` on both bindings over the same type | Python and Node return byte-identical snapshots for the same event sequence; cardinality cap + overflow tested; no new dependency in either wheel |
| **P3** | Live metering: incremental `Usage` where provider-reported, `basis` field, Victor consumer row + labelled live counter | A scripted Anthropic stream yields a monotonic running count; an OpenAI stream yields none until terminal; a test asserts stream `Usage` events are never persisted as `UsageEvent`s |
| **P4** | Optional durable local history (`sandhi-gateway[store]`) | Only if P2 users ask for cross-restart history; wheel stays SQLite-free by default |

P1 now carries the boundary work the draft deferred, so P2 becomes mechanical: the type, the
fold, and the schema already exist, and the bindings only add a transport. P3 remains the only
phase that changes the *stream* contract.

## Pressure test

Each decision, attacked:

1. **"The in-core fold duplicates `sandhi-store`'s SQL."** Not any more: after D1a there is one
   *definition* and SQL is an index over it, kept because scanning a million rows through Rust is
   the wrong tool. The residual risk is that the SQL re-expression drifts from the fold, which is
   why D6 makes equality a test rather than an intention. If that test is ever dropped, this TD's
   main hazard goes live — and #78 is the proof it is not hypothetical.
1b. **"Promoting the aggregate to a schema'd contract type is over-engineering."** The counter-
   test is what happens without it: the bindings get no aggregate, the dashboard codes against a
   store-private struct, and the AnvaiOps control plane later invents its own shape — three
   consumers, three definitions, which is the defect class this TD exists to end. The cost is one
   `JsonSchema` derive, one line in `contract_schema_documents()`, and a facade regeneration that
   `codegen-drift` already polices. Cheap now, unpayable later.
2. **"The cardinality cap silently loses data."** It loses per-key rows and keeps totals exact
   (D2). A user reading "top 1024 subjects + overflow" is not misled; a process OOM-ing after
   three weeks of `session_id` keys would be. Alternative rejected: unbounded map with a
   documented warning.
3. **"A live counter will be read as billing truth."** The most dangerous item here. Three
   guards: the one-event-per-call invariant is untouched, `basis` labels in-flight counts, and a
   test asserts stream `Usage` never reaches a sink. If a downstream consumer sums stream events
   it will double count — that is the failure worth writing a test against, and worth stating in
   the schema description, not just this TD.
4. **"Approximate percentiles are worse than none."** For an operator answering "is this model
   slow for this team", a labelled p95 over recent-N beats an exact number nobody computes.
   Tokens — the thing budgets enforce on — stay exact. The line is: approximate what informs,
   never what enforces.
5. **"Adding `basis` breaks the wire contract."** Additive optional field, absent-when-None, so
   old consumers stay byte-identical (the #68 precedent). The enforcement is mechanical:
   regenerate schemas + facades or `codegen-drift` fails.
6. **"Node/Python will drift again."** TD-0008 P4 already had to fix Node lagging Python. P2's
   acceptance is byte-identical snapshots across both, so parity fails CI rather than review.
7. **"Why not just tell lib users to run the proxy?"** That answer forces a network hop and a
   second process on the in-process path whose entire selling point is neither. It also leaves
   Victor — the flagship consumer — permanently blind.

## Open questions

- Does the Anthropic codec currently surface `message_delta` usage mid-stream, or only fold it
  into the terminal parse? P3 starts with that measurement, not with code.
- Should the aggregator's dimension set be fixed (subject/group/provider/model/session) or
  configurable? Fixed first — configurable cardinality is how observability bills explode.
- Does `sandhi usage` grow a `--local <sink>` mode reading a JSONL sink directly, or is the
  binding snapshot the only lib-path answer? The former helps non-Victor lib users with no
  long-lived process.
