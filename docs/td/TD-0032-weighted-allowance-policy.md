# TD-0032: Weighted allowances over unchanged usage measurements

**Status:** W01 merged in [#370](https://github.com/anvai-labs/sandhi/pull/370); W02–W05 pending. No live weighted enforcement.
**Owner:** Victor/Sandhi co-design session, branch `feat/weighted-budget-policy`.
**Baseline:** `origin/develop` at `c25f006` (2026-10-13).

## Customer outcome

Long coding sessions repeatedly reuse context. Counting every cached token at the same weight
as fresh input can exhaust a development allowance before useful work completes. Operators need
separate category weights without losing truthful measurements or accidentally charging reasoning
twice. Sandhi owns neutral allowance enforcement; AnvaiOps owns money, commercial plans and prices.
A weight is an operator policy choice, not a claim about a provider's invoice or GPU work.

| Category | Proposed development setting | Evidence requirement |
|---|---|---|
| Fresh input | 1x | Qualified final provider-reported input |
| Cache read | 0.1x; explicit 0x allowed | Origin-reported count, including explicit zero; a scoped explicit unsupported declaration requires zero |
| Cache creation | 1x, independently configurable | Qualified final cache-write count |
| Output | 1x, independently configurable | Qualified final output count |
| Reasoning | Inherit output unless independently configured | Differential weight requires reported count and explicit inclusion convention |

The cache weight applies to the **cached category itself**, not 10% of every prompt. If reasoning
is included in output, subtract it before applying separate output and reasoning weights. If it is
separate, add its weighted count once. Unreported reasoning can remain inside reported output at
one shared weight; it must not be displayed as a measured zero. Do not infer reasoning tokens from
text or guess inclusion by comparing magnitudes.

```text
raw measurements ──────────────────────────────► unchanged usage dashboard/export
        │
        └── qualified evidence + admitted policy ─► weighted allowance quote
                                                      │
                                                      ▼
                                            durable settlement receipt
```

## Arithmetic contract (W01)

`crates/sandhi-core/src/allowance.rs` is an opt-in, transport-free calculator. It leaves
`billable()`, UsageV2, existing budgets, schemas, parser behavior and raw aggregates unchanged.

- Integer basis-point weights: 10,000 = 1x; 1,000 = 0.1x; 0 = explicitly free.
- Each category produces fixed-point units; persist these units without rounding individual
  requests to whole tokens. Ten one-token cache reads at 0.1x consume one allowance token.
- Weights range from 0 to 1,000,000 (100x); reject negative, fractional, unknown or oversized config.
- Checked u128 intermediates; reject category or total overflow beyond signed 64-bit persistence.
- Quote errors are unresolved evidence, never zero charge, permission to dispatch or release.
- Final, provider-reported usage is required. Partial/estimated streams need a distinct recovery
  policy, not a final discounted quote. Trust must be established by the server, not caller labels.
- A quote is neither a reservation nor a settlement receipt. The V1 quote records disjoint category
  charges, retaining `None` for unavailable reasoning. No service or binding exposes this API yet.

## Delivery gates

| Gate | State | Required evidence / next action |
|---|---|---|
| W01 arithmetic | Merged (#370) | Pure calculator; red import before implementation; category, qualification, rounding, config and overflow tests; independent review |
| W02 reporting provenance | Pending | Preserve explicit-zero / absent / malformed / unsupported reasoning through parsers; persist basis/completeness and category coverage; generate additive contracts and test bindings |
| W03 durable policy ownership | Pending | Store immutable policy revision + scale with admission; atomically reserve and settle in allowance units; restart/replay/idempotency tests; no reinterpretation of old spend |
| W04 operator integration | Pending | Strict admin/config/CLI contracts; per-scope policy; dashboard separates raw tokens, reserved estimates and settled allowance units; unsupported paths fail explicitly |
| W05 released/live acceptance | Pending | Both proxy planes, streaming/nonstreaming, embeddings, cancellation and missing usage; direct/in-process consumers; actual Victor member; release and deployment evidence |

### Interim global knob (pre-W03)

`SANDHI_CACHE_READ_WEIGHT` (basis points, default 10,000) ships as an interim
operator knob: it scales only the cache-read term of the budget charge at
settlement, is server-admitted process config (never caller data), and leaves
raw usage recording and the neutral SQL aggregate untouched. It is a strict
subset of W03/W04 — per-scope weighted policies with durable revisions
supersede it, and the knob should be retired when those land.

### Why W02 and W03 precede activation

Current `ParsedUsage` conversion drops zero reasoning counts to `None`. That loses the distinction
between a real zero and an absent field. W01 refuses differential reasoning without a count; do not
work around this by inserting zero. The deployed SQLite usage projection also omits basis and
completeness. The 2026-10-13 Victor session `session_a51481ab9933` had no durable terminal-observation
joins. This is not end-to-end weighted or lifecycle acceptance.

Existing terminal observations freeze neutral `billable(usage)`, and leases do not carry a weight
revision. Changing the live neutral formula would silently change existing budget meaning. W03
must persist the admitted policy (or immutable revision and full retained definition), bind it to
reservation and receipt, and use it unchanged on recovery. Updating an operator policy affects new
admissions only. Never let a caller choose cheaper settlement weights.

Admission has no guaranteed cache hit or reasoning split. Reserve against the highest applicable
category weight for unknown input/output splits, with the existing caveat that token estimates are
not strict bounds. Any capacity refund follows qualified usage. Reasoning may exceed a provider's
output bound; unknown liability must stay explicit. Keep request-rate/concurrency controls separate:
a zero cache weight must not mean unlimited requests or zero outstanding-work limits.

### Historical units and separate category caps

Existing `limit_tokens` and historical spend remain neutral. New weighted limits need an explicit
unit/revision and a migration boundary; do not relabel existing tokens as weighted credits. Preserve
raw per-category counters. Optional separate output/reasoning caps may be added in W04 only with
atomic admission across all applicable caps; changing weights alone does not implement those caps.

## Validation and review

Reuse existing neutral accounting tests unchanged as a compatibility guard. New tests in
`crates/sandhi-core/tests/weighted_allowance.rs` cover only the new allowance contract; do not clone
provider-parsing or ledger suites here. The pre-implementation test fails because the module does
not exist. Review must attack unknown/zero conflation, reasoning overlap, arithmetic, policy changes
in flight, replay and raw/weighted labeling. Passing W01 does not close W02–W05.
