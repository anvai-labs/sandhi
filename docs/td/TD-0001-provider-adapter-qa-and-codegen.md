# TD-0001: Provider-adapter QA corpus + codegen pilot

Status: **Complete** (W1 + W2 + W3 all landed). Governed by
[ADR-0003](../adr/0003-provider-adapter-authoring-and-codegen.md).
Date opened / closed: 2026-07-20.

This tracks the concrete work that operationalizes ADR-0003. The ADR is the settled decision;
this TD is the actionable checklist. Each item lands as its own `feat/*` or `test/*` PR against
`develop`, referencing ADR-0003 — **not** folded into the docs PR that records the decision.

## Why this exists

ADR-0003 rules that adapters stay hand-written, that `typify`-generated narrow models and full
generated clients are admitted only as (respectively) an optional strengthening and a CI test
oracle, and that "exhaustive QA" means recorded-fixture replay + differential + property tests.
None of that QA machinery exists yet — today's tests are inline `wiremock` unit tests with
synthetic bodies (`anthropic.rs`, `usage.rs`). This TD closes that gap.

## Work items

### W1 — Recorded-fixture usage corpus (the risk-retiring QA) — *highest value first*

- [x] Add `crates/sandhi-providers/tests/fixtures/<provider>/` with response bodies + SSE streams
      (secrets scrubbed): non-stream JSON and streamed SSE, each with an `expected_usage.json`
      (`ParsedUsage` ground truth). **Anthropic done** (`tests/fixtures/anthropic/`); other shipped
      adapters (OpenAI, Gemini, Cohere, Ollama) still to add.
- [x] Anthropic **first** (highest cache-split risk): a stream that exercises
      `cache_creation_input_tokens` **and** `cache_read_input_tokens` non-zero
      (`stream_cache_split.sse`, both = 2048 / 4096).
- [x] A replay test drives the SSE + non-stream fixtures through `Provider` (wiremock-served) and
      asserts the finalized `ParsedUsage` equals `expected_usage.json` — plus byte-exact
      pass-through on the stream (`tests/anthropic_corpus.rs`). *(Anthropic; extend per provider.)*
- [x] **Chunk-boundary property test:** re-feeds the SSE fixture split at *every* byte offset (and
      one-byte-per-chunk) and asserts the accumulated usage is invariant — guards the
      `metered_passthrough` / `sniff_usage_line` line-buffering against a `usage` field straddling
      two `Bytes` chunks. *(`stream_usage_invariant_across_every_chunk_boundary`.)* As part of this,
      `Anthropic::stream` was refactored to reuse the shared `metered_passthrough` primitive (was a
      duplicated inline loop) so the test covers the exact production path.
- [x] **Forward-compat test:** unknown event types + unknown usage fields
      (`stream_forward_compat.sse`) leave the meter unaffected — no panic, same counts
      (`stream_usage_ignores_unknown_events_and_fields`). *(Anthropic; extend per provider.)*

> **W1 status: COMPLETE.** All shipped adapters now carry the fixture + replay + chunk-boundary +
> forward-compat set — Anthropic (`anthropic_corpus.rs`), and OpenAI / Gemini / Cohere / Ollama
> (`provider_corpus.rs` + per-module unit tests). As part of this, every adapter's streaming path
> was unified on the shared `metered_passthrough` primitive with a named `sniff_usage_line` (OpenAI
> was the last inline loop; Gemini/Cohere/Ollama had anonymous closures), and a shared
> `#[cfg(test)] crate::accumulate_usage` helper drives the chunk-boundary property for every
> provider against its exact production sniff. Fixtures are faithful representative captures of the
> documented shapes; a real recording drops in unchanged (same harness). Next: **W2** (differential
> test oracle) and optional **W3** (typify narrow-model pilot).

### W2 — Differential test oracle (ADR-0003 §3)

- [x] Dev/CI-only: `typify` generates the deserializer from a **byte-pinned** provider schema as a
      **dev-dependency**, in the `tests/differential_oracle.rs` target only — verified **not** in the
      `sandhi-proxy` / python-binding / normal-deps graph (`cargo tree` = 0), so it is never shipped.
