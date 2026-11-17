# TD-0033: The SentinelPass vault backend still speaks the legacy, soon-denied grant dialect

- **Status:** Accepted, 2026-11-17. Implementation in progress; do not close until the live
  strict-daemon and executable-pin acceptance gates pass.
- **Relates to:** [TD-0003](TD-0003-operator-surface-keys-budgets-attribution.md) (the vault this backend
  serves), [TD-0027](TD-0027-three-way-origin-codesign.md) (the co-design discipline this follows),
  [TD-0031](TD-0031-client-credentials-grant.md) (the `sandhi.<provider>.<label>` credstore
  convention), and SentinelPass **SP-1 / ADR-014** (exact-entry service grants), **SP-3 / ADR-016**
  (executable pinning), **SP-4 / ADR-017** (OpenPGP fingerprint enrollment), plus its
  **TD-SEC-10** (same-UID retrieval boundary).

## Why this exists

`SentinelPassIpcVault` is the default vault backend (`SANDHI_VAULT_BACKEND=sentinelpass`). It
retrieves every upstream credential over one IPC message:

```rust
// crates/sandhi-store/src/vault/ipc.rs:104, 171
fn domain(provider: &str, label: &str) -> String { format!("sandhi:{provider}:{label}") }
IpcMessage::GetExternalSecret { client_id, domain, field: Password, purpose: Some("sandhi-vault") }
```

That is SentinelPass's **legacy external-secret surface**. Its own daemon now refuses it under the
strict administrative profile and names the replacement in the error text:

```
// sentinelpass-core/src/daemon/ipc/server.rs:1319
"SP-1 legacy external-secret retrieval denied under strict profile"
"strict profile: legacy external-secret grants are disabled on this daemon;
 use exact-entry service grants"
```

So this is a co-design gap in the TD-0027 sense: the capability exists on one side with no
counterpart on the other. Three consequences, one of them already observed.

**G33a — a strict daemon fail-closes every upstream at once.** Sandhi holds no exact-entry client,
so on a daemon with the strict profile enabled, *all* vault-backed credentials stop resolving
together. There is no per-credential migration path and no startup-time warning that the backend is
speaking a dialect the daemon is about to stop answering.

**G33b — a reference miss does not say which backend looked, or where.** Backend selection is
invisible in the operator surface, and each backend resolves a reference by a different rule. The
credstore backend searches three filenames in its directory — `<provider>__<label>`,
`<provider>.<label>`, `sandhi.<provider>.<label>` (`crates/sandhi-store/src/vault.rs:275`) — while
the IPC backend asks the daemon for a synthesized domain, which the daemon resolves by
`normalize_host`, an indexed `find_entries_by_domain`, then a full scan comparing
`domains_match(domain, entry.url)` (`sentinelpass-core/src/daemon/vault_state.rs:178`). A service
key such as `sandhi:openai:codingplan` is not a hostname, so under the IPC path a match depends on
URL-parsing behaviour rather than on an identifier either side agreed to.

*Observed 2026-11-06.* Adding an `openai:codingplan` upstream to a running gateway returned
`501 vault_unsupported` on `POST /admin/keys` and then `404 vault_missing`
("No credential at the authorized reference", `crates/sandhi-proxy/src/operator.rs:585`) on
`POST /admin/keys/reference`. Provisioning the secret in SentinelPass and granting the `sandhi`
client (`sandhi | sandhi:openai:codingplan | password | required`) changed nothing, and
`sentinelpass secret audit` recorded **no lookup event at all** — because this deployment runs the
**credstore** backend, whose refusal text names the real provisioning path
(`sentinelpass service-credential install`) while the error the operator sees does not. Every
visible signal pointed at the daemon; the daemon was never involved. An operator cannot tell from
`/admin/keys`, from the 404, or from the audit log which backend is active or which name it looked
for.

**G33c — Sandhi declines ACLs SentinelPass already offers.** `ServiceGrantCreate`
(`sentinelpass-protocol/src/service.rs:410`) carries `required_exe_sha256` (SP-3/ADR-016),
`registration_key_fingerprint` (SP-4/ADR-017) and `expires_at`. Sandhi pins the protocol crate that
defines them (`crates/sandhi-store/Cargo.toml:29`, `sentinelpass-protocol = "=0.19.0"`) and uses
none. (Verified against the deployed daemon, `sentinelpass 0.19.1`, which matches that pin; the
`../sentinelpass` working checkout read while writing this TD was workspace `0.17.0`, where these
contracts already exist, so the claims hold across both.) This matters most where SentinelPass is weakest by its own record: TD-SEC-10 states grants are
**same-UID only**, and names exe pinning as "the current per-binary mitigation on Linux". Sandhi
publishes release binaries whose digests are known at release time, so it is the party that *can*
supply that pin — and today does not, leaving any same-UID process with a stolen token able to read
every upstream key.

## Design

**D1 — a credential reference may bind an exact entry.** The vault metadata row for
`provider:label` gains an optional `entry_id` plus the service-grant token reference. Resolution
prefers the exact binding; the synthesized `sandhi:<provider>:<label>` domain becomes a fallback,
not the only name.

