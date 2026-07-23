# TD-0003: Operator surface — keys, virtual keys, budgets, attribution

- **Status:** Draft (proposed)
- **Date:** 2026-07-22
- **Related:** TD-0002 (typed provider runtime), ADR-0047 D3 (Sandhi measures; the commercial layer prices), FEP-0020 (Sandhi↔Victor integration)

## Context

The typed-provider migration (TD-0002) made Sandhi own transport and metering: every admitted call flows through the proxy (or the in-process FFI) and emits a neutral `UsageV2` event attributed to a virtual key / subject / group / provider / session. The **backend pieces already exist** — the virtual-key store, the budget ledger (`sandhi-core/budget.rs`), usage events (`event.rs` / `usage.rs`), a `SqliteStore` with `totals_by_{subject,group,provider}` + `grand_total`, the proxy's bearer-virtual-key auth + per-request budget check, and a basic `/dashboard`.

What is **missing** is the *operator surface*: there is no CLI, no key vault, no admin API, and therefore no way to provision provider credentials, share virtual keys, set budgets, or query attribution. TD-0003 was an 8-line CLI sketch; this document fleshes it into the design that the subsequent build phases implement.

## Design boundary (load-bearing)

**Sandhi measures in neutral tokens and attributes; it does not price.** Dollars / SKU / tier pricing is a *downstream commercial layer* (AnvaiOps, ADR-0047 D3) that consumes Sandhi's usage stream. This is already the stated contract of `sandhi-core` (`budget.rs`: "neutral tokens, not dollars"; `event.rs`: "no dollars, no tier/SKU names"). Therefore:

- **In scope:** key vault, virtual keys, token budgets + enforcement + alerts, token attribution (by key / user / group / provider / session / model), and the operator CLI + admin REST API + dashboard to operate all of it.
- **Out of scope:** $ / token pricing, invoicing, tier/SKU naming. Sandhi exposes the *attributed token usage* the commercial layer prices over; it never stores or computes dollars.

## What exists today (build on this)

| Capability | Where |
|---|---|
| Budget ledger — `spent(scope)`, check-and-reserve, neutral tokens, block policy | `sandhi-core/budget.rs` |
| Usage event — `subject_id`, `group_id`, `virtual_key_id`, `provider`, `model`, `session_id`, `UsageV2` (fresh/cache-read/cache-write/reasoning tokens), `Backend` cost basis | `sandhi-core/event.rs`, `usage.rs` |
| Durable store — `totals_by_subject/group/provider`, `grand_total` (token buckets) | `sandhi-store` (`SqliteStore`) |
| Proxy — virtual-key store, budget ledger, usage sink (emit per call), bearer-vk auth, per-request budget check, `/dashboard` + `/dashboard/api/usage` | `sandhi-proxy` |
| Gateway FFI API — `add_virtual_key`, `set_budget`, `check_budget`, `meter`, `events`, `register_parser`, `meter_tokens` | `bindings/{python,node}` |

## Components to add

### 1. Key vault (provider credentials)
Sandhi stores real upstream provider credentials (API key, or OAuth refresh token) in a **proper secret store**, never as plaintext in the SQLite database. `sandhi keys add` provisions them; raw provider keys never leave Sandhi — clients only ever receive virtual keys. The vault resolves *provider → real credential* for the proxy's upstream calls.

The concern is split:
- **Metadata** (provider, label, scheme, base_url, created_at, status) is durably indexed in the `sandhi-store` `vault` SQLite table, so `list()` returns masked metadata without touching the secret backend.
- **Secrets** are held by a pluggable `Vault` backend:
  - `KeyringVault` (default) — the OS keychain via the Rust `keyring` crate (macOS Keychain / Linux Secret Service / Windows Credential Manager), service `sandhi`, account `<provider>:<label>`.
  - `SentinelPassVault` — reads from the SentinelPass password manager over its CLI (`sentinelpass secret get …`), keeping the coupling loose (no `sentinelpass-core` path dependency). Read-only for now (the CLI exposes no write); native daemon IPC is a follow-up.
  - `InMemoryVault` — process-local, for tests/demos.

Selection is via `SANDHI_VAULT_BACKEND=keyring|sentinelpass` (default `keyring`). (An earlier draft proposed an AES/GCM `secret_blob` + `SANDHI_VAULT_KEY`; that was replaced by the OS keychain model — secrets belong in a real secret store, not a hand-rolled encrypted column.)

### 2. Virtual keys (sharing + scoping)
`sandhi keys share` mints a virtual key (`vk_…`) bound to: a **subject** (user), a **group**, an upstream **provider/model allowlist**, a **budget scope**, an **expiry**, and an optional **rate limit**. The key is printed **once** and only a hash is stored (lookup, like the bearer-vk auth today) — never the plaintext. A vk presented to the proxy selects the real upstream credential (from the vault) and is the unit of attribution and budget enforcement.

