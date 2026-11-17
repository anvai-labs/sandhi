//! Provider credential vault (TD-0003 P1, component 1).
//!
//! Raw upstream provider credentials (API keys / OAuth tokens) live in a **proper secret store**,
//! never as plaintext in the SQLite database. This module splits the concern:
//!
//! - **Metadata** (which providers/labels exist, scheme, base_url, created_at, status) is durably
//!   indexed in the `vault` SQLite table here, so `list()` returns masked metadata without ever
//!   touching the secret backend.
//! - **Secrets** are held by a [`Vault`] backend implementation. The default is [`KeyringVault`]
//!   (the OS keychain via the `keyring` crate — macOS Keychain / Linux Secret Service / Windows
//!   Credential Manager); [`SentinelPassIpcVault`] uses the native daemon protocol when the
//!   `sentinelpass-ipc` feature is enabled, with [`SentinelPassVault`] retained as an explicit CLI
//!   fallback. [`InMemoryVault`] backs tests.
//!
//! Selection is via `SANDHI_VAULT_BACKEND=keyring|sentinelpass|credstore` (default `keyring`). The
//! measure-vs-price boundary is held: no dollars, no SKU/tier — only credentials + neutral token
//! attribution.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// How the upstream authenticates. Stored as vault metadata (non-secret).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialScheme {
    /// Static API key sent as a bearer / `x-api-key`.
    ApiKey,
    /// OAuth bearer token (refresh handled out of band).
    Bearer,
    /// Delegated credential requiring an implemented endpoint lease profile.
    /// Current gateway renewal is owned outside the provider runtime.
    Oauth,
}

impl CredentialScheme {
    fn as_db(&self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::Bearer => "bearer",
            Self::Oauth => "oauth",
        }
    }

    fn from_db(s: &str) -> Self {
        match s {
            "bearer" => Self::Bearer,
            "oauth" => Self::Oauth,
            _ => Self::ApiKey,
        }
    }
}

/// Non-secret metadata for one stored credential. Returned by `list()` (masked).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VaultEntry {
    pub provider: String,
    pub label: String,
    pub scheme: CredentialScheme,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Per-endpoint User-Agent override (agent-allowlisting upstreams).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Exact SentinelPass entry binding. The service-grant token itself is never metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sentinelpass_entry_id: Option<i64>,
    /// Environment variable holding the exact-entry service-grant token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sentinelpass_token_env: Option<String>,
    /// Operator-declared, non-secret ACL facts from the SentinelPass grant
    /// ceremony. Protocol 0.20 has no read-only grant-inspection operation,
    /// so these are projection metadata rather than daemon-verified state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sentinelpass_grant_acl: Option<SentinelPassGrantAcl>,
    pub created_at: String,
    /// Monotonic local metadata generation. Registration/rotation advances it;
    /// an unchanged reference is not proof of unchanged account authority.
    pub generation: u64,
    /// `active` / `revoked`.
    pub status: String,
}

/// A non-secret exact SentinelPass service-grant binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SentinelPassExactBinding {
    pub entry_id: i64,
    pub token_env: String,
}

/// Non-secret SentinelPass grant policy declared when an exact reference is
/// registered. SentinelPass owns and enforces the grant; Sandhi projects the
/// class selected by the operator and never stores the service token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SentinelPassGrantAcl {
    pub exe_pinned: bool,
    pub fingerprint_enrolled: bool,
    /// Unix seconds; `None` means the grant was minted without expiry.
    pub expires_at: Option<i64>,
}

impl SentinelPassGrantAcl {
    fn to_db(self) -> (i64, i64, Option<i64>) {
        (
            i64::from(self.exe_pinned),
            i64::from(self.fingerprint_enrolled),
            self.expires_at,
        )
    }
}

impl SentinelPassExactBinding {
    pub fn new(entry_id: i64, token_env: &str) -> Result<Self, VaultError> {
        validate_exact_binding(entry_id, token_env)?;
        Ok(Self {
            entry_id,
            token_env: token_env.to_owned(),
        })
    }
}

pub fn validate_exact_binding(entry_id: i64, token_env: &str) -> Result<(), VaultError> {
    if entry_id <= 0 {
        return Err(VaultError::InvalidReference);
    }
    let valid_env = !token_env.is_empty()
        && token_env.len() <= 128
        && token_env.bytes().next().is_some_and(|byte| {
            byte.is_ascii_uppercase() || byte.is_ascii_lowercase() || byte == b'_'
        })
        && token_env.bytes().all(|byte| {
            byte.is_ascii_uppercase()
                || byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || byte == b'_'
        });
    if !valid_env || token_env == "SENTINELPASS_CLIENT_TOKEN" {
        return Err(VaultError::InvalidReference);
    }
    Ok(())
}

impl VaultEntry {
    /// The stable credential id under which a provider handle is registered: `provider:label`.
    pub fn credential_id(&self) -> String {
        format!("{}:{}", self.provider, self.label)
    }

    /// Validate the optional persisted binding as one complete, non-secret pair.
    pub fn exact_binding(&self) -> Result<Option<SentinelPassExactBinding>, VaultError> {
        match (
            self.sentinelpass_entry_id,
            self.sentinelpass_token_env.as_deref(),
        ) {
            (Some(entry_id), Some(token_env)) => {
                Ok(Some(SentinelPassExactBinding::new(entry_id, token_env)?))
            }
            (None, None) => Ok(None),
            (Some(_), None) | (None, Some(_)) => Err(VaultError::InvalidReference),
        }
    }

    /// Fail closed if ACL projection is present without its exact binding.
    pub fn grant_acl(&self) -> Result<Option<SentinelPassGrantAcl>, VaultError> {
        match (self.exact_binding()?, self.sentinelpass_grant_acl) {
            (Some(_), acl) => Ok(acl),
            (None, None) => Ok(None),
            (None, Some(_)) => Err(VaultError::InvalidReference),
        }
    }

    /// A masked hint for display, e.g. `sk-…Q12` (never the full secret).
    pub fn masked_secret_hint(secret: &str) -> String {
        let len = secret.chars().count();
        if len <= 8 {
            "…".into()
        } else {
            let head: String = secret.chars().take(3).collect();
            let tail: String = secret.chars().skip(len.saturating_sub(3)).collect();
            format!("{head}…{tail}")
        }
    }
}

fn read_generation(row: &rusqlite::Row<'_>) -> rusqlite::Result<u64> {
    let value: i64 = row.get(7)?;
    if value <= 0 {
        return Err(rusqlite::Error::IntegralValueOutOfRange(7, value));
    }
    Ok(value as u64)
}

fn invalid_vault_metadata(column: &str) -> rusqlite::Error {
    rusqlite::Error::InvalidColumnType(8, column.to_string(), rusqlite::types::Type::Null)
}

fn read_vault_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<VaultEntry> {
    let sentinelpass_entry_id: Option<i64> = row.get(8)?;
    let sentinelpass_token_env: Option<String> = row.get(9)?;
    let exact = match (sentinelpass_entry_id, sentinelpass_token_env.as_deref()) {
        (Some(entry_id), Some(token_env)) => Some(
            SentinelPassExactBinding::new(entry_id, token_env)
                .map_err(|_| invalid_vault_metadata("sentinelpass exact-entry binding"))?,
        ),
        (None, None) => None,
        (Some(_), None) | (None, Some(_)) => {
            return Err(invalid_vault_metadata("sentinelpass exact-entry binding"));
        }
    };
    let exe_pinned: Option<i64> = row.get(10)?;
    let fingerprint_enrolled: Option<i64> = row.get(11)?;
    let expires_at: Option<i64> = row.get(12)?;
    let sentinelpass_grant_acl = match (exact, exe_pinned, fingerprint_enrolled) {
        (None, None, None) if expires_at.is_none() => None,
        (None, _, _) => {
            return Err(invalid_vault_metadata(
                "sentinelpass grant ACL without exact entry",
            ));
        }
        (Some(_), None, None) if expires_at.is_none() => None,
        (Some(_), Some(exe_pinned), Some(fingerprint_enrolled)) => Some(SentinelPassGrantAcl {
            exe_pinned: exe_pinned != 0,
            fingerprint_enrolled: fingerprint_enrolled != 0,
            expires_at,
        }),
        (Some(_), _, _) => {
            return Err(invalid_vault_metadata("partial sentinelpass grant ACL"));
        }
    };
    Ok(VaultEntry {
        provider: row.get(0)?,
        label: row.get(1)?,
        scheme: CredentialScheme::from_db(row.get::<_, String>(2)?.as_str()),
        base_url: row.get(3)?,
        user_agent: row.get(4)?,
        created_at: row.get(5)?,
        status: row.get(6)?,
        generation: read_generation(row)?,
        sentinelpass_entry_id,
        sentinelpass_token_env,
        sentinelpass_grant_acl,
    })
}

