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
all four target groups, InferFlux v0.3.0, and Victor v0.9.5 after green gates. A subsequent
rebase-first investigation prepared a broader v0.10.0 candidate, but the owner's final scope
decision explicitly selects a **focused Victor v0.9.5 cut from main `4e4e0d640`**, excluding
unrelated develop changes. Superseded Victor #1104/#1106 are closed and their branches retained;
their source reviews remain historical, not approval to include that scope. Further merges
require the second account's approving review; no admin review bypass is authorized.
No immutable tag is moved.

## Current delivery checkpoint (2026-09-17)

| Repository | Completed evidence | Still open |
|---|---|---|
| Sandhi | Independent review clean; #261/#262 promoted `e05d7c5`; exact-main [CI 35193468410](https://github.com/anvai-labs/sandhi/actions/runs/35193468410) passed; v0.7.0 [all-target publication 35203459727](https://github.com/anvai-labs/sandhi/actions/runs/35203459727) and independent verifier passed; #263/#264 synchronized evidence with green exact push CI | No Sandhi release gate remains; sibling releases and production acceptance are separate |
| InferFlux | Reviewed [#182](https://github.com/anvai-labs/inferflux/pull/182) merged as main `12cf6f976206bae84081a9d4b155e906f3610af2`; exact-main [CI 35258419838](https://github.com/anvai-labs/inferflux/actions/runs/35258419838) and [GPU gates 35258419858](https://github.com/anvai-labs/inferflux/actions/runs/35258419858) passed. Packaging failed; corrective [#183](https://github.com/anvai-labs/inferflux/pull/183) at `cccb8fd26c7ed7986c1be77c0a4491ab0bb3e623` has green required [CI 35279415459](https://github.com/anvai-labs/inferflux/actions/runs/35279415459) | Second-account approval and merge of #183, fresh exact-main evidence and successful native packaging/install smoke, then v0.3.0 publication and artifact verification |
| Victor | Focused v0.9.5 [#1116](https://github.com/anvai-labs/victor/pull/1116), commit `79c33c4cf7b7828d9aabb6ad47d1bd24825a69b7`, tree `b1dfdb7b017ff0c701583fccf2e778c56bfb0cc8`, from main `4e4e0d6405b4ff2a353aafa9adce1aac8aa4e993` independently reviewed and pushed; only inclusion/billing/provenance fixes, tests, Sandhi 0.7.0 pins and focused records carried. Fresh published-binding CPU/stub probes and full collection (32,560 tests, zero errors) passed | Second-account approval, exact-candidate CI, main promotion and exact-main CI, v0.9.5 publication and verification of declared artifacts |

Sandhi's initial hosted verification failed after six npm platform-package HTTP 404s, although
publication jobs succeeded. The independent verifier subsequently passed all targets; retrying
only the read-only failed verifier produced successful attempt 2. No artifacts were republished
and no tag was moved. GitHub archive sizes/SHA-256, required PyPI platforms, four non-yanked crate
versions and all npm versions/dependencies passed `verify-release.py v0.7.0 --targets
pypi,crates,npm,github --repo anvai-labs/sandhi`.

Sandhi evidence synchronization completed through [#263](https://github.com/anvai-labs/sandhi/pull/263)
at develop `48c0c1f2` and [#264](https://github.com/anvai-labs/sandhi/pull/264) at main `079db9df`.
Their identical tree is `dff7bc7aed1c25d55a0ebd05065a92b301172628`; exact push CI
[35222663146](https://github.com/anvai-labs/sandhi/actions/runs/35222663146) and
[35243214744](https://github.com/anvai-labs/sandhi/actions/runs/35243214744) passed respectively.
The v0.7.0 tag remains at the original release commit, not either documentation merge.

InferFlux [packaging run 35260030376](https://github.com/anvai-labs/inferflux/actions/runs/35260030376)
failed on the Linux DEB install prefix and macOS installer path/metadata checks. Main CI and GPU
success did not establish installer readiness. PR #183 corrects those bounded packaging defects;
its green required CI does not replace a successful native packaging run after approved merge.

The rebased InferFlux candidate passed all 18 local three-way probes (16 origin SDK cases plus
2 actual Victor-provider cases) through the candidate Sandhi wheel. This is CPU/stub wire evidence,
not a test of the published 0.7.0 wheel or GPU/model behavior. Sandhi's reproducibility pin remains
`273780835f120cbe1a8da4860d72905861e71e29`; testing a newer candidate does not silently move it.
Subsequently, the published Sandhi 0.7.0 Linux wheel was installed in the isolated review
environment and passed all 18 three-repository CPU/stub probes. That later published-wheel
evidence does not replace the historical candidate-wheel result or establish final sibling
candidate CI, loaded-model validation or publication success.
The new focused Victor v0.9.5 source was then tested afresh: all 18 three-repository CPU/stub
probes passed with the published Sandhi binding and the earlier reviewed InferFlux `3f45fed87`
runtime binary. That binary is not a build of exact-main `12cf6f97`; this is model-free wire
evidence, separate from InferFlux's exact-main hosted GPU evidence above.

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
9. **Historical broader Victor regressions — excluded from the focused v0.9.5 cut.** Default-off pruning could widen caller-curated tool supply;
   streaming subagents could leak their session ContextVar across outward yields and fail cleanup
   in a different context. Fixes use authoritative curated supply with finalized tracing, and
   scoped setup/stream-advance/cleanup with real ContextVar regressions. Independent source
   review passed for that broader candidate. Its tool/session changes are not carried into the focused release.
10. **Historical frozen v0.10.0 cut regressions — excluded from v0.9.5.** Lazy tool-pruning policy must honor explicit
    configuration before settings/environment; chat curated sets must fail closed when unavailable;
    concurrent teams must retain per-call goals and use the actual formation accessor. The bounded
    corrections and regression tests passed independent review for the broader cut, which is retained separately.
11. **InferFlux release evidence and metadata.** Installer smoke now gates package uploads.
    The reviewed release guard checks actual CI run/attempt provenance, tag/main ancestry,
    CMake version and exact-SHA trusted-main GPU evidence before publication, then rechecks the
    live tag immediately before publishing. Package metadata uses the real repository and pinned
    recursive Git source with static bundled libraries. Offline tests verify these guards, not
    a completed hosted package build or release.

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
- [x] Sandhi v0.7.0 GitHub, PyPI, all four crates, all three npm packages verified by hosted and independent release verifiers.
- [x] Sandhi protected-branch release-evidence synchronization and exact push CI verified.
- [x] Victor exact Sandhi 0.7.0 pin and contract minor tested with the published binding.
- [x] Focused Victor v0.9.5 source and InferFlux #182 independently source-reviewed.
- [x] InferFlux #182 promoted; exact-main CI and CUDA/ROCm gates passed at `12cf6f97`.
- [ ] Second-account approval and merge of InferFlux packaging fix #183; fresh exact-main evidence and successful native installer gates.
- [ ] Focused Victor commit/PR identity, second-account approval, exact-candidate CI, main promotion and exact-main CI.
- [ ] InferFlux v0.3.0 publication and declared artifacts verified.
- [ ] Victor v0.9.5 publication and all declared artifacts verified.
- [ ] Synchronize sibling protected branches and record their final release evidence.

Open optimization items above are a separate backlog, not prerequisites invented for this
model-free contract release. Product acceptance P01–P03 in TD-0026 still gates production use.