### 3. Budgets (enforcement + alerts)
`sandhi budget set <scope> <limit> [--window daily|monthly|total] [--policy block|warn]` — scope is `user:` / `group:` / `key:`; limit is in **neutral tokens**. Enforcement is per-request reserve-then-reconcile (extend `budget.rs` with windows, the `warn` policy, and reservation against the projected max so concurrent calls can't overspend). **Alerts**: threshold rules (e.g. 80% of a window) emit an alert event / webhook / log — a new alert-rule config + a notifier (webhook + log initially).

### 4. Attribution + usage query
`sandhi usage --by key|user|group|provider|session|model [--since] [--until] [--format table|json]` aggregates the usage events. The attribution dimensions already ride on every event (`subject/group/vk/provider/session/model`); this adds the **query/aggregation surface** (extend the store with `totals_by_key/session/model` + time-windowed queries).

### 5. Operator CLI (`sandhi`)
A single `sandhi` artifact (Rust binary in `sandhi-proxy`, with a thin Python console-script shim in the binding) that drives the admin API:

```
sandhi keys add <provider> [--scheme api-key|bearer|oauth]   # prompts; never echoes the raw key
sandhi keys list | mask | revoke <id>
sandhi keys share --user X --budget N --models m1,m2 [--expires ...] [--rate ...]
        # prints the virtual key + endpoint once
sandhi budget set <scope> <limit> [--window] [--policy block|warn]
sandhi budget list | usage <scope>
sandhi usage --by key|user|session|model [--since ...] [--format table|json]
sandhi alerts list | ack <id>
```

### 6. Admin REST API (on the proxy)
New routes the CLI, dashboard, and external automation drive — authed by an **admin token** (distinct from virtual keys):

- `POST /admin/keys` (add provider key), `GET /admin/keys`, `DELETE /admin/keys/{id}`, `POST /admin/keys/share` (mint vk)
- `POST /admin/budget` (set), `GET /admin/budget`, `GET /admin/budget/{scope}/usage`
- `GET /admin/usage?by=…&since=…`
- `GET /admin/alerts`

### 7. Victor virtual-key mode
Victor (and any client) can present a Sandhi virtual key instead of a raw provider key: configure a provider entry with `base_url = <sandhi proxy URL>` + `api_key = vk_…`. Traffic then flows through the proxy → **central attribution + budget enforcement**. The existing in-process FFI path remains for single-user/local use; the proxy path is for multi-user / shared-key / attributed deployments. A per-provider setting selects `direct` (FFI, today) vs `gateway` (proxy + vk).

### 8. Dashboard UX
Extend `/dashboard` to surface: virtual keys (list / masked), budgets (scopes, spent-vs-limit, policy), attribution breakdown (by user/group/provider/model), and alerts. Units stay **neutral tokens**; dollars remain the commercial layer's concern.

## Data model (additions)

- `vault(provider, label, scheme, base_url, created_at, status)` — **non-secret metadata only**; the secret lives in the active `Vault` backend (keychain / SentinelPass), keyed `service=sandhi, account=<provider>:<label>`.
- `virtual_key(id, subject, group, upstream, models[], budget_scope, expires_at, rate_limit, secret_hash, created_at, revoked_at)`
- `budget(scope, limit_tokens, window, policy, window_spent, window_reset_at)`
- `alert(rule_id, scope, threshold_pct, channel, last_fired_at)`
- `usage_event(...)` — **existing**; no schema change.

## Security

- Provider keys held in the OS keychain (`keyring`) by default, or SentinelPass; only non-secret metadata is in SQLite. KMS / hosted-secret-manager integration is a follow-up at the commercial layer (AnvaiOps).
- Virtual keys: store only a hash for lookup; print once; revoke by id; expire.
- Admin API: separate admin token (never a virtual key); require TLS in deployments.
- Append-only audit log for all key / budget / alert mutations.

## Phasing

- **P1 — Operator core:** key vault + virtual-key share/revoke + admin API + the `sandhi` CLI (`keys` / `budget` / `usage`) over the existing backend. This is the TD-0003 surface made real.
- **P2 — Budget depth + alerts:** budget windows, `warn` policy, reservation; threshold alerts (webhook/log).
- **P3 — Victor `gateway` mode:** per-provider `direct` vs `gateway` (proxy + vk) routing + attribution end-to-end.
- **P4 — Dashboard + queries:** keys/budgets/attribution/alerts views; time-windowed + by-key/session/model usage queries.

(Pricing remains the commercial layer's job — out of Sandhi.)

## Acceptance criteria

- An operator can: add a provider key (vault), share a virtual key with a token budget to a user, and that user's client (Victor in `gateway` mode) hits the proxy with the vk, is attributed and budget-enforced, and the operator sees usage by user/key plus an alert at 80%.
- Raw provider keys never leave Sandhi; only `vk_…` values are shared, and only once.
- No dollars are stored or computed in Sandhi (the measure-vs-price boundary is held).

## Open questions

- **CLI host:** resolved — a single Rust `sandhi` binary (`crates/sandhi-proxy/src/cli.rs`) is a thin HTTP client to the admin API; no direct DB access, so it drives any running proxy. A thin Python console-script shim can wrap it later.
- **Vault encryption:** resolved — secrets live in the OS keychain (`keyring`) by default, or SentinelPass; no hand-rolled encrypted blob. KMS / hosted-secret-manager is a follow-up at the commercial layer (AnvaiOps).
- **SentinelPass IPC:** P1 shells out to the `sentinelpass secret get` CLI (loose coupling, no `sentinelpass-core` path dep); native daemon IPC (socket + `IpcMessage`) is a follow-up once a narrow contract is promoted.
- **Alert channels:** webhook + log first; Slack/email later?
- **Tenancy:** is one Sandhi deployment one tenant, or does it need tenant scoping (likely an AnvaiOps concern)?