/// The secret-only backend. Implementations store/retrieve the raw credential under
/// `service = "sandhi"`, `account = "<provider>:<label>"`.
pub trait Vault: Send + Sync {
    fn name(&self) -> &'static str;
    /// Implementation support, not a grant probe or a daemon-health assertion. Custom backends
    /// must explicitly opt into write/delete capability; an implemented trait is not evidence.
    fn capabilities(&self) -> VaultCapabilities {
        VaultCapabilities {
            read: true,
            write: false,
            delete: false,
            bounded_io: false,
        }
    }
    /// Read the raw secret for `provider:label`. `None` if not present.
    fn get_secret(&self, provider: &str, label: &str) -> Result<Option<String>, VaultError>;
    /// Write the raw secret for `provider:label`.
    fn set_secret(&self, provider: &str, label: &str, secret: &str) -> Result<(), VaultError>;
    /// Delete the secret. Returns whether it existed.
    fn delete_secret(&self, provider: &str, label: &str) -> Result<bool, VaultError>;
    /// Non-secret description of the exact names a backend will search. This is not a grant
    /// probe and must not contact the backend.
    fn reference_plan(
        &self,
        provider: &str,
        label: &str,
    ) -> Result<VaultReferencePlan, VaultError> {
        validate_reference(provider, label)?;
        Ok(VaultReferencePlan {
            backend: self.name(),
            resolution: "backend_default",
            searched_references: vec![account(provider, label)],
        })
    }

    /// Retrieve through an exact-entry service grant. Only the native IPC backend implements
    /// this path; the default deliberately fails closed instead of downgrading to a domain.
    fn get_exact_secret(
        &self,
        _binding: &SentinelPassExactBinding,
    ) -> Result<Option<String>, VaultError> {
        Err(VaultError::NotSupported(
            "selected vault backend does not implement SentinelPass exact-entry retrieval".into(),
        ))
    }

    /// Non-secret exact-entry lookup description. The default reports the mismatch rather than
    /// pretending the active backend can search a persisted SentinelPass binding.
    fn exact_reference_plan(
        &self,
        binding: &SentinelPassExactBinding,
    ) -> Result<VaultReferencePlan, VaultError> {
        validate_exact_binding(binding.entry_id, &binding.token_env)?;
        Ok(VaultReferencePlan {
            backend: self.name(),
            resolution: "unsupported_exact_binding",
            searched_references: vec![
                format!("entry:{}", binding.entry_id),
                format!("token_env:{}", binding.token_env),
            ],
        })
    }
}

/// Errors from a vault backend. Kept opaque so a missing OS keychain degrades gracefully rather
/// than panicking.
#[derive(Debug, Clone)]
pub enum VaultError {
    Backend(String),
    NotSupported(String),
    Busy,
    Timeout,
    Locked,
    Denied,
    Configuration,
    InvalidReference,
    StrictLegacyDenied,
}

/// Backend-owned, non-secret reference information for operator errors and inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VaultReferencePlan {
    pub backend: &'static str,
    pub resolution: &'static str,
    pub searched_references: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct VaultCapabilities {
    pub read: bool,
    pub write: bool,
    pub delete: bool,
    pub bounded_io: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretDeletion {
    NotAttempted,
    Unsupported,
    Deleted,
    Missing,
    Failed,
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(m) => write!(f, "vault backend error: {m}"),
            Self::NotSupported(m) => write!(f, "vault operation not supported: {m}"),
            Self::Busy => f.write_str("vault busy; operation not admitted"),
            Self::Timeout => f.write_str("vault deadline exceeded; a dispatched write may have applied; reconcile before retry"),
            Self::Locked => f.write_str("vault locked; unlock through the broker and retry explicitly"),
            Self::Denied => f.write_str("broker rejected operation; check client token and exact read/write grant"),
            Self::Configuration => f.write_str("vault configuration unavailable; check backend feature, client token and daemon token"),
            Self::InvalidReference => f.write_str("provider and label must use lowercase ASCII letters, digits, dots, underscores or hyphens (1–128 characters)"),
            Self::StrictLegacyDenied => f.write_str("strict broker profile denied the legacy external-secret grant"),
        }
    }
}

impl std::error::Error for VaultError {}

const KEYRING_SERVICE: &str = "sandhi";

/// OS keychain backend (default) via the `keyring` crate. Per-call entries are built from
/// `provider:label`; the struct itself holds no state.
pub struct KeyringVault;

impl KeyringVault {
    fn entry(provider: &str, label: &str) -> Result<keyring::Entry, VaultError> {
        keyring::Entry::new(KEYRING_SERVICE, &account(provider, label))
            .map_err(|e| VaultError::Backend(format!("keychain entry: {e}")))
    }
}

impl Vault for KeyringVault {
    fn name(&self) -> &'static str {
        "keyring"
    }

    fn capabilities(&self) -> VaultCapabilities {
        VaultCapabilities {
            read: true,
            write: true,
            delete: true,
            bounded_io: false,
        }
    }

    fn reference_plan(
        &self,
        provider: &str,
        label: &str,
    ) -> Result<VaultReferencePlan, VaultError> {
        validate_reference(provider, label)?;
        Ok(VaultReferencePlan {
            backend: self.name(),
            resolution: "os_service_account",
            searched_references: vec![format!(
                "service={KEYRING_SERVICE};account={}",
                account(provider, label)
            )],
        })
    }

    fn get_secret(&self, provider: &str, label: &str) -> Result<Option<String>, VaultError> {
        let entry = Self::entry(provider, label)?;
        match entry.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(VaultError::Backend(format!("keychain get: {e}"))),
        }
    }

    fn set_secret(&self, provider: &str, label: &str, secret: &str) -> Result<(), VaultError> {
        let entry = Self::entry(provider, label)?;
        entry
            .set_password(secret)
            .map_err(|e| VaultError::Backend(format!("keychain set: {e}")))
    }

    fn delete_secret(&self, provider: &str, label: &str) -> Result<bool, VaultError> {
        let entry = Self::entry(provider, label)?;
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(VaultError::Backend(format!("keychain delete: {e}"))),
        }
    }
}

impl Default for KeyringVault {
    fn default() -> Self {
        Self
    }
}

#[cfg(feature = "sentinelpass-ipc")]
mod ipc;
#[cfg(feature = "sentinelpass-ipc")]
pub use ipc::SentinelPassIpcVault;

/// SentinelPass password-manager backend. Talks to the SentinelPass daemon through its CLI
/// (`sentinelpass secret get …`) to keep the coupling loose — there is intentionally **no path
/// dependency on `sentinelpass-core`**. Read-only for now (the CLI exposes no `set`), so secrets
/// are provisioned inside SentinelPass itself and granted to the `sandhi` client; `set_secret`
/// returns [`VaultError::NotSupported`] with guidance.
///
/// Kept as an opt-in fallback (`SANDHI_SENTINELPASS_FALLBACK_CLI=1`); the default
/// `SANDHI_VAULT_BACKEND=sentinelpass` now uses [`SentinelPassIpcVault`] (native daemon IPC,
/// TD-0003 follow-up shipped via the `sentinelpass-protocol` contract crate).
/// Directory-backed read-only vault for restart-safe deployments
/// (ADR-011 in the sentinelpass repo)
/// (`SANDHI_VAULT_BACKEND=credstore` + `SANDHI_CREDENTIALS_DIR`). Secrets are
/// systemd **encrypted credentials** decrypted by PID 1 into
/// `$CREDENTIALS_DIRECTORY/<cred-name>` at unit start — no SentinelPass
/// daemon, vault, or master password is involved at runtime. Provision with
/// `sentinelpass service-credential install` (sentinelpass ≥ 0.14.0;
/// runbook: docs/SERVICE_CREDENTIALS.md in the sentinelpass repo); rotate
/// by re-running it.
pub struct CredentialStoreVault {
    dir: PathBuf,
}

