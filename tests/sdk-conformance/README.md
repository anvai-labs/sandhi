# Gateway SDK and dashboard conformance

The suite starts a real Sandhi binary with disposable stores and local synthetic upstreams.
No provider account, real key, OS vault or external model request is needed.

Install the exact provider SDK baseline shared with the public CI job, then its
test browser. The current baseline is OpenAI 3.49.0, Anthropic 2.8.0 and Google
GenAI 3.2.0, selected from stable PyPI releases on 2026-11-12:

```sh
python -m pip install -r tests/sdk-conformance/requirements.txt
python -m playwright install chromium
python -m pytest tests/sdk-conformance/ -q
```

The binary fixtures expect the worktree's default `target/` directory; omit a
`CARGO_TARGET_DIR` override for these tests. On macOS, use a fresh short temporary
directory for broker cases because the default pytest path can exceed the Unix
socket path limit:

```sh
python -m pytest tests/sdk-conformance/ -q --basetemp "$(mktemp -d /tmp/sandhi-sdk.XXXXXX)"
```

Pytest clears its base directory, so never point `--basetemp` at retained evidence
or an existing service directory. These fixtures build source binaries; their
results are not installed-release or live-provider acceptance.

Google GenAI 3.x defaults to Gemini `/v1` paths. Sandhi also retains `/v1beta` for
older clients. Generation, streaming and countTokens use the same admission and
accounting handlers on both prefixes. The shared `/v1/models` discovery path uses
Google's credential/client headers to choose its envelope; key authorization
remains unchanged. Gemini discovery omits IDs that cannot be used in its URL path;
a logical alias can name an OpenAI-compatible backend model containing `/`.
Anthropic 2.x uses Bearer API-key authentication, so shared discovery recognizes
its `anthropic-version` header as well as older `x-api-key` clients. Header hints
select serialization, never a backend credential type, account or extra grant.

For SDK refreshes, verify stable release metadata and Python requirements, update
the three exact pins, and run the ordinary vendor-client, route/cache, tool-loop,
cancellation/restart and dashboard suite. Keep source changes required by new SDK
defaults covered by regressions; do not force a new SDK back to an older API
version just to hide an interoperability gap.

`test_dashboard.py` covers authentication, keyboard usage, public/disabled modes, mutation
feedback, one-time key visibility, token clearing and stale replies, hostile metadata, failed
database reads, served CSP/assets and mobile overflow. A synthetic-data screenshot is written
under `target/dashboard-authenticated.png`. Its HTTPS authority fixture also drives real
default-mode SSO login, all three dashboard roles, cookie-authorized writes, logout and expiry.
Only the disposable browser certificate is exempted from browser trust; the proxy verifies
the fixture IdP certificate normally. Requires loopback sockets and headless Chromium.
The navigable console cases exercise fragment/history/keyboard behavior, attribution
filters without changed totals, escaped backend/route inventory and protected-data
clearing. Cache composition requires complete read coverage. Usage-evidence cases
query real persisted rows, distinguish failed/empty results and verify the separate
SSO diagnostics grant using a viewer with and without explicit permission.

`test_management.py` adds durable-write fault injection, invalid policy checks, concurrent
budget writers, actual process restart comparison, partial config/alert reports, sequential
retry deduplication and browser/CLI failure UX. Its SQLite triggers affect only disposable
test databases. Provider metadata faults are exercised in `test_broker.py` after proving that
the synthetic broker received the save, rather than merely failing input validation.

`test_backend_registry.py` exercises explicit Responses backends through the real
CLI/admin API, ordinary OpenAI SDK calls, durable records and process restart.
Disposable read-only credential files simulate external provisioning/rotation;
reference registration rebuilds dependent handles. It checks independent backend
and credential revocation, additive config, stale revision refusal, secret-free
inventory and suppression of key/profile grants after dependency failure. No live
provider or subscription/cache behavior is inferred from these synthetic fixtures.
Its cache-policy case uses all four SDKs and both modes, CLI JSON declarations,
config restart, credential rotation and durable disjoint counts. Its Gemini SDK
case also uses an explicit Responses backend; install google-genai
to execute it. The Gemini suite covers native and cross-family generation,
streaming, discovery and estimated countTokens through the ordinary SDK.

`test_logical_routes.py` exercises the new logical alias through all four SDK APIs
and both delivery modes. CLI route revision changes, candidate reordering, process
restart, registered rotation, unreported credential-file changes and revocation
test durable compatible pins and no silent switching. Config cases preflight
invalid dependencies before writes, apply backend/route/key intent in order and
retain additive definitions across restart. All origins and credentials are
synthetic. Existing live Z.AI evidence below exercises fixed explicit backends;
it is not evidence for this logical-route path or subscription renewal.
Its tracked-context case injects a receipt failure after inference, verifies the
admitted selection/cache/policy record, kills the gateway and restarts under a new
runtime cache weight. Recovery keeps the old charge/context without redispatch;
a rotated fixture credential and a new session produce a distinct new context.
This extends evidence within existing tracked activation, not protocol coverage.
New tracked admissions reserve both budget scopes atomically. Recovery snapshots
include execution context and back-leg linkage; denial must leave every accounting
row unchanged. Receipt-fault cases require both legs held before restart and both
charged by the recovered original receipt. Missing-usage shutdown cases retain
both liabilities in buffered/streamed HTTP/TLS variants. Default legacy recovery
semantics remain separately tested.

