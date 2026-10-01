# ADR-0011: Databricks Model Serving provider — OpenAI-compatible upstream with per-workspace routing

Date: 2026-10-01

## Status

Proposed. Builds on **ADR-0001** (crate layout, wire contract), **ADR-0002** (chat-completion
transport scope), and **ADR-0003** (hand-written adapters; `OpenAiCompat` already covers ~20
OpenAI-compatible providers, usage parsed by `sandhi-core::usage::parse_openai_usage`). This ADR
adds **Databricks Model Serving** (a.k.a. Mosaic AI / Foundation Model APIs) as a first-class
metered upstream. No code lands with this ADR; it is the spec to build against.

## Context

Databricks workspaces expose foundation and custom models through **Model Serving**, which
presents an **OpenAI-compatible** surface:

```
POST https://<workspace-host>/serving-endpoints/<endpoint>/invocations        # native
POST https://<workspace-host>/serving-endpoints/v1/chat/completions           # OpenAI-compat
Authorization: Bearer <PAT | OAuth token>
```

Endpoint names are workspace catalog entries, e.g. `databricks-claude-opus-5-5`,
`databricks-claude-sonnet-5-5`, `databricks-gpt-5-5`, `databricks-gemini-3-8-flash`,
`databricks-bge-large-en`. Responses carry an OpenAI-style `usage` block (`prompt_tokens`,
`completion_tokens`, and for Claude models `prompt_tokens_details.cached_tokens`), so Sandhi's
existing OpenAI usage path already yields a trustworthy token + cache split.

**Why Sandhi should meter it.** Teams run large batch/agentic workloads against Databricks
serving (e.g. prompt optimization with GEPA, and SQL `ai_extract`-equivalent extraction) on a
**shared workspace PAT**, with model-serving capacity shared across jobs. There is today no
per-user/per-job attribution or budget on that shared key — exactly Sandhi's remit. Pointing a
caller's OpenAI base_url (including a GEPA/litellm reflection client) at Sandhi, which forwards to
Databricks serving, meters and attributes every call without changing the caller.

**What works mechanically today.** `OpenAiCompat` takes a configurable `base_url` + bearer (this
is how `config/sandhi.json` wires `ollama`). One can hand-configure
`{ "provider": "openai", "label": "databricks", "base_url": "https://<host>/serving-endpoints/v1" }`
and route. That proves the transport; it does **not** give Databricks a catalog identity, model
routing, capability metadata, per-workspace host configuration, or per-model param admission.

## Decision

Add a `databricks` provider to the catalog that **reuses the `OpenAiCompat` transport and
`parse_openai_usage`**, closing the gaps that keep it from being first-class:

1. **Catalog descriptor (`sandhi-providers::catalog`).** Register canonical slug `databricks`
   (aliases: `mosaic`, `dbrx-serving`) with `ModelEndpointRoute`s for the common serving
   endpoints above. Capability metadata flags OpenAI-compatible chat + embeddings.

2. **Per-workspace `base_url` templating.** The catalog `base_url` is `&'static str`; Databricks'
   host is per-customer (`https://<workspace-host>/serving-endpoints/v1`). Introduce a
   **host-templated base_url** resolved from an instance config field (`workspace_host`) rather
   than a static literal — the one structural change. Model name maps directly to the serving
   endpoint (`model` field == endpoint name); no `/<endpoint>/invocations` path rewriting needed
   when using the `/v1/chat/completions` compat route.

3. **Per-model parameter admission.** Some endpoints reject OpenAI params they do not support —
   e.g. `databricks-claude-sonnet-5-5` returns `BAD_REQUEST: Model … does not support the
   temperature parameter`. The adapter must **drop unsupported params per endpoint** (a small
   per-route deny-set in the descriptor), rather than forwarding them and 400-ing. This is DATA
   on the spec (ADR-0003 philosophy), not code branches.

4. **Auth.** Phase 1: `Authorization: Bearer <PAT>` (works now). Phase 2: Databricks **OAuth
   (M2M service principal)** with token refresh, as a pluggable credential source — PAT is a
   one-time/dev credential; production uses OAuth.

5. **Usage mapping.** Reuse `parse_openai_usage`; verify Databricks' Claude `cached_tokens` lands
   in `ParsedUsage.cache_read_tokens` so the prompt-cache split (ADR-0003 D10) stays correct.
   Databricks does not report `cache_creation_tokens` on the OpenAI route — mark it
   `Estimated`/absent per `usage.v2` `source`, never a fabricated zero.

## Scope / non-goals

- **In:** chat-completions + embeddings over the OpenAI-compat route; metering, attribution,
  budgets, rate limiting via the existing planes.
- **Out:** the Databricks `/invocations` native non-OpenAI payloads (pyfunc/dataframe-split),
  `ai_query`/SQL AI functions (these run in-warehouse, not through an HTTP client Sandhi can
  sit in front of), and foundation-model fine-tuning APIs. Dollar pricing stays downstream
  (ADR-0001): Databricks bills in DBUs; Sandhi emits neutral units only.

## Config example

```json
{
  "providers": [
    { "provider": "databricks", "label": "carlyle",
      "workspace_host": "dbc-xxxxxxxx-xxxx.cloud.databricks.com",
      "auth": { "kind": "pat", "env": "DATABRICKS_TOKEN" } }
  ],
  "bindings": [
    { "upstream": "databricks:carlyle", "subject": "gepa-reflection",
      "group": "prompt-optimization", "rate_limit_per_min": 120 }
  ]
}
```

A GEPA/litellm caller then sets its OpenAI `base_url` to Sandhi's ingress and model to
`databricks-claude-opus-5-5`; Sandhi meters, attributes to `gepa-reflection`, and forwards to the
`carlyle` workspace.

## Open questions

- **Streaming usage.** Confirm Databricks emits a terminal `usage` chunk on SSE streams so
  `metered_passthrough` finalizes real counts (not a byte-derived floor).
- **Embeddings usage shape** for `databricks-bge-large-en` vs the chat usage parser.
- **Endpoint discovery.** Static catalog routes vs. querying the workspace
  `/api/2.0/serving-endpoints` to populate routes dynamically (keeps the catalog fresh as new
  models land, e.g. the fast-moving `databricks-gpt-5-*` / `claude-*` families).

## Consequences

Databricks workloads — including shared-key GEPA prompt optimization and batch extraction —
become metered and attributable with no caller change beyond a base_url. The only structural
addition is per-instance host templating; everything else is catalog data plus a per-route
param deny-set, consistent with ADR-0003.
