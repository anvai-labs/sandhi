//! Object-safe operator-store seams (Phase 3 PR-D) — the trait objects the proxy's
//! `ProxyState` holds so one startup wiring can select the SQLite store (default) or the
//! Postgres twin (`SANDHI_STORE_DSN`, `postgres` feature) without branching at every
//! consumer site.
//!
//! Store for store, method for method: each trait mirrors only the surface the proxy
//! actually calls, and both the SQLite store and its `operator_pg` twin implement it.
//! Error seams stay the SQLite modules' own types — they are the types the proxy already
//! maps to HTTP statuses — and the PG impls convert through the lossless conversions
//! recorded in [`crate::operator_pg`]:
//!
//! - vault / references: the error enum is already shared verbatim.
//! - virtual keys / profiles / alerts / policy receipts: the SQLite seam widens from
//!   `rusqlite::Result` to [`OperatorStoreError`] at the trait boundary (identical
//!   `Display` text; `From<rusqlite::Error>` exists).
//! - backends / routes: the PG twins' mirrored enums convert back onto the SQLite
//!   enum; the driver-detail `Database` arm renders the identical Display string
//!   (the inner driver error is never surfaced through these enums).
//!
//! These traits are an internal wiring seam, not a stability boundary: the inherent
//! store methods remain the documented API, and a trait method may be dropped when the
//! proxy stops calling it.

use std::time::Instant;

use crate::alerts::{AlertRuleRecord, AlertStore, CreateAlertRequest};
use crate::backends::{BackendDefinition, BackendRecord, BackendStore, BackendStoreError};
use crate::policy::PolicyAuditStore;
use crate::profiles::{ProfileRecord, ProfileStore};
use crate::references::{Reference, ReferenceStore};
use crate::routes::{Pin, RouteDefinition, RouteRecord, RouteStore};
use crate::vkeys::{MintRequest, MintedKey, VirtualKeyRecord, VirtualKeyStore};
use crate::vault::{
    CredentialScheme, SecretDeletion, SentinelPassExactBinding, VaultCapabilities, VaultEntry,
    VaultError, VaultReferencePlan, VaultStore,
};
use crate::OperatorStoreError;

// ── TD-0003 P1: provider-credential vault (metadata index) ──────────────────────────

/// The durable provider-credential vault seam: [`VaultStore`] (default) or
/// [`crate::operator_pg::PostgresVaultStore`] (`SANDHI_STORE_DSN`). Metadata only —
/// credential bytes always live in the active [`crate::vault::Vault`] secret backend.
pub trait DynVaultStore: Send + Sync {
    fn set(
        &self,
        provider: &str,
        label: &str,
        scheme: CredentialScheme,
        base_url: Option<&str>,
        user_agent: Option<&str>,
        secret: &str,
    ) -> Result<String, VaultError>;
    #[allow(clippy::too_many_arguments)]
    fn register_metadata(
        &self,
        provider: &str,
        label: &str,
        scheme: CredentialScheme,
        base_url: Option<&str>,
        user_agent: Option<&str>,
        exact: Option<SentinelPassExactBinding>,
        grant_acl: Option<crate::vault::SentinelPassGrantAcl>,
    ) -> Result<String, VaultError>;
    fn get(&self, provider: &str, label: &str) -> Result<Option<String>, VaultError>;
    fn get_exact(
        &self,
        provider: &str,
        label: &str,
        binding: &SentinelPassExactBinding,
    ) -> Result<Option<String>, VaultError>;
    fn resolve(&self, provider: &str) -> Result<Option<(VaultEntry, String)>, VaultError>;
    fn metadata_entry(&self, provider: &str, label: &str)
        -> Result<Option<VaultEntry>, VaultError>;
    fn list(&self) -> Result<Vec<VaultEntry>, VaultError>;
    fn revoke(&self, provider: &str, label: &str) -> Result<bool, VaultError>;
    fn revoke_with_status(
        &self,
        provider: &str,
        label: &str,
    ) -> Result<(bool, SecretDeletion), VaultError>;
    fn backend_name(&self) -> &'static str;
    fn reference_plan(&self, provider: &str, label: &str) -> Result<VaultReferencePlan, VaultError>;
    fn reference_plan_for_exact(
        &self,
        provider: &str,
        label: &str,
        binding: &SentinelPassExactBinding,
    ) -> Result<VaultReferencePlan, VaultError>;
    fn capabilities(&self) -> VaultCapabilities;
}

