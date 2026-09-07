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
//! Selection is via `SANDHI_VAULT_BACKEND=keyring|sentinelpass` (default `keyring`). The
//! measure-vs-price boundary is held: no dollars, no SKU/tier — only credentials + neutral token
//! attribution.

use std::collections::HashMap;
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
    /// OAuth/ADC (refresh-token exchange handled by the typed provider runtime).
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VaultEntry {
    pub provider: String,
    pub label: String,
    pub scheme: CredentialScheme,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub created_at: String,
    /// `active` / `revoked`.
    pub status: String,
}

impl VaultEntry {
    /// The stable credential id under which a provider handle is registered: `provider:label`.
    pub fn credential_id(&self) -> String {
        format!("{}:{}", self.provider, self.label)
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

    /// Pick the backend named by `SANDHI_VAULT_BACKEND` (`keyring` default; `sentinelpass`).
    ///
    /// `sentinelpass` selects the native daemon-IPC backend; setting
    /// `SANDHI_SENTINELPASS_FALLBACK_CLI=1` keeps the legacy CLI shell-out.
    pub fn backend_from_env() -> Box<dyn Vault> {
        match std::env::var("SANDHI_VAULT_BACKEND")
            .unwrap_or_else(|_| "keyring".into())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "sentinelpass" => {
                if std::env::var("SANDHI_SENTINELPASS_FALLBACK_CLI").as_deref() == Ok("1") {
                    Box::new(SentinelPassVault::new())
                } else {
                    #[cfg(feature = "sentinelpass-ipc")]
                    {
                        match SentinelPassIpcVault::new() {
                            Ok(backend) => Box::new(backend),
                            Err(_) => {
                                tracing::warn!(
                                    "native vault configuration unavailable; no fallback selected"
                                );
                                Box::new(UnavailableVault)
                            }
                        }
                    }
                    #[cfg(not(feature = "sentinelpass-ipc"))]
                    {
                        tracing::warn!(
                            "SANDHI_VAULT_BACKEND=sentinelpass daemon IPC requires the \
                             `sentinelpass-ipc` feature; no fallback selected"
                        );
                        Box::new(UnavailableVault)
                    }
                }
            }
            "keyring" => Box::new(KeyringVault),
            _ => Box::new(UnavailableVault),
        }
    }

    fn init(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS vault (
                provider   TEXT NOT NULL,
                label      TEXT NOT NULL,
                scheme     TEXT NOT NULL,
                base_url   TEXT,
                created_at TEXT NOT NULL,
                status     TEXT NOT NULL DEFAULT 'active',
                PRIMARY KEY (provider, label)
            );",
        )
    }

    /// Store a credential: writes the secret to the backend and the metadata to SQLite.
    /// Returns the credential id (`provider:label`).
    pub fn set(
        &self,
        provider: &str,
        label: &str,
        scheme: CredentialScheme,
        base_url: Option<&str>,
        secret: &str,
    ) -> Result<String, VaultError> {
        validate_reference(provider, label)?;
        self.backend.set_secret(provider, label, secret)?;
        self.register_metadata(provider, label, scheme, base_url)
    }

    /// Index a reference only after its caller has successfully resolved and validated the secret.
    /// This never writes to the secret backend; registration is not a grant or a rotation.
    pub fn register_metadata(
        &self,
        provider: &str,
        label: &str,
        scheme: CredentialScheme,
        base_url: Option<&str>,
    ) -> Result<String, VaultError> {
        validate_reference(provider, label)?;
        let conn = self.conn.lock().expect("vault conn poisoned");
        conn.execute(
            "INSERT OR REPLACE INTO vault (provider, label, scheme, base_url, created_at, status) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'active')",
            params![provider, label, scheme.as_db(), base_url, now_rfc3339()],
        )
        .map_err(|e| VaultError::Backend(format!("sqlite vault set: {e}")))?;
        Ok(account(provider, label))
    }

    /// Read the raw secret for `provider:label` (from the secret backend).
    pub fn get(&self, provider: &str, label: &str) -> Result<Option<String>, VaultError> {
        self.backend.get_secret(provider, label)
    }

    /// Resolve the **active** credential for a provider (first active label, ordered), returning
    /// `(entry, secret)`. The proxy uses this to build an upstream handle from a provider slug.
    pub fn resolve(&self, provider: &str) -> Result<Option<(VaultEntry, String)>, VaultError> {
        let Some(entry) = self.first_active_entry(provider)? else {
            return Ok(None);
        };
        match self.backend.get_secret(&entry.provider, &entry.label)? {
            Some(secret) => Ok(Some((entry, secret))),
            None => Ok(None),
        }
    }

    /// Masked metadata list (never the secret), all labels.
    pub fn list(&self) -> Result<Vec<VaultEntry>, VaultError> {
        let conn = self.conn.lock().expect("vault conn poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT provider, label, scheme, base_url, created_at, status \
                 FROM vault ORDER BY provider, label",
            )
            .map_err(|e| VaultError::Backend(format!("sqlite vault list: {e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok(VaultEntry {
                    provider: r.get(0)?,
                    label: r.get(1)?,
                    scheme: CredentialScheme::from_db(r.get::<_, String>(2)?.as_str()),
                    base_url: r.get(3)?,
                    created_at: r.get(4)?,
                    status: r.get(5)?,
                })
            })
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
                "SELECT provider, label, scheme, base_url, created_at, status \
                 FROM vault WHERE provider = ?1 AND status = 'active' \
                 ORDER BY created_at LIMIT 1",
            )
            .map_err(|e| VaultError::Backend(format!("sqlite vault get: {e}")))?;
        stmt.query_row(params![provider], |r| {
            Ok(VaultEntry {
                provider: r.get(0)?,
                label: r.get(1)?,
                scheme: CredentialScheme::from_db(r.get::<_, String>(2)?.as_str()),
                base_url: r.get(3)?,
                created_at: r.get(4)?,
                status: r.get(5)?,
            })
        })
        .optional()
        .map_err(|e| VaultError::Backend(format!("sqlite vault get: {e}")))
    }

    /// The configured backend name.
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
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