- [x] Assert the generated deserializer and the shipped `parse_*_usage` agree on the usage fields
      over the W1 fixtures (OpenAI + Anthropic — the cache-split pair named in ADR-0003). Divergence
      = spec drift or extractor bug — and it earned its keep immediately: the OpenAI oracle caught
      that the fixture omitted the spec-**required** `total_tokens`, which was then fixed in the
      fixtures (not by weakening the schema).
- [x] Pin the source schema in-repo with explicit provenance (`tests/schemas/`): OpenAI is the
      **real** `components/schemas/CompletionUsage` from `openai/openai-openapi` (sha256
      `0bf136e5…`, fetched 2026-07-20); Anthropic is hand-authored from the documented Messages
      `usage` object (no official spec exists; community spec was unreachable — see ADR-0003
      context). Both carry a `$comment` provenance header.

> **W2 status: COMPLETE** for the OpenAI + Anthropic pair (the ADR-0003 acceptance bar). Extending
> the oracle to Gemini/Cohere/Ollama is straightforward (same harness) but lower value — those have
> no cache split — and is left as a follow-up. typify stays a **dev-dependency**; the shipped crate
> and bindings never link it.

### W3 — `typify` narrow-model pilot (ADR-0003 §2 + §4) — *optional, gated on W1/W2 signal*

- [x] Piloted on the **Anthropic** usage/cache-split shape: `parse_anthropic_usage` is now backed by
      the typify-generated `crate::generated::anthropic_usage::AnthropicMessageUsage`
      (`crates/sandhi-core/src/generated/`), regenerated from the byte-pinned schema
      (`crates/sandhi-core/schemas/`) — the parser function is the hand-written **overlay** (ADR-0003
      §4). Behavior-preserving: all schema fields optional ⇒ missing → 0, same as the prior `u64_at`.
- [x] `scripts/gen-provider-models.sh` (regenerate, never patch) + CI **`codegen-drift`** job
      (regenerate → `git diff --exit-code`, gated on the `codegen` path filter). typify runs as a
      **standalone CLI**, never a cargo dependency — verified absent from the sandhi-core/proxy/
      binding build graphs (`cargo tree` = 0); the committed output depends only on `serde`
      (ADR-0003 §3). Generated code excluded from the coverage metric.
- [x] **Decision — do NOT extend generation to the other providers; keep the hand-written `u64_at`
      baseline.** Evidence from the pilot:
      1. **Cost/benefit is upside-down for these shapes.** typify emits **~190 lines** of
         builder/error/`TryFrom` machinery to represent a 4-field struct that the hand parser reads
         in ~8; ~90 of those are dead (`#[allow(dead_code)]`) and 0%-covered.
      2. **It collapses the W2 oracle for the generated provider.** Once `parse_anthropic_usage` is
         itself typify-generated, the W2 Anthropic differential oracle becomes generated-vs-generated
         (tautological). The oracle's independence — its whole value — survives only for the
         **hand-written** providers (OpenAI et al.), so generating *more* providers would erode
         *more* oracle coverage. The two techniques are complementary, not cumulative.
      3. **No spec/complexity pressure exists** (ADR-0003 §2's admission bar): the usage objects are
         flat and stable. Generation earns its keep only for large/nested/churning shapes — none here.
      The pilot proves the machinery works end-to-end (schema → CLI → committed model → overlay →
      drift check) and is available when a shape ever justifies it; the Anthropic pilot stays as the
      reference implementation.

## Acceptance / exit criteria

- W1 corpus covers every shipped adapter; the cache-split, chunk-boundary, and forward-compat
  properties are asserted for each.
- W2 differential oracle runs in CI for at least Anthropic + OpenAI.
- Coverage stays ≥ 75% (the repo gate) and clippy `-D warnings` stays green.
- W3 is explicitly **optional**: closing this TD does not require shipping generated models, only
  proving/deciding the pattern.

## Non-goals (per ADR-0003)

- No full generated client shipped on the request path.
- No new API modality (that is ADR-0002's gate, not this TD).
- No pricing/dollars anywhere near the extractor or fixtures (ADR-0001 boundary).
