# TD-0009: Usage-visibility surfaces — lib parity, read-back, and live metering

- **Status:** Proposed (2026-07-25) — design gate, no code yet
- **Relates to:** ADR-0001 (wire contract, measure-vs-price), TD-0002 (typed runtime,
  additive-only contract policy), TD-0003 (operator surface: CLI, admin API, dashboard),
  TD-0008 (Victor–Sandhi co-design boundary)
- **Companion changes:** #68 (latency + reasoning-token fields), #61 (transparent plane)

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

**D1 — Lib-path aggregation lives in `sandhi-core`, with no new dependency.** A
`UsageAggregator` fed by the existing `Sink` trait, keyed by the same dimensions the store
already groups by, returning the same `Bucket` shape. Rejected: linking `sandhi-store` into the
bindings (adds SQLite's C build and megabytes per abi3/napi wheel, and dissolves the crate
boundary that exists for exactly this reason). Deferred: an optional `sandhi-gateway[store]`
extra for durable local history (P4), only on demand.

**D2 — Bounded cardinality, with an honest overflow.** `subject_id` and `session_id` are
unbounded in a long-lived process, so an unbounded map is a memory leak. Per-dimension
capacity (default 1024 keys, configurable), and everything beyond it folds into a single
`"(overflow)"` bucket. The failure mode is **losing per-key detail, never losing the sum** —
totals stay exact under eviction. Tested explicitly, not documented and hoped for.

**D3 — Read back what we already write; exact tokens, approximate latency.** Extend the query
layer additively (`Bucket` gains optional latency/reasoning fields; existing consumers
unaffected). Token and call counts stay exact SQL aggregates. Latency percentiles are computed
in Rust over a bounded recent-N sample per window and **labelled approximate** — SQLite has no
native percentile, and a t-digest is not worth a dependency for an operator panel.

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

**D6 — One aggregation semantics, two implementations, proven equal.** The in-core aggregator
and the SQL queries must produce identical buckets for the same event sequence. That is a
conformance test over a shared fixture corpus (the TD-0001 pattern), not a code-review promise.

## Phases

| Phase | Scope | Acceptance |
|---|---|---|
| **P1** | Read back #68's fields: store query + `Bucket` extension, `sandhi usage` columns, dashboard panel | Latency p50/p95 + reasoning tokens visible per dimension; approximation labelled; existing `Bucket` consumers compile unchanged |
| **P2** | Lib parity: `UsageAggregator` in core + `usage_snapshot_json()` on both bindings | Python and Node return byte-identical snapshots for the same event sequence; cardinality cap + overflow bucket tested; no new dependency in either wheel |
| **P3** | Live metering: incremental `Usage` where provider-reported, `basis` field, Victor consumer row + labelled live counter | A scripted Anthropic stream yields a monotonic running count; an OpenAI stream yields none until terminal; a test asserts stream `Usage` events are never persisted as `UsageEvent`s |
| **P4** | Optional durable local history (`sandhi-gateway[store]`) | Only if P2 users ask for cross-restart history; wheel stays SQLite-free by default |

P1 is independent and worth landing alone. P3 is the only phase that touches the wire schema.

## Pressure test

Each decision, attacked:

1. **"The in-core aggregator duplicates `sandhi-store`."** It does, deliberately — different
   lifetime (process-local, no durability, no file). The real risk is *semantic drift* between
   the two aggregations, which is why D6 makes equality a test rather than an intention. If that
   test is dropped, this TD's main hazard is live.
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
