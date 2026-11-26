//! Postgres credential-vault metadata index — the PG port of
//! [`crate::vault::VaultStore`]. The split concern is identical: **metadata** (which
//! providers/labels exist, scheme, base_url, user agent, created_at, status, generation,
//! SentinelPass bindings/ACL) is durably indexed in the `vault` table here; **secrets**
//! stay in the configured [`crate::Vault`] backend (OS keyring / SentinelPass /
//! credstore) — in PG mode the same database instance hosts the metadata index, and the
//! secret backend selection is unchanged. Errors keep the twin's [`crate::VaultError`]
//! (the `Backend` arm wraps the sqlx message).
use sqlx::PgPool;

use super::{ensure_column, init_ddl, now_rfc3339, OperatorPool};
use crate::vault::{
    validate_exact_binding, validate_reference, CredentialScheme, SecretDeletion,
    SentinelPassExactBinding, SentinelPassGrantAcl, Vault, VaultCapabilities, VaultEntry,
    VaultError, VaultReferencePlan,
};

/// Idempotent schema init: the *original* SQLite column set, then the additive columns
/// the SQLite twin still `ALTER TABLE`s in for pre-existing databases arrive through
/// `information_schema.columns` probes ([`ensure_column`]) — the same evolution steps,
/// column-for-column (recorded decision, mirroring the evidence subsystem's
/// `ensure_column` migrations).
const DDL: &[&str] = &["CREATE TABLE IF NOT EXISTS vault (
        provider   text NOT NULL,
        label      text NOT NULL,
        scheme     text NOT NULL,
        base_url   text,
        created_at text NOT NULL,
        status     text NOT NULL DEFAULT 'active',
        PRIMARY KEY (provider, label)
    )"];

/// The additive evolution steps, in the SQLite twin's order: the per-endpoint transport
/// override, the monotonic generation, then the five SentinelPass exact-binding/ACL
/// columns. `generation` keeps its `> 0` CHECK (the SQLite `typeof` guard is structural
/// on a typed column).
const EVOLVED_COLUMNS: &[(&str, &str)] = &[
    ("user_agent", "text"),
    (
        "generation",
        "bigint NOT NULL DEFAULT 1 CHECK(generation > 0)",
    ),
    ("sentinelpass_entry_id", "bigint"),
    ("sentinelpass_token_env", "text"),
    ("sentinelpass_exe_pinned", "bigint"),
    ("sentinelpass_fingerprint_enrolled", "bigint"),
    ("sentinelpass_expires_at", "bigint"),
];

/// The one SELECT projection shared by every metadata read (`metadata_entry`, `list`,
/// `first_active_entry`), so the column order cannot drift.
const METADATA_COLUMNS: &str = "provider, label, scheme, base_url, user_agent, created_at, \
     status, generation, sentinelpass_entry_id, sentinelpass_token_env, \
     sentinelpass_exe_pinned, sentinelpass_fingerprint_enrolled, sentinelpass_expires_at";

/// The raw 13-column row behind [`VaultEntry`].
type MetadataRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    i64,
    Option<i64>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

/// The operator-facing vault: a Postgres metadata index composed with a secret backend.
pub struct PostgresVaultStore {
    pool: OperatorPool,
    backend: Box<dyn Vault>,
}

impl PostgresVaultStore {
    /// Open the metadata index on `dsn` with the given secret backend.
    ///
    /// Must be called on a `tokio` runtime; that runtime's handle is retained for every
    /// later blocking statement.
    pub async fn connect(dsn: &str, backend: Box<dyn Vault>) -> Result<Self, VaultError> {
        let options: sqlx::postgres::PgConnectOptions = dsn
            .parse()
            .map_err(|e| VaultError::Backend(format!("vault connect: invalid DSN: {e}")))?;
        Self::connect_with(options, backend).await
    }

