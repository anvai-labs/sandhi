# TD-0001: Provider-adapter QA corpus + codegen pilot

Status: **Open** (tracker). Governed by [ADR-0003](../adr/0003-provider-adapter-authoring-and-codegen.md).
Date opened: 2026-07-20.

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

- [ ] Add `crates/sandhi-providers/tests/fixtures/<provider>/` with recorded **real** response
      bodies + SSE streams (secrets scrubbed): non-stream JSON and streamed SSE, one per provider,
      each with a `expected_usage.json` (`ParsedUsage` ground truth).
- [ ] Anthropic **first** (highest cache-split risk): capture a stream that exercises
      `cache_creation_input_tokens` **and** `cache_read_input_tokens` non-zero.
- [ ] A replay test drives each SSE fixture through `Provider::stream` (wiremock-served) and
      asserts the finalized `ParsedUsage` equals `expected_usage.json`.
- [ ] **Chunk-boundary property test:** re-feed each SSE fixture split at *every* byte offset (or
      a randomized subset) and assert the accumulated usage is invariant — guards the
      `metered_passthrough` / `sniff_usage_line` line-buffering against a `usage` field straddling
      two `Bytes` chunks.
- [ ] **Forward-compat test:** inject an unknown field into each fixture and assert the meter is
      unaffected (no panic, same counts).

### W2 — Differential test oracle (ADR-0003 §3)

- [ ] Dev/CI-only: generate a deserializer for the usage/response shape from a **byte-pinned**
      provider schema (`typify`), under `#[cfg(test)]` or a dev-dependency — never linked into
      `sandhi-proxy`/bindings.
- [ ] Assert the generated deserializer and the shipped `parse_*_usage` agree on the usage fields
      over the W1 fixtures. Divergence = spec drift or extractor bug.
- [ ] Pin the source schema by digest in-repo (OpenAI official spec; Anthropic community spec —
      note the provenance explicitly, per ADR-0003 context).

### W3 — `typify` narrow-model pilot (ADR-0003 §2 + §4) — *optional, gated on W1/W2 signal*

- [ ] Pilot on the **Anthropic** usage/cache-split shape only: `typify`-generate the narrow struct
      under the no-hand-edit-generated + adjacent-overlay discipline; back `parse_anthropic_usage`
      with it.
- [ ] Add a `scripts/gen-provider-models.sh` (regenerate, don't patch) + a CI drift check
      (regenerate → `git diff --exit-code`), mirroring AnvaiOps' `--check` codegen gates.
- [ ] Decide from the pilot whether to extend to other providers or leave them on the hand-written
      `u64_at` baseline (ADR-0003 §2 makes this per-provider and optional).

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
