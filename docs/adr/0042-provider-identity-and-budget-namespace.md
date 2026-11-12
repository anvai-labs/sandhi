# ADR-0042: Separate configured provider identity from immutable budget authority

- Status: Source implementation; independent review and integrated qualification remain separate.
- Follows: [explicit gateway peers](0041-explicit-gateway-peer-contract.md) and [paired accounting](0023-paired-budget-settlement.md).

Configured vendor identity is diagnostic metadata. Endpoint family/profile selects the codec and credential constraints. A canonical upstream budget namespace is separate trusted composition data. Request fields and headers cannot assign it.

Every handle captures its existing effective namespace before diagnostic identity overrides. Registered backends persist that namespace independently of their definition. Initial migration preserves the exact old effective scope; subsequent label, family, credential and revision changes cannot silently select another cap. Namespace mutation is not an ordinary backend API operation. Existing family-named buckets such as `upstream:anthropic` can remain authoritative for a differently named vendor.

The existing version 1 route, cache-plan and transport fingerprints retain their original inputs for unchanged bindings. Migration does not rehash valid affinity rows or change established cache addresses. Immutable backend identity/revision binds the namespace association; a future authorized reassignment requires a separately reviewed revision/generation fence and ledger migration. Genuine current backend/credential/model revisions retain their existing incompatibility checks.

No budget, execution context, reservation, usage or receipt rows are renamed, reset or regrouped. Recovery continues on the original ledger with the original paired scopes. Historical vendor allocation inside shared family buckets remains a separate migration.

New attempt-evidence JSON format version 2 adds optional bounded configured provider/model labels observed at dispatch. Version 1 records stay readable and canonically unchanged. Diagnostic labels do not qualify counts, grant dispatch, choose policy/namespace, prove billing or affect transport completeness. Unsupported labels may be omitted. The persisted single-attempt authorization fence remains unchanged; older readers fail closed on new evidence rather than releasing liability.

The [implementation and regression report](../reviews/provider-identity-and-budget-namespace-2026-11-12.md) records scope, old-only-cap/unknown-hold recovery, old SQL pin/cache compatibility and source-label joins. Broader accounting cutover, native-source qualification, retained reliability failures and release/platform acceptance remain separate gates.
