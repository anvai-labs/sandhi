# TD-0028: Cache accounting availability and bounded diagnostics

- **Status:** In progress (2026-09-18). Regression increment merged; availability and dashboard implemented locally;
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
| C2 | ADR and additive cache-read availability/source contract | Define reported zero, absent, malformed and explicitly unsupported; preserve legacy numeric defaults; parser/event/UsageV2/SQLite/API/generated bindings/schema agreement | [ADR-0010](../adr/0010-cache-read-reporting-availability.md) accepted; implemented locally, PR/CI gates pending |
| C3 | Dashboard availability and coverage | `n_reported/n_total` over the same filtered call population; honest cache read/write and neutral-unit labels | Implemented locally; 25 real-browser dashboard regressions pass; PR/CI gates pending |
| C4 | Bounded credential-free diagnostic lookup/export | Authorized request/session/run correlation, source-labelled timings and counters; explicit late/aborted stream semantics; prompt/body capture opt-in and bounded | After C2; authorization/redaction design required |
| C5 | Joint replay after InferFlux investigation | One actual member trace, same ready model, direct and gateway; cache counts, correlation/session mapping and usage conservation | Owner selected an isolated local WSL gateway instead of Mac access. Actual sanitized member trace and producer investigation remain prerequisites; synthetic local probes do not close this gate |

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
older PR is not separately merged or treated as approved. Each slice targets `develop`
through real CI and independent adversarial review. The owner explicitly authorized
a narrowly scoped proxy-review merge exception for the new Sandhi cache-work PRs;
branch protections stay unchanged and failed/pending CI cannot be bypassed.
No release exception or new version approval is implied.
TD-0026 production gates and existing sibling release gates remain open separately.