## Opt-in Z.AI live qualification

`scripts/qualify_zai_router.py` is separate from the offline test suite. Build the
gateway and install `openai`, `anthropic`, `google-genai` and `httpx` in the runner's
environment. It reads the configured token from `~/.claude/settings.json.zai` into
a private disposable credential file and starts its own gateway/store. It leaves
the user's settings, running services and vault unchanged. The explicit `--live`
flag authorizes provider calls; SDK retries are disabled. For example:

```sh
cargo build --locked -p sandhi-proxy --bins
python scripts/qualify_zai_router.py --live --output /tmp/zai-matrix.json
python scripts/qualify_zai_router.py --live --cache-pairs \
  --cache-profile implicit_only --output /tmp/zai-cache-pairs.json
python scripts/qualify_zai_router.py --live --accounting tracked --delivery streaming \
  --backend responses --client responses --logical-route --cache-pairs \
  --cache-profile implicit_only --output /tmp/zai-tracked-responses-stream.json
```

Defaults exercise four real client SDK APIs, three advertised backend protocols
and buffered/streamed delivery (24 cells). Cache pairs issue two identical
synthetic long-prefix requests per cell (48 calls). Reports contain sanitized SDK
results and drained SQLite usage observations. The synthetic step IDs correlate
new reports without exporting credentials. A first repeat is not guaranteed cold;
use reported read counts rather than latency. Explicit TTL expiration, separate
accounts, Gemini cache resources, subscriptions and full tool loops are separate
qualification gates. See the retained [live review](../../docs/reviews/zai-router-live-2026-11-11.md).

Tracked buffered and streaming runs support the four-client/three-GLM-backend
matrix, with explicit usage profiles and bounded streaming deadlines. These runs
verify durable attempt/context/usage/paired-receipt joins against each cell's actual
delivery mode. Portable-output controls matter: default signed thinking is not
universally translatable. See the [current buffered qualification](../../docs/reviews/zai-buffered-fidelity-cache-pairs-2026-11-12.md)
and [Messages stream controls/evidence](../../docs/reviews/zai-messages-tracked-streams-2026-11-12.md).

`test_reasoning.py` checks separate reasoning below, equal to and above candidate output across
transparent Gemini and translated Chat/Responses/Anthropic, both streamed and complete. Its 24
synthetic cases compare response conventions, stored event categories, run totals and settled
leases. They validate accounting, not real-provider token bounds or strict-cap eligibility.

`test_broker.py` builds with `sentinelpass-ipc` and drives a real proxy against a disposable Unix
socket daemon. Its test-only `sdk_broker_transport` Rust example uses the pinned
protocol crate's real secured session; plaintext is refused. Bounded private pipe
frames connect it to the existing Python grant decisions, without copying crypto
or authorization logic. Each fixture stops and reaps its bridge. It tests synthetic
read/write grants, locked/denied/missing states, timeout and
overlapping mutations, redaction, reference registration (API/CLI/browser), unsupported deletion,
namespace rejection, scheme aliases and metadata faults. Config-apply cases retain canonical
reconciliation facts for storage failure and ambiguous write timeout. Its daemon token lives only in a disposable private fixture file selected explicitly
with `SANDHI_SENTINELPASS_TOKEN_FILE`, including on macOS where the platform configuration
directory does not follow XDG. No user vault or daemon is contacted. Unix coverage is not Windows
named-pipe or live broker certification; those remain joint gates.

`test_buffer_metrics.py` drives the real binary to check configured usage/alert capacities,
explicitly unconfigured states and the existing metrics authentication gate. Deterministic
blocked-writer unit tests separately pin queue/in-flight/drop counters; an empty queue is not
a claim that SQLite commits succeeded.

`test_recovery_snapshot.py` validates the test-only offline snapshot helper, including WAL,
complete fixed-shard file sets, integrity, checksums and private, disjoint destinations.
`test_recovery.py` compares real-process restart/restore evidence, holds uncertain leases
across SIGKILL and reconciles post-snapshot revocations before enabling credentials.
Its tracked SIGKILL and SQLite-contention cases cover buffered/streaming requests,
token compatibility and CA-verified TLS/OIDC with separate admin/member identities,
retained unknown liability and once-only settlement after inference-grant revocation.
The same fixtures exercise OIDC streaming completion, disconnect and SIGTERM after
qualified usage, preserving counts and correlation. `test_shutdown.py` covers
buffered/streamed final and missing usage over HTTP/TLS. Legacy cases retain their defaults.
`test_recovery_broker.py` checks restored metadata against unavailable and re-provisioned
synthetic broker authority. `test_store_startup.py` proves configured database initialization
failures exit before serving, without replacing persisted enforcement with memory. See the
[recovery runbook](../../docs/operator/recovery-drill.md) for scope and limitations.

