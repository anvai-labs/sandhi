# ADR-0049: The whole-store Postgres backend (`SANDHI_STORE_DSN`)

- Status: Accepted (Phase 3, PR-D)
- Amends: [ADR-0043](0043-mandatory-durable-http-execution.md) (the original
  ordinary-file mandate), [ADR-0044](0044-native-reference-ownership.md) (the
  same-ledger reference registry), and extends the Phase 2 usage-store mode
  ([operator guide](../operator/postgres-usage-store.md)).

## Context

The Postgres program has landed in slices behind the non-default `postgres` cargo
feature: the usage store (`SANDHI_USAGE_DSN`, usage-only), the enforcement-ledger core
(`PostgresLedger`), the evidence subsystem, and the eight operator stores
(vault metadata, virtual keys + delegations, backend definitions, gateway profiles,
alert rules, logical routes + session affinity, issued native references, policy
receipts). Every store had a complete `connect`/`connect_with` constructor; nothing
wired them together. Operators running the AnvaiOps control plane want one PostgreSQL
database, not a SQLite file for the ledger/operator tables beside a Postgres usage
history — one DSN, one backup story, one migration.

## Decision

`SANDHI_STORE_DSN` selects the **whole-store mode**: when set (and the binary is built
with `postgres`), the proxy opens **every** durable surface on that ONE database —

- the enforcement ledger (`ProxyLedger::Postgres(PostgresLedger)`), with the identical
  paired-execution/evidence contract as the SQLite `ShardedLedger` arm (the
  `DurableExecutionLedger` trait names the surface; the PG intent-bridge methods perform
  the same original-intent binding checks);
- the usage history (`PostgresStore` as sink **and** every usage read);
- all eight operator stores through the object-safe `Dyn*` seams `ProxyState` holds;
- the policy-receipt audit store when a policy configuration is present;
- the native-reference registry — the proof path opens the PG reference store on the
  same database, preserving ADR-0044's ownership rules (same-database registry, original
  deadlines, no caller-ID adoption).

Selected facts:

1. **Pure replace, no mirror window.** The SQLite `SANDHI_STORE` file is not opened for
   store purposes in this mode and is no longer mandatory here. There is no dual-write
   phase; moving an existing deployment is a migration (below).
2. **Single writer.** One proxy process owns the database. Multiple replicas remain
   unsupported (the durable ledger and rate limiter are single-node, unchanged from
   ADR-0043); `SANDHI_LEDGER_SHARDS` values other than `1` are refused before any store
   opens (the Postgres ledger has no shards).
3. **No-downgrade rule.** A configured backend that cannot open is fatal (exit 1).
   A binary built without `postgres` refuses startup when `SANDHI_STORE_DSN` is set.
   Startup logs name the owning backend per subsystem; the DSN itself is never logged.
4. **Precedence.** `SANDHI_STORE_DSN` wins over the narrower legacy `SANDHI_USAGE_DSN`
   (that variable alone keeps the usage-only topology: usage in Postgres, everything
   else on the mandatory file). Unset keeps today's behavior byte-identical.
5. **Credential paths are independent.** `CREDENTIALS_DIRECTORY` / `oidc.json` /
   client-credential files are unaffected. The vault *metadata* moves to Postgres;
   credential bytes stay in the OS keyring / SentinelPass (`SANDHI_VAULT_BACKEND`).
6. **ADR-0043's file mandate applies to the durable ledger + reference registry, not to
   the file specifically.** The mandate is durability with a validated single owner.
   In this mode the ledger moves to PG and the registry to the PG reference store; the
   file-backed gates report durable-always (`is_file_backed() -> true`, PR-A), so no
   file check blocks execution. Everything else in ADR-0043 (one durable owner freezing
   admitted scopes, one physical attempt + qualified source evidence before one paired
   receipt; unknown usage keeps both liabilities through expiry/restart; `Warn` as a
   soft cap; immutable `upstream_budget_scope`) is backend-neutral and unchanged.
7. **Migration discipline.** `scripts/migrate_sqlite_to_postgres.py` covers every table
   the stores own (usage events, the full `budget_*` ledger/evidence set, the eight
   operator tables, the native-reference registry). Run with writers quiesced; ids are
   preserved verbatim and the `budget_reservation` sequence is re-set afterwards.

## Consequences

- One DSN, one backup story, one migration for operators; the SQLite default stays the
  zero-config starter and every release before this one.
- The proxy's store consumers hold object-safe seams (`Dyn*` traits,
  `DurableExecutionLedger`), so the request path never branches per backend; the
  default-feature build compiles none of the Postgres code.
- The binary keeps no hard sqlx dependency: connect parameters (e.g.
  `application_name`) ride on the DSN and are parsed by each store constructor.
- `SANDHI_USAGE_DSN` remains supported for the usage-only topology and is superseded
  (with a startup note) when both variables are set.

## Notes on amended ADRs

- [ADR-0043](0043-mandatory-durable-http-execution.md): the "original ordinary-file
  `SANDHI_STORE`" requirement is scoped to the SQLite mode; in the whole-store mode the
  durable ledger + evidence and the reference registry live on the validated single
  PostgreSQL database (this ADR), with the same one-owner pairing contract.
- [ADR-0044](0044-native-reference-ownership.md): "same-ledger proof" reads
  "same-database proof" in the whole-store mode — the registry is the PG reference store
  on the enforcement ledger's database; no external-resource adoption, caller-ID or
  header provenance is introduced.
