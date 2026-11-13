# Cohesive router candidate qualification — 2026-11-12

This increment makes the paired durable owner mandatory for standalone HTTP,
initializes reference ownership with that owner, and gives public library servers
the same recovery and bounded shutdown supervision. It joins the protocol/cache
compiler, immutable upstream allowance namespace, source evidence and operator
console into one candidate. See [ADR-0043](../adr/0043-mandatory-durable-http-execution.md)
and [ADR-0044](../adr/0044-native-reference-ownership.md).

## Source and validation boundary

Reviewed checkpoint base: `6918368183b167c73ab74d7ffd1f78733ad8e6ef`.
The binary was frozen from Cargo-selected output before inference and browser tests.
Later corrections below change test synchronization/expectations only. Exact final
commit review and remote CI remain distinct gates.

| Qualification | Result |
| --- | --- |
| Final workspace Rust run | 1,289 passed, zero failed; 13 ignored, including separately exercised SDK media cases |
| Corrected terminal accounting target | All seven pass; includes eight additional CR-only TCP cells; byte, source-usage, persisted-event and budget assertions retained |
| Full real-SDK suite | 731 passed, three skipped, four browser assertions failed in 916.44 seconds |
| Corrected browser module | All five journeys pass against integrated actual gateway and native broker |
| Final Python wheel | 341 binding tests pass |
| Final native Node addon | 88 binding tests pass |
| Victor final-wheel provider/gateway tests | 1,991 passed, five skipped |
| Victor actual agent/tool loops | Eight pass: four client dialects, buffered/streaming; real orchestration and tool services |
| Integrated operator console | 15 browser assertions plus three actual shutdown/readiness checks; AgentBrowser independently inspected it |
| Documentation | Strict MkDocs build passes |
| Workspace lint | All-target Clippy with warnings denied passes |

The initial Rust run had 1,288 passes and the one synchronization failure below;
the complete corrected rerun exits successfully across 58 suites.

The four SDK failures shared an obsolete exact metadata assertion: credential
registration now includes the supported explicit `scheme`. The corrected assertion
requires `api_key` and still excludes secret material. The Rust failure equated
wire EOF with completed background cleanup. Completion publication precedes operation
guard destruction; the corrected test awaits lifecycle idle under its original
five-second bound. Production shutdown already waits for that boundary. Neither
correction weakens accounting or replaces a deadline with a sleep.

## Live GLM protocol and cache evidence

[Sanitized qualification summary](artifacts/cohesive-router-qualification-2026-11-12.json)
retain both the initial failure run and the portable-output qualification.
No credential, full prompt, output content, authorization header or refresh token
is included in this artifact.

There are 24 client/backend/delivery combinations: four client formats (Chat,
Messages, Responses, Gemini), three advertised GLM endpoints and two delivery modes.
Two calls per combination produce 48 successful portable-output calls: Chat and
Responses use default thinking; Messages explicitly disables thinking. Every call
matches SDK usage to original durable source observations and paired settlement.
Positive source-reported cache reads appear on all three endpoint families.

The initial default-thinking run had 36 successful calls and 12 Messages-to-other-
client output refusals. All 48 source attempts, including those refusals, retain
qualified observations and paired settlement. Signed thinking is a plausible
explanation, supported by a separate eight-call synthetic source probe: native
signed Messages output passes, portable projections refuse, and explicit disabled
controls from all four clients reach the Messages source correctly. The sanitized
live report does not retain the exact rejected source field; this is an inference,
not a diagnosis of that provider payload. Unsigned portable reasoning remains
supported. Sandhi does not silently discard unrepresentable source state or retry
inference after a projection failure.

Reported cache reads establish metering, not guaranteed cold/warm cache behavior,
portable cache ownership, market pricing or USD savings. Fresh/read/write/output
units and allowance charges remain separate; source usage and client wire conventions
are separately qualified. This run uses the operator's GLM Coding Plan API credential,
not first-party Anthropic subscription OAuth or a live Codex account. Native Gemini,
Cohere and Ollama source/media support has synthetic SDK/HTTP evidence only here.

## Release boundaries

The prior pushed checkpoint passed all 21 public GitHub-hosted CI jobs, including
macOS ARM and Windows Python and macOS Node. Those jobs do not qualify this newer
increment until its own reviewed commit passes applicable CI. No private or overflow
runners, package publication, deployment or merge is authorized by this report.

[Current Linux reliability acceptance](artifacts/cohesive-linux-reliability-2026-11-12.json)
passes for this exact binary and wheel: 16 unchanged shutdown cases, 11 public
serving cases, four watchdog entries and 30 ordinary standalone TLS child processes.
All original deadlines and source/receipt/no-replay assertions remain. The eight
tracked HTTP/TLS cells preserve four paired receipts/eight settled reservations
and eight held reservations for unknown usage. The largest observed tracked
SIGTERM-to-exit interval is below 1.318 seconds; the unknown-usage TLS case exits
124 within its original six-second bound. No process wait timed out.

Historical TLS shutdown timing and Python TLS rejection/process-crash occurrences
remain retained and unexplained. The current Linux acceptance result does not
assign a cause or claim those incidents were fixed, and it does not establish a
failure-rate bound or acceptance of untested platforms. Independent exact-commit
review, final secret scanning, final public CI and coordinated Victor dependency
release remain required before declaring the release complete.
