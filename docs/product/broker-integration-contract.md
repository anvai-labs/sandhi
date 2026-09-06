# Broker integration and onboarding contract

Status: W04 complete in the working tree; live broker/joint release certification pending.
Date: 2026-09-05. Tracker: [TD-0026](../td/TD-0026-gateway-product-evolution.md).
Native wire dependency: `sentinelpass-protocol` 0.8.1. No new broker messages or grants are assumed.

## Outcome and ownership

An operator can register an existing provider credential using a read grant, distinguish a
locked vault from rejected access or a missing secret, and disable local dispatch without
mistaking it for upstream revocation. Sandhi owns registration and cached provider handles;
the broker owns custody and grant authorization. No model call, unlock or grant creation is
performed during reference registration. Provider-side credential validity is not tested.

## Runtime and admission

The native adapter owns one dedicated thread. Its Tokio runtime is created, entered and dropped
only on that thread, never on an async request worker. A synchronous `Vault` call still waits
for its reply; async embedders must offload the whole operation. The gateway does so for
credential mutations, holding a single-writer permit through storage and handle publication.
Excess mutations return `503 vault_busy` without a waiting queue or broker dispatch. Caller
disconnect does not release the permit while its blocking task is still running.

The adapter independently permits one active IPC operation plus 16 queued calls. Queue admission
is nonblocking. Each request gets one deadline covering queue residence and IPC; expired queued
requests never dispatch. The default is 5 seconds, configurable with
`SANDHI_SENTINELPASS_TIMEOUT_MS` in the range 50–30000. Timeout drops the transport future and
does not retry. Dropping the last adapter sender closes its worker after pending bounded I/O;
there is no blocking join on the caller. Runtime initialization has a one-second handshake.
OS thread scheduling and local token-file/SQLite I/O are not hard-real-time guarantees.

A dispatched write that times out or loses its response **may have applied in the broker**.
The gateway reports `reconcile_before_retry: true`, does not publish new local metadata/handles
on that failure, and does not retry automatically. Check the broker before issuing another write.
Broker save, SQLite metadata and provider-handle publication are not a distributed transaction;
an error after broker save can leave secret material changed but local registration incomplete.
An already-admitted mutation can finish after its HTTP caller disconnects. W05 owns durable
mutation/attempt evidence; W09 owns credential generations and bounded revocation cutoff.

## Backend support is not authorization

Authenticated `GET /admin/version` adds `capabilities.vault` with `backend`, `operations`,
`reference_registration`, `grant_status: "not_checked"` and `credential_generations: false`.
These are local implementation capabilities, not daemon negotiation, live health or a grant
probe. Every native operation still presents the client token and exact reference to the broker.

| Backend | Read | Write | Delete | Bounded external I/O | Interactive reference registration |
|---|---|---|---|---|---|
| Native IPC | Yes, subject to grant | Yes, subject to a write grant | No | Yes | Yes, read grant only |
| Explicit legacy CLI | Yes, legacy behavior | No | No | No | Disabled |
| OS keyring | Yes | Yes | Yes | No adapter deadline | Yes |
| In-memory test backend | Yes | Yes | Yes | No external I/O | Yes |
| Unavailable / invalid configuration | No | No | No | No | No |

`SANDHI_VAULT_BACKEND=sentinelpass` requires the IPC feature, a nonempty
`SENTINELPASS_CLIENT_TOKEN`, and the daemon-token file. Missing configuration preserves an
unavailable vault; it does not select the keyring or a tokenless/CLI path. Unknown backend names
also become unavailable. Existing unrelated provider configuration may still let the gateway
serve; this is not a global startup-abort or readiness guarantee. CLI compatibility requires
the explicit `SANDHI_SENTINELPASS_FALLBACK_CLI=1` switch. Its legacy startup reads remain
unbounded and are not recommended for unattended deployment. No access failure auto-unlocks a vault.

Rust embedders must handle the fallible native constructor; the panic-prone `Default` fallback
was removed. Custom `Vault` implementations must explicitly advertise write/delete support;
the trait default does not assume these capabilities. No core/chat wire-version change is needed.

## Onboarding

Provision the secret and its exact read grant through the broker's supported setup flow, outside
Sandhi. Keep routine runtime authority read-only. Enable a separate write grant only for an
explicit provisioning workflow; this implementation does not mint or automatically switch grants.

