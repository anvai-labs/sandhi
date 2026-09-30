# TD-0031: Non-interactive client-credentials grant

**Status:** Implemented (this change) · **Mode:** `tokens` (OIDC-mode counterpart noted below)
**Driver:** homelab clients (message-hub triage, victor serve) authenticate to the LAN
gateway with long-lived static virtual keys. Static keys go stale on rotation
(observed 2026-09-29: profile keys outlived the gateway's key store → every
inference call 401/502'd until an operator noticed), and the only non-interactive
issuance path is admin-token-authenticated `POST /admin/keys/share` over SSH —
too much authority for a routine client refresh.

## Design

`POST /auth/token` — OAuth 2.0 client-credentials *shape*:

```
{"client_id": "...", "client_secret": "...", "ttl_seconds": 900?}
→ 200 {"access_token": "vk_…", "token_type": "bearer", "expires_in": 900, "scope": "qwen3-coder-30b …"}
```

- **Tokens are gateway-opaque vkeys**, minted through the same
  `VirtualKeyStore::mint` + live-`KeyStore` path as `/admin/keys/share`. Every
  downstream control (model allowlist, budget scope, rate limit, attribution,
  revocation, restart survival) applies unchanged.
- **Registry**: `SANDHI_CLIENT_CREDENTIALS_FILE` (owner-only JSON array of
  `{client_id, secret_hash (sha256), subject_id, upstream_ref, models,
  budget_scope, rate_limit_per_min, max_ttl_seconds, enabled}`). Static config,
  loaded at startup, **malformed file is fatal** (fail-closed). No new store
  tables; rotation = edit + restart, matching every other static config.
- **No new crypto**: opaque CSPRNG secrets + hash-only persistence +
  constant-time compare. No JWT signing — that would add a new RSA/JWT use
  site under the oidc-rsa-advisory guard. In true OIDC mode the IdP remains
  the token issuer (Kanidm service-account exchange, TD-0029); this endpoint
  is the tokens-mode counterpart.

## Hardening

- Unknown `client_id`, wrong secret, AND an over-limit known client all
  return the **identical** 401 body (silent throttle): response shape never
  reveals whether a client_id is registered, at any attempt count. The
  per-client limiter (10/min, fixed window) caps secret evaluations; a
  successful exchange resets the count so legitimate clients cannot
  self-lockout.
- `ttl_seconds` is clamped to `min(request, entry.max_ttl_seconds, 3600)`;
  `ttl_seconds: 0` is rejected (400).
- **Registry entries require a non-empty model allowlist** (an empty list
  would mint tokens valid for every model — fail-closed at load).
- The registry file must be **owner-only (0600)** and client secrets must be
  CSPRNG-generated (the file stores only the sha256 hash; weak human-chosen
  secrets would be offline-crackable if the file leaks). Lowercase hex is
  normalized at load.
- **OIDC mode is mutually exclusive**: the combination is rejected at startup
  (in OIDC mode every vk_ token routes through delegation metadata a plain
  mint does not write, so tokens would be minted-then-dead).
- Mint requires the registered `upstream_ref` to resolve (provider handle or
  active vault entry), the lifecycle mutation guard, and the vkey store.
- Route sits with `/auth/*` under the no-store layer; never cached.

## Deliberate non-goals

- No refresh tokens: the client re-exchanges (credential is long-lived, token
  is short) — the same model as `delegate_key`, no backchannel state.
- No registry admin API: operators edit the file (gap acknowledged; a future
  admin surface can manage the same store rows).
- OIDC-mode deployments keep `POST /auth/keys` (IdP-authenticated delegation);
  this endpoint activates only when `SANDHI_CLIENT_CREDENTIALS_FILE` is set.

## Client integration

Minted tokens land in the existing per-client artifact shape
(`{gateway.url, gateway.virtual_key, …}` — e.g. `victor-gateway/inferflux.json`),
so `scripts/subscription_service.py victor` and victor's gateway env mode work
unchanged. A launchd/cron loop re-exchanging near expiry gives fully
non-interactive, always-fresh credentials.
