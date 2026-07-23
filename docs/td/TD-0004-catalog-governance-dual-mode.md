# TD-0004: Model catalog + unified governance core (dual-mode)

- **Status:** Phase A **implemented** (#44 catalog data + binding/proxy surface; #49 compat vendor
  seeding + Node parity; consumed by Victor #634/#635/#636). Phases B–D remain proposed.
  Implementation note: Phase A reused the existing `ProviderDescriptorV1.models` /
  `ModelDescriptorV1` contract rather than the `ModelSpecV1` sketched below.
- **Contract owner:** `sandhi-core`
- **Catalog/runtime owner:** `sandhi-providers` (+ `sandhi-store` for stateful services)
- **First consumer:** Victor
- **Started:** 2026-07-23
- **Related:** TD-0002 (typed provider runtime), TD-0003 (operator surface — keys/budgets/attribution),
  Victor **ADR-018** (Sandhi adoption), Victor **FEP-0020** (usage gateway), AnvaiOps **ADR-0047**
  (OSS/commercial open-core line), ProximaDB ADR-067 (sibling consumer)

## Decision

Sandhi becomes the authoritative owner of **model catalog data** and the **governance/billing
measurement** layer, exposed through **one typed contract surface** that has two backing
implementations: an **in-process stateless subset** (the FFI library) and a **standalone stateful
full set** (the `sandhi-proxy`). Victor delegates discovery and governance to that surface; Victor
keeps agent orchestration, catalog **policy** (which models to expose/select), and usage display.

This is **completion** of TD-0002/TD-0003 + FEP-0020, with **one deliberate reversal**: Sandhi now
*also* owns catalog **data** (it previously explicitly declined — see Context). Every other concern
named here (transport, metering, keys, budgets, attribution) is already Sandhi's by prior decision.

### Two load-bearing boundaries (held)

1. **Catalog data vs. catalog policy.** Sandhi owns catalog *facts* (model id, context window, max
   output, provider, endpoint family, wire capabilities). Victor owns catalog *policy* (which models
   to expose to an agent, model selection, discovery UX). This mirrors the transport split
   (TD-0002: Sandhi owns wire facts; Victor owns model policy).
2. **Measure vs. price (unchanged, from TD-0003).** Sandhi measures in **neutral tokens** and
   attributes; it does **not** price. `$`/tier/SKU/invoicing stays the downstream commercial layer
   (AnvaiOps ADR-0047 D4). The catalog therefore carries **no pricing** fields.

## Context

### Why Sandhi declined a catalog (the prior stance)

`sandhi-providers/src/catalog.rs` states: *"This is deliberately not a model/capability catalog.
Sandhi owns transport facts (canonical slug, aliases, endpoint routing); consumers such as Victor
own model policy, tool selection, context budgeting, and user-facing discovery."* Victor ADR-018
(L52-53) and FEP-0020 (L247-249) restate it: *"Sandhi's catalog contains only stable wire facts …
deliberately not a second model catalog. Victor retains volatile model metadata and agent-facing
capability policy."* The rationale: discovery *policy* is consumer-specific and model metadata is
volatile.

### Why revisit

1. **The multi-consumer case is real and was always the reason Sandhi exists.** Sandhi is consumed
   by Victor (Python), ProximaDB (Rust), and AnvaiOps (Python) precisely because a shared
   cross-language layer beats three drifting implementations (FEP-0020 §Motivation). Model
   *discovery* is the same triplication today: each consumer re-implements it (Victor's per-provider
   SDK calls + static lists; ProximaDB would need its own). A shared catalog at the gateway serves
   all consumers uniformly — the identical argument that moved transport into Sandhi.
2. **Catalog data is more stable than the prior rationale claims.** Model id / context window / max
   output change on new releases (quarterly), not daily. "Volatile" describes *policy* (which models
   to expose, deprecation handling, UX), not the underlying *facts*. The data/policy split (above)
   resolves the original concern: Sandhi owns the stable facts; Victor owns the volatile policy.
3. **Competitor baseline.** Every production AI gateway ships a model catalog + List Models API as
   core (OpenRouter `/api/v1/models`; LiteLLM's bundled `model_prices_and_context_window.json`;
   Portkey's model registry). The catalog is table-stakes for the gateway category Sandhi occupies.
4. **Dual-mode is already Sandhi's shape.** FFI (`Gateway`) and `sandhi-proxy` already share one
   `ProviderRuntime` (TD-0002 Phase 4). Threading catalog + governance through both modes is an
   extension of an established pattern, not a new one.

### The dual-mode reality today (and its gaps)

- **In-process (FFI):** `Gateway` holds an in-memory `KeyStore`, in-memory `BudgetLedger`, and an
  in-memory `Vec<UsageEvent>` (+ optional JSONL append). The binding links `sandhi-core` +
  `sandhi-providers` **only** — no `sandhi-store`. It **cannot be a system of record**.
- **Standalone (`sandhi-proxy`):** axum reverse-proxy with SQLite (`sandhi-store`): durable usage
  sink, attribution queries, `VirtualKeyStore`, `VaultStore`, admin REST, dashboard.
- **Known asymmetries (must close to realize this design):** (1) governance/vault/admin surface is
  proxy-only; (2) budgets are in-memory in *both* modes and not durable (the TD-0003 `budget` table
  is unimplemented); (3) `BudgetLedger` is not shared across processes; (4) rate limits are stored
  but **not enforced**; (5) two metering code paths (`proxy::usage_event` vs `MeteredProvider`) do
  not align; (6) in-process vkey resolution is exact-string only (minted hashed vkeys unusable
  in-process); (7) catalog is `models: Vec::new()` for almost every provider; (8) the operator CLI
  is HTTP-to-proxy only, with no in-process equivalent.

## Design

### Primary architecture boundary: stateful vs. stateless

This is the competitor-validated cleavage plane (LiteLLM, Bifrost): ship **one shared core**; the
in-process library is a **stateless subset**, the standalone proxy is the **full stateful set**;
both implement **the same typed interfaces**.

| Concern | Class | In-process (FFI) | Standalone (proxy) |
|---|---|---|---|
| Transport / wire facts | Stateless | full | full |
| **Catalog data** (model facts) | Stateless | compiled-in (NEW) | compiled-in + optional live refresh |
| Routing / fallback / retry | Stateless (+ ephemeral) | full | full |
| Policy **evaluation** (declarative rules) | Stateless | full | full |
| Metering **emission** (`UsageEvent`) | Stateless emit | callback / JSONL / no-op | SQLite sink |
| Cost calc (from catalog, neutral tokens) | Stateless | full | full |
| Virtual-key **system of record** | Stateful | in-memory **or injected store** | SQLite (authoritative) |
| Persistent budgets / windows | Stateful | in-memory, process-local/lossy **or injected store** | SQLite (authoritative) |
| Rate-limit counters (global) | Stateful | per-process token bucket | shared counters |
| Spend **aggregation** / rollups | Stateful | emit only | SQLite + dashboard |
| Audit log | Stateful | — | SQLite |
| `$` pricing / invoicing | — | **out (AnvaiOps)** | **out (AnvaiOps)** |

The in-process library implements **only the stateless core** plus **explicitly-lossy in-memory**
versions of stateful services (clearly documented as process-local). The proxy composes the
stateless core **plus** authoritative stateful services (SQLite). **Same interfaces, different
impls.** The escape hatch for stateful-in-process: allow the host (Victor) to **inject a store**
(local SQLite path or a callback) so the surface stays unified without pretending a library is a
system of record.

### Revised ownership (the normative change is catalog DATA → Sandhi)

| Concern | Owner | Notes |
|---|---|---|
| **Catalog data** (id, context window, max output, provider, endpoint family, wire caps, status) | **Sandhi** ← reversal | compiled into `sandhi-core`; both modes; no pricing |
| Catalog **policy** (expose/select/discovery UX, agent model-selection) | Victor | unchanged |
| Transport / wire | Sandhi | both (TD-0002) |
| Metering measurement (neutral `UsageV2`) | Sandhi | emit both; aggregate proxy (TD-0003) |
| Policy-enforcement **mechanism** (budget check, key resolve, allowlist gate) | Sandhi | stateless eval both; stateful record proxy |
| Virtual-key SoR, persistent budgets, rate-limit counters, audit, dashboard | Sandhi | proxy-authoritative; in-process = lossy/injected |
| Usage **display** / rollup, orchestration, prompt/tool/model-selection policy | Victor | unchanged (consumes Sandhi events) |
| `$` / tier / invoicing | AnvaiOps | unchanged (measure-vs-price held) |

### Catalog DATA design (curated + versioned — the LiteLLM model)

A versioned catalog compiled into `sandhi-core` as static data (Rust `const` table / bundled
resource), mirroring LiteLLM's `model_prices_and_context_window.json`. **No pricing fields**
(measure-vs-price line). Schema sketch:

```rust
// sandhi-core/src/catalog_models.rs (NEW) — stable, versioned, no pricing
use crate::chat::EndpointFamilyV1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelStatus { Current, Deprecated, Retired }

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ModelSpecV1 {
    pub id: &'static str,                 // "claude-opus-4-8"
    pub display_name: &'static str,       // "Claude Opus 4.8"
    pub provider: &'static str,           // slug, e.g. "anthropic"
    pub endpoint_family: EndpointFamilyV1,
    pub max_input_tokens: u32,            // context window
    pub max_output_tokens: u32,
    pub status: ModelStatus,
    pub aliases: &'static [&'static str],
    // NOTE: deliberately NO pricing/tier/SKU fields (measure-vs-price line, TD-0003).
}

/// Curated, release-versioned catalog. The source of truth for model *facts*.
/// Compiled in (both modes); the proxy may live-augment but this baseline is always present.
pub const MODEL_CATALOG: &[ModelSpecV1] = &[
    ModelSpecV1 { id: "claude-fable-5", display_name: "Claude Fable 5",
        provider: "anthropic", endpoint_family: EndpointFamilyV1::AnthropicMessages,
        max_input_tokens: 1_000_000, max_output_tokens: 131_072,
        status: ModelStatus::Current, aliases: &[] },
    // … Opus 4.8, Sonnet 5, Sonnet 4.6, Haiku 4.5, OpenAI gpt-5 family, Gemini 3, etc.
];
```

**Binding surface (both Python and Node):**

```python
# sandhi_gateway (PyO3) — parallel to provider_descriptor_json
def provider_models_json(slug: str) -> str: ...   # JSON list[ModelSpecV1]
```

The proxy additionally serves it: `GET /catalog/models?provider=<slug>` (and per-ingress
`/v1/models` for OpenAI-compat parity). Both modes read the same compiled `MODEL_CATALOG`.

**Victor consumption:** `AnthropicProvider.list_models()` (and every provider's) queries
`provider_models_json(slug)` and applies Victor's catalog *policy* (filtering, ordering, UX) on top.
PR victor #632's per-provider SDK live-discovery **demotes to an optional enrichment/fallback** —
the catalog is the primary source.

### Threading dual mode — typed-interface unification (the core mechanism)

> **Amended 2026-07-23 (see §Phasing):** the 2026-07-23 code audit revised this sketch. The
> unification seam is the shared *request-path pipeline* in `sandhi-core` (W1), not five store
> traits; traits are admitted only where impls genuinely differ (the existing `Sink`, plus a
> durable-store seam in W2/W4). The sketch below is kept for the record of the original idea.

One trait set in `sandhi-core`, two implementations each. Victor's delegation code is
**mode-agnostic** — it calls the same interface whether wired to FFI (in-process subset) or the
proxy (full set):

```rust
pub trait Catalog { fn models(&self, provider: &str) -> &[ModelSpecV1]; }   // stateless: compiled (both)
pub trait Policy { fn evaluate(&self, req: &PolicyInput, rules: &PolicyDoc) -> Decision; } // stateless eval (both)
pub trait KeyStore { fn resolve(&self, token: &str) -> Result<VirtualKey, _>; }  // in-mem | SQLite
pub trait Budget { fn check(&self, scope: &str, need: u64) -> Result<(), _>; /* + reserve/reconcile */ } // lossy | durable
pub trait MeterSink { fn emit(&self, ev: UsageEvent); }   // callback/no-op | SQLite
```

- **In-process binding** wires: `CompiledCatalog` + `StatelessPolicy` + `InMemoryKeyStore` +
  `InMemoryBudgetLedger` + `CallbackMeterSink`. Stateful impls are **explicitly process-local/lossy**;
  a host may inject a SQLite-backed store to upgrade them.
- **Proxy** wires: `CompiledCatalog` (+ optional live refresh) + `StatelessPolicy` +
  `SqliteKeyStore` + `DurableBudget` + `SqliteMeterSink` — authoritative.
- The operator CLI (`sandhi …`) drives the **same** operations: over HTTP to the proxy, or as direct
  function calls in-process. Same surface, different transport.

This is the answer to "how do catalog + governance + billing thread through the dual usage": **the
surface is the interface; the modes differ only in which implementations are wired.**

## Phasing — CONVERGED EXECUTION PLAN (revised 2026-07-23, supersedes the original B–D sketch)

> **Reconciliation note.** The original Phase B–D sketch overlapped TD-0005 (declarative policy
> engine) and TD-0006 (two-plane transparent metering), both drafted after this TD: Phase D *is*
> TD-0005; Phase C's durable budgets *are* ADR-0004 D3 / TD-0005 P2; Phase B's metering
> convergence overlaps TD-0006 Step 5. A code audit (2026-07-23, post TD-0003 P1/P2/P4) also
> showed the five-trait sketch above misdiagnoses the seam: the asymmetries do not come from
> missing *store* abstractions — they come from the **request-path pipeline (resolve key → gate →
> reserve → dispatch → reconcile → alert → emit) being written twice** (proxy
> `RequestAccounting` vs binding `Gateway.record_and_build`) with drift, plus a third dormant
> path (`MeteredProvider`). The revised plan unifies the *pipeline logic in `sandhi-core`* and
> admits traits only where implementations genuinely differ (durable store presence). Each item
> below names its **single owning doc** — no item is specified twice.

- **Phase A — Catalog data: DONE** (#44 data + surface, #49 compat seeding + Node parity;
  consumed by Victor #634/#635/#636). Owner: this TD.

- **Workstream W1 — Shared governance core (owner: this TD).** Move the decision/accounting
  logic both modes must share into `sandhi-core`, then point both call sites at it:
  - **W1a — parity primitives (no new concepts):** secret resolution (`exact → SHA-256 hash →
    expiry`) moves from `sandhi-proxy::resolve_virtual_key` into `sandhi-core::keys` so the
    bindings stop being exact-string-only (closes asymmetry #6); the bindings gain the
    `permits_model` gate (closes the #47 proxy-only gap), windowed/warn budgets
    (`set_budget` currently exposes total/block only), and reservation; `UsageEvent`
    construction converges on one core builder used by proxy `usage_event()` and binding
    `record_and_build` (closes the live half of asymmetry #5); binding emission goes through
    the existing `Sink` trait (`InMemorySink` + a `JsonlSink`) instead of a bespoke
    `Vec`+JSONL. `MeteredProvider` stays the adapter-layer decorator for raw-`Provider`
    hosts — documented as such, not a third request path.
  - **W1b — the policy brain:** `PolicyDocumentV1` + pure `PolicyEngine::evaluate` per
    **TD-0005 P1 (owner: TD-0005)** — the engine folds allowlist/expiry/budget/rate-limit into
    one decision; proxy `handle()` and binding `Gateway` both call it (two front doors, one
    brain). The W1a primitives are its inputs (`LedgerView`, resolved key).

- **Workstream W2 — Durable/shared substrate (owner: TD-0005 P2 ≡ ADR-0004 D3).** The
  `budgets` table in `sandhi-store` (persist `BudgetSpec`, rehydrate window spend from
  `usage_events` on startup — the re-derivation `budget.rs` promises but does not implement);
  **enforce** `rate_limit_per_min` (closes asymmetry #4) as a windowed counter consulted by the
  engine; ADR-0004 D4 remnants (constant-time admin-token compare, one `billable()` definition
  shared by ledger + event, dashboard access gating). Single-node SQLite is sufficient;
  multi-replica HA stays an explicit non-goal until a real deployment needs it.

- **Workstream W3 — Two-plane proxy (owner: TD-0006).** Raw `ProviderHandle` + plane selector +
  golden byte-identity tests (Steps 1–2), Gemini/Cohere ingress (Step 3), first-class
  `cache_control` (Step 4). Orthogonal to W1/W2 except Step 5 (billable), which is folded into
  W2 above and dropped from TD-0006's scope.

- **Workstream W4 — In-process governance surface + distribution (owner: this TD for the
  store/CLI seam; TD-0005 P3/P4 for policy distribution + guest).** The injected-store option
  for FFI hosts (an off-by-default `durable` cargo feature or host-supplied SQLite path —
  resolves asymmetry #1 without bloating default wheels), `sandhi` CLI `--db` direct mode
  (closes asymmetry #8), then `GET /policy/bundle` + signing + SDK pull/cache and the guest
  policy per TD-0005.

**Sequencing:** W1a → W1b → W2 → W3 (W3 may run parallel to W2) → W4. Each lands as its own
non-breaking PR train into develop; the release after W1–W3 is the meaningful minor
("byte-exact two-plane proxy, curated catalog, one policy brain in both modes, durable
budgets").

## Acceptance criteria

- A host (Victor) in **either** mode can call `provider_models_json("anthropic")` and receive the
  current curated catalog (Fable 5 / Opus 4.8 / Sonnet 5 / …), with no network in FFI mode.
- Victor's per-provider discovery delegates to the Sandhi catalog; the SDK live-discovery path is
  optional/enrichment only.
- The **same** `Catalog` / `KeyStore` / `Budget` / `MeterSink` interfaces back both modes; a
  stateful service in FFI mode is either explicitly lossy-and-documented or backed by an
  injected store.
- No `$`/tier/SKU field exists anywhere in the catalog or governance surfaces (measure-vs-price held).
- ProximaDB (Rust) and AnvaiOps can consume the same catalog/governance surface as Victor (the
  multi-consumer win that justifies the reversal).
- The eight dual-mode asymmetries above are each resolved or explicitly scoped out.

## Required doc amendments (companion changes)

- **`sandhi-providers/src/catalog.rs`** — revise the module doc from "deliberately not a model/capability
  catalog" to "catalog DATA lives in `sandhi-core::MODEL_CATALOG`; catalog POLICY stays with consumers."
- **Victor ADR-018** (L52-53) — change "Victor retains volatile model metadata" to "Sandhi owns model
  catalog *data*; Victor owns catalog *policy* (selection/exposure/UX)."
- **Victor FEP-0020** (L247-249) — same revision; add a § reference to this TD as authoritative for the
  catalog contract.
- **AnvaiOps ADR-0047** — note the catalog-data line moved into the OSS core (still no pricing).

## Open questions

- **Catalog refresh cadence / governance:** who curates `MODEL_CATALOG` between releases, and is there
  a lightweight live-refresh contract for the proxy (cached `/v1/models` pull) without making Sandhi a
  credential-holding aggregator in FFI mode?
- **In-process injected store:** exact interface Victor (and other hosts) use to inject a SQLite store
  into the FFI `Gateway` — env var path, a constructor arg, or a callback trait?
- **Cross-instance budget coordination:** out of MVP, but if multi-replica proxy deployments need
  shared budget state, is that a Postgres/external-store option behind the same `Budget` trait?
- **Policy-as-data schema (Phase D):** does policy reuse a typed `PolicyDoc` in `sandhi-core`, or stay
  consumer-declared (Victor config) and Sandhi only *evaluates* a passed doc?
- **Schema versioning:** `ModelSpecV1` + a `CATALOG_SCHEMA_VERSION`; confirm the byte-pinned JSON
  schema convention (as `ChatRequestV1` uses) applies.
