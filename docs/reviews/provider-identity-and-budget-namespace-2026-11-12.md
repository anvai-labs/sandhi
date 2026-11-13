# Provider identity and immutable upstream budget namespaces

Configured provider identity, endpoint family and upstream budget authority now have separate owners. A configured vendor label appears consistently in handle diagnostics and dispatch observations; it does not select a protocol, credential, cache capability or budget. The endpoint family still selects the actual codec/profile.

Every handle captures its existing effective upstream namespace at construction. Changing its diagnostic identity or declared family does not recompute that namespace. Registered backends persist the namespace separately from their definition, expose it as read-only `upstream_budget_scope` inventory metadata, and restore it on rehydration. Ordinary backend revision, label, family, credential and revoke/reactivate changes cannot reassign it. Namespace selection is trusted Rust composition metadata; request JSON and headers do not supply it.

## Compatibility migration

The first database migration records each existing backend's exact old effective namespace: fixed `upstream:anthropic`, `upstream:gemini`, `upstream:cohere` and `upstream:ollama` for their native families; the old configured slug for Chat and Responses. New backends use the same compatibility defaults once, so existing shared family caps continue to constrain them. Subsequent family or label changes retain the stored namespace.

The migration and namespace immutability trigger commit in one immediate transaction. An already migrated database with a missing binding is corrupt; it is not repaired by guessing from its current label/family. Backend revision CAS and binding validation remain required. Existing version 1 route, cache-plan and execution transport fingerprint inputs are preserved for an unchanged binding, including the original transport label used for cache addressing. Startup does not rehash or discard valid pins or cache namespaces. The immutable backend-ID-to-namespace association remains bound through its backend revision; genuine definition changes keep their existing revision/credential/model fences. A future separately authorized namespace reassignment must carry a generation/revision fence and its own safe ledger migration.

No budget limits, reservations, execution contexts, usage observations or receipts are renamed, moved, merged or reset. Admission continues to reserve the original paired scopes; recovery validates the original back leg against the immutable execution context. Fresh namespace assignment is not a weaker second cap.

This is not a retrospective merge of histories that already used multiple namespaces before the migration. Those original scopes, caps and unresolved liabilities remain in place. Existing native family buckets may represent several vendors; allocating that historical spend/unknown liability to individual vendors needs a separately reviewed migration. There is no ordinary API for reassigning a registered backend's namespace in this checkpoint. Quiesce old writers for deployment: an older executable does not implement this namespace invariant, and old binaries cannot safely consume the new attempt evidence format.

## Diagnostic transport evidence

New immutable attempt evidence uses JSON format version 2 with optional bounded `observed_provider` and requested upstream model labels from the dispatch observation. The existing authorization fence remains single-attempt version 1; diagnostic JSON format version 2 grants no new dispatch authority. Provider/model limits align with the frozen execution context at 128/512 bytes. Unsupported diagnostic labels are omitted, rather than preventing otherwise valid accounting.

Version 1 evidence remains readable and byte-identical when encoded again. Old records are never relabeled from current configuration. Labels do not enter `AttemptEvidence::complete`, source usage qualification, charge computation, policy selection or settlement eligibility. Missing/dropped transport evidence and unavailable usage still retain liability. Older readers reject version 2 instead of interpreting it as no send or releasing a hold.

## Focused validation

The retained private logs and [artifact](artifacts/provider-identity-and-budget-namespace-2026-11-12.json) describe the exact implemented source snapshot and checks. Regression coverage includes:

- Legacy backend migration across all six native endpoint families; provider/family revision changes and reopen preserve the namespace, stale revisions refuse, and namespace mutation/missing migrated bindings fail closed.
- A database with only the old family cap, existing settled spend, an unknown paired execution and a qualified version 1 execution. Migration/reopen preserves original rows and context; the qualified execution settles into its original scope, unknown liability survives expiry/recovery, and another admission remains denied.
- Eight actual HTTP entry cases after a vendor/family change and restart, with only the original Anthropic cap configured: all refuse with zero origin requests and zero reservations.
- Legacy SQL route-affinity rows and pre-migration cache-binding hashes survive unchanged-binding startup migration across all four ingress/backend families; a genuine subsequent backend revision still refuses the old pin.
- The existing 112-call native/raw/typed/translated matrix now joins configured observed labels and actual model labels through version 2 durable evidence while preserving the original namespace and paired receipt.
- Native provider dispatch observations and host-handle namespace preservation; old evidence canonical round trips and optional-label nonauthority.

These are synthetic implementation checks. The known accounting admission/settlement waits, native Python exit, original SDK shutdown occurrence, live subscription/cache lifecycle, mandatory accounting cutover, consumer rollout and actual historical namespace reassignment remain separate gates. This checkpoint is not release, merge or full-goal approval.