    /// Like [`Self::connect`] from pre-parsed connect options: the startup wiring sets
    /// `application_name` (and the test suite points `search_path` at a per-test schema).
    pub async fn connect_with(
        options: sqlx::postgres::PgConnectOptions,
        backend: Box<dyn Vault>,
    ) -> Result<Self, VaultError> {
        let pool = OperatorPool::connect_with(options, 2)
            .await
            .map_err(|e| VaultError::Backend(format!("vault connect: {e}")))?;
        init_schema(pool.pool())
            .await
            .map_err(|e| VaultError::Backend(format!("vault init: {e}")))?;
        Ok(Self { pool, backend })
    }

    /// Store a credential: writes the secret to the backend and the metadata to Postgres.
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

    /// Index a reference only after its caller has successfully resolved and validated
    /// the secret. This never writes to the secret backend; registration is not a grant
    /// or a rotation.
    // The seven domain fields describe one vault row; collapsing them into a request DTO
    // would move the same coupling to every existing store caller without improving
    // validation (same shape as the SQLite twin).
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
        self.block(async {
            sqlx::query(
                "INSERT INTO vault (provider, label, scheme, base_url, user_agent, created_at, \
                 status, generation, sentinelpass_entry_id, sentinelpass_token_env, \
                 sentinelpass_exe_pinned, sentinelpass_fingerprint_enrolled, \
                 sentinelpass_expires_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, 'active', 1, $7, $8, $9, $10, $11) \
                 ON CONFLICT(provider, label) DO UPDATE SET scheme=excluded.scheme, \
                 base_url=excluded.base_url, user_agent=excluded.user_agent, \
                 created_at=excluded.created_at, status='active', generation=vault.generation+1, \
                 sentinelpass_entry_id=excluded.sentinelpass_entry_id, \
                 sentinelpass_token_env=excluded.sentinelpass_token_env, \
                 sentinelpass_exe_pinned=excluded.sentinelpass_exe_pinned, \
                 sentinelpass_fingerprint_enrolled=excluded.sentinelpass_fingerprint_enrolled, \
                 sentinelpass_expires_at=excluded.sentinelpass_expires_at",
            )
            .bind(provider)
            .bind(label)
            .bind(scheme.as_db())
            .bind(base_url)
            .bind(user_agent)
            .bind(now_rfc3339())
            .bind(exact.as_ref().map(|binding| binding.entry_id))
            .bind(exact.as_ref().map(|binding| binding.token_env.clone()))
            .bind(exe_pinned)
            .bind(fingerprint_enrolled)
            .bind(expires_at)
            .execute(self.pool.pool())
            .await
            .map_err(|e| VaultError::Backend(format!("postgres vault set: {e}")))
        })?;
        Ok(format!("{provider}:{label}"))
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

    /// Resolve the **active** credential for a provider (first active label, ordered),
    /// returning `(entry, secret)`.
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
        self.block(async {
            let row = sqlx::query_as::<_, MetadataRow>(&format!(
                "SELECT {METADATA_COLUMNS} FROM vault WHERE provider=$1 AND label=$2"
            ))
            .bind(provider)
            .bind(label)
            .fetch_optional(self.pool.pool())
            .await
            .map_err(|e| VaultError::Backend(format!("postgres vault metadata: {e}")))?;
            row.map(read_vault_entry).transpose()
        })
    }

    /// Masked metadata list (never the secret), all labels.
    pub fn list(&self) -> Result<Vec<VaultEntry>, VaultError> {
        self.block(async {
            let rows = sqlx::query_as::<_, MetadataRow>(&format!(
                "SELECT {METADATA_COLUMNS} FROM vault ORDER BY provider, label"
            ))
            .fetch_all(self.pool.pool())
            .await
            .map_err(|e| VaultError::Backend(format!("postgres vault list: {e}")))?;
            rows.into_iter().map(read_vault_entry).collect()
        })
    }

    /// Revoke (soft-delete) a credential: marks metadata `revoked` and deletes the
    /// secret. Backends that cannot delete only fail the secret deletion — the metadata
    /// revocation proceeds.
    pub fn revoke(&self, provider: &str, label: &str) -> Result<bool, VaultError> {
        self.revoke_with_status(provider, label)
            .map(|(revoked, _)| revoked)
    }

    /// Commit local revocation before attempting secret deletion. Unsupported brokers
    /// receive no delete request. Report cleanup independently; neither step revokes a
    /// provider API key.
    pub fn revoke_with_status(
        &self,
        provider: &str,
        label: &str,
    ) -> Result<(bool, SecretDeletion), VaultError> {
        let changed = self.block(async {
            sqlx::query(
                "UPDATE vault SET status = 'revoked' WHERE provider = $1 AND label = $2 \
                 AND status = 'active'",
            )
            .bind(provider)
            .bind(label)
            .execute(self.pool.pool())
            .await
            .map(|result| result.rows_affected())
            .map_err(|e| VaultError::Backend(format!("postgres vault revoke: {e}")))
        })?;
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
        self.block(async {
            let row = sqlx::query_as::<_, MetadataRow>(&format!(
                "SELECT {METADATA_COLUMNS} FROM vault WHERE provider = $1 AND status = 'active' \
                 ORDER BY created_at LIMIT 1"
            ))
            .bind(provider)
            .fetch_optional(self.pool.pool())
            .await
            .map_err(|e| VaultError::Backend(format!("postgres vault get: {e}")))?;
            row.map(read_vault_entry).transpose()
        })
    }

    /// The configured backend name.
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Describe the non-secret names this configured backend searches for an exact
    /// reference.
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

    fn block<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
        self.pool.block(fut)
    }
}