```sh
# Uses SANDHI_ADMIN_URL and SANDHI_ADMIN_TOKEN; no secret argument or stdin read.
sandhi keys reference openai default
```

Equivalent API: `POST /admin/keys/reference` with
`{"provider":"openai","label":"default","scheme":"api_key"}`. Optional `base_url` remains
privileged operator configuration. Unknown fields (including `secret`) are rejected. The
dashboard's “Existing reference (read-only)” mode disables and clears the secret input.
Credential schemes accept case-insensitive `api_key`/`api-key`, `bearer` and `oauth`, with
`api_key` as the omitted default. Unknown schemes fail before broker access; inventory uses
canonical underscore/lowercase spellings.

The reference maps to `sandhi:openai:default`, field `password`, with the configured client ID.
New provider/label components must be 1–128 lowercase ASCII letters, digits, dots, underscores
or hyphens, without a trailing dot. Wildcards, whitespace, separators and case-normalization
collisions are rejected, not silently rewritten. Existing noncanonical native references need
explicit review/migration; do not relabel their grants automatically. Provider aliases are not
silently merged. At restart, each active label resolves its own secret, not the first label for
that provider. Resolved material remains in cached handles; it is not persisted in SQLite.

## Failure and offboarding semantics

| Outcome | HTTP / code | Meaning and recovery |
|---|---|---|
| Missing/wrong admin credential | Existing admin 401/403 | No broker access |
| Mutation busy | 503 / `vault_busy` | Not admitted; retry only with current operator intent |
| Native configuration unavailable | 503 / `vault_configuration` | Inspect feature, selected backend and both token requirements |
| Locked | 423 / `vault_locked` | Unlock through the broker; then retry explicitly |
| Broker denies read or rejects save | 403 / `vault_denied` | Check exact client/domain/field and read/write grant; no plaintext fallback |
| Authorized reference has no value | 404 / `vault_missing` | Provision or correct the reference; no metadata registered |
| Unsupported operation | 501 / `vault_unsupported` | Consult capabilities; do not infer success |
| Deadline exceeded | 504 / `vault_timeout` | A write may have applied; reconcile before retry |
| Transport/protocol/storage failure | 503 / `vault_unavailable` | Details redacted; do not present an empty success |

Protocol 0.8.1 save errors are not fully typed: a rejected save is not proof of which grant,
storage or policy check failed. Sandhi does not parse free-form error strings to invent a reason.
Arbitrary daemon error text is never returned to the admin client or logged by this adapter.
Config apply returns 503 for incomplete application; each provider failure preserves its
underlying status and canonical `code`, `reconcile_before_retry`, `metadata_committed` and
`credential_id` when present. It does not forward arbitrary broker error text. A generic
partial-apply result must not be mistaken for permission to retry an ambiguous secret write.

`DELETE /admin/keys/{provider}/{label}` commits local revocation first and removes the cached
handle. Its response separates `revoked` from `secret_deletion` (`deleted`, `missing`, `failed`,
`unsupported`, `not_attempted`), and explicitly returns `broker_grant_revoked: false` and
`provider_key_revoked: false`. Native/CLI broker backends receive no unsupported delete request.
Failed local commit does not delete the secret. The UI calls this local disablement.
Already-dispatched work is not undone; other cached handles or provider credentials are not
revoked. Secret cleanup failures require separate operator reconciliation.

## Verification and joint gates

Rust regressions exercise construction/use/drop inside an async runtime, fixed queue capacity,
expired work without dispatch, unsupported deletion, normalization collisions, failed local
revocation and exact-label restart. Real-proxy tests use a disposable Unix socket daemon,
disposable daemon/client tokens, SQLite faults, timeout/overlap, rejected grants and browser
reference onboarding. Fake grants test the gateway boundary, not the real daemon's authorization.

No user vault, provider account or sibling source was changed. Windows named-pipe behavior,
minimum/latest real-daemon compatibility, actual grant expiry/revocation and broker security
certification remain SP4/joint gates. AgentBrowser's existing real-service/Chromium gateway smoke
remains in the suite; its destination/action-bound secret resolver remains AB03, not implemented
by this native adapter. See the [three-way design](../upstream/browser-gateway-vault-codesign.md).
