# Three-way co-design: cumulative adversarial review and delivery gates

Review scope: InferFlux producer → Sandhi proxy/typed binding → Victor consumption and pricing,
including S1-S5's cumulative diff against Sandhi main. This is a same-session code/behavior
review, not itself an independent reviewer attestation. Subsequent independent reviews and
remote evidence are recorded below. No claim of loaded-model or GPU validation from CPU tests.

## Release inventory at review start

| Repository | Integration | Published release | Gap |
|---|---|---|---|
| InferFlux | main `ac8587319` includes origin pin `273780835` and tokenize #177 | v0.2.0 | Origin fixes postdate release |
| Sandhi | develop `aea3b56`; main `39d2998` | v0.6.1 | S1-S5 not promoted or published |
| Victor | #1066 `3b5aa2b0d` is in main and v0.9.4 | v0.9.4 | Package still pins sandhi-gateway 0.5.0 |

The earlier TD “Complete” meant integration, not delivery. Owner approved Sandhi v0.7.0 on
all four target groups, InferFlux v0.3.0, and Victor v0.9.5 after green gates. The later explicit
rebase-first instruction supersedes the original main-only Victor scope: the release candidates
now include current develop, requiring fresh cumulative review and CI. No immutable tag is moved.

## Current delivery checkpoint (2026-09-17)

| Repository | Completed evidence | Still open |
|---|---|---|
| Sandhi | Independent review clean; #261 integrated at `db74cf91`; #262 promoted to `e05d7c5`; exact-main [CI 35193468410](https://github.com/anvai-labs/sandhi/actions/runs/35193468410) passed; v0.7.0 [all-target publication 35203459727](https://github.com/anvai-labs/sandhi/actions/runs/35203459727) and independent verifier passed | Protected evidence back-sync |
| InferFlux | Original release preparation #180 merged at `274b74bc`; broader candidate rebased onto develop `d10b5bbc`; 47 CPU CTest groups pass; independent commit-bound review clean at `3f45fed87` | [PR #181](https://github.com/anvai-labs/inferflux/pull/181) hosted CI, promotion, exact-source CUDA/ROCm and native packaging/install smoke, v0.3.0 publication |
| Victor | Candidate rebased onto develop `2311fa1f2`; inclusion/provenance and per-call accounting fixes present; curated-tool/session-context fixes independently reviewed with 125 tests and additional cancellation probes | Published Sandhi 0.7.0 dependency validation, final commit-bound review, fresh CI, promotion and v0.9.5 publication |

Sandhi's initial hosted verification failed after six npm platform-package HTTP 404s, although
publication jobs succeeded. The independent verifier subsequently passed all targets; retrying
only the read-only failed verifier produced successful attempt 2. No artifacts were republished
and no tag was moved. GitHub archive sizes/SHA-256, required PyPI platforms, four non-yanked crate
versions and all npm versions/dependencies passed `verify-release.py v0.7.0 --targets
pypi,crates,npm,github --repo anvai-labs/sandhi`.

The rebased InferFlux candidate passed all 18 local three-way probes (16 origin SDK cases plus
2 actual Victor-provider cases) through the candidate Sandhi wheel. This is CPU/stub wire evidence,
not a test of the published 0.7.0 wheel or GPU/model behavior. Sandhi's reproducibility pin remains
`273780835f120cbe1a8da4860d72905861e71e29`; testing a newer candidate does not silently move it.

The local independent-review hook checks the actual pushed commit/tree, including annotated
tags. It is an unsigned local process guard, not a GitHub approval or tamper-proof trust boundary.

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
8. **Rebased InferFlux regressions.** Mixed-format tool calls could reorder; streamed tool calls
   could lose reasoning/prose/usage; a global KV bound could cross-contaminate model capacities;
   ordinary nested JSON could be misclassified as a tool call. The independently reviewed local
   fixes preserve encounter order, stream fields and per-context/per-request capacity. CPU tests
   pass; release-platform evidence remains open.
9. **Rebased Victor regressions.** Default-off pruning could widen caller-curated tool supply;
   streaming subagents could leak their session ContextVar across outward yields and fail cleanup
   in a different context. Fixes use authoritative curated supply with finalized tracing, and
   scoped setup/stream-advance/cleanup with real ContextVar regressions. Independent source
   review passed; fresh exact-commit CI and publication remain open.

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

## Delivery gates

- [x] Sandhi review fixes merged through green PR checks; exact post-merge CI verified.
- [x] Cumulative Sandhi develop→main promotion independently reviewed; exact-main CI green.
- [ ] InferFlux v0.3.0 source/version and artifacts verified; original pin remains reproducible.
- [x] Sandhi v0.7.0 GitHub, PyPI, all four crates, all three npm packages verified by hosted and independent release verifiers.
- [ ] Victor exact Sandhi 0.7.0 pin and contract minor tested with the published binding; latest-develop
  candidate reviewed/green/promoted and v0.9.5 published.
- [ ] Back-sync protected branches and record final release evidence.

Open optimization items above are a separate backlog, not prerequisites invented for this
model-free contract release. Product acceptance P01–P03 in TD-0026 still gates production use.
