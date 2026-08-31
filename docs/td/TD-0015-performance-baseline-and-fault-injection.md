# TD-0015: Performance baseline and fault injection — the measurement every other decision is waiting on

- **Status:** Draft (proposed), 2026-08-31. Owns gaps **G09, G10**.
- **Relates to:** [ADR-0006](../adr/0006-layer-boundary-and-protocol-scope.md) (whose **Proposed**
  status is gated on this TD's baseline), [TD-0016](TD-0016-enforcement-throughput-ceiling.md) (the
  hypothesis this TD tests), [TD-0014](TD-0014-data-plane-resource-safety.md) (whose fixes this TD
  measures), [TD-0020](TD-0020-operational-readiness-and-transport-observability.md) (the runtime
  gauges; this TD is the offline counterpart).

## Why this exists

Sandhi has no performance measurement of any kind. There is no `benches/` directory, no `criterion`,
no `iai`, and no `[[bench]]` target in any manifest in the workspace. CI gates `fmt`, `clippy`,
`test`, and line coverage, and nothing else.

The consequence is not merely that performance is unknown. It is that **every architectural
question is unanswerable**, and the project has been carrying at least one load-bearing belief with
no evidence behind it:

> *Inference (ADR-0006 §Context F1):* the enforcement ledger's two `fsync`-durable, globally
> serialized SQLite commits per request are the throughput ceiling, and byte movement is not.

If that is true, a large class of work (lower-layer transport, `io_uring`, `splice`, `kTLS`) is
provably not worth doing, and TD-0016 is the highest-value project in the repository. If it is
false, ADR-0006's first argument collapses and its verdict needs revision. **Nobody currently
knows.** That is the gap.

The second half is fault behaviour. `tests/proxy.rs` covers one adverse case well —
`client_disconnect_mid_stream_still_meters` (`:797`) — and the TD-0013 partial-settlement tests are
genuinely good. But there is no coverage for slowloris, slow consumers, half-close, RST, upstream
flapping, DNS stalls, cancellation storms, partial writes, queue saturation, or shutdown under load.
Each of those is a path where a lease can leak, a usage event can be lost, or a bound can fail to
bind — the three things Sandhi must never do.

## First principles

1. **A benchmark that is not in CI is a benchmark that will drift.** The deliverable is a recorded,
   comparable baseline, not a number someone once saw.
2. **Measure what the product promises.** Sandhi's promise is *counted and attributed*. So the
   headline metrics are not just latency and throughput — they include **accounting correctness
   under fault**, which no conventional load tool measures.
3. **Build to the tooling that exists, not to the tooling in the textbook.** `wrk2`, `h2load`,
   `vegeta`, `toxiproxy`, `tc`/`netem`, `perf`, and `valgrind` are all absent from the development
   environment, and `valgrind` has no usable ARM-macOS story, which rules out `iai-callgrind`
   locally. A harness that requires installing six tools will not be run.
4. **Separate the reproducible from the representative.** Micro-benchmarks are deterministic and
   belong in CI. Kernel-level measurements are neither and belong on a Linux runner, on demand.
5. **A fault test asserts an invariant, not an absence of crashes.** "It did not panic" is not a
   result; "the lease settled exactly once and one usage event was emitted" is.

## Non-goals

- **No competitive benchmarking.** This TD does not compare Sandhi to other gateways. The baseline
  exists to compare Sandhi to itself across changes.
- **No performance targets in this TD.** Setting an SLO before the first measurement would be
  inventing a number. Targets are proposed in a follow-up once the baseline exists.
- **No optimisation.** This TD adds measurement only. Acting on it is TD-0014 and TD-0016.

## Decisions

**D1 — Two tiers, split by reproducibility.** **Tier 1** is pure-Rust, deterministic, and runs in CI
on every PR that touches the data path. **Tier 2** is kernel-level, runs on the existing private
Linux runner on demand or nightly, and is never a merge gate. Rejected: one tier — either CI becomes
flaky and slow, or the kernel questions never get asked.