/// Upstream credentials are API keys / short tokens (< 4 KiB in practice); a
/// file larger than this is a provisioning mistake, not a credential.
const CREDSTORE_MAX_SECRET_BYTES: u64 = 64 * 1024;

impl CredentialStoreVault {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Filename candidates for `provider:label`, covering both the
    /// double-underscore scheme and the `service-credential install`
    /// cred-name convention (`<topic>.<provider>.<label>`). Rejects
    /// separators and traversal before anything touches the filesystem.
    fn candidates(&self, provider: &str, label: &str) -> Option<Vec<PathBuf>> {
        fn filename_safe(s: &str) -> bool {
            !s.is_empty()
                && Path::new(s)
                    .file_name()
                    .is_some_and(|f| f.to_str() == Some(s))
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        }
        if !filename_safe(provider) || !filename_safe(label) {
            return None;
        }
        Some(vec![
            self.dir.join(format!("{provider}__{label}")),
            self.dir.join(format!("{provider}.{label}")),
            // The service-credential install convention:
            // `<topic>.<provider>.<label>` (TD-0031 deployments use `sandhi.`).
            self.dir.join(format!("sandhi.{provider}.{label}")),
        ])
    }
}

impl Vault for CredentialStoreVault {
    fn name(&self) -> &'static str {
        "credstore"
    }

    fn capabilities(&self) -> VaultCapabilities {
        VaultCapabilities {
            read: true,
            write: false,
            delete: false,
            bounded_io: false,
        }
    }

    fn reference_plan(
        &self,
        provider: &str,
        label: &str,
    ) -> Result<VaultReferencePlan, VaultError> {
        let candidates = self
            .candidates(provider, label)
            .ok_or(VaultError::InvalidReference)?;
        Ok(VaultReferencePlan {
            backend: self.name(),
            resolution: "filesystem_candidates",
            searched_references: candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
        })
    }

    fn get_secret(&self, provider: &str, label: &str) -> Result<Option<String>, VaultError> {
        let Some(candidates) = self.candidates(provider, label) else {
            return Ok(None);
        };
        // Credential-lifecycle integrity: when more than one candidate exists,
        // or an earlier candidate fails to read before a later one succeeds,
        // say so — a stale shadow file must not silently defeat rotation.
        let existing: Vec<&PathBuf> = candidates.iter().filter(|p| p.exists()).collect();
        if existing.len() > 1 {
            tracing::warn!(
                "credstore: multiple candidate credential files for {provider}:{label}; using the first — remove stale schemes to avoid shadowing"
            );
        }
        for path in candidates {
            // Reject symlinks outright: a swapped link could redirect the
            // read outside the provisioned directory, and the systemd
            // credential store is plain files by contract.
            if std::fs::symlink_metadata(&path)
                .map(|meta| meta.file_type().is_symlink())
                .unwrap_or(true)
            {
                tracing::warn!("credstore: skipping symlink candidate {path:?}");
                continue;
            }
            match std::fs::read(&path) {
                // systemd-creds output may carry a trailing newline; trim
                // surrounding Unicode whitespace — the secret itself is
                // never logged.
                Ok(bytes) => {
                    if bytes.len() as u64 > CREDSTORE_MAX_SECRET_BYTES {
                        return Err(VaultError::Backend(format!(
                            "credstore read {}: {} bytes exceeds the {}-byte credential cap",
                            path.display(),
                            bytes.len(),
                            CREDSTORE_MAX_SECRET_BYTES
                        )));
                    }
                    let secret = String::from_utf8(bytes)
                        .map_err(|e| VaultError::Backend(format!("credstore utf8: {e}")))?;
                    let trimmed = secret.trim();
                    if trimmed.is_empty() {
                        // A whitespace-only credential file is a provisioning
                        // accident, not a credential: report absent so the
                        // provider fails LOUD instead of 401ing on "".
                        return Ok(None);
                    }
                    return Ok(Some(trimmed.to_string()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(VaultError::Backend(format!(
                        "credstore read {}: {e}",
                        path.display()
                    )))
                }
            }
        }
        Ok(None)
    }

    fn set_secret(&self, _provider: &str, _label: &str, _secret: &str) -> Result<(), VaultError> {
        Err(VaultError::NotSupported(
            "credstore is provisioned by `sentinelpass service-credential install`; rotate by re-running it"
                .into(),
        ))
    }

    fn delete_secret(&self, _provider: &str, _label: &str) -> Result<bool, VaultError> {
        Err(VaultError::NotSupported(
            "credstore is provisioned by `sentinelpass service-credential install`".into(),
        ))
    }
}

pub struct SentinelPassVault {
    client_id: String,
    /// Path/executable override; defaults to `sentinelpass`.
    binary: String,
}

impl SentinelPassVault {
    pub fn new() -> Self {
        Self {
            client_id: std::env::var("SANDHI_SENTINELPASS_CLIENT_ID")
                .unwrap_or_else(|_| "sandhi".into()),
            binary: std::env::var("SANDHI_SENTINELPASS_BIN")
                .unwrap_or_else(|_| "sentinelpass".into()),
        }
    }

    fn domain(provider: &str, label: &str) -> String {
        format!("sandhi:{provider}:{label}")
    }
}

impl Default for SentinelPassVault {
    fn default() -> Self {
        Self::new()
    }
}

impl Vault for SentinelPassVault {
    fn name(&self) -> &'static str {
        "sentinelpass"
    }

    fn capabilities(&self) -> VaultCapabilities {
        VaultCapabilities {
            read: true,
            write: false,
            delete: false,
            bounded_io: false,
        }
    }

    fn reference_plan(
        &self,
        provider: &str,
        label: &str,
    ) -> Result<VaultReferencePlan, VaultError> {
        validate_reference(provider, label)?;
        Ok(VaultReferencePlan {
            backend: self.name(),
            resolution: "legacy_domain",
            searched_references: vec![Self::domain(provider, label)],
        })
    }

    fn get_secret(&self, provider: &str, label: &str) -> Result<Option<String>, VaultError> {
        let out = std::process::Command::new(&self.binary)
            .args([
                "secret",
                "get",
                "--client-id",
                &self.client_id,
                "--domain",
                &Self::domain(provider, label),
                "--field",
                "password",
                "--output",
                "plain",
            ])
            .output()
            .map_err(|e| VaultError::Backend(format!("sentinelpass spawn: {e}")))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // A missing secret surfaces as a non-zero exit; treat not-found as None.
            if stderr.contains("not available") || stderr.contains("not found") {
                return Ok(None);
            }
            return Err(VaultError::Backend(format!(
                "sentinelpass get failed: {stderr}"
            )));
        }
        let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if value.is_empty() {
            Ok(None)
        } else {
            Ok(Some(value))
        }
    }

    fn set_secret(&self, _provider: &str, _label: &str, _secret: &str) -> Result<(), VaultError> {
        Err(VaultError::NotSupported(
            "SentinelPass has no CLI write path; provision the credential inside SentinelPass \
             then `sentinelpass secret allow --client-id sandhi --domain sandhi:<provider>:<label> \
             --field password`"
                .into(),
        ))
    }

    fn delete_secret(&self, _provider: &str, _label: &str) -> Result<bool, VaultError> {
        Err(VaultError::NotSupported(
            "revoke the SentinelPass grant instead (sentinelpass secret revoke)".into(),
        ))
    }
}

/// A process-local vault for tests/demos (secrets held in memory).
pub struct InMemoryVault {
    secrets: Mutex<HashMap<String, String>>,
}

impl Default for InMemoryVault {
    fn default() -> Self {
        Self {
            secrets: Mutex::new(HashMap::new()),
        }
    }
}

impl InMemoryVault {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Vault for InMemoryVault {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn capabilities(&self) -> VaultCapabilities {
        VaultCapabilities {
            read: true,
            write: true,
            delete: true,
            bounded_io: true,
        }
    }