/// Create the table and evolve it with the SQLite twin's additive columns.
async fn init_schema(pool: &PgPool) -> Result<(), sqlx::Error> {
    init_ddl(pool, DDL).await?;
    for (column, declaration) in EVOLVED_COLUMNS {
        ensure_column(pool, "vault", column, declaration).await?;
    }
    Ok(())
}

/// Decode one metadata row with the same fail-closed rules as the SQLite twin: the
/// SentinelPass exact-binding pair and the grant ACL must be complete or the read is a
/// backend error, and the generation must be positive.
fn read_vault_entry(row: MetadataRow) -> Result<VaultEntry, VaultError> {
    let (provider, label, scheme, base_url, user_agent, created_at, status, generation) =
        (row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7);
    let sentinelpass_entry_id = row.8;
    let sentinelpass_token_env = row.9;
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
    let exe_pinned = row.10;
    let fingerprint_enrolled = row.11;
    let expires_at = row.12;
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
    if generation <= 0 {
        return Err(invalid_vault_metadata("generation"));
    }
    Ok(VaultEntry {
        provider,
        label,
        scheme: CredentialScheme::from_db(&scheme),
        base_url,
        user_agent,
        created_at,
        status,
        generation: generation as u64,
        sentinelpass_entry_id,
        sentinelpass_token_env,
        sentinelpass_grant_acl,
    })
}

