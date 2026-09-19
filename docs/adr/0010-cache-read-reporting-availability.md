# ADR-0010: Cache-read reporting availability without changing neutral accounting

Date: 2026-09-18

## Status

**Accepted for implementation.** Consumer decisions for TD-0028 C2 were independently
reviewed before implementation. The additive contract is implemented at minor 9;
merge, deployment and release gates remain distinct. This does not authorize a release.
The preceding corpus increment is [PR #268](https://github.com/anvai-labs/sandhi/pull/268).

Relates to [TD-0028](../td/TD-0028-cache-accounting-availability.md),
[the live-path handoff](../upstream/inferflux-cache-codesign-2026-09-18.md),
[ADR-0001](0001-sandhi-architecture-and-wire-contract.md) (neutral units), and
[ADR-0008](0008-inferflux-admission-and-session-affinity.md) (catalog-owned facts).

## Verified problem and non-goals

The current shared OpenAI parser correctly splits inclusive prompt counts. It maps
absent or invalid cache-read fields to numeric zero, making those cases look like
explicitly reported zero in event/store/dashboard consumers. Existing prefix-cache
capability booleans default to false and are not explicit negative declarations.

The change must not implement caching, infer hits from latency, change provider
selection, add another provider parser, change neutral billable arithmetic, or claim
an upstream field proves that a backend actually performed reuse. The producer's
execution-versus-lookup correctness investigation remains upstream work.

## D1. One optional field-level observation, unchanged numeric counters

Add optional `cache_read_observation` consistently to `ParsedUsage`, `UsageV2` and
`UsageEvent`. Its object contains bounded `status` and `source` enums:

| Status | Meaning | Source |
|---|---|---|
| `reported` | The recognized cache-read field contains a validated unsigned integer, including zero | `origin_usage`, or `caller_supplied` for an explicit manual observation |
| `absent` | A received, inspected usage response lacks the applicable cache-read field | `origin_usage` |
| `malformed` | The applicable field or its container exists but has invalid shape/type or exceeds the existing plausible-count limit | `origin_usage` |
| `unsupported` | Explicit, scoped evidence declares cache-read reporting unsupported | `explicit_capability` |
| Object omitted | Legacy, uninstrumented, no observable response, or unrecognized observation | Unknown; do not fabricate a source |

`cache_read_tokens` remains a `u64` with its current zero default. Observation metadata
never changes budget settlement, billing, cache-write numbers or reasoning inclusion.
Source labels identify a measurement path, not authenticated evidence or backend behavior.
Manual consumers may continue supplying numbers without metadata; omission stays unknown.
Do not infer manual presence from an argument that already defaulted to zero.

Unsupported means unsupported **reporting**, not unsupported caching. No current
defaulted capability boolean produces this status. Initially no provider is marked
unsupported without separately reviewed explicit, model/route-scoped evidence. Apply
this precedence only after the protocol-specific stream reduction in D3:

| Observed response/field | Scoped unsupported declaration | Result |
|---|---|---|
| Valid cache field, including zero | Either | `reported` / `origin_usage` |
| Explicitly malformed cache field or usage container | Either | `malformed` / `origin_usage` |
| Response inspected but applicable field absent | Present and applicable to resolved model/route | `unsupported` / `explicit_capability` |
| Response inspected but applicable field absent | Absent or scope unconfirmed | `absent` / `origin_usage` |
| No observable response | Either | Unknown (object omitted) |

A reported zero is never rewritten as unsupported. A valid integer exceeding inclusive prompt is still reported:
retain the existing clamp/warning for that consistency error, rather than silently
changing billing or treating field presence as absence.

Optional metadata must not make previously accepted numeric payloads fail. Unrecognized
enum values, missing source, or invalid status/source pairs degrade the observation to
unknown at input boundaries; they must not become `reported` or `unsupported`. Generated
bindings must pin this behavior with the same fixtures as Rust. Emit only canonical,
validated combinations; never persist arbitrary enum strings or raw invalid field values.

## D2. One classifier beside the existing parser; absence is not a finalized call

Classify field presence/type before numeric coercion, respecting the parser's existing
precedence (including DeepSeek top-level hit/miss fields). The numeric result remains
unchanged. Cover OpenAI Chat/Responses, Anthropic and Gemini shapes in shared core logic;
families with no recognized cache-read field remain absent/unknown, not unsupported.

Retain the existing `Option<ParsedUsage>` result semantics. Some parsers return `None`
for absent/malformed whole usage; changing that to `Some(default)` solely to carry
metadata could mark an unmeasured response as finalized. Use a shared internal
observation result carrying optional numeric usage separately from optional metadata,
with existing parser entry points delegating to it. Both adapter and transparent
metering paths consume that result; neither re-derives cache splitting.

When no response was received, availability is unknown. An inspected buffered response
missing its usage object is absent (subject to D1's explicit-capability precedence);
a present invalid usage container, including null, is malformed. Streaming has
protocol-specific placeholder exceptions described in D3. This classification never
promotes completeness from unavailable/partial to final.

## D3. Streaming observes fields, not frames or latency

Canonical streams (including Python/Node `stream_json` / `streamJson`) may emit an
accounting-only `Usage` update with `completeness=unavailable` and a validated cache
observation before content or after a numeric measurement. This carries field metadata,
not another numeric verdict: consumers retain their prior numeric measurement and merge
the observation. `ResponseStart` remains first; callers must not assume a fixed total
event count. These updates are suppressed on translated provider wire responses, and
transparent SSE remains byte-exact. Exactly one final numeric usage report remains one
final report; metadata updates do not become additional logical-call events.

Preserve the existing family-specific snapshot/delta accounting semantics. Do not sum
cumulative snapshots or count each SSE frame as another call. Normal content/finish
frames without a usage object do not erase earlier observations. OpenAI's legal
`usage: null` placeholders on content/finish frames with nonempty `choices` are also
non-observations, not malformed usage. A recognizable terminal usage frame
(`choices: []` with a present `usage` key) containing null or another invalid usage
container is malformed. Other families must apply their existing framing semantics;
do not classify every JSON null using a protocol-independent rule. For cache-bearing
usage frames, a later valid observation replaces the earlier cache-read observation;
a later explicitly malformed cache field marks reporting malformed without inventing
a replacement count. An omitted cache field in a later partial usage update preserves
an earlier valid cache observation. The retained numeric behavior remains unchanged.

At stream completion, if a valid response was received but no cache observation was
ever available, mark absent (or D1's explicitly declared unsupported). Null-placeholder-only
streams with no terminal usage therefore remain absent, not malformed. Valid cache
usage followed by a normal null placeholder retains reported status/counts, even if
the terminal usage frame never arrives. A malformed terminal frame marks metadata
malformed while retaining existing numeric behavior; a later valid frame restores
reported status. Transport/setup failure with no observable response stays
unknown. An aborted stream may retain reported cache input while output is estimated;
field reporting is independent of whole-call `UsageBasis` and `UsageCompleteness`.
Exactly one logical-call event contributes to aggregate coverage, whether final or
aborted. Tests must cover late terminal usage, split frames, malformed updates,
no-terminal completion, cancellation and failure before headers.

## D4. Nullable storage and constant-size, same-population coverage

Add nullable status/source columns using SQLite's existing additive migration. Old
rows stay unknown, including positive historical counters: zero versus nonzero is not
evidence of wire presence or source. Migration is idempotent, handles partially
upgraded databases and propagates unexpected database errors. Store only validated
canonical values; invalid historical pairs read as unknown.

Add optional `cache_read_coverage` to `UsageAggregateV1`: five counters named
`reported`, `absent`, `malformed`, `unsupported`, `unknown`. Their sum equals that
row's existing `calls`. Memory folding, SQL grouping/grand totals, overflow buckets,
run-tree own/rollup and aggregate merge must agree. Merging an old aggregate with
no coverage contributes its `calls` to unknown, not zero known coverage.

Coverage is cache-field reporting coverage, not hit rate, completed-call rate, or
fraction of prompt tokens reused. Each row uses precisely its own filters/population.
Do not compare filtered buckets with an unfiltered grand total or use latency-sample
counts as a denominator. SQLite currently does not persist completeness/basis; this
change therefore does not promise a final-provider-reported-only denominator.

## D5. Consumer and protocol decisions

| Consumer | Required behavior |
|---|---|
| Core/SQLite/operator API/run tree | Preserve observations and identical same-population coverage; unchanged numeric fields |
| Python and Node bindings | Same optional observation and aggregate shape; omitted manual inputs stay unknown; invalid metadata cannot relabel counters as reported |
| Generated schemas/facades/stubs | One canonical shape; regenerate, parity-test and bump the chat contract minor/digest when implementation lands |
| Dashboard/CLI | Explicit reported zero remains `0`; absent is “not reported”; malformed/unsupported/unknown are labelled; mixed totals show reporting `n_reported/n_total` alongside existing sums |
| Transparent provider response | Byte-preserving; never inject metadata into forwarded upstream bodies/SSE |
| Translated provider response | Keep current wire compatibility for this increment; do not claim lossless availability round trips |

The last distinction is intentional. OpenAI Chat, Responses and Anthropic codecs
currently synthesize zero cache fields, while Gemini drops explicit zero. Changing
those protocol shapes is a separate compatibility change with dialect-specific tests.
For now, authoritative availability lives in Sandhi's canonical events/bindings/API,
not in a cache field re-parsed from a translated response. No new proprietary wire
extension or silent optional-field omission is introduced here.

Cache-read and cache-write counters are neutral accounting categories. An OpenAI
cache-write zero does not prove no KV state was created; cache-read tokens are not
a dollar discount. Identical subject/group totals are expected when they select the
same call population. Old API payloads lacking coverage render as unknown, not 0%.

## D6. Compatibility, rollout and remaining increments

Optional JSON additions preserve old numeric consumers, but added public Rust struct
fields break exhaustive struct literals. Record that source compatibility change in
release notes and obtain explicit version/target approval before a new release; the
previous v0.7.0 approval does not authorize another tag. No major wire rename or
numeric default changes are permitted. Update manual propagation in provider/runtime,
proxy partial-usage/event construction, bindings and SQL run-tree reconstruction.

Implement C2 as one reviewed vertical contract increment, with parity/migration tests,
before C3 UI coverage. C4 diagnostic export needs its own authorization/retention
review: request/session/run identifiers are lookup keys, not credentials. Preserve
existing admin authorization, bound returned rows/bytes, avoid raw keys/prompts/bodies
by default, and do not invent historical completeness or raw reported fields that
were never persisted. Prompt capture, if introduced, is a separate opt-in bounded path.

C5 uses the owner-selected isolated WSL gateway instead of requiring Mac access, and
still requires a sanitized actual member replay after the InferFlux investigation.
Model-free corpus tests or synthetic live probes cannot close that live gate. No
shared cache clearing, server restart or credential export is authorized by this ADR.

## Required regression matrix before acceptance

- Present zero/positive, missing field/container/usage, invalid null/string/bool/float/
  negative/absurd values, DeepSeek precedence and valid-but-inconsistent counters.
- All supported families, both forwarding paths, late/fragmented/cancelled streams,
  null-placeholder-only without terminal usage, valid-then-null, malformed terminal
  null, no response, and manual legacy/defaulted inputs.
- Explicit capability precedence for absent, valid-zero, malformed and no-response
  cases; unconfirmed model/route declarations must not produce unsupported.
- Old and partially upgraded SQLite databases, invalid stored metadata, SQL/memory
  parity, filters, overflow, mixed legacy rows and run-tree merging.
- Python/Node/Rust parity, generated schema/minor checks, old wire payloads and unknown
  future observation values; numeric billing/reservation behavior remains identical.
- UI zero versus unknown, mixed reporting coverage and empty aggregates; no secrets
  or prompts in events, default logs, fixture artifacts or diagnostics.
