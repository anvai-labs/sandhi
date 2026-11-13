# Stable budget namespaces, qualified native sources and Gemini inline images

Status: local integration candidate; independent exact-commit review and public CI
remain required. This increment does not complete TD-0034 or approve a release.

Provider labels, endpoint protocols and upstream budget authority are separate.
Existing backends retain their exact historical `upstream_budget_scope` through
label/protocol changes and restart. The migration preserves existing caps, spend,
unknown holds and v1 transport/cache/route binding digests. Inventory exposes the
namespace read-only; diagnostic provider/model labels cannot choose a budget or
establish billing authority. New transport evidence uses v2 optional bounded
labels while historical v1 records remain readable and verifiable.

Cohere v2 and Ollama native backend adapters now qualify buffered and streaming
source usage under the existing paired owner. Cohere physical counts come from
`usage.tokens`, independently of `billed_units`. Ollama reported cached input is a
validated subset of prompt evaluation input. Missing optional counts remain
unavailable; malformed or incomplete source evidence retains liability. These are
backend families behind the existing four client dialects. Tool IDs and tool
results retain their association across turns. This increment preserves the old
execution-mode configuration; mandatory accounting retirement is a separate gate.

The actual Gemini SDK sends an inline image MIME field as `mime_type` and can
encode bytes with URL-safe base64. The canonical decoder now accepts its MIME
alias, rejects conflicting/invalid aliases or bytes, and normalizes portable image
bytes. Compatible native input retains its original body. The base64 dependency
is a production dependency, so a normal binary build exercises this code too.

## Validation and retained failures

- Integrated Rust workspace before the final Gemini change: 1,249 passed,
  six ignored across 55 test artifacts. Final proxy library after Gemini and the
  normal-build dependency correction: 218 passed. Final workspace/all-target
  Clippy and Rust formatting passed. The earlier sandbox run failed to open local
  sockets; it is retained separately from the authorized loopback rerun.
- Integrated Python wheel: 341 passed; Node: 88 passed. Initial stale Cohere/Ollama
  binding fixtures failed, then were corrected to carry qualified actual counts
  and explicit completion reasons; their original logs remain retained.
- Full SDK candidate run: 673 passed, three failed, 19 skipped in 689.58 seconds.
  All three failures were the qualification report verifier's v1-only assumption
  after transport evidence gained v2 labels. The corrected three actual SQL-join
  tests pass and also reject unknown versions, malformed labels and corrupted
  charges/scopes/receipts. Historical v1 and bounded unrelated diagnostic labels
  remain accepted without changing accounting authority.
- Sixteen InferFlux CPU-origin cases skipped in that run because its binary
  variable was absent; an explicit follow-up using the existing pinned build
  passed all 16 in 15.30 seconds. The other three skips remain explicit.
- Eight actual Gemini SDK inline-image cells passed across all four backend
  dialects in buffered and streaming delivery, checking exact bytes and original
  source receipts.

The separate seven-profile media executable passed 56 actual SDK cells after the
Gemini fix and corrections to synthetic Messages event names and Codex client
instructions. Its source inventory and executable are frozen locally; integration
of that acceptance harness into ordinary SDK CI remains pending. This is
synthetic source evidence, not live vendor/cache/billing qualification.

The pushed parent `0c61886` passed all 14 public CI jobs, including 590 SDK cases
and three skips. Actual allocations used public GitHub-hosted `ubuntu-24.04` and
`ubuntu-24.04-arm` runners. That public result does not qualify this newer local
source. Mandatory accounting cutover, native reference issuer ownership, original
shutdown/TLS investigations, coordinated Victor/release platform qualification
and final live GLM checks remain open.
