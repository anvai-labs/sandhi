# The whole-store Postgres backend (`SANDHI_STORE_DSN`)

Phase 3 of the Postgres program (ADR-0049): with `SANDHI_STORE_DSN` configured and the
binary built with `--features postgres`, **every** durable surface moves onto ONE
PostgreSQL database — a pure replace, no mirror window:

- the **enforcement ledger** (budgets, holds, evidence subsystem, dispatch fences,
  receipts) — the same paired-execution contract as the SQLite ledger, one database,
  no shards;
- the **usage history** (`usage_events` behind the dashboard, `/admin/usage*` and
  bounded diagnostics) — sink and read path both;
- all eight **operator stores** — the credential-vault *metadata index* (secrets still
  live in the OS keyring / SentinelPass via `SANDHI_VAULT_BACKEND`), virtual keys +
  OIDC delegations, backend definitions, gateway profiles, alert rules, logical routes
  + session affinity, issued native references, and policy admission receipts;
- the **native-reference registry** (ADR-0044) — the proof path opens the PG reference
  store on the same database instead of the file-backed ledger's own store.

The SQLite `SANDHI_STORE` file is **not opened for store purposes** in this mode; it is
no longer mandatory here. The credential and authentication paths
(`CREDENTIALS_DIRECTORY`, `oidc.json`, `SANDHI_CLIENT_CREDENTIALS_FILE`) are independent
of the store and unchanged. Units stay neutral tokens — no dollars, tiers or SKUs cross
this boundary (ADR-0001's measure-vs-price line).

## Selection and precedence

| Environment | Behavior |
|---|---|
| `SANDHI_STORE_DSN` set | whole-store Postgres mode (this document) |
| only legacy `SANDHI_USAGE_DSN` set | the #518 usage-only topology: usage history in Postgres, everything else on the mandatory `SANDHI_STORE` file (see [postgres-usage-store](postgres-usage-store.md)) |
| both set | `SANDHI_STORE_DSN` **wins** — the whole-store mode; a startup line notes the supersession |
| neither set | today's default: one SQLite file, byte-identical behavior |

## Setup

1. Build with the feature:

   ```sh
   cargo build -p sandhi-proxy --features postgres --bin sandhi-proxy
   ```

   A binary built **without** the feature refuses to start if `SANDHI_STORE_DSN` is set —
   a configured store never silently downgrades to SQLite.

2. Point the proxy at the one database:

   ```sh
   export SANDHI_STORE_DSN=postgres://sandhi:…@db-host:5432/sandhi
   ```

   Connect parameters ride on the DSN itself (e.g. `?application_name=sandhi-proxy`);
   every Postgres store parses them from the DSN, keeping the proxy binary sqlx-free.

3. Start the proxy. On connect every store runs its schema init **idempotently**
   (`CREATE TABLE IF NOT EXISTS` + additive migrations); no migration tool is needed for
   a fresh database.

## Postures

- **Fatal, never downgrade.** Connect failure, permission failure or an unreadable DSN
  is fatal at startup (exit 1). `SANDHI_LEDGER_SHARDS` values other than `1` are refused
  before any store opens (the Postgres ledger has no shards). There is no volatile or
  permissive serving path in this mode.
- **Single writer.** One proxy process owns the database. Multiple replicas remain
  unsupported; the durable ledger and rate limiter are single-node by design.
- **The DSN is never logged.** Startup logs name the owning backend per subsystem
  (`store backend: postgres — enforcement ledger + evidence, usage history, operator
  tables, native-reference registry`); the DSN itself carries the credential and is
  never printed.
- **Pure replace.** The SQLite file simply stops receiving writes; no dual-write mirror
  window exists. Moving an existing deployment is a migration (below), not a cutover
  dance.

## Migration

`scripts/migrate_sqlite_to_postgres.py` covers **every** table the stores own — usage
events, the full `budget_*` ledger/evidence set, the eight operator tables, and the
native-reference registry. Run it **with writers quiesced** (the proxy stopped),
optionally per shard; receipt/execution/attempt ids are preserved verbatim and the
`budget_reservation` sequence is re-set afterwards so later inserts cannot collide.

## Schema

Each store's PostgreSQL schema is the direct port of its SQLite twin's DDL: same column
names and constraints, `TEXT`→`text`, `INTEGER`→`bigint`/`integer`, strftime clock
defaults rendered as byte-identical UTC `to_char` fragments. Backends' budget-namespace
immutability and every upsert conflict target are enforced natively (triggers /
`ON CONFLICT`). Write serialization uses per-table transactional advisory locks —
SQLite's whole-file write lock narrowed to one table. See
`crates/sandhi-store/src/operator_pg.rs` for the recorded dialect decisions.
