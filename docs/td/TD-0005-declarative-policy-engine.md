# TD-0005: Declarative policy engine — `PolicyDocumentV1`, an in-process `PolicyEngine`, and Ranger-style distribution

Status: Draft (proposed)
Date: 2026-07-22
Depends on: [ADR-0004](../adr/0004-two-plane-proxy-and-enforcement-boundary.md) D2/D3,
[TD-0003](TD-0003-operator-surface-keys-budgets-attribution.md) (vault, vkeys, budgets),
[TD-0002](TD-0002-typed-provider-runtime.md) (versioned wire-contract discipline)

> **Update 2026-07-23 — TD-0003 P2/P4 landed.** The imperative `BudgetLedger` now has
> daily/monthly/total **windows**, a block/**warn** policy, reservation, and **threshold alerts**
> (P2), and the **model allowlist is enforced** in the request path (P4). This engine therefore
> layers a declarative, distributable, in-process decision *over an existing windowed ledger* —
> it no longer needs to introduce windows/warn itself. The still-missing substrate it depends on
> is durability + a shared ledger + real per-minute rate limits (ADR-0004 D3). Phasing below is
> updated accordingly.

> **Refined 2026-07-23 by [ADR-0005](../adr/0005-enforcement-correctness-reservation-ledger-observe-enforce-split.md)
> (pressure-test).** The enforcement *substrate* this engine sits on is specified there
> (reservation ceilings, TTL leases, idempotent settle, atomic in-store decrement, observe/enforce
> split). Two things this engine must adopt from that review:
> - **The pure `PolicyEngine` decides eligibility, but the budget commit is an atomic store
>   operation that re-checks and can still return `Denied`** — snapshot-decide-then-write is a
>   TOCTOU that overruns caps across replicas. `PolicyDecision::Allow` carries the **cap** to
>   enforce (D1), not just an amount to record.
> Policy-schema hardening (details in ADR-0005 rationale), to fold in before implementation:
> - **Structural deny-by-default:** replace `allow: bool` (defaults `false` → a budget-only rule
>   silently *denies*) with an explicit `Allow{…} | Deny{reason}` effect; deny unless a rule
>   *explicitly* allows; reject an all-permissive `default` unless an explicit flag is set.
> - **Explicit exact-vs-prefix match kinds**, preserving **exact** as the default when compiling
>   legacy vkey `models` — a prefix matcher silently broadens authorization (`claude-*` would admit
>   `claude-*-experimental`, contradicting `keys.rs`'s deliberate no-wildcard test).
> - **Signed bundles need freshness:** `issued_at` + `valid_until` *inside* the signature and a
>   **persisted** revision floor (an in-memory floor starting at zero lets an old, more-permissive
>   signed bundle replay against a fresh client); the **signing key custody must be separate from
>   the admin API** (one non-constant-time admin-token compare must not be able to mint signed
>   fleet-wide policy); the served bundle is **caller-scoped and confidential** (never leak other
>   subjects' identities/quotas).
> - **Freemium is a minted guest credential + a global guest ceiling + a hard rate limit**, never
>   `guest:<ip>` (sybil via IPv6 /64; NAT collateral-deny; XFF spoof).
> - **Add decision logs, a `shadow`/`enforce` mode, and an append-only policy-mutation audit**
>   (table stakes vs OPA/Cedar/Ranger). `require_attribution` guarantees *presence*, not
>   *authenticity*, off the proxy plane.

## Motivation

Today policy is **imperative Rust mutating an in-memory `HashMap`**: an operator calls
`/admin/budget`, which calls `ledger.set_limit(...)`. There is no serializable, versioned,
distributable policy object; `BudgetSpec` is `Serialize`-only and never leaves memory. Three
requirements from ADR-0004 and the product vision cannot be met by that shape:

1. **One engine, two front doors.** Enforcement must run both in-process (SDK, Tier 0/1,
   headless — no network hop) and in the proxy (Tier 2), from the *same* logic. Today only the
   proxy enforces; the SDK path is unwired.
2. **Downloadable, client-side policy.** Non-SDK integrators need to fetch a policy artifact;
   SDK integrators need to cache one locally and evaluate against it offline.
3. **Freemium / guest.** A caller with no virtual key (or a free-tier key) must get a bounded
   default policy, not "unlimited" or "denied."

Apache Ranger solves the structurally identical problem for Hadoop-family services, and its
decomposition maps cleanly onto Sandhi's existing "two shapes, one core."

## Prior art: what to borrow from Apache Ranger (and what not to)

Ranger splits policy into three roles:

- **PAP** (Policy Administration Point) — a central admin server authors and stores policies.
- **PDP** (Policy Decision Point) — a **plugin embedded in each protected service** that
  periodically *pulls* policies, caches them locally, and **evaluates in-process** with no
  per-request call to the admin server.
- **PIP** (Policy Information Point) — supplies the attributes a decision needs.

The load-bearing idea for Sandhi: **the decision is always local; the server only distributes
policy.** That is exactly the headless property required. What we deliberately do *not* borrow:
Ranger's Java plugin model, its RDBMS-centric admin, and its per-request tag/attribute service.
Sandhi's PIP is trivial (the request facts + the ledger state), and distribution is a single
signed document, not a policy database sync protocol.

## Design

### 1. `PolicyDocumentV1` — the declarative artifact (`sandhi-core`, versioned like the wire contract)

A JSON document, schema-exported from Rust+schemars and codegen-checked exactly like
`chat-request.v1` (so it inherits the `codegen-drift` gate and the binding facades). Sketch:

```rust
pub const POLICY_SCHEMA_VERSION_V1: &str = "1";

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PolicyDocumentV1 {
    #[serde(default = "policy_schema_v1")]
    pub schema_version: String,
    /// Monotonic; the client keeps the highest it has seen and ignores staler bundles.
    pub revision: u64,
    /// Ordered rules; first match wins (Ranger-style), with an implicit final deny/guest.
    pub rules: Vec<PolicyRuleV1>,
    /// Applied when no rule matches (freemium/guest lives here).
    pub default: PolicyEffectV1,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PolicyRuleV1 {
    /// Attribute predicates (all must match). None => wildcard.
    pub r#match: PolicyMatchV1,
    pub effect: PolicyEffectV1,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
pub struct PolicyMatchV1 {
    pub subject_id: Option<Vec<String>>,
    pub group_id: Option<Vec<String>>,
    pub virtual_key_id: Option<Vec<String>>,
    pub provider: Option<Vec<String>>,        // e.g. ["anthropic","openai"]
    pub model: Option<Vec<String>>,           // exact or prefix ("claude-*")
    pub route: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct PolicyEffectV1 {
    /// Deny outright (allowlist/denylist of providers, models, routes).
    #[serde(default)]
    pub allow: bool,
    /// Neutral-TOKEN budgets. Multiple windows may coexist (e.g. per-minute + monthly).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub budgets: Vec<BudgetRuleV1>,
    /// Requests-per-window ceilings (the rate limits currently stored-but-dead).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rate_limits: Vec<RateRuleV1>,
    /// Attribution that must be present or the call is refused (e.g. subject_id required).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require_attribution: Vec<String>,
    /// Alert thresholds (fraction of a budget) -> channel ref; observability, not enforcement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<AlertRuleV1>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct BudgetRuleV1 {
    pub scope: String,                 // "user:alice" | "group:platform" | "key:vk_..."
    pub tokens: u64,                   // neutral tokens — NEVER dollars
    pub window: BudgetWindowV1,        // Total | Daily | Monthly | Rolling{secs}
    pub on_exceed: EnforcementModeV1,  // Block | Warn
}
```

Notes:
- **Neutral units only.** `tokens`, never dollars/tiers/SKUs — the measure-vs-price boundary is
  held in the schema itself (ADR-0001 §Context, TD-0003 boundary).
- **First-match-wins with an explicit `default`** is what makes freemium expressible: the guest
  policy is just the `default` effect (a small `Block` budget, a provider allowlist).
- `extensions` gives the same additive-evolution escape hatch as the chat contract; breaking
  changes bump to `PolicyDocumentV2` (TD-0002 versioning discipline).

### 2. `PolicyEngine` — a pure, in-process decision function (`sandhi-core`, no I/O)

```rust
pub struct PolicyRequestFacts<'a> {
    pub subject_id: Option<&'a str>,
    pub group_id: Option<&'a str>,
    pub virtual_key_id: Option<&'a str>,
    pub provider: &'a str,
    pub model: &'a str,
    pub route: Option<&'a str>,
    pub estimated_tokens: u64,   // pre-flight reservation estimate
}

pub enum PolicyDecision {
    Allow { reservations: Vec<Reservation> },  // scopes+amounts to reserve
    Deny  { reason: DenyReason },              // model-not-allowed, budget-exceeded, ...
    Warn  { reservations: Vec<Reservation>, alerts: Vec<AlertFired> },
}

impl PolicyEngine {
    /// Pure: (facts, policy, ledger snapshot) -> decision. No transport, no clock injection
    /// beyond an explicit `now` argument (keeps it deterministic + testable).
    pub fn evaluate(
        facts: &PolicyRequestFacts,
        policy: &PolicyDocumentV1,
        ledger: &dyn LedgerView,
        now: OffsetDateTime,
    ) -> PolicyDecision { ... }
}
```

This is the **PDP**, and it is the single enforcement brain. Both callers use it:

- **Proxy (Tier 2):** in `handle()`, replace the ad-hoc `estimate → reserve → 429` block with
  `PolicyEngine::evaluate(...)` → on `Allow`/`Warn` reserve, on `Deny` map to the right HTTP
  status. The authoritative shared ledger (ADR-0004 D3) is the `LedgerView`.
- **SDK (Tier 0/1, headless):** the bindings expose `runtime.with_policy(doc)` so an in-process
  call evaluates locally before dispatch, against a local (or attesting) ledger. No network hop.

`permits_model` (enforced imperatively since P4) and the budget check stop being two separate
code paths — the engine folds both into one evaluation, so the allowlist, expiry, budget, and
rate limit are decided together instead of by scattered checks.

### 3. Distribution (the PAP) — optional, signed, pull-and-cache

The gateway serves the artifact; nobody is forced to use it:

- `GET /policy/bundle` → `{ policy: PolicyDocumentV1, signature: <Ed25519 over canonical JSON> }`,
  scoped to the caller's virtual key (or the guest policy for no/free key).
- **SDK clients pull + cache + poll** (Ranger PDP behavior): fetch on startup, re-fetch on a TTL
  or `revision` bump, evaluate offline against the cached copy. Network is needed only to
  *refresh* policy, never to *decide* — so connectivity is not on the hot path.
- **Non-SDK integrators** can `GET` the JSON and enforce with their own code, or just read it for
  visibility.

**Signing semantics — state both halves together, always (ADR-0004 D2):**
- Signing provides **integrity of content**: a client cannot silently edit `tokens: 1000` to
  `tokens: 1_000_000_000` and have the bundle still verify against Sandhi's public key.
- Signing does **not** provide **enforcement of execution**: a Tier-0/1 client that controls its
  process can decline to run the engine. That is why untrusted/freemium callers are Tier 2
  (credential held server-side), where skipping the engine is impossible because the client
  never holds the real key.

### 4. Freemium / guest

- A built-in `PolicyDocumentV1::guest()` constant ships in `sandhi-core`: `default.allow = true`
  for a small provider/model allowlist, a tight `Block` token budget on scope `guest:<ip|anon>`,
  `require_attribution = []`. Embedded in the lib so Tier 0 works with zero configuration.
- Presenting a valid virtual key swaps the guest document for the fetched, key-scoped one.
- In the proxy, an unkeyed request resolves to the guest policy instead of the current flat 401,
  enabling a freemium front door **only when the operator opts in** (guest disabled by default,
  since Tier 2 credential custody still applies — the guest tier must route through a
  Sandhi-held key, never expose upstream credentials).

## Migration from TD-0003's imperative budgets

- `BudgetSpec` (serialize-only) → subsumed by `BudgetRuleV1` inside `PolicyEffectV1`.
- `/admin/budget` keeps working but becomes sugar that edits the active `PolicyDocumentV1` and
  bumps `revision`, rather than mutating a bare ledger.
- `set_limit`/`check`/`reserve`/`reconcile` on the ledger stay — the engine calls them; it does
  not replace the accounting mechanic, only the *policy* over it.
- The vkey fields already present (`models`, `budget_scope`, `rate_limit_per_min`, `expires_at`)
  become inputs the operator API compiles into rules. `models`/`expires_at` are already enforced;
  `rate_limit_per_min` is still stored-but-dead and gets enforced when the shared ledger lands.

## Phasing

> **Converged-plan mapping (2026-07-23, see TD-0004 §Phasing):** this TD **owns** P1–P4 below.
> In the cross-TD execution plan: P1 = workstream **W1b** (preceded by W1a's parity primitives in
> `sandhi-core` — shared secret resolution, `permits_model` in both modes, windowed binding
> budgets, one `UsageEvent` builder — which are this engine's inputs); P2 = workstream **W2**;
> P3/P4 = workstream **W4**. TD-0004's original "Phase D policy-as-data" is subsumed by this TD
> and no longer separately specified.

- **P1** — `PolicyDocumentV1` + schema/codegen + `PolicyEngine::evaluate` (pure, unit-tested with
  table-driven fixtures) + proxy wired to it (folds the allowlist/expiry/budget/warn checks that
  P2/P4 wired separately into one evaluation). **Both front doors:** the binding `Gateway` calls
  the same evaluate pre-flight (Tier 0/1), not just the proxy.
- **P2** — durable + shared ledger (ADR-0004 D3) so budgets survive restart and hold across
  replicas; real per-minute rate limits. (Windows/warn/alerts already landed in TD-0003 P2.)
- **P3** — `GET /policy/bundle` + Ed25519 signing + SDK `with_policy` pull/cache/poll (headless
  Tier 0/1).
- **P4** — guest/freemium default policy + opt-in unkeyed proxy front door.

## Non-goals

- No dollars, prices, tiers, or SKUs anywhere in the policy schema or engine (measure-vs-price
  boundary; TD-0003).
- Not a general-purpose ABAC/XACML engine — first-match-wins over a fixed attribute set, not
  arbitrary boolean algebra. Complexity is admitted only when a second real consumer needs it
  (ADR-0002's ≥2-consumer discipline).
- Tenancy scoping (one deployment = one tenant?) remains a TD-0003 open question / likely an
  AnvaiOps concern; the policy document is tenant-agnostic and can be scoped externally.

## Acceptance

- Same `PolicyDocumentV1` fixture yields the identical `PolicyDecision` from the in-process
  engine and the proxy (the "two front doors, one brain" gate — mirrors TD-0002's FFI/proxy
  parity gate).
- A key scoped to `claude-*` is denied a `gpt-4o` call (allowlist now enforced).
- A tampered bundle (any byte changed) fails signature verification.
- Restart preserves accumulated spend (once P2 lands); two replicas enforce one shared cap.
- No dollars stored or computed.