impl DynVaultStore for VaultStore {
    fn set(
        &self,
        provider: &str,
        label: &str,
        scheme: CredentialScheme,
        base_url: Option<&str>,
        user_agent: Option<&str>,
        secret: &str,
    ) -> Result<String, VaultError> {
        VaultStore::set(self, provider, label, scheme, base_url, user_agent, secret)
    }
    fn register_metadata(
        &self,
        provider: &str,
        label: &str,
        scheme: CredentialScheme,
        base_url: Option<&str>,
        user_agent: Option<&str>,
        exact: Option<SentinelPassExactBinding>,
        grant_acl: Option<crate::vault::SentinelPassGrantAcl>,
    ) -> Result<String, VaultError> {
        VaultStore::register_metadata(
            self,
            provider,
            label,
            scheme,
            base_url,
            user_agent,
            exact,
            grant_acl,
        )
    }
    fn get(&self, provider: &str, label: &str) -> Result<Option<String>, VaultError> {
        VaultStore::get(self, provider, label)
    }
    fn get_exact(
        &self,
        provider: &str,
        label: &str,
        binding: &SentinelPassExactBinding,
    ) -> Result<Option<String>, VaultError> {
        VaultStore::get_exact(self, provider, label, binding)
    }
    fn resolve(&self, provider: &str) -> Result<Option<(VaultEntry, String)>, VaultError> {
        VaultStore::resolve(self, provider)
    }
    fn metadata_entry(
        &self,
        provider: &str,
        label: &str,
    ) -> Result<Option<VaultEntry>, VaultError> {
        VaultStore::metadata_entry(self, provider, label)
    }
    fn list(&self) -> Result<Vec<VaultEntry>, VaultError> {
        VaultStore::list(self)
    }
    fn revoke(&self, provider: &str, label: &str) -> Result<bool, VaultError> {
        VaultStore::revoke(self, provider, label)
    }
    fn revoke_with_status(
        &self,
        provider: &str,
        label: &str,
    ) -> Result<(bool, SecretDeletion), VaultError> {
        VaultStore::revoke_with_status(self, provider, label)
    }
    fn backend_name(&self) -> &'static str {
        VaultStore::backend_name(self)
    }
    fn reference_plan(&self, provider: &str, label: &str) -> Result<VaultReferencePlan, VaultError> {
        VaultStore::reference_plan(self, provider, label)
    }
    fn reference_plan_for_exact(
        &self,
        provider: &str,
        label: &str,
        binding: &SentinelPassExactBinding,
    ) -> Result<VaultReferencePlan, VaultError> {
        VaultStore::reference_plan_for_exact(self, provider, label, binding)
    }
    fn capabilities(&self) -> VaultCapabilities {
        VaultStore::capabilities(self)
    }
}

// ── TD-0003 P1: durable virtual-key store (hashes + scope) ──────────────────────────

/// The durable virtual-key store seam: [`VirtualKeyStore`] or
/// [`crate::operator_pg::PostgresVirtualKeyStore`]. Errors are [`OperatorStoreError`] on
/// both arms (the SQLite seam widens from `rusqlite::Result` at this boundary).
pub trait DynVirtualKeyStore: Send + Sync {
    fn mint(&self, req: MintRequest) -> Result<MintedKey, OperatorStoreError>;
    fn mint_delegated(
        &self,
        req: MintRequest,
        identity_json: &str,
    ) -> Result<MintedKey, OperatorStoreError>;
    fn delegation(&self, id: &str) -> Result<Option<String>, OperatorStoreError>;
    fn find_by_secret(
        &self,
        presented_secret: &str,
    ) -> Result<Option<VirtualKeyRecord>, OperatorStoreError>;
    fn find_by_hash(&self, hash: &str) -> Result<Option<VirtualKeyRecord>, OperatorStoreError>;
    fn find_by_id(&self, id: &str) -> Result<Option<VirtualKeyRecord>, OperatorStoreError>;
    fn list(&self) -> Result<Vec<VirtualKeyRecord>, OperatorStoreError>;
    fn revoke(&self, id: &str) -> Result<bool, OperatorStoreError>;
}

