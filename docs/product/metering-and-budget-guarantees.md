# Metering semantics and budget guarantees

Status: W03 complete in the working tree; release/migration contract below. Strict-cap support remains unavailable.
Date: 2026-09-05
Tracker: [TD-0026](../td/TD-0026-gateway-product-evolution.md).

## Reasoning accounting: contract minor 7

`ParsedUsage`, `UsageV2` and `UsageEvent` carry optional `reasoning_included`:

| Marker | Meaning | Neutral measured total |
|---|---|---|
| `true` | Reasoning is already part of `tokens_out` | fresh input + cache creation + cache read + output |
| `false` | Reasoning is separate from `tokens_out` | fresh input + cache creation + cache read + output + reasoning |
| Absent / null | Legacy or custom measurement with unspecified convention | Preserve legacy arithmetic: add reasoning only when it exceeds output; this is compatibility behavior, not an inference of provider semantics |

The marker is set by the parser, not inferred from the provider label or count magnitude.
Gemini keeps candidate tokens in output and thoughts separately. OpenAI Chat/Responses keep
reasoning included in output. Anthropic, Cohere, Ollama and the Bedrock body parsers report no
separate reasoning dimension and mark output inclusive. Compatible servers must honor their
chosen dialect; arbitrary vendor deviations are not certified by a family label.

The official [OpenAI reasoning guide](https://developers.openai.com/api/docs/guides/reasoning)
shows reasoning as output-token detail and describes the output limit as covering reasoning.
The [Gemini usage metadata reference](https://ai.google.dev/api/generate-content#UsageMetadata)
defines separate candidate and thought counts, with their sum contributing to the total.
These definitions were checked on 2026-09-05; no live provider account was used for validation.

For example, 100 prompt tokens (30 cached), 40 candidate tokens and 25 Gemini thought tokens
measure **165**, not 140. Output counts remain 40 and reasoning remains 25; no category is
flattened merely to make totals agree. SQL computes the formula per row, and Rust aggregates
per event, so mixed conventions cannot cancel each other out at the aggregate level.

Transparent responses remain provider-byte-preserving. Translated usage converts into the
destination convention: Gemini separates candidates/thoughts; OpenAI/Responses/Anthropic include
reasoning in output. Prompt totals for OpenAI/Responses/Gemini include the cache categories;
Anthropic retains its distinct input/cache fields. No pricing weights are introduced.

Gemini's canonical Finish is deferred until successful stream EOF, after final usage, so
Responses completion embeds the measured count rather than zero. A transport failure cannot
publish the deferred success. Transparent Gemini bytes remain unchanged.

## Compatibility and historical data

- Major wire version stays 1; contract minor becomes 7. Schemas and Python/TypeScript facades
  are regenerated from the Rust contract. The formerly standalone event schema is now generated
  too, preserving its nonempty IDs, date-time, version, nonnegative count and strict-property
  validation constraints.
- SQLite adds a nullable `reasoning_included` column. Existing rows stay null and retain their
  prior totals. New writes store the marker; run-tree reconstruction carries it back into Rust.
  There is no automatic event rewrite, ledger adjustment or historical billing backfill.
- Some historical proxy events never stored reasoning at all. A provider slug cannot recover
  the missing count or certify its original convention. Reconciliation requires trusted external
  evidence and an explicit migration/adjustment decision; do not relabel those rows as corrected.
- Upgrade strict-schema validators and accounting consumers before exporting minor-7 events.
  Older Rust readers may ignore the optional field but compute the wrong total for separate
  reasoning; old JSON validators with `additionalProperties: false` may reject it. An additive
  field is not proof that an old accountant is semantically compatible.
- Back up before upgrade. Do not roll an accounting reader back onto mixed new/legacy data and
  assume its sums remain correct. Existing ledger charges are not recomputed when readers change.
- New custom parser integrations must set the marker explicitly. Omitting it intentionally
  retains legacy behavior. Missing provider counts remain unknown; this change does not infer
  unreported reasoning, tool-internal prompts, media detail or physical retry consumption.

## What budget enforcement actually guarantees

Admission reserves `ceil(ingress_body_bytes / 4) + effective_output_max`, at least one token.
The output default is injected for a capped Block request that omits an output limit, which
requires translation. The input term is a heuristic, not a tokenizer or upper-bound proof.

| Provider/path or request category | Input-bound evidence in Sandhi | Output/reasoning control | Advertised class |
|---|---|---|---|
| OpenAI Chat / compatible servers | bytes/4; no model-tokenizer proof | Mapped token maximum; compatibility/model behavior varies | Estimated admission |
| OpenAI Responses | bytes/4 | Mapped output maximum; official convention includes reasoning | Estimated admission |
| Anthropic | bytes/4, including serialized tools | Mapped maximum; thinking behavior depends on model/request | Estimated admission |
| Gemini | bytes/4 | Mapped output maximum and optional thinking configuration; no joint bound proof | Estimated admission |
| Cohere / Ollama upstream codecs | bytes/4 | Codec output parameter, dependent on server behavior | Estimated admission |
| Bedrock | Body parser only; no native signed proxy transport | No proxy admission certification | Not admitted as a native proxy transport |
| CJK, multilingual text, tools, cached prompts | Encoding size is not tokenizer output; provider-added material may differ | Output-only controls do not bound input | Estimated admission |
| Images, audio, remote media, provider tool use, multiple candidates | No validated all-category token bound | No proven total-cost bound | No strict guarantee |

**No provider/model combination currently qualifies as a strict total-token cap.** There is no
strict mode to select today. A Block policy atomically rejects a reservation exceeding available
capacity; it does not promise that later measured usage stays below that reservation. Warn
admits over budget and tracks usage. Volatile mode resets on restart; durable single-node mode
persists leases and charges. Fleet coordination remains a separate gate.

Measured settlement is never clamped to an estimate. Every in-flight underestimated call can
contribute excess: two reservations of 50 under a 100 cap may settle at 80 and 90, producing 170
spent. The next admission is refused, but the 70 excess is from two calls, not bounded by one.
`sandhi_settle_overshoot_total` and `sandhi_settle_overshoot_tokens_total` expose this discrepancy.
Interrupted streams can also contain estimated/partial counts; finality and measurement basis
are distinct from whether an admission estimate was accurate.

Future strict eligibility requires a model/request-specific input bound plus all output,
reasoning, candidate and tool/media dimensions, tested under concurrency and failure. Unsupported
combinations must be rejected by such a future strict policy, never silently downgraded. These
proofs are not supplied by the current corpus or by a provider's advertised context length.

## Verification scope

- Core parser/formula tests cover zero, smaller, equal and larger reasoning counts, both explicit
  conventions, legacy round trips, cache splits and saturating arithmetic.
- Existing provider corpus and chunk-boundary tests pin parser/sniffer conventions for supported
  adapters. Proxy tests carry reasoning through partial usage and persisted event construction.
- Real-gateway tests cover four ingress dialects × complete/streaming × three reasoning sizes,
  checking encoded counts, SQLite events, run aggregates and actual lease settlement.
- SQL tests mix explicit and legacy rows and compare query totals with the Rust formula and run
  tree. Lease tests pin concurrent under-reservation behavior. Binding tests validate exported
  counts, markers and accumulated spend.

These are synthetic contract and regression tests, not model-tokenizer calibration, live-provider
certification, full historical reconciliation or durable physical-attempt accounting (W05).
