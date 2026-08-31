# ADR-0007: Embedding modality — applying the ADR-0002 gate, and the verdict

Date: 2026-08-31

## Status

**Accepted — the modality is NOT admitted.** `sandhi-providers` remains chat-only. The
`feat/embedding-modality` branch is formally **parked**, not pending. This ADR is the "its own ADR"
artefact that [ADR-0002](0002-provider-transport-scope-and-modality-admission.md) §2 requires; it
returns a *negative* on the current evidence and states precisely what would flip it.

Relates to [ADR-0002](0002-provider-transport-scope-and-modality-admission.md) §2/§3 (the gate),
[ADR-0001](0001-sandhi-architecture-and-wire-contract.md) (neutral units), and
[ADR-0006](0006-layer-boundary-and-protocol-scope.md) (the same "prove the capability" discipline
applied to layers). Closes gap **G23**.

## Why this exists

ADR-0002 §3 deferred embeddings and preserved the prototype on `feat/embedding-modality`. The
branch has since sat in an ambiguous state — alive in the repository, referenced by an ADR, never
formally judged. Ambiguity is the actual defect: a contributor cannot tell whether the branch is
work-in-progress, a rejected experiment, or something awaiting a rebase, and the ADR-0002 gate has
never been *applied* to it, only *cited*.

This ADR applies the gate and records an answer, so the branch has a status instead of a vibe.

## Evidence — the branch as it stands

`feat/embedding-modality` is one commit (`bea6934`) ahead of `develop`, touching four files:

```
crates/sandhi-providers/src/cohere.rs | 96 ++++++
crates/sandhi-providers/src/embed.rs  | 84 ++++++
crates/sandhi-providers/src/lib.rs    |  2 ++
crates/sandhi-providers/src/openai.rs | 80 ++++++
4 files changed, 262 insertions(+)
```

It introduces `EmbeddingProvider`, `EmbedRequest`, `EmbedResponse`, `EmbedUsage`, and adapter impls
for OpenAI-compatible and Cohere, with wiremock tests for both. The code is competent and the usage
extraction is source-measured and neutral, consistent with ADR-0001.

**It predates `MeteredProvider` — but not the whole decorator stack, and the distinction matters.**
`crates/sandhi-providers/src/metering.rs` does not exist on that branch:

```
$ git show feat/embedding-modality:crates/sandhi-providers/src/metering.rs
fatal: path '...' exists on disk, but not in 'feat/embedding-modality'
```

`resilience.rs` **does exist on the branch** — `ResilientProvider` with its circuit breaker, retry
and timeout is right there (`resilience.rs:89,138`) — but it contains zero references to
`EmbeddingProvider` or `embed`, and `embed.rs` assembles no `UsageEvent` and produces no
`ParsedUsage`. So the precise statement is: the branch predates `MeteredProvider` entirely, and was
written without entering the resilience decorator that *did* exist. ADR-0002 itself corroborates the
timeline — written the same week as PR #13, it describes the stack as "`ResilientProvider`, and the
metering decorator to come." The verdict is unchanged; the earlier draft's stronger phrasing
("the decorator stack did not exist") was wrong.

## Applying the ADR-0002 §2 gate

| Criterion | Required | Actual | Verdict |
|---|---|---|---|
| **≥2 real consumers**, actual adopters not anticipated ones | 2 | 1, and anticipated. `embed.rs`'s own doc comment names "ProximaDB's embedding drainer (ADR-067)" as an in-process consumer; no second consumer is named anywhere, and the first is a design reference rather than a shipped adoption. | ❌ **Fails** |
| **Enters the decorator stack** — metering (neutral-event assembly) + circuit breaker + retry + timeout, uniformly with chat | Yes | No. `MeteredProvider` did not exist when the branch was written, and the branch was never wired into the `ResilientProvider` that did — `resilience.rs` has zero embedding references. No `UsageEvent`, no metering path at all. ADR-0002 §1's test applies exactly: this is "just an HTTP client," which a consumer can write locally. | ❌ **Fails** |
| **Its own ADR**, naming the consumers and the routing shape | Yes | This document. | ✅ Satisfied by writing it |

On the letter of the gate, all three fail — the third row's "satisfied" is satisfied only in the
sense that this document now exists; ADR-0002 §2 requires it to *name the ≥2 consumers*, and there
are none to name. The substantive failures are the first two: one anticipated consumer instead of
two real ones, and no decorator participation at all.

## Decision

### D1. Embeddings are not admitted. `sandhi-providers` stays chat-only.