impl DynVirtualKeyStore for VirtualKeyStore {
    fn mint(&self, req: MintRequest) -> Result<MintedKey, OperatorStoreError> {
        VirtualKeyStore::mint(self, req).map_err(OperatorStoreError::from)
    }
    fn mint_delegated(
        &self,
        req: MintRequest,
        identity_json: &str,
    ) -> Result<MintedKey, OperatorStoreError> {
        VirtualKeyStore::mint_delegated(self, req, identity_json).map_err(OperatorStoreError::from)
    }
    fn delegation(&self, id: &str) -> Result<Option<String>, OperatorStoreError> {
        VirtualKeyStore::delegation(self, id).map_err(OperatorStoreError::from)
    }
    fn find_by_secret(
        &self,
        presented_secret: &str,
    ) -> Result<Option<VirtualKeyRecord>, OperatorStoreError> {
        VirtualKeyStore::find_by_secret(self, presented_secret).map_err(OperatorStoreError::from)
    }
    fn find_by_hash(&self, hash: &str) -> Result<Option<VirtualKeyRecord>, OperatorStoreError> {
        VirtualKeyStore::find_by_hash(self, hash).map_err(OperatorStoreError::from)
    }
    fn find_by_id(&self, id: &str) -> Result<Option<VirtualKeyRecord>, OperatorStoreError> {
        VirtualKeyStore::find_by_id(self, id).map_err(OperatorStoreError::from)
    }
    fn list(&self) -> Result<Vec<VirtualKeyRecord>, OperatorStoreError> {
        VirtualKeyStore::list(self).map_err(OperatorStoreError::from)
    }
    fn revoke(&self, id: &str) -> Result<bool, OperatorStoreError> {
        VirtualKeyStore::revoke(self, id).map_err(OperatorStoreError::from)
    }
}

// ── Gateway profiles (Phase 5) ──────────────────────────────────────────────────────

/// The gateway-profile store seam: [`ProfileStore`] or
/// [`crate::operator_pg::PostgresProfileStore`].
pub trait DynProfileStore: Send + Sync {
    fn upsert_profile(&self, record: &ProfileRecord) -> Result<(), OperatorStoreError>;
    fn profile_for_group(&self, group: &str)
        -> Result<Option<ProfileRecord>, OperatorStoreError>;
    fn list_profiles(&self) -> Result<Vec<ProfileRecord>, OperatorStoreError>;
    fn mint_profile_token(
        &self,
        subject: &str,
        group: &str,
    ) -> Result<String, OperatorStoreError>;
    fn lookup_active_profile_token(
        &self,
        presented: &str,
    ) -> Result<Option<(String, String)>, OperatorStoreError>;
    fn revoke_profile_tokens(
        &self,
        subject: &str,
        group: &str,
    ) -> Result<usize, OperatorStoreError>;
}

impl DynProfileStore for ProfileStore {
    fn upsert_profile(&self, record: &ProfileRecord) -> Result<(), OperatorStoreError> {
        ProfileStore::upsert_profile(self, record).map_err(OperatorStoreError::from)
    }
    fn profile_for_group(
        &self,
        group: &str,
    ) -> Result<Option<ProfileRecord>, OperatorStoreError> {
        ProfileStore::profile_for_group(self, group).map_err(OperatorStoreError::from)
    }
    fn list_profiles(&self) -> Result<Vec<ProfileRecord>, OperatorStoreError> {
        ProfileStore::list_profiles(self).map_err(OperatorStoreError::from)
    }
    fn mint_profile_token(
        &self,
        subject: &str,
        group: &str,
    ) -> Result<String, OperatorStoreError> {
        ProfileStore::mint_profile_token(self, subject, group).map_err(OperatorStoreError::from)
    }
    fn lookup_active_profile_token(
        &self,
        presented: &str,
    ) -> Result<Option<(String, String)>, OperatorStoreError> {
        ProfileStore::lookup_active_profile_token(self, presented)
            .map_err(OperatorStoreError::from)
    }
    fn revoke_profile_tokens(
        &self,
        subject: &str,
        group: &str,
    ) -> Result<usize, OperatorStoreError> {
        ProfileStore::revoke_profile_tokens(self, subject, group).map_err(OperatorStoreError::from)
    }
}

// ── TD-0003 P2: alert rules ─────────────────────────────────────────────────────────

/// The durable alert-rule store seam: [`AlertStore`] or
/// [`crate::operator_pg::PostgresAlertStore`].
pub trait DynAlertStore: Send + Sync {
    fn create(&self, req: CreateAlertRequest) -> Result<AlertRuleRecord, OperatorStoreError>;
    fn list(&self) -> Result<Vec<AlertRuleRecord>, OperatorStoreError>;
    fn list_by_scope(&self, scope: &str) -> Result<Vec<AlertRuleRecord>, OperatorStoreError>;
    fn find_by_id(&self, id: &str) -> Result<Option<AlertRuleRecord>, OperatorStoreError>;
    fn mark_fired(&self, id: &str) -> Result<bool, OperatorStoreError>;
    fn ack(&self, id: &str) -> Result<bool, OperatorStoreError>;
    fn delete(&self, id: &str) -> Result<bool, OperatorStoreError>;
}