**D2 — prefer `ServiceGetSecret`, and make the fallback observable.** `get_secret` issues
`ServiceGetSecret { client_id, entry_id, field, token }` when a binding exists. The legacy
`GetExternalSecret` remains only for references with no binding, and taking it emits a counted,
logged deprecation event. A silent downgrade is what turned G33b into an unexplainable 404.

**D3 — distinguish the strict denial.** A strict-profile refusal maps to its own code
(`vault_legacy_denied`, 409) instead of today's generic failure mapping
(`crates/sandhi-proxy/src/operator.rs:480`), so the operator reads the cause rather than a symptom.

**D4 — report the backend and the ACL class.** `GET /admin/keys` states which backend is active
and, per credential, whether it resolves by exact entry or legacy domain, and whether it is
exe-pinned, fingerprint-enrolled or expiring. A `vault_missing` response names the backend and the
reference it searched. Metadata only; never the secret. Today an operator can tell none of this,
which is what made G33b unreadable.

**D5 — pin the proxy binary.** Release automation (TD-0023) publishes the `sandhi-proxy` SHA-256 so
an operator can mint grants with `--exe-sha256`, converting TD-SEC-10's documented residual risk
into a configuration an operator can actually apply.

**D6 — admit the provisioning ceremony honestly, including the identity split.**
`ServiceGrantCreate` is step-up by design: an operator types the master password and the token is
shown once. Worse, provisioning a credstore credential cannot be done by either identity alone.
Retrieval must come from the owner's uid, because the daemon refuses uid 0 (the same-UID boundary
of TD-SEC-10) — observed 2026-11-06: as root the CLI reports "No reachable SentinelPass daemon",
as the owner the same call reaches the vault. Encryption must come from root, because
`systemd-creds` reads `/var/lib/systemd/credential.secret` (mode `-r--------`), and the final
`/etc/credstore.encrypted` write plus the `LoadCredentialEncrypted=` drop-in are root-owned too.
On a host without a TPM, `--protection tpm2` is unavailable, so the only non-interactive path is to
run the install as the owner and delegate `--systemd-creds` to a root wrapper, keeping the
plaintext inside the stdin pipe the CLI designed for it. `docs/operator/` must carry that recipe
verbatim; a reader who tries either identity alone will conclude the credential cannot be
provisioned at all.

The counterpart ask on the SentinelPass side — auditing a domain miss, and warning when a grant
names a domain no entry resolves to — is filed there, not here.

## Non-goals

Write support is **not** uniform and this TD does not make it so. The IPC backend does implement
`set_secret` by sending `SaveSecret` (`crates/sandhi-store/src/vault/ipc.rs:195`), which needs a
write grant the `sandhi` client is not given today; credstore
(`crates/sandhi-store/src/vault.rs:358`) and the CLI fallback (`:447`) refuse writes outright and
name their provisioning paths. Provisioning stays SentinelPass's job in every case. Only the IPC
backend gains the exact-entry retrieval path; credstore and keychain keep their current
resolution.

## Implementation state (2026-11-17)

The Sandhi slice implements D1–D3 and the inventory-projection half of D4:

- Vault metadata persists the optional exact `sentinelpass_entry_id` plus
  `sentinelpass_token_env` pair, never the service token.
- Exact retrieval sends protocol 0.20 `VaultOp::ServiceGetSecret` with the configured client ID,
  exact entry, password field, and token read from the named process environment.
- Unbound references retain the legacy domain request. A successful legacy retrieval is counted
  and warned once per reference; the pinned strict-profile diagnostic maps to HTTP 409
  `vault_legacy_denied`.
- Reference registration, CLI, dashboard inventory, and missing-reference errors expose the
  active backend and exact names searched without contacting the daemon.
- Exact references can carry the non-secret ACL class declared from the SentinelPass grant
  report. Inventory reports executable pinning, enrollment and expiry (or an explicit
  `unknown_service_grant` state). Protocol 0.20 has no read-only grant inspection, so this is
  operator-declared projection metadata, not a daemon-verified claim.
- Backend mismatch and partial bindings fail closed. A direct secret write clears an exact binding
  on the next metadata generation.

Remaining before TD closure: actual miss coverage across every deployed backend family (the
current tests cover backend/reference plans plus native IPC behavior), daemon-verified ACL state,
live strict-daemon qualification, deployed keychain parity, and an end-to-end grant pinned to the
published proxy digest. The public Rust vault API grows fields and changes
`VaultStore::register_metadata`; this is deliberately planned as the breaking Sandhi 0.21.0
source release rather than a 0.20.1 patch.

## Acceptance

1. A daemon with the strict profile serves Sandhi's upstreams through the exact-entry path, and a
   reference with no binding is refused with `vault_legacy_denied` rather than a generic failure.
2. A legacy fallback retrieval increments a counter and logs once per reference.
3. `GET /admin/keys` reports the ACL class of every credential.
4. Backend parity tests cover ipc-exact, ipc-legacy, credstore and keychain against the same
   reference set, and a miss in each reports the active backend and the searched reference.
5. A release publishes the proxy digest, and an exe-pinned grant is verified end to end against the
   published value.