    fn reference_plan(
        &self,
        provider: &str,
        label: &str,
    ) -> Result<VaultReferencePlan, VaultError> {
        validate_reference(provider, label)?;
        Ok(VaultReferencePlan {
            backend: self.name(),
            resolution: "process_map",
            searched_references: vec![account(provider, label)],
        })
    }

    fn get_secret(&self, provider: &str, label: &str) -> Result<Option<String>, VaultError> {
        Ok(self
            .secrets
            .lock()
            .expect("in-memory vault poisoned")
            .get(&account(provider, label))
            .cloned())
    }

    fn set_secret(&self, provider: &str, label: &str, secret: &str) -> Result<(), VaultError> {
        self.secrets
            .lock()
            .expect("in-memory vault poisoned")
            .insert(account(provider, label), secret.into());
        Ok(())
    }

    fn delete_secret(&self, provider: &str, label: &str) -> Result<bool, VaultError> {
        Ok(self
            .secrets
            .lock()
            .expect("in-memory vault poisoned")
            .remove(&account(provider, label))
            .is_some())
    }
}

fn account(provider: &str, label: &str) -> String {
    format!("{provider}:{label}")
}

/// Reject normalization collisions and wildcards rather than silently widening a broker domain.
pub fn validate_reference(provider: &str, label: &str) -> Result<(), VaultError> {
    for value in [provider, label] {
        if value.is_empty()
            || value.ends_with('.')
            || value.len() > 128
            || !value.bytes().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-')
            })
        {
            return Err(VaultError::InvalidReference);
        }
    }
    Ok(())
}

/// Preserve a configured-but-unavailable backend; never silently select another secret store.
struct UnavailableVault;
impl Vault for UnavailableVault {
    fn name(&self) -> &'static str {
        "unavailable"
    }
    fn capabilities(&self) -> VaultCapabilities {
        VaultCapabilities {
            read: false,
            write: false,
            delete: false,
            bounded_io: false,
        }
    }
    fn get_secret(&self, _: &str, _: &str) -> Result<Option<String>, VaultError> {
        Err(VaultError::Configuration)
    }
    fn set_secret(&self, _: &str, _: &str, _: &str) -> Result<(), VaultError> {
        Err(VaultError::Configuration)
    }
    fn delete_secret(&self, _: &str, _: &str) -> Result<bool, VaultError> {
        Err(VaultError::Configuration)
    }

    fn reference_plan(
        &self,
        _provider: &str,
        _label: &str,
    ) -> Result<VaultReferencePlan, VaultError> {
        Ok(VaultReferencePlan {
            backend: self.name(),
            resolution: "unavailable",
            searched_references: Vec::new(),
        })
    }
}

/// The operator-facing vault: a SQLite metadata index composed with a secret backend.
pub struct VaultStore {
    conn: Mutex<Connection>,
    backend: Box<dyn Vault>,
}

