# TD-0019: Ingress codec hardening — the largest untrusted-input surface has no fuzzing

- **Status:** Draft (proposed), 2026-08-31. Owns gaps **G13, G14**.
- **Relates to:** [TD-0010](TD-0010-ingress-dialect-parity.md) (the dialect surface this hardens),
  [TD-0001](TD-0001-provider-adapter-qa-and-codegen.md) (the differential-oracle pattern this
  extends from provider *responses* to client *requests*),
  [TD-0014](TD-0014-data-plane-resource-safety.md) (whose G01 is the resource-exhaustion sibling of
  G13's parsing surface), [ADR-0006](../adr/0006-layer-boundary-and-protocol-scope.md) D4.4 (which
  makes a fuzz target mandatory for new parsing surface).

## Why this exists

The proxy's parsing surface is the largest attacker-reachable code in the repository, and it has
**zero fuzz or property coverage**. There is no `proptest`, no `cargo-fuzz`, no `quickcheck`, and no
`arbitrary` in any manifest in the workspace, and no `fuzz/` directory.

The surface has two halves, both reached before or during enforcement:

**Client-controlled (fully untrusted).** `codec.rs` is 2,316 lines implementing `decode_request`
(`:144`), `encode_response` (`:1279`), and `encode_stream_event` (`:1426`) across four ingress
dialects — OpenAI Chat, Anthropic Messages, OpenAI Responses, and Gemini. Every byte it decodes came
from a client. It runs **after** virtual-key resolution but **before** the budget reservation, on a
body up to `SANDHI_MAX_REQUEST_BODY_BYTES` (2 MiB by default). `normalize_envelope`
(`sandhi-providers/src/raw.rs:371`) also rewrites client bytes on the transparent plane.

**Upstream-controlled (semi-trusted, and the more dangerous of the two).** Six typed streaming
decoders parse provider SSE. "Semi-trusted" is doing a lot of work there: an operator can register
*any* `base_url` through the vault, so a misconfigured or hostile upstream feeds these decoders
directly. TD-0014 G01 covers their memory behaviour; their *parsing* behaviour is uncovered here.

Existing coverage is good but structurally limited. `provider_corpus.rs`,
`anthropic_corpus.rs`, `live_parser_conformance.rs`, and `differential_oracle.rs` are all
**example-based** — they prove the decoders handle the inputs someone thought of. TD-0001's
differential oracle is the right idea applied to provider responses; this TD applies the same
discipline to the inputs nobody thought of.

## First principles

1. **Anything reachable before enforcement must be hardened first.** A panic in `decode_request` is
   a request that never reached the budget check, from a caller who only needed a valid virtual key.
2. **Example tests prove presence, properties prove absence.** The corpus tests prove the decoders
   handle known shapes. Only generated input argues about unknown ones.
3. **A round-trip is an invariant, not a test case.** `decode` then `encode` should preserve
   semantics for every dialect pair. Stating that as a property tests the whole cross-product;
   stating it as examples tests the pairs someone enumerated.
4. **Fuzz the boundary, not the internals.** The targets are the public entry points — one body in,
   one result out — so they stay stable across refactors and do not ossify internal structure.
5. **A crash is the least interesting bug.** The interesting failures are silent: a decode that
   *succeeds* while losing a field, or shifts attribution, or produces a reservation ceiling of zero.
   The property assertions matter more than the panic-freedom.

## Non-goals

- **No new dialects.** This hardens what exists.
- **No schema validation as a rejection layer.** The dialects are permissive by design so vendor SDKs
  work unmodified; tightening acceptance would break TD-0010 parity. This TD makes the permissive
  path *safe*, not strict.
- **No fuzzing of `serde_json` itself.** It is a maintained dependency with its own corpus. The
  targets are Sandhi's interpretation of parsed JSON, not the JSON parser.
- **Not a replacement for the corpus tests.** Example tests pin known-good behaviour and stay.

## Decisions

**D1 — `proptest` for invariants, `cargo-fuzz` for panic-freedom, and they are different jobs.**
`proptest` runs in `cargo test` and in CI on every PR, generating structured near-valid inputs and
asserting semantic properties. `cargo-fuzz`/libFuzzer runs on demand and nightly on the Linux runner
with a persistent corpus, generating arbitrary bytes and asserting only that nothing panics, hangs,
or OOMs. Rejected: one tool for both — a coverage-guided fuzzer is too slow for a merge gate, and a
property runner explores structured space too narrowly to find a parser crash.

**D2 — Six fuzz targets, all at public boundaries.** `decode_request` per dialect (4);
`normalize_envelope` per family; and one shared typed-stream-decoder target driven by a family
selector byte. Each takes raw bytes and asserts no panic, no unbounded allocation, and bounded time.

**D3 — Four property families, asserted over generated `ChatRequestV1`/`ChatResponseV1` values.**

1. **Round-trip fidelity (G14).** For every dialect pair `(ingress, upstream)`, decoding then
   re-encoding preserves the semantically load-bearing fields — messages, tools, model,
   `max_output_tokens`, streaming intent. This is the property that guards ADR-0004 D1's translation
   plane, whose known weakness is silently dropping provider-specific extras.