**D2 — The load generator is in-repo, not a dependency.** A ~150-line Tokio harness drives
`build_app` over a loopback listener against a `wiremock` upstream, in both closed-loop and
open-loop (constant-arrival-rate) modes, recording an HDR-style latency histogram. Rejected:
depending on `wrk2`/`vegeta`/`oha` — none is installed, all are per-developer installs, and none can
assert Sandhi's *accounting* invariants, which is the point of §First principles 2. Open-loop is
non-negotiable: a closed-loop harness hides coordinated omission, and this system's interesting
behaviour is precisely at saturation.

**D3 — `criterion` for the component benches, with the ledger as the first target.** Three suites:
(a) `decode_request`/`encode_response` per dialect; (b) `metered_passthrough` versus each typed
decoder over a recorded SSE corpus — this quantifies TD-0014 G01 directly; (c)
`SqliteLedger::reserve_durable` at `synchronous=FULL` versus `NORMAL` versus a batched/group-commit
variant. Suite (c) is the one ADR-0006 is waiting on.

**D4 — Fault tests are ordinary `#[tokio::test]`s that assert accounting invariants.** Every fault
below is expressible with Tokio, a raw `TcpStream`, and `wiremock`; none needs `toxiproxy`. Each
asserts the same three invariants: **exactly one usage event per logical call**, **the lease settled
exactly once**, and **no permit, connection, or task leaked**.

**D5 — The baseline is a committed artefact.** A JSON file of recorded metrics, regenerated by a
script and diffed in CI with a generous regression threshold (fail on >25% p99 regression, warn on
>10%). Rejected: publishing to an external service — it adds an outage-prone dependency to CI for a
project whose whole posture is self-hosted.

**D6 — `loom` and `miri` for the concurrency primitives, not for the whole system.** `BufferedSink::close`
(`sandhi-core/src/sink.rs:76-110`) and the rate limiter's sweep (`ratelimit.rs`) are small, subtle,
and have shutdown-ordering semantics that a load test will never reliably hit.

## The measurement set

**Workloads:** short unary requests; long SSE token streams; many mostly-idle connections; burst
connection establishment; large bodies at the `SANDHI_MAX_REQUEST_BODY_BYTES` boundary; mixed
tenants at a 10:1 size skew; transparent plane versus translation plane at identical offered load
(the ADR-0004 adoption cost, currently unquantified).

**Metrics:** requests/sec and connections/sec; sustained throughput; time-to-first-byte and
time-to-first-token; p50/p95/p99/p99.9 and jitter; CPU-ms per request; allocations per request; RSS
and file-descriptor high-water; queue delay; cross-tenant fairness (Jain index); overload rejection
shape; graceful-drain duration. Tier 2 adds syscalls per request, context switches, user/kernel
copies, and flamegraphs.

**Faults (Tier 1):** slowloris (one header byte per second); slow consumer (SSE body read at 10 B/s);
client half-close and RST mid-stream; upstream flapping; upstream that never sends a terminal usage
frame; cancellation storm (10k spawn-then-abort); partial writes; queue saturation; shutdown under
load at and past the grace deadline. **Tier 2 adds:** packet loss/latency/reordering via `netem`,
DNS stalls, and TLS rotation once TD-0017 lands.

## Phases

| Phase | Scope | Acceptance (the failing test to write first) |
|---|---|---|
| **P1** | D3(c) — the ledger bench, run first because ADR-0006 is blocked on it | A committed number for reserve+settle throughput at `FULL` vs `NORMAL` vs batched, single- and multi-threaded, with the serialized-commit hypothesis explicitly confirmed or falsified. The result is written back into ADR-0006's Status line either way |
| **P2** | D2 — the in-repo load harness, closed- and open-loop | The harness drives `build_app` at a fixed arrival rate and reports a latency histogram plus per-request allocation and CPU; running it twice on unchanged code produces results within the D5 noise threshold |
| **P3** | D3(a)(b) — codec and stream-decoder criterion suites | `metered_passthrough` vs. each typed decoder is quantified over 1k/10k/100k-frame corpora, producing the number TD-0014 P1 must improve |
| **P4** | D4 — the fault suite | Each fault asserts the three D4 invariants; the suite fails today on at least the slowloris and slow-consumer cases (TD-0014 G02/G03), which is the point — it is written to fail first |
| **P5** | D5 + D6 — CI wiring, the committed baseline, `loom`/`miri` targets | A PR that regresses p99 by >25% fails CI with a diff against the committed baseline; `loom` covers `BufferedSink::close` shutdown ordering |

