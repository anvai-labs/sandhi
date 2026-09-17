# Three-way co-design: cumulative adversarial review and delivery gates

Review scope: InferFlux producer → Sandhi proxy/typed binding → Victor consumption and pricing,
including S1-S5's cumulative diff against Sandhi main. This is a same-session code/behavior
review, not an independent reviewer attestation. No claim of loaded-model or GPU validation.

## Release inventory at review start

| Repository | Integration | Published release | Gap |
|---|---|---|---|
| InferFlux | main `ac8587319` includes origin pin `273780835` and tokenize #177 | v0.2.0 | Origin fixes postdate release |
| Sandhi | develop `aea3b56`; main `39d2998` | v0.6.1 | S1-S5 not promoted or published |
| Victor | #1066 `3b5aa2b0d` is in main and v0.9.4 | v0.9.4 | Package still pins sandhi-gateway 0.5.0 |

The earlier TD “Complete” meant integration, not delivery. Owner approved Sandhi v0.7.0 on
all four target groups, InferFlux v0.3.0, and focused Victor v0.9.5 after green gates. Victor's
unrelated develop changes are excluded from the focused release. No immutable tag is to be moved.

## Findings and corrective work

1. **Release blocker — producer/consumer release mismatch.** Promote and publish Sandhi before
   updating Victor's exact dependency; verify InferFlux's release contains the conformance pin.
2. **Correctness — reasoning inclusion lost in Victor.** Neutral mapping discarded
   `reasoning_included`; count comparison alone underprices separate reasoning when it is smaller
   than visible output. Preserve the flag and fold reasoning per call before aggregation.
   A real three-way probe additionally exposed Pydantic `Dict[str, int]` coercing the flag to
   0/1; response/stream models must preserve booleans. Test both folded/unfolded counts and mixed calls.
3. **Correctness — timing provenance lost in Victor.** Preserve independent source labels into
   terminal diagnostics and metrics; reject nonfinite/boolean timing and avoid reusing stale
   provenance when a newer measurement has no label.
4. **Correctness — absent/malformed origin timing.** `ParsedUsage.apply` cleared an existing
   boundary value when origin timing was absent. Preserve fallback independently per field;
   reject values that would wrap negative in SQLite's signed integer storage.
5. **Evidence — conformance recorder masked defects.** `urllib` normalized header case and the
   recorder collected whole SSE bodies. Use case-preserving `http.client` and progressive
   forwarding; probe header/scheme casing directly with valid/invalid keys; inject spoofed
   attribution; assert reasoning ordering and exact rounded persistence, not merely presence.
6. **Evidence — overstated calibration/coverage claims.** S3's tuples are synthetic, not a
   captured provider/CJK corpus. Label them accurately. S4 coverage was 88.32% lines and 86.80%
   regions, not 86.8% lines. Keep product validation distinct from wire-contract conformance.
7. **Release routing — the S1 runner-label collision also remained in release jobs.** Organization
   runners still advertise `ubuntu-latest`; use explicit `ubuntu-24.04` across Sandhi release
   builds/publishers, matching CI, and pin the rule in a workflow regression test.

## Verified local evidence

- `cargo test --workspace --quiet`: passed, with existing opt-in tests ignored.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- Strengthened exact-pin SDK origin suite: 16 passed, both think-tag and harmony fixtures.
- Victor targeted semantics/transport/conformance/parser/stream tests: 158 passed before the
  real-probe-discovered Pydantic fix; expanded tests and three-way probe are release gates below.
- Candidate Python wheel built from this worktree; installed in isolated `/tmp/codesign-review-venv`.
- Actual Victor policy → candidate binding → proxy → pinned InferFlux probe: 2 passed, covering
  buffered and streamed calls for both reasoning families, inclusion flags, provenance, and auth.

## Residual limitations and optimization opportunities

- Calibration keys use request provider/model, not resolved origin identity. Alias routing or
  model replacement can change a key's distribution; cold-start reset/version-aware identity
  deserves a separate design. Current EWMA is a statistical estimate, never a hard budget bound.
- Calibration's process-local map has no cardinality cap. Consider a bounded cache with cold-start
  fallback and admission-estimator telemetry; benchmark contention before sharding the mutex.
- Synthetic low-ratio regressions do not establish coverage under distribution shifts. Capture
  paired multilingual request/authoritative-usage fixtures without adding tokenizer calls.
- Origin and boundary timing cover different intervals (and possibly different retry scopes).
  Persisted provenance must remain visible in analytics; aggregate percentiles currently mix
  sources. Source-separated aggregation is a follow-up, not an end-to-end latency claim.
- Stub tests prove protocol behavior, not real tokenizer accuracy, model reasoning quality,
  throughput, loaded-model cancellation/recovery, or production usability.
- The optional Victor probe must use an explicitly selected source checkout and candidate wheel;
  ordinary SDK CI must not silently import an arbitrary installed Victor.

## Delivery gates (open until remote evidence is recorded)

- Focused review fixes merged through green PR checks; exact post-merge CI verified.
- Cumulative Sandhi develop→main promotion reviewed, approved as required, and exact-main CI green.
- InferFlux v0.3.0 source/version and artifacts verified; original pin remains reproducible.
- Sandhi v0.7.0 GitHub, PyPI, all four crates, all three npm packages verified by release verifier.
- Victor exact pin advanced to Sandhi 0.7.0, known contract minor updated, focused main release
  tested with actual binding, and v0.9.5 published. Back-sync protected branches afterward.
- Release notes/status updated only when these gates really complete.
