# TD-0028: Cache accounting availability and bounded diagnostics

- **Status:** In progress (2026-09-19). C1–C4 merged; new actual-member baseline and C4 joins verified;
  C5 terminal-stream accounting reproduced and locally repaired; CI/merge, deployment,
  strict rerun and the originating Mac mixed-team gate remain distinct.
- **Scope:** Sandhi items in the [cache co-design handoff](../upstream/inferflux-cache-codesign-2026-09-18.md).
- **Related:** TD-0013 (measurement fidelity), TD-0027 (origin co-design),
  [ADR-0008](../adr/0008-inferflux-admission-and-session-affinity.md) (catalog-owned affinity).

## Findings rechecked before implementation

The preserved, credential-free local audit has 18 non-stream usage projections and
nine Sandhi SQLite rows. All nine rows conserve the reported counts: plain repeated
451 prompt = 25 fresh + 426 cached; appended 488 = 62 + 426; tools 587 = 73 + 514;
the recorded JSON/logprobs probes have 436 fresh and zero cached. Gateway requests
followed direct requests, so the first gateway plain/tools request was already warm.

`parse_openai_usage` already subtracts cached input exactly once. The raw forwarder
and OpenAI-compatible adapter share the family parser. The catalog owns the InferFlux
session and client-request-ID headers. Existing complete/SSE fixtures already cover
a fully cached 50-token prompt; no new provider parser or hard-coded routing branch
is needed. Missing and malformed cache fields currently normalize numerically to zero;
that is an availability limitation, not proof of an upstream cache miss.

Evidence does not establish why the earlier Victor member run reported zero cache.
It also does not prove that lookup matches always equal reuse actually executed by
InferFlux; that producer-side investigation remains independently owned upstream.

## Ordered increments and gates

