# Single-node recovery rehearsal

Status: W06c implementation under verification; integration is C01d in
[TD-0026](../td/TD-0026-gateway-product-evolution.md).

This runbook rehearses recovery in disposable, isolated directories with synthetic credentials.
It is not an online backup service, an automatic production restore command, or a fleet recovery
guarantee. Keep the original files and snapshot untouched; never rehearse over a running deployment.

## What must survive

For a clean shutdown, verify saved usage and attribution, budget limits and settled spend,
reservation identifiers, alert configuration/fire state, credential references, and hashed virtual
keys. A continued request must add only its own measured usage and settlement. Revoked and expired
keys in the saved state remain unusable; a lost plaintext virtual key cannot be recovered from its
hash. Provision new keys if the caller no longer has its original secret.

A forced exit has a different contract. New paired reservations with unknown dispatch or
unqualified usage remain held through expiry/restart and still constrain admission; missing
usage is never zero consumption. Historical unpaired leases keep their former TTL behavior.
Recovery may finish retained accounting from immutable source evidence, never replay inference.
The required owner already retains physical attempts, paired receipts and native-reference
proofs; these drills do not promise lossless telemetry or reconstruct evidence lost before commit.

## Before taking a snapshot

1. Record the build/commit and binary digest, configuration revision, SQLite/schema identity,
   exact topology (current serving requires one shard), UTC snapshot time, shutdown result and last observed committed
   usage. Keep this record with the protected archive, without secrets.
2. Stop admission, then stop **all** writers, including other processes, CLI sessions and maintenance
   tasks. Wait for the actual process exit. Exit `0` proves the shutdown coordinator completed,
   not that every best-effort observation ever enqueued was persisted. Exit `124` or SIGKILL means
   unfinished cleanup may remain; mark the snapshot as crash recovery, not clean shutdown.
3. Keep the source directory stable. With one shard, the base SQLite file contains enforcement
   and management/usage state. With N>1, capture the base plus **every**
   `<base>-ledger-shard-<i>.db`, for i from 0 through N−1, together while writers are stopped.
   This N>1 case is offline preservation of legacy stores, not permission to serve them with
   0.17.0. Keep the complete original topology and plan migration separately; current serving
   rejects shard counts other than one before ledger open. Multi-file live snapshots are not
   transactionally consistent, and restoring files is not a topology migration.
