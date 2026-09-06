# Gateway SDK and dashboard conformance

The suite starts a real Sandhi binary with disposable stores and local synthetic upstreams.
No provider account, real key, OS vault or external model request is needed.

Install the SDK test dependencies from the `sdk-conformance` CI job and Python
`playwright==1.58.0`, then install its Chromium:

```sh
python -m playwright install chromium
python -m pytest tests/sdk-conformance/ -q
```

`test_dashboard.py` covers authentication, keyboard usage, public/disabled modes, mutation
feedback, one-time key visibility, token clearing and stale replies, hostile metadata, failed
database reads, served CSP/assets and mobile overflow. A synthetic-data screenshot is written
under `target/dashboard-authenticated.png`. Requires loopback sockets and headless Chromium.

`test_management.py` adds durable-write fault injection, invalid policy checks, concurrent
budget writers, actual process restart comparison, partial config/alert reports, sequential
retry deduplication and browser/CLI failure UX. Its SQLite triggers affect only disposable
test databases. Provider metadata faults are exercised in `test_broker.py` after proving that
the synthetic broker received the save, rather than merely failing input validation.

`test_reasoning.py` checks separate reasoning below, equal to and above candidate output across
transparent Gemini and translated Chat/Responses/Anthropic, both streamed and complete. Its 24
synthetic cases compare response conventions, stored event categories, run totals and settled
leases. They validate accounting, not real-provider token bounds or strict-cap eligibility.

`test_broker.py` builds with `sentinelpass-ipc` and drives a real proxy against a disposable Unix
socket daemon. It tests synthetic read/write grants, locked/denied/missing states, timeout and
overlapping mutations, redaction, reference registration (API/CLI/browser), unsupported deletion,
namespace rejection, scheme aliases and metadata faults. Config-apply cases retain canonical
reconciliation facts for storage failure and ambiguous write timeout. Its daemon token lives only under a disposable XDG
configuration directory. No user vault or daemon is contacted. Unix coverage is not Windows
named-pipe or live broker certification; those remain joint gates.

## Optional AgentBrowser integration

Use a built sibling checkout with Node 22 and its Chromium installed. Reviewed source revision:
`dec28b3882eb2da9cbe2dbefa571efbbbb951292`. Build that checkout using its documented `pnpm build`
workflow before running; stale `dist/` packages may not have the snapshot/plan methods.

```sh
SANDHI_AGENTBROWSER_ROOT=/absolute/path/to/agentbrowser \
  python -m pytest tests/sdk-conformance/test_agentbrowser_smoke.py -q
```

Without that variable the optional test skips; an explicitly configured broken checkout fails.
The regular CI job runs the Playwright regressions but does not fetch a sibling repository.
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
