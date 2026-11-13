# ADR-0043: Mandatory durable HTTP execution

Status: implementation candidate; exact-tip review, paired qualification, public
CI, packaging and deployment acceptance remain separate gates.

## Decision

HTTP generation uses one existing durable execution owner and the original
single-file ledger. The standalone process requires `SANDHI_STORE`, refuses SQLite
memory/URI forms and any shard setting except 1 **before any store opens**, and
rejects the removed `SANDHI_BUFFERED_ACCOUNTING` variable. Informational CLI flags
remain initialization free. The standalone process always initializes its owner and starts bounded recovery before serving;
there is no permissive request admission, zero-release-on-unknown, or logical meter
dedup execution branch. Pure provider FFI remains ledger free.

Public serve paths validate canonical ownership before binding. The normal app
builder initializes it only against a valid supplied ledger; operator-only states
can be assembled without generation authority, remain unready, and reject all
generation before credential/routing processing. Initialization never substitutes
another database or volatile allowance engine.

The gateway supplies an operator-owned 4096-token output default when omitted,
with a validated `SANDHI_DEFAULT_MAX_OUTPUT_TOKENS` override and only an explicit
backend-model cap narrowing it. Injection forces canonical encoding so supported
protocol controls reach the upstream; native state survives or refuses. The
response identifies default use. This is an estimated total liability, not a
universal physical token guarantee: Codex retains its codec's missing wire max.
Streams use validated default setup 30s / idle 90s / body 180s under a 240s ceiling; explicit
streaming policy remains configurable.

Block and Warn both reserve/observe/settle their two linked scopes through the
same SQL owner. Warn permits exceeding a soft cap, never storage-error fail-open.
Unknown dispatch or unqualified usage keeps both liabilities. Qualified final
source evidence may settle despite failed client delivery; no transport replay is
authorized by an accounting timeout or missing receipt.

## Migration invariants

Reuse the exact original file, reservation IDs, limits/windows/policies and
source records. Preserve old settled spend/live holds, dedup history and route
pins. Do not fabricate historical attempts, cache weights or paired receipts.
Historical unpaired TTL behavior stays distinct from conservative tracked unknown
retention. Quiesce older writers; do not downgrade a writer onto tracked evidence.
Multi-shard topology changes require their own offline migration. An old pin to
an unqualified source must refuse, without silently switching to an alternative.

## Evidence and boundaries

The historical-store foundation checks exact old rows and affinity, atomic paired
admission against old spend/holds, original weighted receipts across reopen,
Prepared/MayHaveDispatched retention and all Block/Warn combinations. Startup tests
use an actual frozen binary and disposable old stores; the former binary fails
no-store/off/shards=2 cases. New tests require refusal without listener, shard
creation or any change to original bytes/directory inventory. HTTP budget tests
use real qualified synthetic sources, with one dispatch/receipt and storage-error
refusal even for Warn.

Four client dialects remain independent of backend/source qualification. Native
Cohere/Ollama source qualification preserves their existing HTTP backend support
under the same owner, with strict usage/EOF/attempt evidence; pure FFI support
alone is insufficient. No provider
label authorizes a different persisted budget namespace, and no migration here
renames upstream scopes. See the separate identity migration.

[Operator contract and controls](../operator/buffered-accounting.md),
[paired settlement](0023-paired-budget-settlement.md),
[physical attempts](0024-durable-physical-attempt-evidence.md), and
[explicit gateway peer](0041-explicit-gateway-peer-contract.md).

Native Text Completions and input-only Embeddings retain their paths and wire
bytes through this same owner. New context freezes an explicit operation;
historical absent operation remains unknown. Embedding input must be reported
and totals consistent; output is inapplicable by operation, never a substitute
for unknown generative usage. Text streams require their own native envelope,
finished choices and final usage. Missing usage, failed delivery and restart
retain the same unknown liabilities without a guessed charge or source replay.
