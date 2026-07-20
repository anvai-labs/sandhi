# ADR-0003: Provider adapter authoring — hand-written transport, generated types as a narrow reference + test oracle

Date: 2026-07-20

## Status

Accepted. Refines **ADR-0001** (which established `sandhi-providers` and named the
adapter / strategy / factory / decorator patterns) and complements **ADR-0002** (which
scoped the crate to chat-completion transport). This ADR settles a question ADR-0001 left
implicit: **how provider adapters are *produced and maintained*** — hand-written vs.
generated from each provider's OpenAPI description. Tracked implementation + QA doctrine
live in [TD-0001](../td/TD-0001-provider-adapter-qa-and-codegen.md).

## Context

Providers publish (or imply) OpenAPI descriptions, and Rust has real OpenAPI→client tooling
(`progenitor`, `typify`, `openapi-generator`). The recurring proposal is therefore: *generate
the first cut of each provider client from its spec, hand-maintain later, and QA exhaustively —
isn't that the faster mechanism?* This ADR answers that, and fixes the answer so it is not
re-litigated per provider.

**What the crate actually is today** (verified against the code, not intention):

- Every adapter is **hand-written** with `reqwest` directly — `anthropic.rs`, `openai.rs`
  (`OpenAiCompat`, covering ~20 OpenAI-compatible providers), `gemini.rs`, `cohere.rs`,
  `local.rs` (Ollama), plus the `escape_hatch.rs` host-language `FnProvider`.
- The hot path is already **byte-transparent**: `metered_passthrough` forwards each upstream
  chunk verbatim (O(1) memory, ADR-0047 D9) while sniffing newline-delimited lines for usage;
  `stream()` yields raw `Bytes` and finalizes usage on the terminal item.
- **Usage extraction is the metering-critical core** and lives in `sandhi-core::usage` as pure
  functions over the provider's real response JSON (`parse_anthropic_usage`,
  `parse_openai_usage`, `parse_gemini_usage`, `parse_cohere_usage`, `parse_ollama_usage`,
  `parse_bedrock_usage`), returning `ParsedUsage { tokens_in, tokens_out,
  cache_creation_tokens, cache_read_tokens }`. Getting the **prompt-cache split** right is the
  whole reason the meter is trustworthy (ADR-0047 D10 / AnvaiOps ADR-0020 D4).

**The ecosystem fact that decides it.** In May 2026 Anthropic **acquired Stainless** — the SDK
compiler OpenAI, Gemini, and Meta depend on — and is **winding down the hosted generator**.
Anthropic's own OpenAPI description is derived from its TypeScript SDK and is *not officially
published* (only an unofficial community spec exists). OpenAI and Azure OpenAI publish clean
specs; Anthropic — our most important and most cache-split-sensitive provider — effectively does
not. So "generate the client from the provider's published OpenAPI" would rest on the *weakest,
unofficial* spec exactly where metering trust matters most. A design that hard-depends on full
generated SDKs is only as strong as its flimsiest provider spec.

## Decision

### 1. Hand-written adapters are the default and the only *shipped* transport

Full-surface OpenAPI client generation (`progenitor`, `openapi-generator`) is **not** shipped on
the request path. Rationale:

- **The gateway's job is transport + usage extraction, not a typed SDK.** Each adapter needs
  ~4 provider-specific facts — auth header shape, base URL, where usage lives in the response,
  and SSE framing — roughly 40 lines. That variance is *smaller and clearer hand-written* than
  a generated client carved down.
- **Streaming is where generated clients are weakest.** Provider SSE is a discriminated union
  (`message_start` / `content_block_delta` / `message_delta` …) — the exact construct
  generators handle poorly — and ADR-0047 D9 forbids deserialize-then-reserialize (the byte
  prefix must stay exact for prompt-cache hits; JSON round-trips are ~83× slower than byte
  pass-through, AnvaiOps ADR-0028). A generated full client fights both constraints.
- **Spec churn + unofficial specs.** Post-Stainless, provider specs drift and (for Anthropic)
  are unofficial. A shipped generated client is a maintenance treadmill pinned to the least
  reliable input.

### 2. Narrow typed *models* MAY be `typify`-generated — as an optional strengthening