4. Include any required WAL or rollback-journal state when staging stopped files. Do not delete
   sidecars or copy only the main file of an active WAL database. The drill stages the stopped
   files privately, then uses SQLite backup to produce standalone databases. SQLite documents
   both the [backup API](https://www.sqlite.org/backup.html) and why the
   [WAL file is part of database state](https://www.sqlite.org/wal.html).
5. Store snapshots privately and protect them as sensitive data. Provider secrets, broker grants,
   admin tokens and client-held plaintext keys are provisioned separately, but attribution,
   key hashes and potentially bearer-bearing webhook URLs remain sensitive database contents.
   The rehearsal helper does not encrypt, sign, scrub or durably publish archives across power loss.

## Validate and quarantine the restore

The test helper in `tests/sdk-conformance/recovery_snapshot.py` writes a versioned manifest,
fixed payload names, sizes and SHA-256 digests, and checks SQLite integrity. It rejects incomplete
or unexpected file sets, mismatched shard topology, malformed manifests and existing destinations.
It is **test-only**, for caller-owned stable directories; checksums are not authenticity against
someone who can rewrite the manifest. It does not protect against concurrent hostile filesystem
changes or provide a schema-version migration mechanism.

Before starting any restored service:

1. Restore into a **new**, access-restricted directory outside the source/archive. Reject a missing
   database or shard, failed integrity check, checksum mismatch, incompatible manifest, unexpected
   schema or unknown topology. Compare expected table identities and rows with the saved evidence.
   Do not boot an incomplete output directory after an interrupted copy.
2. Use a compatible binary and validate topology offline first. A legacy multi-shard archive
   must not be booted with the current single-file owner before an explicit migration. Provision
   configuration separately and validate destinations; block external webhook/provider egress.
   Keep old writers stopped and preserve historical caps/spend/holds/pins; never restore onto a
   different empty ledger to obtain apparent capacity.
3. Bind to isolated loopback, keep load balancers and callers disconnected, and withhold provider
   credentials/broker grants. `/readyz=200` is **not** a storage-integrity or credential-health
   check. Current serving requires the original ordinary-file `SANDHI_STORE`, one shard and an
   unset `SANDHI_BUFFERED_ACCOUNTING`. Missing/empty configuration, memory/URI forms and
   unsupported topology refuse before stores open; a later component initialization can still
   fail after earlier schema work. There is no development memory-serving fallback. File
   completeness/integrity and saved caps/spend/holds/pins must be checked before startup, then
   verified through SQL and authenticated APIs afterward. `/admin/version` owner capability is
   a separate configured fact, not a per-request receipt or live storage-health guarantee.
4. Reconcile changes made **after** the snapshot: virtual-key revocations, local credential status,
   provider/broker revocations, budgets and configuration. An old database can resurrect a later
   revocation. There is no automatic revocation overlay. If the current authoritative revocation
   state is unavailable, keep the restored service quarantined instead of assuming access is safe.
5. Verify expired/revoked keys are denied, then explicitly provision an authorized broker read
   grant and register the exact provider reference. Inventory alone is not proof of usable secret
   authority. Locked, missing, denied and feature-disabled broker paths must not dispatch upstream.
   This startup drill does not prove live invalidation of already-cached provider handles.
6. Exercise a synthetic allowed call and a denied call. Compare usage, attribution, enforcement
   spend and remaining leases independently. Browser data must agree with authenticated APIs;
   refreshing the dashboard must not manufacture new observations. Only then consider a separately
   approved traffic cutover with a named operator and rollback rule.

Do not restore an older binary over new state as an improvised rollback. Retain the original
snapshot and failed restore separately, investigate the failure, and use another fresh target.
Record the data-loss window and unknown consumption; do not infer RPO=0 from a successful copy.

## Reproduce the automated rehearsal

Use the existing SDK-conformance environment and installed browser dependencies. Every process,
socket, database and secret below is created by the fixtures; no personal vault is read or written.

```bash
python -m pytest tests/sdk-conformance/test_recovery_snapshot.py \
  tests/sdk-conformance/test_recovery.py \
  tests/sdk-conformance/test_recovery_broker.py \
  tests/sdk-conformance/test_store_startup.py -q

SANDHI_AGENTBROWSER_ROOT=/path/to/built/agentbrowser \
  python -m pytest tests/sdk-conformance/test_recovery_agentbrowser.py -q
```

The cases cover current single-file serving/restart, offline legacy two-shard preservation and
pre-open serving refusal, plus standalone WAL recovery;
malformed/changed/missing snapshot rejection; real SIGKILL with a held lease; post-snapshot
revocation reconciliation before credential-enabled startup; and synthetic native/plain broker
recovery. AgentBrowser exercises the served restored dashboard's locked state, secret-reference
authentication, restored evidence, Refresh, Clear token and exact-origin egress denial. Browser
automation does not establish storage durability or actual-user usability.

For each deployment rehearsal, record:

| Evidence | Required content |
|---|---|
| Snapshot identity | UTC time, binary/config/schema identity, complete file set and topology, digests |
| Source outcome | Clean/forced exit, last committed observations, uncertain operations, known loss window |
| Restore outcome | Integrity/row comparisons, observed restore duration, errors, target isolation |
| Access reconciliation | Accepting operator, post-snapshot revocation/policy source, denied-key checks |
| Continued service | One allowed/denied synthetic call, measured usage/spend, held leases, dashboard evidence |
| Cutover decision | Explicit operator approval, rollback rule, retained original snapshot |

### Startup compatibility change

A deployment that previously logged a durable-store error and continued without enforcement
state now exits nonzero before serving requests. Repair the configured storage; do not unset
`SANDHI_STORE` or use a new empty file to bypass this failure. Existing data stays on its original
file; startup initializes the current owner and bounded native-proof metadata there without
inventing historical attempts or receipts. External broker unavailability is
different: metadata may open successfully while individual secret references remain unresolved
and cannot dispatch. `Warn` remains a soft cap; both policies fail closed on admission storage
failure and conserve unknown paired holds. Initialization is not one
cross-component transaction: earlier successful schema setup can remain if a later component
fails. Preserve the pre-startup snapshot; a refused listener is not a promise of zero disk writes.

Automated results are recorded in TD-0026. Actual-user acceptance belongs to W06d. Power-loss
testing, live broker certification, automatic restore quarantine, signed manifests, online
cross-file backups, fleet recovery and complete unknown-liability reconstruction remain open.