| Slice | Bounded change | Acceptance | State |
|---|---|---|---|
| C1 | Extend existing InferFlux corpus with the 18 sanitized usage objects | Explicit zero/partial/full cache accounting; both forwarding paths; unchanged response bytes and one late terminal SSE usage emission | [PR #268](https://github.com/anvai-labs/sandhi/pull/268) merged after clean independent review and real CI pass |
| C2 | ADR and additive cache-read availability/source contract | Define reported zero, absent, malformed and explicitly unsupported; preserve legacy numeric defaults; parser/event/UsageV2/SQLite/API/generated bindings/schema agreement | [PR #269](https://github.com/anvai-labs/sandhi/pull/269) merged after exact-head CI and clean independent review |
| C3 | Dashboard availability and coverage | `n_reported/n_total` over the same filtered call population; honest cache read/write and neutral-unit labels | Merged in #269; 25 real-browser dashboard regressions pass |
| C4 | Bounded credential-field-free diagnostic lookup/export | Admin-authorized persisted request/session/run projection, source-labelled timings and normalized counters; unavailable historical evidence stated explicitly; no prompt/body capture added | [PR #270](https://github.com/anvai-labs/sandhi/pull/270) merged after clean independent review and exact-head CI; isolated WSL HTTP/CLI validation passed |
| C5 | Joint replay after InferFlux investigation | One actual member trace, same ready model, direct and gateway; cache counts, correlation/session mapping and usage conservation | Actual-member baseline passes; subsequent accepted-runtime five-stream oracle exposed terminal accounting failure. Local regression/repair verified; CI, deployment/rerun and broader mixed-team gates remain open below |

The C1 non-stream envelopes and SSE frames are constructed around recorded usage
objects. The original audit did not retain full response bodies or live SSE captures.
Passing their replay is protocol regression evidence, not a new live-model run.
Future corpus extensions must retain that provenance distinction.

Local C1 validation: all 619 workspace tests passed (four existing opt-in tests
ignored), including both new 18-case replays and the pre-existing full-cache corpus.
Workspace clippy with warnings denied, formatting, generated binding-facade drift
and whitespace checks passed. All 18 fixture usage objects equal the saved evidence;
all nine gateway expectations match its SQLite projections, including rounded origin
durations. No live gateway was contacted and no deployed binary was changed.

Local C2/C3 validation (2026-09-18): 645 workspace tests passed with four existing
opt-in tests ignored; strict workspace Clippy, formatting and generated-facade checks
passed. Fresh final-source Python binding tests: 92 passed; Node: 56 passed. All 25 real-browser dashboard
tests passed. Independent cross-review findings on stream corrections, invalid SQLite
metadata and schema status/source pairs were fixed and rechecked before checkpointing.
The first PR CI run exposed stale fixed-event-sequence binding assertions after the
canonical metadata-only stream update. Tests now validate those updates explicitly,
reject duplicate numeric verdicts and cover stop-after-content consumers; both native
bindings were rebuilt from final provider source before the counts above were recorded.
Exact-head PR CI [35412103862](https://github.com/anvai-labs/sandhi/actions/runs/35412103862)
passed for `8d4190f`; #269 merged as `a49df36`, with the same reviewed tree. The next
worktree starts from that merged `develop`, not the earlier CI-failing checkpoint.
Post-merge `develop` CI [35412748884](https://github.com/anvai-labs/sandhi/actions/runs/35412748884)
also passed at `a49df36`.

An isolated WSL gateway was launched at `127.0.0.1:18789`, with its own SQLite state and
admin authentication. One synthetic direct call followed by two buffered gateway calls
and one streaming gateway call used the ready local `qwen3-coder-30b` model. Each origin
response explicitly reported 588 prompt, zero cached and one output token. All three
gateway events preserved those counts and `reported/origin_usage`; coverage was 3/3,
with one terminal SSE usage report. Thus the current live probe is **reported zero**,
not missing reporting and not evidence of a Sandhi accounting error. It does not prove
cache reuse or explain the earlier member trace. The shared InferFlux service and Mac
gateway were not restarted, reconfigured or cleared. Sanitized runtime evidence is
retained locally at `/tmp/sandhi-local-cache.q6l7xr/smoke-evidence.json`; no credentials
or prompt/response text are exported there.

## Bounded diagnostics validation

Local C4 validation (2026-09-18): all 677 workspace tests passed, with four existing
opt-in tests ignored; strict workspace Clippy and formatting passed. Independent
cross-review found and resolved a shutdown-tracking gap: diagnostic work now retains
both admission and lifecycle guards until SQLite finishes, even after HTTP cancellation.
Regression tests cover admin-first authorization, cutoff, malformed SQLite fields,
indexed exact selectors, byte/row bounds, duplicate IDs and private CLI failures.

Only the isolated local gateway at `127.0.0.1:18789` was restarted with the C4 build;
its private SQLite data was retained. HTTP and `sandhi diagnose` returned identical
three-row evidence from the earlier synthetic probe (2,212 serialized HTTP bytes,
reported-zero cache status). Missing admin authorization returned 401; responses
were `no-store`. This check made no new origin calls and does not close C5.

Exact-head PR CI [35414465068](https://github.com/anvai-labs/sandhi/actions/runs/35414465068)
passed for `3240906`; #270 merged as `52bd8ec`, preserving reviewed tree
`c9c1541daad0dd0c3143a37d43421ce9f98c3975`. Rust, coverage, Python/Node,
SDK/dashboard, security and release-safeguard jobs actually passed; the inactive
all-skipped mirror was not accepted as evidence. Post-merge CI
[35414947570](https://github.com/anvai-labs/sandhi/actions/runs/35414947570) also passed
at `52bd8ec`, including the substantive jobs and `CI Success`.

## Originating Mac session handoff: prerequisite received, C5 partially verified

The owner requested completing independent work on WSL and handing origin-dependent
work back to the originating Mac session. C1–C4 are merged into `develop`, not released.
The local diagnostics gateway is `127.0.0.1:18789` on WSL; the Mac gateway and its tunnel
were not modified. No Mac deployment is implied by the local launch.

The original local evidence contains 18 controlled-probe usage records and nine
SQLite rows, but no ordered actual-member request replay. Controlled
plain/tool cache reuse and JSON/logprob zero reporting are verified; the original 40
Victor zero-cache calls remain unexplained because their request/response bodies were
not retained. Do not treat the historical usage projections or the new synthetic smoke
as an actual-member trace.

On 2026-09-19 the originating session supplied
`/tmp/victor-member-replay-63ad80a3f502/`: five approved ordered request payloads from
**one new actual Victor writer member**, manifest, checksum inventory, producer findings
and Mac direct/gateway baseline. All supplied SHA256SUMS entries passed before and after
the WSL replay; the bundle was not modified. Victor capture source is
`3d42bd03b9701161f62d36854165cf7a25329f81`; the recorded member deliverable check passed
one pytest test. That verdict belongs to the supplied capture, not a new WSL member run.
Manifest SHA-256: `9257a1d1276eb3a8e6a76b499dcb9c9f4d4452e4e62f9523d5e175776933a73f`.
The missing-artifact prerequisite is therefore resolved; this is not a reconstruction
of the historical 40 calls, and their cause remains unproven.

### WSL baseline replay and C4 acceptance

Replay `member-joint-e7a3f553602c` ran on 2026-09-19 at 04:29 UTC. For each original
ordinal it sent the same checksum-verified payload directly, then through Sandhi,
preserving order within each arm. Captured tool outcomes were frozen: no returned tools
were executed, and generated responses were not fed into later requests. Session IDs
were explicitly remapped to `<replay-run>-direct` and `<replay-run>-gateway`; the latter
was also the Sandhi run ID. This is a replay of actual-member inputs, not a fresh
autonomous member run or an output-equivalence test.

All ten requests returned HTTP 200 against ready `qwen3-coder-30b` (`llama_cpp_rocm`).
All explicitly included `prompt_tokens_details.cached_tokens=0`. Identical payloads
reported identical prompt counts in both arms, matching the supplied capture baseline:

| Ordinal | Inclusive prompt, both arms | Direct output | Gateway output | Cache read, both arms | C4 fresh input | C4 availability |
|---|---:|---:|---:|---:|---:|---|
| 0 | 1701 | 197 | 197 | 0 | 1701 | reported / origin_usage |
| 1 | 1932 | 140 | 140 | 0 | 1932 | reported / origin_usage |
| 2 | 2443 | 37 | 105 | 0 | 2443 | reported / origin_usage |
| 3 | 3099 | 126 | 18 | 0 | 3099 | reported / origin_usage |
| 4 | 4026 | 25 | 45 | 0 | 4026 | reported / origin_usage |

Each echoed gateway correlation ID matched **exactly one** C4 request diagnostic row;
session and run selectors returned exactly the same five IDs, without truncation.
Every row preserved provider/model, session/run identity, origin-sourced rounded duration,
and `fresh + cache_read + cache_creation = reported prompt`, with matching output counts.
Coverage is 5/5 reported, not missing or inferred availability. HTTP responses were
`no-store`; `sandhi diagnose --run` matched the HTTP JSON. This verifies persisted
logical-call correlation, not the execution of an origin session lease or its affinity.
No direct SQLite query was used for this WSL reconciliation: the wire artifact correctly
says `ledger_checked=false`; the separate diagnostics artifact supplies that verification.

Sanitized, prompt/body/credential-free evidence:

- [Wire outcomes](../upstream/evidence/member-c5-2026-09-19-wire.json), SHA-256
  `559aace81f221036d14ba7474c2e55a9a6fa8515fda2d445603faef9a73d66e0`.
- [C4 joins and runtime fingerprints](../upstream/evidence/member-c5-2026-09-19-diagnostics.json),
  SHA-256 `ab9252d46d74df5bea19b20f005f9ae1fdab217a88992ec075d3b9b58db3832b`.
- Local verifier and preflight records remain under `/tmp/sandhi-member-c5-evidence.eTEFHC/`;
  approved request bodies remain only in the supplied bundle, not in this repository.

The supplied runner initially stopped **before any model call**: Sandhi's environment
demo key has an empty static `/v1/models` catalog, not live InferFlux discovery. Source
inspection and a private process-setting check verified its upstream route was exactly
the same `127.0.0.1:8080/v1` origin. A local copy of the runner recorded empty gateway
discovery and used independently checked origin readiness; it did not invent a gateway
model response. Its initial bundle-path mistake also stopped before model calls. Only
the successful ten-call replay reached inference. The unchanged supplied replay script
and adapted script hashes are retained in the diagnostics artifact. This is a harness
discovery limitation, not a runtime catalog fix or a claim of live gateway discovery.

### Runtime identities and remaining owner handoff

Fetched Sandhi `develop` was `647d7d5df064b3b3c3e74ee8635521f82d3552cb` (#271).
The C4 serving build's source was `324090639998d39a1512899e3c15f2ab8c23aa1e`, tree
`c9c1541daad0dd0c3143a37d43421ce9f98c3975`; runtime source under `crates/` and Cargo
manifests is unchanged between that build and fetched develop. PID 1499867 and binary
SHA-256 `604155cecdc0b122c8fdc972f23926e8ac1465d1cc2b9c63f8798c43c25ff9e7`
were unchanged before/after. InferFlux PID 554660 remained at launch checkout
`aea7a24d035fd386ce24db4f8cc68710b4b11543`, with executable SHA-256
`cff60d525a60d26574013696f012d27a63a1ad1c7cfa1cbde634bbec19ed499d`.
These are verified preserved launch checkouts and executable fingerprints, not signed
embedded build attestations. Origin health remained ready. Neither service was restarted,
reconfigured or replaced, and shared cache was not cleared. No Mac deployment occurred.

The supplied InferFlux findings cover #185–#192. Subsequent read-only reconciliation
verified tokenizer-unit repair [#194](https://github.com/anvai-labs/inferflux/pull/194)
merged as `930f580e0caeacdc3c583a1fd4d7fb98ec47a11d`, and promotion
[#195](https://github.com/anvai-labs/inferflux/pull/195) merged to main
`9edab96b45aad7b9f1e4fc7bcd333ba59a5e41d8`. Those fixes were **not deployed** to the
preserved Qwen process used here. The bundle's statement that #194/#195 were open is
a dated snapshot, not current status. Wire/C4 conservation is reporting consistency;
it does not prove the older runtime's prompt-tokenizer units are accurate.

At that baseline checkpoint, remaining acceptance belonged to the coordinated origin/Victor sessions
(the subsequent accepted-runtime update below supersedes its runtime prerequisite):

1. InferFlux owns trusted exact-main-SHA CUDA/ROCm gates and any controlled diagnostic
   deployment/rollback. Re-run this actual-member bundle and C4 joins on that accepted
   runtime, recording exact binary/source identities and executed reuse versus candidates.
   Do not replace or restart shared Qwen merely to force a cold sample.
2. Verify tokenized prefix/accepted reuse and origin lease/session execution with origin
   diagnostics. This frozen buffered zero-cache baseline proves neither positive reuse nor
   tokenizer accuracy, cold-cache behavior, contention/eviction handling or lease affinity.
3. Keep stream/late-usage/cancellation and the broader tools/JSON/logprobs/session matrix
   separate. Complete the full six-Qwen/one-ZAI mixed-team gate through the originating
   session's approved connection; no ZAI credentials are transferred here.
4. Preserve the new-versus-historical distinction and private capture boundary. Share
   sanitized counters, correlation IDs, fingerprints and artifact locations only. C5 is
   partially verified, not complete, until the target-runtime and remaining gates have
   their own acceptance evidence.

### Accepted-runtime streaming finding and repair (2026-09-19)

InferFlux subsequently accepted source `c5d4eb89f71dcfb3064b974d63b88c4ce88f42a1`,
PID `2254917`, at `http://127.0.0.1:8081`, ready model `qwen3-coder-30b` on
`llama_cpp_cuda`. Binary SHA-256:
`312f49e63dae7609dd804e3b74aa4a4b2dd06b72aebf8aa6a322ed619e974de6`.
Its exact-source GPU acceptance passed. This is a separate accepted process, not a
replacement or restart of shared Qwen8080. Coordination is tracked in
[InferFlux #184](https://github.com/anvai-labs/inferflux/issues/184) and the
[Sandhi #272 finding](https://github.com/anvai-labs/sandhi/pull/272#issuecomment-5740849386).

The five-stream oracle on Sandhi `647d7d5` delivered valid finish, terminal usage
and `[DONE]` for all calls, but persisted **14,387 output tokens versus 288 on wire**.
Prompt/cache counts matched. Preserved evidence is
`/tmp/inferflux-deployed-stream-overlap-fh_gryvj/{report.json,failure-supplement.json,failure-supplement.md}`.
Independent review confirmed the discrepancy. Physical upstream EOF was not traced
in that run; the original EOF-race explanation was source-supported inference.

A deterministic delayed-EOF regression now reproduces that path: closing after DONE
persisted 86 output tokens for a wire report of 7, while draining EOF was correct.
The local fix publishes observed usage before yielding the complete OpenAI-compatible
DONE data line. EOF does not emit a second terminal measurement; post-DONE bytes remain
transparent without changing the finalized numeric or cache observations. Genuine
preterminal cancellation and missing-usage DONE retain partial estimates, not final zero.
This is the OpenAI single-data-line event profile, not a general SSE event parser.
Non-OpenAI families retain EOF completion. Legacy `MeteredProvider` Drop outcome/
completeness classification is separate and is not claimed resolved by the raw-proxy fix.

Six provider regressions cover every two-chunk split, CRLF, marker false positives,
oversized-line suffixes, missing/zero usage and post-DONE updates. Four real-TCP proxy
tests cover 24 scenarios: close-at-DONE, drain-to-EOF, cancellation before and after
usage but before DONE, and missing usage. They assert unchanged bytes, exactly one
sink/SQLite row, authoritative prompt/cache/output counts, ledger settlement and boundary
TTFT (including a coalesced first chunk). Local workspace validation passes 687 tests
with four existing opt-in tests ignored; strict Clippy passes. Independent source review
is clean. These are regression results, **not a successful deployed rerun**.

Next gates: review before push, real green exact-head CI and merge, then deploy an
isolated updated Sandhi binary and repeat the unchanged five-call oracle on accepted
8081. Reconcile wire, SQLite, C4 and dashboard with exact runtime fingerprints. Preserve
the failed evidence, all origin cache state and privately held credentials. Buffered
actual-member replay and client-close/recovery checks reported by the origin session
do not close this streaming finding. The originating Victor/Mac session still owns the
full six-Qwen/one-ZAI run; C5 remains open.

### Buffered timeout assessment for the Mac mixed-team run

The stock raw buffered upstream deadline covers the request **and response body** and
is 120 seconds (`raw.rs`); typed resilient completion also defaults to 120 seconds per
attempt. A replay client allowing 900 seconds does not override either gateway limit;
gateway timeout maps to HTTP 504. `ProviderRuntime` and bindings can configure
`timeout_secs`, but the stock standalone provider configuration currently exposes no
upstream-timeout knob. `SANDHI_HEADER_READ_TIMEOUT_SECS` only covers incoming headers.
No production timeout has been changed here.

Before the mixed-team run, the originating session must choose a bounded configuration
supported by its gateway construction or explicitly accept the 120-second limit and
record timeouts. A longer isolated per-provider deadline must leave room below the
900-second reservation TTL and client deadline, including any retries; blindly matching
900 seconds is unsafe because expired reservation settlement is a no-op. Streaming is
not an unconditional workaround: setup remains 30 seconds and the default inter-chunk
idle deadline is 90 seconds. Queued or non-flushing origins can still time out.

## Contract decisions implemented by C2

Do not infer unsupported capability from a missing counter or a reported zero. Define
how validated wire presence and an explicit producer/catalog capability declaration
compose, including malformed/inconsistent counts. Specify legacy-row defaults,
per-call versus attempt denominators, missing terminal usage, aborted streams,
multi-call aggregation and unknown future enum values. Source identifiers must be
bounded vocabulary, never raw provider objects or arbitrary diagnostic strings.

Consumer decisions must cover Rust source compatibility as well as additive JSON
compatibility, SQLite migration/backfill behavior, generated Python/Node facades and
schema/contract-minor rules. No version bump or publication is implied by this TD.
Numeric billable-unit arithmetic must not change and must not become dollar pricing.

## Operational and release boundaries

Work happens on aiserver1 WSL. The retained gateway and its `127.0.0.1:18788` listener
are on the Mac; the Mac's `127.0.0.1:18080` is its SSH tunnel endpoint, not a local
gateway on this host. Do not probe those loopbacks here as deployment evidence.
Keep Qwen available; never clear shared cache or restart the shared server to force
a test. No secrets, prompts or full provider bodies enter default metering records.

The independently reviewed JSON Content-Type correction from PR #265 is included as
a local-launch dependency (one header plus four byte-preserving regressions); that
older PR was closed as superseded after verifying its entire patch is in #269, not
separately merged or treated as approved; its branch was retained. Each slice targets `develop`
through real CI and independent adversarial review. The owner explicitly authorized
a narrowly scoped proxy-review merge exception for the new Sandhi cache-work PRs;
branch protections stay unchanged and failed/pending CI cannot be bypassed.
No release exception or new version approval is implied.
TD-0026 production gates and existing sibling release gates remain open separately.