fn invalid_vault_metadata(column: &str) -> VaultError {
    VaultError::Backend(format!("vault metadata: invalid {column}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A synthetic exact-entry backend (the SQLite tests' fixture): reports the
    /// `sentinelpass-ipc` backend name and serves synthetic secrets.
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
                delete: true,
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

    fn pg_vault(tag: &str) -> (String, PostgresVaultStore) {
        super::super::pg_test::connect_in_schema(tag, |options| {
            PostgresVaultStore::connect_with(options, Box::new(crate::vault::InMemoryVault::new()))
        })
    }

    fn drop_schema(schema: &str) {
        super::super::pg_test::drop_schema_best_effort(schema)
    }

    // ── Non-DB dialect tests (run wherever the `postgres` feature compiles). ──

    #[test]
    fn ddl_starts_with_the_original_columns_and_evolves_additively() {
        let joined = DDL.join("\n");
        assert!(joined.contains("CREATE TABLE IF NOT EXISTS vault"));
        assert!(joined.contains("PRIMARY KEY (provider, label)"));
        assert!(
            !joined.contains("user_agent"),
            "user_agent arrives via the probe"
        );
        assert!(
            !joined.contains("generation"),
            "generation arrives via the probe"
        );
        assert!(!joined.contains("INSERT OR"));
        // The evolved set mirrors the SQLite twin's ALTER order, generation last-CHECKed.
        let names: Vec<&str> = EVOLVED_COLUMNS.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            [
                "user_agent",
                "generation",
                "sentinelpass_entry_id",
                "sentinelpass_token_env",
                "sentinelpass_exe_pinned",
                "sentinelpass_fingerprint_enrolled",
                "sentinelpass_expires_at",
            ]
        );
        assert!(EVOLVED_COLUMNS
            .iter()
            .any(|(name, decl)| *name == "generation" && decl.contains("CHECK(generation > 0)")));
    }

    // ── Live-Postgres tests: SANDHI_TEST_PG_DSN, per-test schema isolation. ──

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn metadata_round_trip_and_masked_listing() {
        let (schema, store) = pg_vault("vault-crud");
        let id = store
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
        // The in-memory test backend holds the secret.
        assert_eq!(
            store.get("anthropic", "default").unwrap().as_deref(),
            Some("sk-secret-123456")
        );

        let listed = store.list().unwrap();
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
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn generation_advances_survives_reopen_and_revocation_clears_activation() {
        let (schema, store) = pg_vault("vault-generation");
        store
            .set(
                "openai",
                "fixture",
                CredentialScheme::ApiKey,
                None,
                None,
                "first",
            )
            .unwrap();
        store
            .set(
                "openai",
                "fixture",
                CredentialScheme::ApiKey,
                None,
                None,
                "rotated",
            )
            .unwrap();
        assert_eq!(
            store
                .metadata_entry("openai", "fixture")
                .unwrap()
                .unwrap()
                .generation,
            2
        );

        let reopened = super::super::pg_test::connect_in_schema("vault-generation", |options| {
            PostgresVaultStore::connect_with(options, Box::new(crate::vault::InMemoryVault::new()))
        })
        .1;
        assert_eq!(
            reopened
                .metadata_entry("openai", "fixture")
                .unwrap()
                .unwrap()
                .generation,
            2,
            "generation survives restart"
        );

        assert!(reopened.revoke("openai", "fixture").unwrap());
        assert!(reopened.resolve("openai").unwrap().is_none());
        assert_eq!(reopened.list().unwrap()[0].status, "revoked");
        assert!(!reopened.revoke("openai", "fixture").unwrap());
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn exact_entry_metadata_routes_retrieval_and_direct_secrets_clear_it() {
        let (schema, store) = pg_vault_with_backend("vault-exact");
        let binding = SentinelPassExactBinding::new(42, "SANDHI_TEST_SERVICE_TOKEN").unwrap();
        store
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
        let entry = store.metadata_entry("openai", "default").unwrap().unwrap();
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
            store.get("openai", "default").unwrap().as_deref(),
            Some("synthetic-entry-42")
        );
        let plan = store.reference_plan("openai", "default").unwrap();
        assert_eq!(plan.resolution, "exact_entry");
        assert_eq!(plan.searched_references, vec!["entry:42"]);

        store
            .set(
                "openai",
                "default",
                CredentialScheme::ApiKey,
                None,
                None,
                "synthetic-direct-key",
            )
            .unwrap();
        let rotated = store.metadata_entry("openai", "default").unwrap().unwrap();
        assert_eq!(rotated.exact_binding().unwrap(), None);
        assert_eq!(rotated.grant_acl().unwrap(), None);
        assert_eq!(
            store.get("openai", "default").unwrap().as_deref(),
            Some("synthetic-legacy-key")
        );
        drop_schema(&schema);
    }

    fn pg_vault_with_backend(tag: &str) -> (String, PostgresVaultStore) {
        let (schema, options) = super::super::pg_test::scoped_options(tag);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let store = rt
            .block_on(PostgresVaultStore::connect_with(
                options,
                Box::new(SyntheticExactBackend {
                    exact_calls: Mutex::new(0),
                }),
            ))
            .map(|store| PostgresVaultStore {
                backend: Box::new(SyntheticExactBackend {
                    exact_calls: Mutex::new(0),
                }),
                ..store
            })
            .expect("connect vault store");
        std::mem::forget(rt);
        (schema, store)
    }
}