2. **Metadata isolation.** No decode path can populate `subject_id`, `group_id`, or `virtual_key_id`
   in `RequestMetadataV1` from the request *body*. Those are key-authoritative (ADR-0004 D4), and the
   403 attribution check guards the *headers*; nothing currently proves the body cannot reach them.
3. **Ceiling soundness.** `reservation_ceiling` returns `≥ 1` and never overflows for any decodable
   request, including adversarial `max_output_tokens` values. A zero or wrapped ceiling is a budget
   bypass, not a rounding error.
4. **Error-shape totality.** Every rejected input produces a dialect-shaped error (TD-0010 D2), never
   a bare `{"error": ...}` and never a panic-derived 500.

**D4 — The corpus is committed, and every finding becomes a regression test.** Fuzz corpora live in
the repository so findings are reproducible and CI is not dependent on a fuzzing service. A crash
becomes a named `#[test]` in the relevant module before it is fixed — standard TDD, applied to
generated input.

**D5 — CI runs `proptest` always and `cargo-fuzz` nightly.** Property tests are fast, deterministic
with a fixed seed, and belong on the merge path. Fuzzing is time-boxed, non-deterministic, and
belongs on a schedule where a finding files an issue rather than blocking a PR.

**D6 — This TD establishes the obligation ADR-0006 D4.4 references.** After it lands, "new parsing
surface ships with a fuzz target in the same PR" is a rule with an existing harness to plug into,
rather than an unfunded mandate.

## Phases

| Phase | Scope | Acceptance (the failing test to write first) |
|---|---|---|
| **P1** | D2 — fuzz targets for the four ingress decoders | Each target builds and runs; a seeded corpus derived from the existing example tests reaches meaningful coverage; a deliberately malformed body is handled without panic. Any crash found is filed as a named regression test *before* the fix |
| **P2** | D3.2 + D3.3 — the two security properties | No generated request body can populate key-authoritative attribution fields; `reservation_ceiling` is `≥ 1` and overflow-free for every decodable request, including `max_output_tokens: u64::MAX` |
| **P3** | D3.1 — round-trip fidelity across all dialect pairs | For each `(ingress, upstream)` pair, decode-then-encode preserves the load-bearing field set. **Expected to fail initially** on cross-family pairs — ADR-0004 D1 already documents that the translation plane can drop extras; this phase converts a known caveat into a measured one |
| **P4** | D2 (stream decoders) + D3.4 | The typed-stream fuzz target survives a nightly run against all six families; every rejected ingress input renders in its caller's dialect |
| **P5** | D4 + D5 — CI wiring and the committed corpus | `proptest` runs on every PR; `cargo-fuzz` runs nightly on the Linux runner; a seeded crash reproduces from the committed corpus |

P2 is the highest-value phase: both properties are security invariants the codebase currently
*assumes* rather than proves, and both are cheap to state.

## Pressure test

1. **"The codec is well-tested — there are four corpus test files."** All example-based, and that is
   exactly the limit. They prove the decoders handle inputs someone imagined. The Gemini path added a
   fourth dialect and a colon-suffixed route parse (`lib.rs:199-203`) that no generated input has ever
   touched.
2. **"Fuzzing is a lot of infrastructure for a JSON decoder."** `serde_json` is not the target;
   Sandhi's *interpretation* is. `decode_request` reaches into loosely-typed `Value` trees across
   four dialects with per-family special cases — the classic shape for a panic on an unexpected type,
   and the classic shape for a silent field loss.
3. **"P3 will fail and we will just mark it `#[ignore]`."** The real risk. Which is why P3's
   acceptance is phrased as *measuring* a documented caveat rather than eliminating it: the
   deliverable is a precise statement of what the translation plane drops per dialect pair. That is
   useful even if the answer is "quite a lot."
4. **"Attribution isolation is already enforced by `permits_attribution`."** That check guards
   *headers* (`lib.rs:1573-1586`). D3.2 asserts the complementary property nothing currently tests:
   that no *body* field can reach those metadata slots. Two different paths to the same invariant,
   one of them unguarded.
5. **"Property tests are flaky."** With a fixed seed in CI and a separate exploratory mode they are
   deterministic. Non-determinism belongs in the nightly fuzz run, where a finding files an issue
   rather than failing someone's unrelated PR.
6. **"Upstream responses are trusted — operators register their own upstreams."** Any operator can
   register any `base_url` through the vault, and a compromised or merely broken upstream then feeds
   six decoders directly. Treating upstream bytes as trusted is an assumption worth writing down
   before relying on it.

## Open questions

- Should `proptest` generate `ChatRequestV1` values and encode *down* to a dialect, or generate
  dialect JSON and decode *up*? Down-then-up tests the encoder's totality; up-then-down tests the
  decoder's. Probably both, but they are different generators and different amounts of work.
- Is a coverage-guided fuzzer worth the nightly runner time versus a longer `proptest` run? Leaning:
  yes for the byte-level targets (D2) and no for the structured properties (D3).
- Does the round-trip property need a per-dialect "known lossy fields" allowlist to be useful? Almost
  certainly, and that allowlist is itself the valuable artefact — it is the first precise statement
  of what cross-family translation costs.
- Should the committed corpus be gitignored above some size? A large binary corpus in git is a real
  cost; a corpus that lives only on a runner is not reproducible. Leaning: commit minimised
  reproducers only, regenerate the rest.