impl VaultStore {
    /// Open (creating if needed) the metadata index at `path` with the given secret backend.
    pub fn with_backend(path: &str, backend: Box<dyn Vault>) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            backend,
        })
    }

    /// In-memory metadata index + an [`InMemoryVault`] (tests / demos).
    pub fn in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            backend: Box::new(InMemoryVault::new()),
        })
    }

    /// Pick the backend named by `SANDHI_VAULT_BACKEND`
    /// (`keyring` default; `sentinelpass`; `credstore`).
    ///
    /// `sentinelpass` selects the native daemon-IPC backend; setting
    /// `SANDHI_SENTINELPASS_FALLBACK_CLI=1` keeps the legacy CLI shell-out.
    /// `credstore` selects the sentinelpass-ADR-011 directory-backed read-only backend and
    /// requires `SANDHI_CREDENTIALS_DIR` (typically
    /// `$CREDENTIALS_DIRECTORY` inside a system unit); refusing to start
    /// without it is deliberate — an empty directory would silently drop
    /// every provider credential.
    pub fn backend_from_env() -> Result<Box<dyn Vault>, String> {
        match std::env::var("SANDHI_VAULT_BACKEND")
            .unwrap_or_else(|_| "keyring".into())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "credstore" => {
                let dir = std::env::var("SANDHI_CREDENTIALS_DIR")
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if dir.is_empty() {
                    return Err(
                        "SANDHI_VAULT_BACKEND=credstore requires SANDHI_CREDENTIALS_DIR"
                            .to_string(),
                    );
                }
                let dir = std::path::PathBuf::from(&dir);
                if !dir.is_dir() {
                    return Err(format!(
                        "SANDHI_CREDENTIALS_DIR '{}' is not a directory",
                        dir.display()
                    ));
                }
                Ok(Box::new(CredentialStoreVault::new(dir)))
            }
            "sentinelpass" => {
                if std::env::var("SANDHI_SENTINELPASS_FALLBACK_CLI").as_deref() == Ok("1") {
                    Ok(Box::new(SentinelPassVault::new()))
                } else {
                    #[cfg(feature = "sentinelpass-ipc")]
                    {
                        match SentinelPassIpcVault::new() {
                            Ok(backend) => Ok(Box::new(backend)),
                            Err(_) => {
                                tracing::warn!(
                                    "native vault configuration unavailable; no fallback selected"
                                );
                                Ok(Box::new(UnavailableVault))
                            }
                        }
                    }
                    #[cfg(not(feature = "sentinelpass-ipc"))]
                    {
                        tracing::warn!(
                            "SANDHI_VAULT_BACKEND=sentinelpass daemon IPC requires the \
                             `sentinelpass-ipc` feature; no fallback selected"
                        );
                        Ok(Box::new(UnavailableVault))
                    }
                }
            }
            "keyring" => Ok(Box::new(KeyringVault)),
            _ => Ok(Box::new(UnavailableVault)),
        }
    }

    fn init(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS vault (
                provider   TEXT NOT NULL,
                label      TEXT NOT NULL,
                scheme     TEXT NOT NULL,
                base_url   TEXT,
                user_agent TEXT,
                created_at TEXT NOT NULL,
                status     TEXT NOT NULL DEFAULT 'active',
                generation INTEGER NOT NULL DEFAULT 1 CHECK(typeof(generation) = 'integer' AND generation > 0),
                sentinelpass_entry_id INTEGER,
                sentinelpass_token_env TEXT,
                sentinelpass_exe_pinned INTEGER,
                sentinelpass_fingerprint_enrolled INTEGER,
                sentinelpass_expires_at INTEGER,
                PRIMARY KEY (provider, label)
            );",
        )?;
        // Additive columns for databases created before the per-endpoint transport
        // overrides existed. SQLite has no ADD COLUMN IF NOT EXISTS — attempt and
        // ignore the duplicate-column error so init stays idempotent.
        let _ = conn.execute("ALTER TABLE vault ADD COLUMN user_agent TEXT", []);
        let mut columns = conn.prepare("PRAGMA table_info(vault)")?;
        let names = columns
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !names.iter().any(|name| name == "generation") {
            conn.execute("ALTER TABLE vault ADD COLUMN generation INTEGER NOT NULL DEFAULT 1 CHECK(typeof(generation) = 'integer' AND generation > 0)", [])?;
        }
        for (statement, duplicate) in [
            (
                "ALTER TABLE vault ADD COLUMN sentinelpass_entry_id INTEGER",
                "duplicate column name: sentinelpass_entry_id",
            ),
            (
                "ALTER TABLE vault ADD COLUMN sentinelpass_token_env TEXT",
                "duplicate column name: sentinelpass_token_env",
            ),
            (
                "ALTER TABLE vault ADD COLUMN sentinelpass_exe_pinned INTEGER",
                "duplicate column name: sentinelpass_exe_pinned",
            ),
            (
                "ALTER TABLE vault ADD COLUMN sentinelpass_fingerprint_enrolled INTEGER",
                "duplicate column name: sentinelpass_fingerprint_enrolled",
            ),
            (
                "ALTER TABLE vault ADD COLUMN sentinelpass_expires_at INTEGER",
                "duplicate column name: sentinelpass_expires_at",
            ),
        ] {
            if let Err(error) = conn.execute(statement, []) {
                if !error.to_string().contains(duplicate) {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Store a credential: writes the secret to the backend and the metadata to SQLite.
    /// Returns the credential id (`provider:label`).
    pub fn set(
        &self,
        provider: &str,
        label: &str,
        scheme: CredentialScheme,
        base_url: Option<&str>,
        user_agent: Option<&str>,
        secret: &str,
    ) -> Result<String, VaultError> {
        validate_reference(provider, label)?;
        self.backend.set_secret(provider, label, secret)?;
        self.register_metadata(provider, label, scheme, base_url, user_agent, None, None)
    }

    /// Index a reference only after its caller has successfully resolved and validated the secret.
    /// This never writes to the secret backend; registration is not a grant or a rotation.
    // The seven domain fields describe one vault row; collapsing them into a request DTO would
    // move the same coupling to every existing store caller without improving validation.
    #[allow(clippy::too_many_arguments)]
    pub fn register_metadata(
        &self,
        provider: &str,
        label: &str,
        scheme: CredentialScheme,
        base_url: Option<&str>,
        user_agent: Option<&str>,
        exact: Option<SentinelPassExactBinding>,
        grant_acl: Option<SentinelPassGrantAcl>,
    ) -> Result<String, VaultError> {
        validate_reference(provider, label)?;
        if let Some(binding) = &exact {
            validate_exact_binding(binding.entry_id, &binding.token_env)?;
            if self.backend.name() != "sentinelpass-ipc" {
                return Err(VaultError::NotSupported(
                    "exact-entry metadata requires the native SentinelPass IPC backend".into(),
                ));
            }
        }
        if exact.is_none() && grant_acl.is_some() {
            return Err(VaultError::InvalidReference);
        }
        let (exe_pinned, fingerprint_enrolled, expires_at) = grant_acl
            .map(SentinelPassGrantAcl::to_db)
            .map_or((None, None, None), |(exe, enrolled, expires)| {
                (Some(exe), Some(enrolled), expires)
            });
        let conn = self.conn.lock().expect("vault conn poisoned");
        conn.execute(
            "INSERT INTO vault (provider, label, scheme, base_url, user_agent, created_at, status, generation, \
             sentinelpass_entry_id, sentinelpass_token_env, sentinelpass_exe_pinned, \
             sentinelpass_fingerprint_enrolled, sentinelpass_expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', 1, ?7, ?8, ?9, ?10, ?11) \
             ON CONFLICT(provider, label) DO UPDATE SET scheme=excluded.scheme, \
             base_url=excluded.base_url, user_agent=excluded.user_agent, created_at=excluded.created_at, \
             status='active', generation=vault.generation+1, sentinelpass_entry_id=excluded.sentinelpass_entry_id, \
             sentinelpass_token_env=excluded.sentinelpass_token_env, \
             sentinelpass_exe_pinned=excluded.sentinelpass_exe_pinned, \
             sentinelpass_fingerprint_enrolled=excluded.sentinelpass_fingerprint_enrolled, \
             sentinelpass_expires_at=excluded.sentinelpass_expires_at",
            params![
                provider,
                label,
                scheme.as_db(),
                base_url,
                user_agent,
                now_rfc3339(),
                exact.as_ref().map(|binding| binding.entry_id),
                exact.as_ref().map(|binding| binding.token_env.clone()),
                exe_pinned,
                fingerprint_enrolled,
                expires_at,
            ],
        )
        .map_err(|e| VaultError::Backend(format!("sqlite vault set: {e}")))?;
        Ok(account(provider, label))
    }

    /// Read the raw secret for `provider:label` (from the secret backend).
    pub fn get(&self, provider: &str, label: &str) -> Result<Option<String>, VaultError> {
        if let Some(binding) = self
            .metadata_entry(provider, label)?
            .and_then(|entry| entry.exact_binding().transpose())
            .transpose()?
        {
            return self.backend.get_exact_secret(&binding);
        }
        self.backend.get_secret(provider, label)
    }

    /// Resolve an unregistered exact reference during operator onboarding.
    pub fn get_exact(
        &self,
        provider: &str,
        label: &str,
        binding: &SentinelPassExactBinding,
    ) -> Result<Option<String>, VaultError> {
        validate_reference(provider, label)?;
        if self.backend.name() != "sentinelpass-ipc" {
            return Err(VaultError::NotSupported(
                "exact-entry retrieval requires the native SentinelPass IPC backend".into(),
            ));
        }
        self.backend.get_exact_secret(binding)
    }

    /// Resolve the **active** credential for a provider (first active label, ordered), returning
    /// `(entry, secret)`. The proxy uses this to build an upstream handle from a provider slug.
    pub fn resolve(&self, provider: &str) -> Result<Option<(VaultEntry, String)>, VaultError> {
        let Some(entry) = self.first_active_entry(provider)? else {
            return Ok(None);
        };
        let binding = entry.exact_binding()?;
        let secret = if let Some(binding) = binding {
            self.backend.get_exact_secret(&binding)?
        } else {
            self.backend.get_secret(&entry.provider, &entry.label)?
        };
        match secret {
            Some(secret) => Ok(Some((entry, secret))),
            None => Ok(None),
        }
    }

    /// Exact non-secret metadata, including the durable local generation.
    pub fn metadata_entry(
        &self,
        provider: &str,
        label: &str,
    ) -> Result<Option<VaultEntry>, VaultError> {
        validate_reference(provider, label)?;
        let conn = self.conn.lock().expect("vault conn poisoned");
        conn.query_row("SELECT provider, label, scheme, base_url, user_agent, created_at, status, generation, sentinelpass_entry_id, sentinelpass_token_env, sentinelpass_exe_pinned, sentinelpass_fingerprint_enrolled, sentinelpass_expires_at FROM vault WHERE provider=?1 AND label=?2",
            params![provider,label], read_vault_entry)
            .optional()
            .map_err(|error| VaultError::Backend(format!("sqlite vault metadata: {error}")))
    }

    /// Masked metadata list (never the secret), all labels.
    pub fn list(&self) -> Result<Vec<VaultEntry>, VaultError> {
        let conn = self.conn.lock().expect("vault conn poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT provider, label, scheme, base_url, user_agent, created_at, status, generation, \
                 sentinelpass_entry_id, sentinelpass_token_env, sentinelpass_exe_pinned, \
                 sentinelpass_fingerprint_enrolled, sentinelpass_expires_at \
                 FROM vault ORDER BY provider, label",
            )
            .map_err(|e| VaultError::Backend(format!("sqlite vault list: {e}")))?;
        let rows = stmt
            .query_map([], read_vault_entry)
            .map_err(|e| VaultError::Backend(format!("sqlite vault list: {e}")))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| VaultError::Backend(format!("sqlite vault list: {e}")))
    }

    /// Revoke (soft-delete) a credential: marks metadata `revoked` and deletes the secret.
    /// Backends that cannot delete (e.g. sentinelpass, where revoking the grant is the
    /// off-boarding path) only fail the secret deletion — the metadata revocation proceeds.
    pub fn revoke(&self, provider: &str, label: &str) -> Result<bool, VaultError> {
        self.revoke_with_status(provider, label)
            .map(|(revoked, _)| revoked)
    }

    /// Commit local revocation before attempting secret deletion. Unsupported brokers receive
    /// no delete request. Report cleanup independently; neither step revokes a provider API key.
    pub fn revoke_with_status(
        &self,
        provider: &str,
        label: &str,
    ) -> Result<(bool, SecretDeletion), VaultError> {
        let conn = self.conn.lock().expect("vault conn poisoned");
        let changed = conn
            .execute(
                "UPDATE vault SET status = 'revoked' WHERE provider = ?1 AND label = ?2 \
                 AND status = 'active'",
                params![provider, label],
            )
            .map_err(|e| VaultError::Backend(format!("sqlite vault revoke: {e}")))?;
        drop(conn);
        let deletion = if changed == 0 {
            SecretDeletion::NotAttempted
        } else if !self.backend.capabilities().delete {
            SecretDeletion::Unsupported
        } else {
            match self.backend.delete_secret(provider, label) {
                Ok(true) => SecretDeletion::Deleted,
                Ok(false) => SecretDeletion::Missing,
                Err(_) => SecretDeletion::Failed,
            }
        };
        Ok((changed > 0, deletion))
    }

    fn first_active_entry(&self, provider: &str) -> Result<Option<VaultEntry>, VaultError> {
        let conn = self.conn.lock().expect("vault conn poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT provider, label, scheme, base_url, user_agent, created_at, status, generation, \
                 sentinelpass_entry_id, sentinelpass_token_env, sentinelpass_exe_pinned, \
                 sentinelpass_fingerprint_enrolled, sentinelpass_expires_at \
                 FROM vault WHERE provider = ?1 AND status = 'active' \
                 ORDER BY created_at LIMIT 1",
            )
            .map_err(|e| VaultError::Backend(format!("sqlite vault get: {e}")))?;
        stmt.query_row(params![provider], read_vault_entry)
            .optional()
            .map_err(|e| VaultError::Backend(format!("sqlite vault get: {e}")))
    }

    /// The configured backend name.
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Describe the non-secret names this configured backend searches for an exact reference.
    pub fn reference_plan(
        &self,
        provider: &str,
        label: &str,
    ) -> Result<VaultReferencePlan, VaultError> {
        if let Some(binding) = self
            .metadata_entry(provider, label)?
            .and_then(|entry| entry.exact_binding().transpose())
            .transpose()?
        {
            return self.backend.exact_reference_plan(&binding);
        }
        self.backend.reference_plan(provider, label)
    }

    /// Describe a caller-supplied exact reference before it is committed locally.
    pub fn reference_plan_for_exact(
        &self,
        provider: &str,
        label: &str,
        binding: &SentinelPassExactBinding,
    ) -> Result<VaultReferencePlan, VaultError> {
        validate_reference(provider, label)?;
        self.backend.exact_reference_plan(binding)
    }

    pub fn capabilities(&self) -> VaultCapabilities {
        self.backend.capabilities()
    }
}

/// SHA-256 hex digest of a presented virtual-key secret. Used as the durable + live lookup key so
/// the plaintext secret is never stored (only its hash). Public so the proxy can hash a presented
/// token before resolving it against the in-memory store.
///
/// This is a **lookup index over high-entropy secrets, not a password hash.** A minted key is
/// `vk_` + 16 bytes (≥128 bits) of OS CSPRNG entropy ([`vkeys`](crate::vkeys)::generate_secret),
/// so an offline brute-force of this digest against a guessed secret is 2¹²⁸ work — infeasible
/// regardless of salt or pepper, which exist to defend *low-entropy* secrets (passwords) that
/// virtual keys are not. It is therefore deliberately unsalted: a salt would not slow that attack
/// and would add a stored column for no security gain. If a future mint path ever produced a
/// low-entropy secret this assumption must be revisited — `minted_secret_has_min_128_bits_of_entropy`
/// exists to fail loudly if it does.
pub fn hash_secret(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    // Table-driven (design audit A7): the per-byte `format!` built a temporary String for each
    // of the 32 nibble pairs of a SHA-256 digest on every virtual-key lookup miss.
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SyntheticExactBackend {
        exact_calls: Mutex<usize>,
    }

    impl Vault for SyntheticExactBackend {
        fn name(&self) -> &'static str {
            "sentinelpass-ipc"
        }
        fn capabilities(&self) -> VaultCapabilities {
            VaultCapabilities {
                read: true,
                write: true,
                delete: false,
                bounded_io: true,
            }
        }
        fn get_secret(&self, _provider: &str, _label: &str) -> Result<Option<String>, VaultError> {
            Ok(Some("synthetic-legacy-key".into()))
        }
        fn set_secret(
            &self,
            _provider: &str,
            _label: &str,
            _secret: &str,
        ) -> Result<(), VaultError> {
            Ok(())
        }
        fn delete_secret(&self, _: &str, _: &str) -> Result<bool, VaultError> {
            Ok(false)
        }
        fn get_exact_secret(
            &self,
            binding: &SentinelPassExactBinding,
        ) -> Result<Option<String>, VaultError> {
            *self.exact_calls.lock().unwrap() += 1;
            Ok(Some(format!("synthetic-entry-{}", binding.entry_id)))
        }
        fn exact_reference_plan(
            &self,
            binding: &SentinelPassExactBinding,
        ) -> Result<VaultReferencePlan, VaultError> {
            Ok(VaultReferencePlan {
                backend: self.name(),
                resolution: "exact_entry",
                searched_references: vec![format!("entry:{}", binding.entry_id)],
            })
        }
    }

    #[test]
    fn legacy_metadata_generation_migrates_and_never_reuses_a_revoked_generation() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE vault(provider TEXT NOT NULL,label TEXT NOT NULL,scheme TEXT NOT NULL,base_url TEXT,user_agent TEXT,created_at TEXT NOT NULL,status TEXT NOT NULL,PRIMARY KEY(provider,label)); INSERT INTO vault VALUES('openai','fixture','api_key',NULL,NULL,'fixture','active');").unwrap();
        VaultStore::init(&conn).unwrap();
        let vault = VaultStore {
            conn: Mutex::new(conn),
            backend: Box::new(InMemoryVault::new()),
        };
        assert_eq!(
            vault
                .metadata_entry("openai", "fixture")
                .unwrap()
                .unwrap()
                .generation,
            1
        );
        vault
            .register_metadata(
                "openai",
                "fixture",
                CredentialScheme::ApiKey,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(vault.list().unwrap()[0].generation, 2);
        vault.revoke("openai", "fixture").unwrap();
        vault
            .register_metadata(
                "openai",
                "fixture",
                CredentialScheme::ApiKey,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            vault
                .metadata_entry("openai", "fixture")
                .unwrap()
                .unwrap()
                .generation,
            3
        );
        VaultStore::init(&vault.conn.lock().unwrap()).unwrap();
        assert_eq!(vault.list().unwrap()[0].generation, 3);
        assert!(
            vault.get("openai", "fixture").unwrap().is_none(),
            "metadata registration does not mutate secrets"
        );
        vault
            .conn
            .lock()
            .unwrap()
            .execute("UPDATE vault SET generation=9223372036854775807", [])
            .unwrap();
        assert!(vault
            .register_metadata(
                "openai",
                "fixture",
                CredentialScheme::ApiKey,
                None,
                None,
                None,
                None,
            )
            .is_err());
        assert_eq!(vault.list().unwrap()[0].generation, i64::MAX as u64);
    }

    #[test]
    fn observed_credential_generation_survives_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("vault.db");
        let open = || {
            VaultStore::with_backend(path.to_str().unwrap(), Box::new(InMemoryVault::new()))
                .unwrap()
        };
        let vault = open();
        for (secret, generation) in [("fixture-original", 1), ("fixture-rotated", 2)] {
            vault
                .set(
                    "openai",
                    "fixture",
                    CredentialScheme::ApiKey,
                    None,
                    None,
                    secret,
                )
                .unwrap();
            assert_eq!(
                vault
                    .metadata_entry("openai", "fixture")
                    .unwrap()
                    .unwrap()
                    .generation,
                generation
            );
        }
        drop(vault);
        assert_eq!(
            open()
                .metadata_entry("openai", "fixture")
                .unwrap()
                .unwrap()
                .generation,
            2
        );
    }

    #[test]
    fn exact_entry_metadata_routes_retrieval_and_direct_secrets_clear_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("vault.db");
        let backend = SyntheticExactBackend {
            exact_calls: Mutex::new(0),
        };
        let vault = VaultStore::with_backend(path.to_str().unwrap(), Box::new(backend)).unwrap();
        let binding = SentinelPassExactBinding::new(42, "SANDHI_TEST_SERVICE_TOKEN").unwrap();
        vault
            .register_metadata(
                "openai",
                "default",
                CredentialScheme::ApiKey,
                None,
                None,
                Some(binding.clone()),
                Some(SentinelPassGrantAcl {
                    exe_pinned: true,
                    fingerprint_enrolled: true,
                    expires_at: Some(1_893_456_000),
                }),
            )
            .unwrap();
        let entry = vault.metadata_entry("openai", "default").unwrap().unwrap();
        assert_eq!(entry.exact_binding().unwrap().as_ref(), Some(&binding));
        assert_eq!(
            entry.grant_acl().unwrap(),
            Some(SentinelPassGrantAcl {
                exe_pinned: true,
                fingerprint_enrolled: true,
                expires_at: Some(1_893_456_000),
            })
        );
        assert_eq!(
            vault.get("openai", "default").unwrap().as_deref(),
            Some("synthetic-entry-42")
        );
        let plan = vault.reference_plan("openai", "default").unwrap();
        assert_eq!(plan.resolution, "exact_entry");
        assert_eq!(plan.searched_references, vec!["entry:42"]);

        vault
            .set(
                "openai",
                "default",
                CredentialScheme::ApiKey,
                None,
                None,
                "synthetic-direct-key",
            )
            .unwrap();
        let rotated = vault.metadata_entry("openai", "default").unwrap().unwrap();
        assert_eq!(rotated.exact_binding().unwrap(), None);
        assert_eq!(rotated.grant_acl().unwrap(), None);
        assert_eq!(
            vault.get("openai", "default").unwrap().as_deref(),
            Some("synthetic-legacy-key")
        );
    }

    #[test]
    fn persisted_exact_entry_fails_closed_when_backend_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("vault.db");
        let vault = VaultStore::with_backend(
            path.to_str().unwrap(),
            Box::new(SyntheticExactBackend {
                exact_calls: Mutex::new(0),
            }),
        )
        .unwrap();
        let binding = SentinelPassExactBinding::new(42, "SANDHI_TEST_SERVICE_TOKEN").unwrap();
        vault
            .register_metadata(
                "openai",
                "default",
                CredentialScheme::ApiKey,
                None,
                None,
                Some(binding),
                None,
            )
            .unwrap();
        drop(vault);
        let mismatch =
            VaultStore::with_backend(path.to_str().unwrap(), Box::new(InMemoryVault::new()))
                .unwrap();
        assert!(matches!(
            mismatch.get("openai", "default"),
            Err(VaultError::NotSupported(_))
        ));
        let plan = mismatch.reference_plan("openai", "default").unwrap();
        assert_eq!(plan.backend, "memory");
        assert_eq!(plan.resolution, "unsupported_exact_binding");
    }

    #[test]
    fn corrupted_or_orphaned_acl_metadata_fails_closed_on_reopen() {
        let vault = VaultStore::with_backend(
            ":memory:",
            Box::new(SyntheticExactBackend {
                exact_calls: Mutex::new(0),
            }),
        )
        .unwrap();
        let valid = "sentinelpass_entry_id = 42, sentinelpass_token_env = \
                     'SANDHI_TEST_SERVICE_TOKEN', sentinelpass_exe_pinned = 1, \
                     sentinelpass_fingerprint_enrolled = 1, sentinelpass_expires_at = NULL";
        vault
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO vault (provider,label,scheme,created_at,status,generation) \
                 VALUES ('openai','default','api_key','fixture','active',1)",
                [],
            )
            .unwrap();
        for assignment in [
            "sentinelpass_entry_id = NULL",
            "sentinelpass_token_env = NULL",
            "sentinelpass_exe_pinned = NULL",
            "sentinelpass_fingerprint_enrolled = NULL",
        ] {
            vault
                .conn
                .lock()
                .unwrap()
                .execute(&format!("UPDATE vault SET {valid}"), [])
                .unwrap();
            assert!(vault.list().is_ok());
            vault
                .conn
                .lock()
                .unwrap()
                .execute(&format!("UPDATE vault SET {assignment}"), [])
                .unwrap();
            assert!(
                vault.list().is_err(),
                "corrupt assignment `{assignment}` must fail inventory"
            );
            assert!(vault.metadata_entry("openai", "default").is_err());
            assert!(vault.get("openai", "default").is_err());
        }
    }

    #[test]
    fn exact_entry_bindings_require_a_complete_nonsecret_pair() {
        for (entry_id, token_env) in [
            (0, "SANDHI_TEST_SERVICE_TOKEN"),
            (42, ""),
            (42, "BAD TOKEN"),
            (42, "1BAD_TOKEN"),
            (42, "SENTINELPASS_CLIENT_TOKEN"),
        ] {
            assert!(SentinelPassExactBinding::new(entry_id, token_env).is_err());
        }
        let binding = SentinelPassExactBinding::new(42, "SANDHI_TEST_SERVICE_TOKEN_2").unwrap();
        assert_eq!(binding.entry_id, 42);
        assert_eq!(binding.token_env, "SANDHI_TEST_SERVICE_TOKEN_2");
    }

    #[test]
    fn new_references_reject_broker_normalization_collisions() {
        for label in [
            "UPPER",
            "tail.",
            ".",
            "..",
            "white space",
            "*",
            "bad:label",
            "bad/label",
        ] {
            assert!(matches!(
                validate_reference("openai", label),
                Err(VaultError::InvalidReference)
            ));
        }
        assert!(validate_reference("openai", "team.prod-1_a").is_ok());
    }

    #[test]
    fn failed_local_revocation_does_not_delete_the_secret() {
        let vault = VaultStore::in_memory().unwrap();
        vault
            .set(
                "openai",
                "default",
                CredentialScheme::ApiKey,
                None,
                None,
                "synthetic-key",
            )
            .unwrap();
        vault.conn.lock().unwrap().execute_batch("CREATE TRIGGER deny_revoke BEFORE UPDATE ON vault BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END;").unwrap();
        assert!(vault.revoke_with_status("openai", "default").is_err());
        assert_eq!(
            vault.get("openai", "default").unwrap().as_deref(),
            Some("synthetic-key")
        );
        assert_eq!(vault.list().unwrap()[0].status, "active");
    }

    #[test]
    fn in_memory_round_trip_and_masked_listing() {
        let vault = VaultStore::in_memory().unwrap();
        let id = vault
            .set(
                "anthropic",
                "default",
                CredentialScheme::ApiKey,
                None,
                None,
                "sk-secret-123456",
            )
            .unwrap();
        assert_eq!(id, "anthropic:default");

        // Secret is retrievable.
        assert_eq!(
            vault.get("anthropic", "default").unwrap().as_deref(),
            Some("sk-secret-123456")
        );

        // Listing is masked metadata only — no secret appears.
        let listed = vault.list().unwrap();
        assert_eq!(listed.len(), 1);
        let entry = &listed[0];
        assert_eq!(entry.provider, "anthropic");
        assert_eq!(entry.label, "default");
        assert_eq!(entry.status, "active");
        let serialized = serde_json::to_string(&listed).unwrap();
        assert!(
            !serialized.contains("sk-secret-123456"),
            "secret must never appear in the masked listing"
        );
        assert_eq!(
            VaultEntry::masked_secret_hint("sk-secret-123456"),
            "sk-…456"
        );
    }

    #[test]
    fn resolve_returns_first_active_credential() {
        let vault = VaultStore::in_memory().unwrap();
        vault
            .set(
                "openai",
                "default",
                CredentialScheme::ApiKey,
                None,
                None,
                "key-one",
            )
            .unwrap();
        assert_eq!(vault.resolve("openai").unwrap().unwrap().1, "key-one");
        // Unknown provider → None.
        assert!(vault.resolve("nope").unwrap().is_none());
    }

    #[test]
    fn revoke_marks_revoked_and_clears_secret() {
        let vault = VaultStore::in_memory().unwrap();
        vault
            .set(
                "anthropic",
                "default",
                CredentialScheme::ApiKey,
                None,
                None,
                "sk-x",
            )
            .unwrap();
        assert!(vault.revoke("anthropic", "default").unwrap());
        // After revoke, the secret is gone and resolve no longer finds an active entry.
        assert!(vault.get("anthropic", "default").unwrap().is_none());
        assert!(vault.resolve("anthropic").unwrap().is_none());
        let entry = &vault.list().unwrap()[0];
        assert_eq!(entry.status, "revoked");
        // Idempotent: revoking again reports not-found.
        assert!(!vault.revoke("anthropic", "default").unwrap());
    }

    #[test]
    fn missing_secret_errors_clearly_via_backend() {
        let vault = VaultStore::in_memory().unwrap();
        // Nothing stored yet → get is None (not an error).
        assert!(vault.get("anthropic", "missing").unwrap().is_none());
    }

    #[test]
    fn hash_secret_is_stable_and_not_the_plaintext() {
        let h1 = hash_secret("vk_abc");
        let h2 = hash_secret("vk_abc");
        assert_eq!(h1, h2, "hash must be deterministic");
        assert_eq!(h1.len(), 64, "sha-256 hex is 64 chars");
        assert!(!h1.contains("vk_abc"));
        assert_ne!(hash_secret("vk_abd"), h1);
        // Known-answer vector (FIPS 180-2 example): pins the hex encoding itself, not just its
        // shape — the table-driven rewrite of hex_encode must reproduce it byte-for-byte.
        assert_eq!(
            hash_secret("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sentinelpass_get_with_missing_binary_errors_cleanly() {
        let vault = SentinelPassVault {
            client_id: "sandhi".into(),
            binary: "definitely-not-on-path-xyz".into(),
        };
        let err = vault.get_secret("p", "l").unwrap_err();
        assert!(matches!(err, VaultError::Backend(_)), "{err:?}");
        // set is not supported.
        assert!(matches!(
            vault.set_secret("p", "l", "s").unwrap_err(),
            VaultError::NotSupported(_)
        ));
    }
}

#[cfg(test)]
mod credstore_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "sandhi-credstore-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn vault_reference_plans_name_the_active_backend_and_searched_names() {
        let dir = temp_dir("reference-plan");
        let backends: Vec<Box<dyn Vault>> = vec![
            Box::new(KeyringVault),
            Box::new(CredentialStoreVault::new(dir.clone())),
            Box::new(InMemoryVault::new()),
        ];

        let plans: Vec<VaultReferencePlan> = backends
            .iter()
            .map(|backend| backend.reference_plan("openai", "default").unwrap())
            .collect();
        assert_eq!(plans[0].backend, "keyring");
        assert_eq!(plans[0].resolution, "os_service_account");
        assert_eq!(
            plans[0].searched_references,
            vec!["service=sandhi;account=openai:default"]
        );
        assert_eq!(plans[1].backend, "credstore");
        assert_eq!(plans[1].resolution, "filesystem_candidates");
        assert_eq!(
            plans[1].searched_references,
            vec![
                dir.join("openai__default").display().to_string(),
                dir.join("openai.default").display().to_string(),
                dir.join("sandhi.openai.default").display().to_string(),
            ]
        );
        assert_eq!(plans[2].backend, "memory");
        assert_eq!(plans[2].searched_references, vec!["openai:default"]);

        for backend in backends {
            assert!(matches!(
                backend.reference_plan("bad/reference", "default"),
                Err(VaultError::InvalidReference)
            ));
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn credstore_reads_trimmed_secret_from_double_underscore_name() {
        let dir = temp_dir("read");
        std::fs::write(dir.join("inferflux__default"), "sk-live-key\n").unwrap();
        let vault = CredentialStoreVault::new(dir.clone());
        assert_eq!(
            vault.get_secret("inferflux", "default").unwrap().as_deref(),
            Some("sk-live-key")
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn credstore_falls_back_to_dotted_cred_name() {
        let dir = temp_dir("dotted");
        std::fs::write(dir.join("sandhi.inferflux.default"), "sk-live").unwrap();
        let vault = CredentialStoreVault::new(dir.clone());
        assert_eq!(
            vault.get_secret("inferflux", "default").unwrap().as_deref(),
            Some("sk-live")
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn credstore_first_existing_candidate_wins_and_dotted_scheme_is_second() {
        let dir = temp_dir("precedence");
        // Both the double-underscore and dotted schemes present: the first
        // candidate must win, pinning the rotation-shadowing semantics.
        std::fs::write(dir.join("inferflux__default"), "sk-old").unwrap();
        std::fs::write(dir.join("inferflux.default"), "sk-new").unwrap();
        std::fs::write(dir.join("sandhi.inferflux.default"), "sk-newest").unwrap();
        let vault = CredentialStoreVault::new(dir.clone());
        assert_eq!(
            vault.get_secret("inferflux", "default").unwrap().as_deref(),
            Some("sk-old")
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn credstore_whitespace_only_file_is_absent_not_a_live_empty_credential() {
        let dir = temp_dir("whitespace");
        std::fs::write(dir.join("inferflux__default"), "\n  \n").unwrap();
        let vault = CredentialStoreVault::new(dir.clone());
        assert_eq!(vault.get_secret("inferflux", "default").unwrap(), None);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn credstore_invalid_utf8_is_a_backend_error() {
        let dir = temp_dir("utf8");
        std::fs::write(dir.join("inferflux__default"), [0xff, 0xfe, 0x41]).unwrap();
        let vault = CredentialStoreVault::new(dir.clone());
        assert!(matches!(
            vault.get_secret("inferflux", "default"),
            Err(VaultError::Backend(_))
        ));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn credstore_missing_file_is_none() {
        let vault = CredentialStoreVault::new(temp_dir("missing"));
        assert_eq!(vault.get_secret("inferflux", "default").unwrap(), None);
    }

    #[test]
    fn credstore_write_and_delete_are_not_supported() {
        let vault = CredentialStoreVault::new(temp_dir("write"));
        assert!(matches!(
            vault.set_secret("inferflux", "default", "x"),
            Err(VaultError::NotSupported(_))
        ));
        assert!(matches!(
            vault.delete_secret("inferflux", "default"),
            Err(VaultError::NotSupported(_))
        ));
    }

    #[test]
    fn credstore_rejects_unsafe_names_without_touching_the_filesystem() {
        let vault = CredentialStoreVault::new(temp_dir("unsafe"));
        for (provider, label) in [
            ("../escape", "default"),
            ("inferflux", "../../etc/passwd"),
            ("inferflux", "a/b"),
            ("", "default"),
            ("inferflux", ""),
        ] {
            assert_eq!(
                vault.get_secret(provider, label).unwrap(),
                None,
                "({provider}, {label}) must resolve to None"
            );
        }
    }

    #[test]
    fn credstore_unreadable_file_is_a_backend_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("unreadable");
        let path = dir.join("inferflux__default");
        std::fs::write(&path, "sk").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let vault = CredentialStoreVault::new(dir.clone());
        assert!(matches!(
            vault.get_secret("inferflux", "default"),
            Err(VaultError::Backend(_))
        ));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn credstore_symlink_candidate_is_skipped_not_followed() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("symlink");
        // The link points at a file OUTSIDE the provisioned directory: if the
        // reader followed it, the outside secret would leak into the vault.
        let outside = dir.parent().unwrap().join("outside-secret.txt");
        std::fs::write(&outside, "leaked").unwrap();
        let link = dir.join("inferflux__default");
        symlink(&outside, &link).unwrap();

        let vault = CredentialStoreVault::new(dir.clone());
        assert!(vault.get_secret("inferflux", "default").unwrap().is_none());
        std::fs::remove_file(&outside).ok();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn backend_from_env_credstore_without_dir_is_an_error_not_an_exit() {
        // Serialize: backend_from_env reads process-global env vars.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("SANDHI_VAULT_BACKEND", "credstore");
        std::env::remove_var("SANDHI_CREDENTIALS_DIR");
        assert!(VaultStore::backend_from_env().is_err());
        std::env::set_var("SANDHI_CREDENTIALS_DIR", "/nonexistent-credstore-dir");
        assert!(VaultStore::backend_from_env().is_err());
        std::env::remove_var("SANDHI_VAULT_BACKEND");
        std::env::remove_var("SANDHI_CREDENTIALS_DIR");
    }
}

#[cfg(test)]
mod user_agent_tests {
    use super::*;

    #[test]
    fn user_agent_round_trips_through_metadata() {
        let vault = VaultStore::in_memory().unwrap();
        vault
            .set(
                "anthropic",
                "codingplan",
                CredentialScheme::Bearer,
                Some("https://upstream.example/apps/anthropic"),
                Some("claude-cli/1.0.0"),
                "synthetic-secret",
            )
            .unwrap();
        let entry = &vault.list().unwrap()[0];
        assert_eq!(entry.user_agent.as_deref(), Some("claude-cli/1.0.0"));
        let (resolved, _secret) = vault
            .resolve("anthropic")
            .unwrap()
            .expect("active credential");
        assert_eq!(resolved.user_agent.as_deref(), Some("claude-cli/1.0.0"));
    }
}