impl DynAlertStore for AlertStore {
    fn create(&self, req: CreateAlertRequest) -> Result<AlertRuleRecord, OperatorStoreError> {
        AlertStore::create(self, req).map_err(OperatorStoreError::from)
    }
    fn list(&self) -> Result<Vec<AlertRuleRecord>, OperatorStoreError> {
        AlertStore::list(self).map_err(OperatorStoreError::from)
    }
    fn list_by_scope(&self, scope: &str) -> Result<Vec<AlertRuleRecord>, OperatorStoreError> {
        AlertStore::list_by_scope(self, scope).map_err(OperatorStoreError::from)
    }
    fn find_by_id(&self, id: &str) -> Result<Option<AlertRuleRecord>, OperatorStoreError> {
        AlertStore::find_by_id(self, id).map_err(OperatorStoreError::from)
    }
    fn mark_fired(&self, id: &str) -> Result<bool, OperatorStoreError> {
        AlertStore::mark_fired(self, id).map_err(OperatorStoreError::from)
    }
    fn ack(&self, id: &str) -> Result<bool, OperatorStoreError> {
        AlertStore::ack(self, id).map_err(OperatorStoreError::from)
    }
    fn delete(&self, id: &str) -> Result<bool, OperatorStoreError> {
        AlertStore::delete(self, id).map_err(OperatorStoreError::from)
    }
}

// ── Revisioned backend definitions ──────────────────────────────────────────────────

/// The backend-definition store seam: [`BackendStore`] or
/// [`crate::operator_pg::PostgresBackendStore`]. Both arms return the SQLite module's
/// [`BackendStoreError`]; the PG twin's mirrored enum converts losslessly (see
/// [`crate::operator_pg::backends`]).
pub trait DynBackendStore: Send + Sync {
    fn get(&self, id: &str) -> Result<Option<BackendRecord>, BackendStoreError>;
    fn list(&self) -> Result<Vec<BackendRecord>, BackendStoreError>;
    fn put(
        &self,
        definition: &BackendDefinition,
        expected_revision: Option<u64>,
    ) -> Result<BackendRecord, BackendStoreError>;
    fn revoke(&self, id: &str, expected_revision: u64)
        -> Result<BackendRecord, BackendStoreError>;
}

impl DynBackendStore for BackendStore {
    fn get(&self, id: &str) -> Result<Option<BackendRecord>, BackendStoreError> {
        BackendStore::get(self, id)
    }
    fn list(&self) -> Result<Vec<BackendRecord>, BackendStoreError> {
        BackendStore::list(self)
    }
    fn put(
        &self,
        definition: &BackendDefinition,
        expected_revision: Option<u64>,
    ) -> Result<BackendRecord, BackendStoreError> {
        BackendStore::put(self, definition, expected_revision)
    }
    fn revoke(
        &self,
        id: &str,
        expected_revision: u64,
    ) -> Result<BackendRecord, BackendStoreError> {
        BackendStore::revoke(self, id, expected_revision)
    }
}

// ── Logical routes + session affinity ───────────────────────────────────────────────

/// The logical-route store seam: [`RouteStore`] or
/// [`crate::operator_pg::PostgresRouteStore`]. Both arms return the SQLite module's
/// error enum (see [`crate::operator_pg::routes`]).
pub trait DynRouteStore: Send + Sync {
    fn get(&self, id: &str) -> Result<Option<RouteRecord>, crate::routes::Error>;
    fn list(&self) -> Result<Vec<RouteRecord>, crate::routes::Error>;
    fn put(
        &self,
        definition: &RouteDefinition,
        expected: Option<u64>,
    ) -> Result<RouteRecord, crate::routes::Error>;
    fn revoke(&self, id: &str, expected: u64) -> Result<RouteRecord, crate::routes::Error>;
    fn pin(&self, namespace: &str, route_id: &str, now: i64)
        -> Result<Option<Pin>, crate::routes::Error>;
    fn confirm(
        &self,
        route: &RouteRecord,
        pin: Option<&Pin>,
        now: i64,
    ) -> Result<(), crate::routes::Error>;
}

