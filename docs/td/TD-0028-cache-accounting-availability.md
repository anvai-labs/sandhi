# TD-0028: Cache accounting availability and bounded diagnostics

- **Status:** In progress (2026-09-18). Regression, availability, dashboard and diagnostics merged;
  CI/review gates remain distinct from merge, deployment and the joint live replay.
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
| C5 | Joint replay after InferFlux investigation | One actual member trace, same ready model, direct and gateway; cache counts, correlation/session mapping and usage conservation | Isolated WSL diagnostics complete; actual-member replay remains open with originating-Mac handoff below. Approved ordered member artifact and producer findings for replay remain prerequisites |

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

## Originating Mac session handoff: remaining C5 gate

The owner requested completing independent work on WSL and handing origin-dependent
work back to the originating Mac session. C1–C4 are merged into `develop`, not released.
The local diagnostics gateway is `127.0.0.1:18789` on WSL; the Mac gateway and its tunnel
were not modified. No Mac deployment is implied by the local launch.

The referenced local evidence was rechecked: it contains 18 controlled-probe usage
records and nine SQLite rows, but no ordered actual-member request replay. Controlled
plain/tool cache reuse and JSON/logprob zero reporting are verified; the original 40
Victor zero-cache calls remain unexplained because their request/response bodies were
not retained. Do not treat the historical usage projections or the new synthetic smoke
as an actual-member trace.

For the originating session:

1. Locate an approved, sanitized ordered request replay from one actual Victor member.
   If unavailable, arrange an explicitly approved new member capture and label it as new
   evidence, not a reconstruction of the historical run. Do not enable unrestricted
   prompt/body capture or export credentials.
2. Return the safe artifact location and the InferFlux producer-investigation findings,
   distinguishing matched prefixes from reuse actually executed. Preserve request order,
   prompt/tool/options shape, session mapping and request correlation for the joint gate.
3. Once prerequisites are available, coordinate direct and gateway replay against the
   same ready model. Verify explicit origin cache counts, normalized usage conservation,
   request IDs and session affinity. Record warm/cold provenance without clearing shared
   cache or restarting shared InferFlux; latency alone is not proof of reuse.
4. Keep tokens, keys, vault secrets and unapproved prompt/response content private. Mac
   loopbacks `18788`/`18080` are not local WSL endpoints. Share sanitized findings and safe
   artifact locations, not credentials. Mark C5 complete only when this actual-member gate
   has evidence; it is not closed by diagnostics availability.

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