The ≥2-consumer bar is the softer of the two failures — it could plausibly be met by a second
adopter appearing. The decorator-stack bar is the hard one, and it is exactly the criterion that
distinguishes "Sandhi transport" from "an HTTP client someone pasted into the wrong crate." Admitting
a modality that bypasses metering would mean shipping a provider call that Sandhi *cannot meter* —
in a project whose entire thesis is that every model call is counted and attributed. That is not a
gap to close later; it is a contradiction of ADR-0001.

### D2. The branch is parked, and labelled as parked.

`feat/embedding-modality` is retained as a design reference for whoever revives the modality, on the
explicit understanding that **it is not a starting point for a rebase.** It predates the decorator
stack, the typed runtime (TD-0002), the neutral chat contract's current shape, and the two-plane
proxy. Reviving it means re-deriving the design against today's architecture and salvaging the
adapter-level parsing, not merging the branch.

### D3. What would flip this verdict.

A future ADR admitting embeddings must demonstrate all of:

1. **Two named, shipped consumers** — actual adopters, per ADR-0002 §2. One must be outside the
   organisation's own agent stack, or the "one consumer implements it locally" argument still holds.
2. **Full decorator participation** — an embedding call is wrapped by `MeteredProvider` and
   `ResilientProvider`, emits a `UsageEvent` through the same `Sink`, and is subject to the same
   circuit-breaker/retry/timeout semantics as chat.
3. **A neutral usage answer for a non-chat unit.** Embeddings report input tokens and no output
   tokens, and have no cache-creation/cache-read split. `billable()` (ADR-0005 D4) is currently
   defined over the chat usage shape. The admitting ADR must state what `billable()` means for an
   embedding call **before** any enforcement path sees one, or budgets silently mis-charge a whole
   modality.
4. **An explicit routing decision** — in-process-only, or also proxy-routed. If proxy-routed, it
   needs an ingress dialect (TD-0010's parity discipline), a plane decision (ADR-0004 D1), and a
   reservation-ceiling rule, none of which exist for a modality with no output tokens.
5. **A rate-limit and allowlist story** — `permits_model` is chat-shaped; an embedding model
   allowlist is a separate namespace or an explicit decision to share one.

Items 3–5 are the ones the original prototype never had to answer, and they are the substantive
work. That is the honest cost estimate for reviving this: not 262 lines.

## Consequences

- **Positive.** The branch has a definite status. The next "just add embeddings" proposal has a
  concrete, five-item bar rather than a deferral. D3 items 3–5 surface the real design questions
  early, where they are cheap.
- **Negative.** A consumer needing embedding transport today writes it themselves — which is
  precisely the friction ADR-0002 §2 intends, and the outcome ADR-0002's Consequences section
  already accepted.
- **Neutral.** No code change. This ADR ratifies and sharpens the ADR-0002 §3 status quo.

## Pressure test

1. **"The code exists and works — parking it wastes 262 lines."** The lines are not the cost. Every
   modality in `sandhi-providers` carries a permanent maintenance, conformance-testing, and
   binding-surface obligation. Admitting one to avoid discarding a prototype inverts the cost model.
2. **"ProximaDB is a real consumer; that plus a second is nearly there."** Nearly is the whole point
   of a bar. ADR-0002 §2 says *actual adopters, not anticipated ones*, and the reference in
   `embed.rs` is a doc comment, not an adoption. When ProximaDB actually links it and a second
   consumer appears, D3 is a short document to write.
3. **"You could admit it now and wire the decorators in a follow-up."** That would ship an unmetered
   provider call in a metering gateway, and follow-ups to close a correctness hole reliably slip.
   ADR-0002 §2 lists the decorator stack as a precondition rather than a deliverable for this exact
   reason.
4. **"This ADR is a rejection dressed up as governance."** It is a rejection, stated plainly in the
   Status line, with a falsifiable five-item path to reversal. The alternative — leaving the branch
   in limbo — is the version that hides a decision.
5. **"`billable()` for embeddings is obvious: input tokens."** Probably, but ADR-0005 D4 exists
   because "obvious" definitions of billable diverged three times across the codebase. Writing it
   down before enforcement sees it costs one sentence; discovering it after costs a migration.

## References

- [ADR-0002](0002-provider-transport-scope-and-modality-admission.md) §1–§3 — the scope statement,
  the three-part gate, and the original deferral.
- [ADR-0001](0001-sandhi-architecture-and-wire-contract.md) — neutral units; why an unmetered
  provider call is a contradiction rather than a gap.
- [ADR-0005](0005-enforcement-correctness-reservation-ledger-observe-enforce-split.md) D4 — the
  single `billable()` definition that D3 item 3 must extend.
