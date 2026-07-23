# ADR-0004: Two-plane proxy (transparent metering vs. translation) and the trust-tiered enforcement boundary

Date: 2026-07-22

## Status

Proposed. Amends [ADR-0001](0001-sandhi-architecture-and-wire-contract.md) §4 (byte-exact
forwarding) and clarifies the enforcement location left implicit across ADR-0001 §2 and
[TD-0003](../td/TD-0003-operator-surface-keys-budgets-attribution.md). Companion design detail
lives in [TD-0005](../td/TD-0005-declarative-policy-engine.md) (declarative policy engine) and
[TD-0006](../td/TD-0006-two-plane-proxy-transparent-metering.md) (implementation plan for the
transparent-metering plane). Does not touch the measure-vs-price boundary — still no dollars.

## Context

An audit of the shipped code against the docs (2026-07-22) surfaced a structural mismatch and
a set of enforcement gaps. Two are load-bearing:

1. **The proxy re-encodes everything.** ADR-0001 §4 commits to forwarding "the cacheable prefix
   **byte-exact**," and the README markets Sandhi as "prompt-cache safe." In reality the proxy
   ingress pipeline does **decode → normalize to `ChatRequestV1` → re-encode** on every request
   *and* rebuilds the response and every stream frame from the neutral subset
   (`sandhi-proxy/src/lib.rs`, `typed.rs:97-99`). The O(1) `metered_passthrough` primitive
   exists but is wired only into the raw adapter layer, not the proxy. Consequences observed:
   - **Message-level Anthropic `cache_control` breakpoints are silently dropped** (only
     system/tool breakpoints are re-grafted), a direct prompt-cache regression — the exact
     failure Sandhi exists to prevent.
   - Provider-specific **request** fields survive only when ingress dialect == upstream family
     (via a raw-body stash in `extensions`); they are dropped on any cross-family route.
   - Provider-specific **response** fields (`system_fingerprint`, `service_tier`, response
     `logprobs`, Anthropic `thinking` blocks) are dropped in every case.
   - **Gemini has no ingress dialect** — a native Gemini client cannot point at the proxy at
     all.

2. **Enforcement custody is undefined.** ADR-0001 §2 says the proxy "holds the real upstream key
   server-side," and TD-0003 adds virtual keys and budgets, but nothing states *where the policy
   decision must run* or *what threat model each deployment defends against*. The budget ledger
   is in-memory, cumulative-token-only, and proxy-only; the SDK path has no wired enforcement.
   This leaves two real questions unanswered: can enforcement be headless (in-process), and can
   a client circumvent it?

The root cause of (1) is that Sandhi conflates two responsibilities that have opposite
requirements: **observing usage** (wants byte-exact forwarding) and **translating dialects**
(inherently lossy). Forcing both through one pipeline pays the translation cost — and its
lossiness — on the common same-dialect path that should be pure passthrough.

## Decision

### D1. The proxy has two planes; `ChatRequestV1` is a translation target, not a chokepoint

