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
//!   Credential Manager); [`SentinelPassVault`] reads from the SentinelPass password manager over
//!   its CLI (native daemon IPC is a TODO). [`InMemoryVault`] backs tests.
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
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(m) => write!(f, "vault backend error: {m}"),
            Self::NotSupported(m) => write!(f, "vault operation not supported: {m}"),
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

/// SentinelPass password-manager backend. Talks to the SentinelPass daemon through its CLI
/// (`sentinelpass secret get …`) to keep the coupling loose — there is intentionally **no path
/// dependency on `sentinelpass-core`**. Read-only for now (the CLI exposes no `set`), so secrets
/// are provisioned inside SentinelPass itself and granted to the `sandhi` client; `set_secret`
/// returns [`VaultError::NotSupported`] with guidance.
///
/// TODO(TD-0003): replace the CLI shell-out with native daemon IPC (socket + `IpcMessage`) once a
/// narrow, stable IPC contract is promoted out of `sentinelpass-core`.
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
    pub fn backend_from_env() -> Box<dyn Vault> {
        match std::env::var("SANDHI_VAULT_BACKEND")
            .unwrap_or_else(|_| "keyring".into())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "sentinelpass" => Box::new(SentinelPassVault::new()),
            _ => Box::new(KeyringVault),
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
        self.backend.set_secret(provider, label, secret)?;
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
    pub fn revoke(&self, provider: &str, label: &str) -> Result<bool, VaultError> {
        let _ = self.backend.delete_secret(provider, label)?;
        let conn = self.conn.lock().expect("vault conn poisoned");
        let changed = conn
            .execute(
                "UPDATE vault SET status = 'revoked' WHERE provider = ?1 AND label = ?2 \
                 AND status = 'active'",
                params![provider, label],
            )
            .map_err(|e| VaultError::Backend(format!("sqlite vault revoke: {e}")))?;
        Ok(changed > 0)
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
}

/// SHA-256 hex digest of a presented virtual-key secret. Used as the durable + live lookup key so
/// the plaintext secret is never stored (only its hash). Public so the proxy can hash a presented
/// token before resolving it against the in-memory store.
pub fn hash_secret(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
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