impl DynRouteStore for RouteStore {
    fn get(&self, id: &str) -> Result<Option<RouteRecord>, crate::routes::Error> {
        RouteStore::get(self, id)
    }
    fn list(&self) -> Result<Vec<RouteRecord>, crate::routes::Error> {
        RouteStore::list(self)
    }
    fn put(
        &self,
        definition: &RouteDefinition,
        expected: Option<u64>,
    ) -> Result<RouteRecord, crate::routes::Error> {
        RouteStore::put(self, definition, expected)
    }
    fn revoke(&self, id: &str, expected: u64) -> Result<RouteRecord, crate::routes::Error> {
        RouteStore::revoke(self, id, expected)
    }
    fn pin(
        &self,
        namespace: &str,
        route_id: &str,
        now: i64,
    ) -> Result<Option<Pin>, crate::routes::Error> {
        RouteStore::pin(self, namespace, route_id, now)
    }
    fn confirm(
        &self,
        route: &RouteRecord,
        pin: Option<&Pin>,
        now: i64,
    ) -> Result<(), crate::routes::Error> {
        RouteStore::confirm(self, route, pin, now)
    }
}

// ── ADR-0044: issued native references (the reference registry) ─────────────────────

/// The native-reference registry seam: [`ReferenceStore`] (file-backed, opened on the
/// original ledger) or [`crate::operator_pg::PostgresReferenceStore`] (the PG ledger's
/// registry). Both arms share [`crate::references::Error`] verbatim.
pub trait DynReferenceStore: Send + Sync {
    fn authorize(&self, binding: &str, owner: &str, refs: &[Reference]) -> Result<(), crate::references::Error>;
    fn authorize_until(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        deadline: &dyn Fn() -> Instant,
    ) -> Result<(), crate::references::Error>;
    fn issue(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        expires_at: i64,
    ) -> Result<(), crate::references::Error>;
    fn issue_until(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        expires_at: i64,
        deadline: &dyn Fn() -> Instant,
    ) -> Result<(), crate::references::Error>;
}

impl DynReferenceStore for ReferenceStore {
    fn authorize(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
    ) -> Result<(), crate::references::Error> {
        ReferenceStore::authorize(self, binding, owner, refs)
    }
    fn authorize_until(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        deadline: &dyn Fn() -> Instant,
    ) -> Result<(), crate::references::Error> {
        ReferenceStore::authorize_until(self, binding, owner, refs, || deadline())
    }
    fn issue(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        expires_at: i64,
    ) -> Result<(), crate::references::Error> {
        ReferenceStore::issue(self, binding, owner, refs, expires_at)
    }
    fn issue_until(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        expires_at: i64,
        deadline: &dyn Fn() -> Instant,
    ) -> Result<(), crate::references::Error> {
        ReferenceStore::issue_until(self, binding, owner, refs, expires_at, || deadline())
    }
}

// ── TD-0032: policy admission receipts ──────────────────────────────────────────────

/// The policy-receipt audit store seam: [`PolicyAuditStore`] or
/// [`crate::operator_pg::PostgresPolicyAuditStore`].
pub trait DynPolicyAuditStore: Send + Sync {
    fn summary(&self) -> Result<serde_json::Value, OperatorStoreError>;
    fn record(
        &self,
        identity: &sandhi_core::policy::Identity,
        key_id: &str,
        decision: &sandhi_core::policy::Decision,
    ) -> Result<String, OperatorStoreError>;
}

impl DynPolicyAuditStore for PolicyAuditStore {
    fn summary(&self) -> Result<serde_json::Value, OperatorStoreError> {
        PolicyAuditStore::summary(self).map_err(OperatorStoreError::from)
    }
    fn record(
        &self,
        identity: &sandhi_core::policy::Identity,
        key_id: &str,
        decision: &sandhi_core::policy::Decision,
    ) -> Result<String, OperatorStoreError> {
        PolicyAuditStore::record(self, identity, key_id, decision).map_err(OperatorStoreError::from)
    }
}

// ── Postgres twins (`postgres` feature) ─────────────────────────────────────────────

#[cfg(feature = "postgres")]
mod pg_impls {
    use std::sync::Arc;
    use std::time::Instant;