Where a provider's **usage/cache-split response shape** is worth compile-time safety, that narrow
struct MAY be generated with `typify` from a **byte-pinned** provider schema and used to back the
corresponding `parse_*_usage`. It is admitted **only** for the metering-critical shape, never the
full API surface, and only under the discipline in §4. Today's hand-written `u64_at` parsers are
acceptable and remain the baseline; generating is a per-provider *option*, exercised when a
spec/complexity pressure justifies it — not a mandate.

### 3. Full generated clients are admitted **only as a CI test oracle**, never shipped

A full client generated from a provider spec is valuable as a *check*, not as production code.
In CI, run the generated deserializer and the shipped `parse_*_usage` against the **same recorded
real fixtures** and assert they agree on the usage fields. Agreement raises confidence for free;
divergence flags either spec drift or an extractor bug. The generated code lives in dev/CI only
and is never linked into `sandhi-proxy` or the bindings.

### 4. Generated code is never hand-edited — generated core + adjacent overlay

Any generated artifact (a `typify` model per §2, an oracle client per §3) is regenerated from its
pinned source and **never hand-edited**; all human changes live in an *adjacent* layer (newtype
wrappers, extension traits, the hand-written adapter). This is the same generated-core +
hand-overlay discipline AnvaiOps already runs successfully — `types.generated.ts` (regenerated,
untouched) beside `types.ts` (hand overlay), and the spec-driven `ingest_documents` client
(ADR-041) regenerated rather than patched. The first manual edit to generated code kills
regeneration; the overlay rule prevents that.

### 5. "Exhaustive QA" means fixture-replay + differential + property tests — not code audit

The QA that retires metering risk is a corpus of **recorded real provider streams**
(wiremock-served, extending the existing `anthropic.rs` / `usage.rs` tests), asserting that the
accumulated `ParsedUsage` equals the provider-reported usage — including the **cache split**,
**chunk-boundary line splitting** (usage tokens straddling two `Bytes` chunks), and
**unknown-field forward-compatibility** (a new provider field must not fault the meter). Auditing
the lines of a generated client is *not* the QA that matters and does not substitute for this.

### 6. Why "generate-then-own" is scoped the way it is

Generate-first-then-hand-maintain is genuinely faster **only where generated ≈ hand-written**:
the narrow models of §2. For a full client it *inverts* — you end up owning and exhaustively
testing far more code than you ship, to meter two-to-four integers. So generate-then-own is
admitted for narrow models (§2) and the test oracle (§3), and **excluded from the shipped
transport** (§1). The cost that matters is never generation; it is ownership + verification,
and this scoping keeps that surface minimal and pinned to exactly what is billed.

## Consequences

- **Positive:** sandhi stays a focused usage gateway; the hot path is forward-compatible and
  never couples to an unofficial/churning provider spec; the test surface is small and pinned to
  the billed fields; codegen is still *available* where it earns its keep, with a clear admission
  bar.
- **Cost:** the `parse_*_usage` extractors are hand-maintained — mitigated by the §5 fixture +
  §3 differential corpus, which catches drift at the field that is billed. A future *typed-client
  SDK product* (should one ever exist) is a separate cold-path artifact governed by ADR-0002's
  modality discipline, not this transport.
- **Neutral:** ratifies the current hand-written status quo and adds the codegen-admission rule
  (§2–§4) + QA doctrine (§5) so the next "just generate the clients" proposal has a settled bar.

## References

- [ADR-0001](0001-sandhi-architecture-and-wire-contract.md) — architecture, adapter/decorator patterns
- [ADR-0002](0002-provider-transport-scope-and-modality-admission.md) — chat-only scope + modality gate
- [TD-0001](../td/TD-0001-provider-adapter-qa-and-codegen.md) — implementation + QA tracker for this ADR
- AnvaiOps ADR-0047 D9/D10 (cache/KV affinity, unified transport), ADR-0020 D3/D4 (measure≠bill, full usage breakdown), ADR-0028 (byte-vs-JSON cost)
- Tooling: `oxidecomputer/progenitor` (OpenAPI 3.0 client), `oxidecomputer/typify` (JSON Schema → serde types)
- Ecosystem: Anthropic's acquisition of Stainless and wind-down of the hosted SDK generator (May 2026); Anthropic has no officially published OpenAPI spec (community spec only)