Split the proxy request path into two planes, selected per request by comparing the **ingress
dialect** (the `/v1/*` route the client hit) to the **upstream family** (resolved from the
virtual key's `upstream_ref`):

- **Transparent-metering plane (default, same-family).** When ingress dialect == upstream
  family, forward the request body **byte-exact** to the upstream, stream the response through
  `metered_passthrough`, and extract usage with the existing `sniff_usage_line` parsers from
  `sandhi-core::usage`. **No decode, no re-encode.** This restores full fidelity, cache safety,
  and O(1) streaming, and is strictly faster.
- **Translation plane (opt-in, cross-family).** Only when ingress dialect ≠ upstream family
  does the request route through `ChatRequestV1` and the per-family codec. This path is
  documented as **best-effort with declared lossiness** — a client asking OpenAI-in /
  Anthropic-out is accepting translation, not expecting fidelity.

This makes TD-0002's own rule true in code: "native passthrough is allowed only when ingress
and upstream dialects match" — today the code does the opposite of that sentence.

Corollaries:
- Add the missing **Gemini** and **Cohere** ingress dialects so "drop-in for the three majors"
  is literally true.
- Give `cache_control` (and other cache-affecting affordances) a **first-class home** on
  `ChatMessageV1` / content parts so breakpoints survive even on the translation plane.
- The in-process bindings already avoid re-encoding for native-JSON calls; they are unaffected.

Detailed implementation sequencing is in TD-0006.

### D2. Enforcement is trust-tiered; custody of the credential is the only hard boundary

State the theorem plainly so the docs stop implying otherwise:

> Enforcement cannot be made tamper-proof inside a process the adversary controls **if that
> process holds the real upstream credential**. A client that can edit or skip the policy
> evaluator can equally call the provider directly. **Custody of the upstream key is the
> enforcement boundary; nothing else is.**

Therefore Sandhi ships **one** policy engine and **one** policy document (TD-0005), deployed in
three tiers that differ only in *where the credential lives* and *whether the decision is
authoritative*:

| Tier | Audience | Credential custody | Decision | Circumvention |
|---|---|---|---|---|
| **0 — trusted / headless** | first-party apps, internal teams | in the client process (SDK) | in-process, advisory-but-local | possible, **out of threat model** — value is preventing accidents and runaway loops with zero network hop |
| **1 — semi-trusted** | revocable partners | client process; usage **attested** to a collector | in-process decision + reported usage; key revocable | detectable, not preventable |
| **2 — untrusted / freemium** | external multi-tenant | **server-side only (inline proxy)** | authoritative | prevented |

Consequences of D2:
- **Headless enforcement is real and endorsed for Tiers 0–1.** The same `PolicyEngine` runs
  in-process (SDK) and in the proxy — no network round-trip per request. This directly answers
  the "network connectivity must not be a bottleneck" requirement.
- **Freemium / untrusted multi-tenant MUST use Tier 2.** There is no headless design that is
  simultaneously tamper-proof against an untrusted end user. Documentation and marketing must
  not imply the SDK can police an adversary.
- **Signed policy bundles give integrity, not enforcement.** Signing (Ed25519/JWS) prevents a
  client from silently editing policy *content*; it does not force the client to *run* the
  evaluator. Both facts must be stated together wherever policy distribution is described.

### D3. Budgets must be durable, windowed, and shared before Tier 2 is trustworthy

The current in-memory `BudgetLedger` is disqualifying for Tier 2 for two independent reasons,
both of which are enforcement holes, not mere inconveniences:

- **Restart resets spend and caps to zero** — a client benefits from every crash/redeploy.
- **Per-replica ledgers enforce independently** — N proxy replicas yield an effective cap of
  N× the configured cap.

Decision: budget state moves behind a durable, atomic, **shared** ledger (SQLite is sufficient
single-node; a Redis/Postgres backing is required for HA), with real time windows and the
per-minute rate limits that are currently stored-but-dead. The `BudgetLedger` trait stays in
`sandhi-core`; the durable implementation lives in `sandhi-store`. This is the enforcement
substrate the TD-0005 policy engine records against.

### D4. Close the incidental gaps the audit found

Independent of the above, and cheap: enforce the per-key **model allowlist** (`permits_model`
is stored but never called in the request path); require auth on **`/dashboard`** (it currently
serves usage aggregates unauthenticated); use a **constant-time compare** for the admin token;
and settle a single definition of "billable" (the budget currently bills `tokens_in + tokens_out`
while the usage event meters the cache split too).

## Consequences

- **Positive:** the "prompt-cache safe" and "drop-in replacement" promises become true on the
  proxy path, not just the bindings; fidelity loss becomes an explicit, opt-in property of
  cross-family translation rather than a silent default; enforcement gets an honest,
  deployable trust model that spans SDK and gateway with one engine; budgets become trustworthy
  under restart and horizontal scale.
- **Cost:** the proxy grows a second code path (mitigated — the transparent plane is *less*
  code, mostly deletion of re-encode work on the common path); a first-class `cache_control`
  affordance is a v1 chat-contract addition (additive, non-breaking per TD-0002 policy); the
  durable ledger adds a `sandhi-store` dependency to the enforcement path (already present for
  vault/vkeys).
- **Boundary preserved:** none of this emits dollars, tiers, or SKUs. Budgets and policies are
  neutral-token enforcement knobs; pricing stays downstream (ADR-0001 §Context, ADR-0047 D3).

## References

- [ADR-0001](0001-sandhi-architecture-and-wire-contract.md) §2, §4 (amended by this ADR)
- [TD-0002](../td/TD-0002-typed-provider-runtime.md) (typed runtime; "native passthrough only
  when dialects match")
- [TD-0003](../td/TD-0003-operator-surface-keys-budgets-attribution.md) (vault, vkeys, budgets)
- [TD-0005](../td/TD-0005-declarative-policy-engine.md) (declarative policy engine + distribution)
- [TD-0006](../td/TD-0006-two-plane-proxy-transparent-metering.md) (transparent-metering plane
  implementation plan)
- Apache Ranger PDP/PAP/PIP separation (prior art for local decision + central distribution)