    use super::{
        DynAlertStore, DynBackendStore, DynPolicyAuditStore, DynProfileStore, DynReferenceStore,
        DynRouteStore, DynVaultStore, DynVirtualKeyStore,
    };
    use crate::alerts::{AlertRuleRecord, CreateAlertRequest};
    use crate::backends::{BackendDefinition, BackendRecord, BackendStoreError};
    use crate::operator_pg::{
        PostgresAlertStore, PostgresBackendStore, PostgresPolicyAuditStore, PostgresProfileStore,
        PostgresReferenceStore, PostgresRouteStore, PostgresVaultStore, PostgresVirtualKeyStore,
    };
    use crate::profiles::ProfileRecord;
    use crate::references::Reference;
    use crate::routes::{Pin, RouteDefinition, RouteRecord};
    use crate::vkeys::{MintRequest, MintedKey, VirtualKeyRecord};
    use crate::vault::{
        CredentialScheme, SecretDeletion, SentinelPassExactBinding, VaultCapabilities, VaultEntry,
        VaultError, VaultReferencePlan,
    };
    use crate::routes::Error as RouteError;
    use crate::OperatorStoreError;

    /// The PG backend twin's mirrored enum converts back onto the SQLite module's enum —
    /// the seam the proxy maps to HTTP statuses. Only the driver-detail `Database` arm has
    /// no shared inner type; its Display text is identical, so the driver error rides as
    /// the wrapped cause of the equivalent rusqlite arm (never surfaced through this enum).
    impl From<crate::operator_pg::backends::BackendStoreError> for BackendStoreError {
        fn from(error: crate::operator_pg::backends::BackendStoreError) -> Self {
            use crate::operator_pg::backends::BackendStoreError as Pg;
            match error {
                Pg::Invalid(message) => Self::Invalid(message),
                Pg::Conflict => Self::Conflict,
                Pg::NotFound => Self::NotFound,
                Pg::Capacity => Self::Capacity,
                Pg::Corrupt => Self::Corrupt,
                Pg::Database(error) => {
                    Self::Database(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
                }
            }
        }
    }

    /// Same conversion for the mirrored route-store enum.
    impl From<crate::operator_pg::routes::Error> for RouteError {
        fn from(error: crate::operator_pg::routes::Error) -> Self {
            use crate::operator_pg::routes::Error as Pg;
            match error {
                Pg::Invalid(message) => Self::Invalid(message),
                Pg::Conflict => Self::Conflict,
                Pg::NotFound => Self::NotFound,
                Pg::Capacity => Self::Capacity,
                Pg::Corrupt => Self::Corrupt,
                Pg::Database(error) => {
                    Self::Database(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
                }
            }
        }
    }

    impl DynVaultStore for PostgresVaultStore {
        fn set(
            &self,
            provider: &str,
            label: &str,
            scheme: CredentialScheme,
            base_url: Option<&str>,
            user_agent: Option<&str>,
            secret: &str,
        ) -> Result<String, VaultError> {
            PostgresVaultStore::set(self, provider, label, scheme, base_url, user_agent, secret)
        }
        fn register_metadata(
            &self,
            provider: &str,
            label: &str,
            scheme: CredentialScheme,
            base_url: Option<&str>,
            user_agent: Option<&str>,
            exact: Option<SentinelPassExactBinding>,
            grant_acl: Option<crate::vault::SentinelPassGrantAcl>,
        ) -> Result<String, VaultError> {
            PostgresVaultStore::register_metadata(
                self, provider, label, scheme, base_url, user_agent, exact, grant_acl,
            )
        }
        fn get(&self, provider: &str, label: &str) -> Result<Option<String>, VaultError> {
            PostgresVaultStore::get(self, provider, label)
        }
        fn get_exact(
            &self,
            provider: &str,
            label: &str,
            binding: &SentinelPassExactBinding,
        ) -> Result<Option<String>, VaultError> {
            PostgresVaultStore::get_exact(self, provider, label, binding)
        }
        fn resolve(&self, provider: &str) -> Result<Option<(VaultEntry, String)>, VaultError> {
            PostgresVaultStore::resolve(self, provider)
        }
        fn metadata_entry(
            &self,
            provider: &str,
            label: &str,
        ) -> Result<Option<VaultEntry>, VaultError> {
            PostgresVaultStore::metadata_entry(self, provider, label)
        }
        fn list(&self) -> Result<Vec<VaultEntry>, VaultError> {
            PostgresVaultStore::list(self)
        }
        fn revoke(&self, provider: &str, label: &str) -> Result<bool, VaultError> {
            PostgresVaultStore::revoke(self, provider, label)
        }
        fn revoke_with_status(
            &self,
            provider: &str,
            label: &str,
        ) -> Result<(bool, SecretDeletion), VaultError> {
            PostgresVaultStore::revoke_with_status(self, provider, label)
        }
        fn backend_name(&self) -> &'static str {
            PostgresVaultStore::backend_name(self)
        }
        fn reference_plan(
            &self,
            provider: &str,
            label: &str,
        ) -> Result<VaultReferencePlan, VaultError> {
            PostgresVaultStore::reference_plan(self, provider, label)
        }
        fn reference_plan_for_exact(
            &self,
            provider: &str,
            label: &str,
            binding: &SentinelPassExactBinding,
        ) -> Result<VaultReferencePlan, VaultError> {
            PostgresVaultStore::reference_plan_for_exact(self, provider, label, binding)
        }
        fn capabilities(&self) -> VaultCapabilities {
            PostgresVaultStore::capabilities(self)
        }
    }