P1 is the critical path for ADR-0006, TD-0016, and by extension the whole layer question. Land it
first, alone, and publish the number.

## Pressure test

1. **"Writing a load generator is reinventing `wrk2`."** It reimplements the easy half (arrival-rate
   scheduling and a histogram, both well under 150 lines) and adds the half no load tool has:
   asserting that every request settled its lease exactly once. Given `wrk2` is not installed and the
   accounting assertions are the actual product risk, the build-versus-install trade is not close.
2. **"CI benchmarks are flaky and will be muted within a month."** Which is why D1 keeps only
   deterministic Tier-1 work in CI, D5 sets a deliberately loose 25% gate rather than a tight one,
   and the kernel-sensitive measurements are explicitly not gates. A muted benchmark is worse than
   none; the design is aimed squarely at not being muted.
3. **"P1 will just confirm what we already believe about `fsync`."** Possibly, and confirming it
   converts ADR-0006 from Proposed to Accepted and retires a recurring architectural argument — that
   alone pays for a day's work. If it does *not* confirm it, the project avoids building TD-0016
   against a false premise.
4. **"Measuring before fixing delays the fixes."** TD-0014 is explicitly *not* blocked on this TD;
   its P1–P3 are unit-testable today. Only TD-0016, where the fix is expensive and the premise
   uncertain, is sequenced behind measurement.
5. **"macOS numbers will not represent production Linux."** True, and D1 says so by splitting the
   tiers. Tier 1's job is *relative* comparison of Sandhi against Sandhi on identical hardware, which
   is valid on any platform. Absolute claims come from Tier 2.
6. **"A 25% regression gate is too loose to catch anything."** It is calibrated to CI noise, not to
   ambition. A tighter gate on shared runners produces false failures, which produces muting — see
   objection 2. The committed baseline artefact means a 5% drift is still *visible* in the diff even
   when it does not fail the build.

## Resolved

**R1 — Both arrival models; fixed-rate is the CI gate, Poisson is on demand.** Fixed-rate is
reproducible enough to diff against a committed baseline (D5); Poisson exposes the queueing
behaviour that fixed-rate hides, which is exactly what the overload and fairness metrics are for.
Neither substitutes for the other, and only one of them can be a merge gate.

**R2 — The SSE corpus is synthetic, with frame shapes derived from
`crates/sandhi-providers/tests/provider_corpus.rs`.** Recorded provider streams would embed real
prompt and completion content in a public repository, which is not a trade this project should make
for benchmark fidelity. Deriving the *shapes* from the existing fixtures keeps the corpus
representative of what the decoders actually parse.

**R3 — The harness lives in `crates/sandhi-proxy/benches/`, as a normal workspace member.** The
earlier draft of this question warned that a workspace member "would be published to crates.io by
the release workflow unless excluded." **That premise was false.** Both `.github/workflows/release.yml:117-120`
and `.github/workflows/publish-crates.yml:35-38` publish an explicit four-crate allowlist
(`-p sandhi-core`, `-providers`, `-store`, `-proxy`). A new member is not published unless someone
adds it to both lists. No separate crate is needed.

**R4 — The fault suite runs serialised; the rest of the suite does not.** Several faults are
timing-sensitive (slowloris, slow consumer, drain-at-deadline) and would flake under contention.
Serialising *only* that target keeps the honest signal without slowing the main suite; the repo
already has `cargo-nextest`, which can express it as its own profile.

**R5 — This TD also owns the measurements other TDs are gated on.** Three deferred questions
elsewhere resolve to a number this harness can produce, and they should be explicit deliverables
rather than incidental: the largest real frame per provider family (TD-0014's
`MAX_TYPED_LINE_BYTES`), the right `pool_max_idle_per_host` (TD-0020), and the ledger commit
breakdown (TD-0016 P0, already P1 here).

## Still open

Nothing. Every question this TD raised is decided above.
