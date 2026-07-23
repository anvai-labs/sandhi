# TD-0004: Model catalog + unified governance core (dual-mode)

- **Status:** Proposed (draft for review)
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

## Phasing (each default-off / non-breaking; ships behind the next Sandhi minor)

- **Phase A — Catalog data (the reversal, smallest + highest clarity):** add `MODEL_CATALOG` +
  `ModelSpecV1` to `sandhi-core`; expose `provider_models_json` in both bindings + `GET /catalog/models`
  in the proxy; seed with the current Anthropic/OpenAI/Gemini lineups; amend `catalog.rs`, Victor
  ADR-018 (L52-53), FEP-0020 (L247-249). Victor `list_models` consumes it (#632 SDK path → fallback).
- **Phase B — Typed-interface unification (no behavior change):** define the trait contracts above;
  refactor the existing `KeyStore`/`BudgetLedger`/`MeterSink`/`MeteredProvider` behind them; wire the
  in-memory (FFI) and SQLite (proxy) impls; converge the two metering paths into one.
- **Phase C — Close the dual-mode gaps:** durable budgets (the unimplemented TD-0003 `budget` table);
  hash-capable in-process vkey resolution; **enforce** rate limits; the in-process governance surface
  via an **injected-store** option; align CLI vs binding transports.
- **Phase D — Policy-as-data:** declarative stateless policy evaluation (model allowlists, deterministic
  guardrails) running identically in both modes; stateful/LLM-judge enforcement stays proxy-side.

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