    impl DynVirtualKeyStore for PostgresVirtualKeyStore {
        fn mint(&self, req: MintRequest) -> Result<MintedKey, OperatorStoreError> {
            PostgresVirtualKeyStore::mint(self, req)
        }
        fn mint_delegated(
            &self,
            req: MintRequest,
            identity_json: &str,
        ) -> Result<MintedKey, OperatorStoreError> {
            PostgresVirtualKeyStore::mint_delegated(self, req, identity_json)
        }
        fn delegation(&self, id: &str) -> Result<Option<String>, OperatorStoreError> {
            PostgresVirtualKeyStore::delegation(self, id)
        }
        fn find_by_secret(
            &self,
            presented_secret: &str,
        ) -> Result<Option<VirtualKeyRecord>, OperatorStoreError> {
            PostgresVirtualKeyStore::find_by_secret(self, presented_secret)
        }
        fn find_by_hash(&self, hash: &str) -> Result<Option<VirtualKeyRecord>, OperatorStoreError> {
            PostgresVirtualKeyStore::find_by_hash(self, hash)
        }
        fn find_by_id(&self, id: &str) -> Result<Option<VirtualKeyRecord>, OperatorStoreError> {
            PostgresVirtualKeyStore::find_by_id(self, id)
        }
        fn list(&self) -> Result<Vec<VirtualKeyRecord>, OperatorStoreError> {
            PostgresVirtualKeyStore::list(self)
        }
        fn revoke(&self, id: &str) -> Result<bool, OperatorStoreError> {
            PostgresVirtualKeyStore::revoke(self, id)
        }
    }

    impl DynProfileStore for PostgresProfileStore {
        fn upsert_profile(&self, record: &ProfileRecord) -> Result<(), OperatorStoreError> {
            PostgresProfileStore::upsert_profile(self, record)
        }
        fn profile_for_group(
            &self,
            group: &str,
        ) -> Result<Option<ProfileRecord>, OperatorStoreError> {
            PostgresProfileStore::profile_for_group(self, group)
        }
        fn list_profiles(&self) -> Result<Vec<ProfileRecord>, OperatorStoreError> {
            PostgresProfileStore::list_profiles(self)
        }
        fn mint_profile_token(
            &self,
            subject: &str,
            group: &str,
        ) -> Result<String, OperatorStoreError> {
            PostgresProfileStore::mint_profile_token(self, subject, group)
        }
        fn lookup_active_profile_token(
            &self,
            presented: &str,
        ) -> Result<Option<(String, String)>, OperatorStoreError> {
            PostgresProfileStore::lookup_active_profile_token(self, presented)
        }
        fn revoke_profile_tokens(
            &self,
            subject: &str,
            group: &str,
        ) -> Result<usize, OperatorStoreError> {
            PostgresProfileStore::revoke_profile_tokens(self, subject, group)
        }
    }

    impl DynAlertStore for PostgresAlertStore {
        fn create(&self, req: CreateAlertRequest) -> Result<AlertRuleRecord, OperatorStoreError> {
            PostgresAlertStore::create(self, req)
        }
        fn list(&self) -> Result<Vec<AlertRuleRecord>, OperatorStoreError> {
            PostgresAlertStore::list(self)
        }
        fn list_by_scope(&self, scope: &str) -> Result<Vec<AlertRuleRecord>, OperatorStoreError> {
            PostgresAlertStore::list_by_scope(self, scope)
        }
        fn find_by_id(&self, id: &str) -> Result<Option<AlertRuleRecord>, OperatorStoreError> {
            PostgresAlertStore::find_by_id(self, id)
        }
        fn mark_fired(&self, id: &str) -> Result<bool, OperatorStoreError> {
            PostgresAlertStore::mark_fired(self, id)
        }
        fn ack(&self, id: &str) -> Result<bool, OperatorStoreError> {
            PostgresAlertStore::ack(self, id)
        }
        fn delete(&self, id: &str) -> Result<bool, OperatorStoreError> {
            PostgresAlertStore::delete(self, id)
        }
    }