`test_workload_acceptance.py` adds a small real-proxy workload gate and negative tests for
stream completion, accounting isolation, overload, missing metrics and evidence output safety.
`workload_acceptance.py` runs the larger reproducible profile and emits full/compact JSON evidence.
See [M1 acceptance](../../docs/product/m1-acceptance.md) for the measured baseline, commands and
the separate actual-user gate. This is not a production capacity or performance regression SLO.

`test_acceptance_decisions.py` stitches browser reference registration and one-time key minting
to actual requests, persistent attribution, displayed run totals, budget denial/recovery and
locked/missing/denied broker recovery. `test_run_tree_rendering.py` checks the served run-response
envelope, escaped step labels and rejection of malformed or unsafe counts.

To retain synthetic, masked review aids alongside the authoritative suite result:

```sh
SANDHI_DECISION_EVIDENCE_DIR=/absolute/private/output/directory \
  python -m pytest tests/sdk-conformance/test_acceptance_decisions.py -q \
  --junitxml=/absolute/private/output/junit.xml
```

Each completed journey writes allowlisted JSON and a masked PNG in a fresh subdirectory.
Artifacts describe completed assertions only: use the final pytest/JUnit result for teardown
and suite success. They contain synthetic data, are not a general redaction guarantee and do
not record human acceptance. See the [decision packet](../../docs/product/m1-acceptance-decisions.md)
for the remaining owner choices and observed-user tasks.

## Pinned InferFlux origin contract

`test_inferflux_origin.py` adds a real OpenAI SDK → Sandhi → InferFlux path. It launches the
CPU/stub server recorded by `inferflux_pin` twice: once with a `<think>` completion and once with
gpt-oss/harmony channels. Both fixtures pin buffered and streaming reasoning separation,
terminal usage/cache details, request and trace correlation, credential/attribution isolation,
and the direct origin error envelope. CI checks out and builds that exact full commit SHA with
ccache; no model or GPU is required.

The qualified pin is InferFlux **v0.5.0** (`bfdf32c00ccf40c7af73b86df2eedabc20a7ae85`).
All 16 existing origin-contract cases also passed against the installed macOS
Homebrew release. This is synthetic wire evidence, not GPU or actual-member C5 acceptance.

For a local focused run, build the pinned InferFlux checkout and provide both paths:

```sh
INFERFLUX_SERVER_BIN=/absolute/inferflux/build/inferfluxd \
INFERFLUX_CONFIG=/absolute/inferflux/config/server.yaml \
  python -m pytest tests/sdk-conformance/test_inferflux_origin.py -q
```

A contract-changing PR must update `inferflux_pin` in the same PR or a same-day follow-up, and
the new revision must pass this focused suite before the pin moves.

## Optional AgentBrowser integration

Use a built sibling checkout with Node 24 LTS and its Chromium installed. Qualified source:
AgentBrowser **v1.16.1** (re-qualified 2026-10-11; smoke + recovery pass).
Build that exact tag using its frozen lockfile and documented `pnpm build`
workflow before running; stale `dist/` packages may not have the snapshot/plan methods.

```sh
SANDHI_AGENTBROWSER_ROOT=/absolute/path/to/agentbrowser \
  .venv-codesign/bin/python -m pytest tests/sdk-conformance/test_agentbrowser_smoke.py \
  tests/sdk-conformance/test_recovery_agentbrowser.py -q
```

Without that variable the optional test skips; an explicitly configured broken checkout fails.
The regular CI job runs the Playwright regressions but does not fetch a sibling repository.
`test_recovery_agentbrowser.py` uses the same optional checkout to verify restored dashboard
evidence, Refresh and token clearing without generating new inference observations.
Both tests pass against v1.16.1, including the recovery test's negative
numeric/DOM evidence controls. This is an optional test integration, not a Sandhi
runtime package dependency; no AgentBrowser package is added to gateway installs.
It also compares accessibility-visible attribution/budget rows against restored evidence and
deliberately corrupts values, headings and visibility to check that the verifier rejects
mismatches. Overview cards receive exact DOM-value/label checks only: the current sibling
observation API omits their static text from accessibility output, so card visibility is not
certified by this integration.
Joint pinned-checkout CI remains AB02 in the
[co-design tracker](../../docs/upstream/browser-gateway-vault-codesign.md).

The harness invokes the real in-process AgentBrowser service and Chromium engine. It uses
snapshot semantic refs and one-step plans, a synthetic `vault://` registry entry, redacted
untrusted observations, and an exact-origin test policy for the disposable loopback gateway.
It checks locked → authenticated usage → cleared/locked, and denies another loopback port.
It does not start an AgentBrowser REST listener, bypass action approval, export cookies or
capture artifacts, and does not claim REST tenant or live SentinelPass coverage. Production
loopback defenses are unchanged. Both service and browser close in `finally`; the subprocess
has a 90-second deadline.
