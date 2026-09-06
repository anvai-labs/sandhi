# Checkpoint adversarial review — 2026-09-05

Scope: PR #230, `ed1781e` → `97e4195`, plus the corrections recorded below. W01–W04
share runtime paths; W05a is an inactive store foundation, not proxy attempt accounting.
This is a source/test review, not a security certification or an approving GitHub PR review.

## Findings and disposition

| Finding | Impact | Correction and evidence |
|---|---|---|
| Credential validation rejected documented `api-key` and prior case-insensitive spellings | Valid onboarding/configuration fails before broker access | One fallible normalized parser preserves aliases and rejects unknown schemes; direct/reference, CLI and config regressions |
| Python `parse_usage` dropped reasoning fields | Custom parsers delegating to the helper lose the reasoning category and separate-reasoning charge | Export both fields; eight direct/custom-parser roundtrips cover zero, lower, equal and higher reasoning with included/separate semantics |
| Provider metadata-fault fixture used unsupported `none` scheme | Test could pass on validation without reaching the injected SQLite failure | Replace with synthetic native broker test proving `SaveSecret` before rejected metadata, empty inventory, redaction and subsequent recovery |
| Config apply discarded canonical broker reconciliation facts | Operators cannot distinguish ambiguous writes from safe retry or committed metadata | Retain only four allowlisted canonical fields from a bounded internal response; timeout and metadata-failure tests |

Security/operational and accounting/storage reviews ran independently. Re-review of the fixes
found no remaining confirmed merge-blocking defect in scope. Review checked dashboard auth and
script safety, durable/live management ordering, native broker deadline/cancellation semantics,
reasoning propagation and Gemini completion, atomic receipt/replay/claim transactions and
migration guards. A clean verdict is bounded by this scope and the synthetic evidence below.

## Verification after corrections

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo clippy --workspace --all-targets --features sandhi-proxy/sentinelpass-ipc -- -D warnings`
- `cargo test --workspace --features sandhi-proxy/sentinelpass-ipc`
- `cargo llvm-cov --workspace --features sentinelpass-ipc --fail-under-lines 75 --ignore-filename-regex 'src/generated/' --summary-only`: **86.96%** lines; receipts **90.61%**.
- `SANDHI_AGENTBROWSER_ROOT=/home/vsingh/code/agentbrowser python -m pytest tests/sdk-conformance/ -q -x`: **103 passed, 1 skipped**; missing local Google SDK is installed by CI.
- `python -m pytest tests/sdk-conformance/test_broker.py -q -x`: **27 passed**.
- Rebuilt Python extension, `python -m pytest bindings/python/tests/ -q`: **37 passed**; binding fmt/clippy pass.
- `python3 scripts/gen-binding-contract-facades.py --check` and `git diff --check`: pass.

Tests use disposable stores, synthetic secrets/upstreams and a disposable Unix broker, with
headless Chromium and the optional sibling AgentBrowser engine. No real vault, grant, provider
credential or model service was used. Workspace coverage does not include external browser
execution and is not proof that every failure path is covered.

## Residual and integration gates

- An existing interrupted-stream byte fallback may conservatively overlap reasoning bytes and
  reported reasoning units. It is not introduced by this checkpoint and does not establish a
  strict-cap guarantee; category-aware estimation remains follow-up work under W03/W05.
- Live broker/Windows certification, revocation generations, fleet guarantees, attempt wiring,
  unknown liability, export and consumer acceptance remain the explicit roadmap gates.
- Separate W06b branch `800753d` received a read-only adversarial review of buffer accounting,
  observer lifetime, close/panic behavior and metric families; focused tests passed with no
  confirmed blocker. This does not fold W06b into PR #230 or complete readiness/recovery/M1.
- Hosted CI [34005979889](https://github.com/anvai-labs/sandhi/actions/runs/34005979889) is green
  for pre-correction `97e4195`. The correction commit must receive its own green CI.
- GitHub requires `CI Success` and one approving PR review. At review time no approval is
  recorded. Owner authorization in the working conversation does not satisfy that check;
  do not self-approve, weaken protection or use administrator bypass.

Integration and subsequent release status remain tracked in
[TD-0026](../td/TD-0026-gateway-product-evolution.md), not inferred from this review.