    impl DynBackendStore for PostgresBackendStore {
        fn get(&self, id: &str) -> Result<Option<BackendRecord>, BackendStoreError> {
            PostgresBackendStore::get(self, id).map_err(BackendStoreError::from)
        }
        fn list(&self) -> Result<Vec<BackendRecord>, BackendStoreError> {
            PostgresBackendStore::list(self).map_err(BackendStoreError::from)
        }
        fn put(
            &self,
            definition: &BackendDefinition,
            expected_revision: Option<u64>,
        ) -> Result<BackendRecord, BackendStoreError> {
            PostgresBackendStore::put(self, definition, expected_revision)
                .map_err(BackendStoreError::from)
        }
        fn revoke(
            &self,
            id: &str,
            expected_revision: u64,
        ) -> Result<BackendRecord, BackendStoreError> {
            PostgresBackendStore::revoke(self, id, expected_revision)
                .map_err(BackendStoreError::from)
        }
    }

    impl DynRouteStore for PostgresRouteStore {
        fn get(&self, id: &str) -> Result<Option<RouteRecord>, RouteError> {
            PostgresRouteStore::get(self, id).map_err(RouteError::from)
        }
        fn list(&self) -> Result<Vec<RouteRecord>, RouteError> {
            PostgresRouteStore::list(self).map_err(RouteError::from)
        }
        fn put(
            &self,
            definition: &RouteDefinition,
            expected: Option<u64>,
        ) -> Result<RouteRecord, RouteError> {
            PostgresRouteStore::put(self, definition, expected).map_err(RouteError::from)
        }
        fn revoke(&self, id: &str, expected: u64) -> Result<RouteRecord, RouteError> {
            PostgresRouteStore::revoke(self, id, expected).map_err(RouteError::from)
        }
        fn pin(
            &self,
            namespace: &str,
            route_id: &str,
            now: i64,
        ) -> Result<Option<Pin>, RouteError> {
            PostgresRouteStore::pin(self, namespace, route_id, now).map_err(RouteError::from)
        }
        fn confirm(
            &self,
            route: &RouteRecord,
            pin: Option<&Pin>,
            now: i64,
        ) -> Result<(), RouteError> {
            PostgresRouteStore::confirm(self, route, pin, now).map_err(RouteError::from)
        }
    }

    impl DynReferenceStore for PostgresReferenceStore {
        fn authorize(
            &self,
            binding: &str,
            owner: &str,
            refs: &[Reference],
        ) -> Result<(), crate::references::Error> {
            PostgresReferenceStore::authorize(self, binding, owner, refs)
        }
        fn authorize_until(
            &self,
            binding: &str,
            owner: &str,
            refs: &[Reference],
            deadline: &dyn Fn() -> Instant,
        ) -> Result<(), crate::references::Error> {
            PostgresReferenceStore::authorize_until(self, binding, owner, refs, || deadline())
        }
        fn issue(
            &self,
            binding: &str,
            owner: &str,
            refs: &[Reference],
            expires_at: i64,
        ) -> Result<(), crate::references::Error> {
            PostgresReferenceStore::issue(self, binding, owner, refs, expires_at)
        }
        fn issue_until(
            &self,
            binding: &str,
            owner: &str,
            refs: &[Reference],
            expires_at: i64,
            deadline: &dyn Fn() -> Instant,
        ) -> Result<(), crate::references::Error> {
            PostgresReferenceStore::issue_until(self, binding, owner, refs, expires_at, || deadline())
        }
    }

    impl DynPolicyAuditStore for PostgresPolicyAuditStore {
        fn summary(&self) -> Result<serde_json::Value, OperatorStoreError> {
            PostgresPolicyAuditStore::summary(self)
        }
        fn record(
            &self,
            identity: &sandhi_core::policy::Identity,
            key_id: &str,
            decision: &sandhi_core::policy::Decision,
        ) -> Result<String, OperatorStoreError> {
            PostgresPolicyAuditStore::record(self, identity, key_id, decision)
        }
    }

    /// Re-exported for the proxy wiring: an `Arc<dyn Dyn…Store>` constructor helper set.
    pub(crate) fn _keep_arc_import(_: Option<Arc<PostgresVaultStore>>) {}
}
