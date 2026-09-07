# Synthetic operator decision evidence — 2026-09-07 UTC

This packet supports [UA01–UA06](../../m1-acceptance-decisions.md); it is not human acceptance.
All requests, credentials, attribution and broker grants are disposable synthetic fixtures.

## Review aids

Each screenshot has same-basename JSON containing allowlisted completed assertions, UTC capture
time, test identity and the immutable executable SHA-256. Final suite status is separate from
these per-journey artifacts and includes fixture teardown.

| Journey | Screenshot | Assertions |
|---|---|---|
| Browser-minted key used for an attributed request | [First request](first-request.png) | [JSON](first-request.json) |
| Committed zero cap, real 429, no upstream/event/lease delta | [Budget denial](budget-denial.png) | [JSON](budget-denial.json) |
| Explicit cap change, successful request and reconciled totals | [Budget recovery](budget-recovery.png) | [JSON](budget-recovery.json) |
| Locked broker reference rejected | [Locked](locked-registration.png) | [JSON](locked-registration.json) |
| Same authorized reference registered after synthetic unlock | [Locked recovery](locked-recovery.png) | [JSON](locked-recovery.json) |
| Missing reference rejected | [Missing](missing-registration.png) | [JSON](missing-registration.json) |
| Same reference registered after synthetic provisioning | [Missing recovery](missing-recovery.png) | [JSON](missing-recovery.json) |
| Reference outside the read grant rejected | [Denied](denied-registration.png) | [JSON](denied-registration.json) |
| Exact authorized reference registered without widening the grant | [Denied recovery](denied-recovery.png) | [JSON](denied-recovery.json) |

Magenta blocks deliberately mask inputs, one-time keys and inventory/config panels. They are
capture overlays, not product styling. The fixture checks known secrets and allowlists visible
confirmation/error copy before capture; this is not general secret-redaction certification.
No traces, HAR, cookies, request headers/bodies or vault contents are published.

## Specific questions for observed review

- Can the reviewer find the run view and explain its own/subtree counts without coaching?
- Does the budget warning communicate estimate-based admission? The denial image retains
  **14 spent / 0 limit**; setting a cap does not erase prior settled consumption.
- Is broker recovery unambiguous? Recovery images retain the earlier transient error toast
  alongside the new successful registration confirmation. Toasts expire after four seconds;
  the JSON/API assertions establish recovery, but operator comprehension still needs checking.
- Does the denied-reference copy give enough guidance to distinguish correcting a reference
  from requesting an appropriate broker grant? Automation verifies safe denial, not that inference.

Do not interpret a screenshot inspection as an unassisted user session. Record confusing copy
as a finding and decide whether it blocks this checkpoint; no acceptance result is prefilled.

## Provenance and verification

Source and harness: `2b4788bbf3f08a46a66ea6502807ec88f08ae8d2`, clean at suite start.
Native decision-journey executable SHA-256:
`ff4ae4f671b42ead672ca2dd0420abd06d1160a9cb00d8105e992e2d6c2f2702`.
The fixture builds `sandhi-proxy --bins --features sentinelpass-ipc` and copies the executable
before using it. The wider suite also builds default-feature binaries; this hash identifies
the nine decision artifacts, not every process in the suite.

Final full-suite result: **251 passed, zero skipped, zero failures/errors, 211.29 seconds**.
The unmodified [JUnit result](junit.xml) includes teardown and two recovery properties;
pytest emitted two `record_property`/xunit2 compatibility warnings, not test failures.
JUnit SHA-256: `e0c7bfc71baacf2e53bdc157a9f9f1bb93f7fda6e62148dfd345dcdd470bde0d`.

Command from the Sandhi root (the output directory must be private and disposable):

```sh
SANDHI_AGENTBROWSER_ROOT=/home/vsingh/code/agentbrowser \
SANDHI_DECISION_EVIDENCE_DIR=/home/vsingh/code/sandhi/target/m1-decision-evidence-2b4788b \
  /tmp/sandhi-decision-venv.taSymF/bin/python -m pytest tests/sdk-conformance/ -q \
  --junitxml=target/m1-decision-evidence-2b4788b-junit.xml
```

The fresh isolated Python environment passed `pip check`: Python 3.12.2, pytest 9.1.1,
Playwright 1.58.0, HTTPX 0.28.1, OpenAI SDK 2.54.0, Anthropic SDK 0.125.0 and Google GenAI
1.75.0. All three SDKs and both optional AgentBrowser checks ran; no real provider was called.
The AgentBrowser tracked checkout was clean at `dec28b3882eb2da9cbe2dbefa571efbbbb951292`,
using Node 22.19.0 and its existing compiled packages/Chromium. This run did not rebuild those
packages, so their source-to-binary provenance is not independently established by this packet.
Joint clean-build CI remains a separate gate.

Local Rust verification also passed: `cargo test --workspace`,
`cargo test -p sandhi-proxy --features sentinelpass-ipc`,
`cargo clippy --workspace --all-targets -- -D warnings`, native-feature proxy clippy, and
`cargo fmt --all --check`. Remote CI/review/integration are tracked in TD-0026 and the focused PR.

The source remained unchanged during sequential execution; documentation/artifact additions
followed capture. Nine PNGs were visually inspected, and published JSON/XML were checked for
fixture secret values and captured request/log bodies. Re-run rather than reuse the random
`journey-*` output subdirectories. The copies here preserve the emitted bytes and fixed basenames.

## Additional broker-source automation

A clean isolated public-source checkout of SentinelPass at
`00d1e7de09e3d360954240be7588dd7c73ac317e` passed the following existing tests:

| Command in the pinned checkout | Result |
|---|---|
| `cargo test --locked -p sentinelpass-core --lib daemon::ipc::tests -- --test-threads=1` | 3 passed, 40.66 s; initial core-only build 57.94 s |
| `cargo test --locked --offline -p sentinelpass-core --lib external_secret_access::tests -- --test-threads=1` | 15 passed, 0.00 s reported |

The real Unix-socket tests cover exact grants/fields, missing/wrong client tokens, rotation and
revocation, locked read/write states, authorized upsert/readback, read-only write denial and
unsupported deletion. Grant-policy tests include persistence, token hashing and private file
permissions. They do not prove elapsed-time expiry or live Sandhi interoperability.

Isolation: checkout `/tmp/sentinelpass-contract.i5BdVv`; `CARGO_TARGET_DIR` under its `target`,
`CARGO_BUILD_JOBS=2`, and `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_RUNTIME_DIR`, `TMPDIR` set to
separate `target/probe-config`, `probe-data`, `probe-runtime`, `probe-tmp` directories. Existing
tests use explicit disposable vault/allowlist paths and UUID-named temporary sockets. The IPC
command had a 300-second outer timeout; the cached grant tests had a 60-second bound. No home
directory override, personal vault, existing daemon, keyring or provider account was used.

Core test executable SHA-256:
`6e0436e632e2c52d1b0db24c52b227bf2469343e6add9491a9cfa193ecc30bc8`.
Pinned `Cargo.lock` SHA-256:
`3180bb11938c04e911152d4698f46d8469b32c150a94ca6d3aebce791c9f4d41`.
The checkout remained clean. This workspace identifies as **0.8.2**; Sandhi embeds protocol
**0.8.1**. Eighteen source-component tests are additional evidence, not a joint-version,
live-daemon, desktop/extension or production lifecycle certificate.
