# Postgres usage store (`SANDHI_USAGE_DSN`)

> **Scope note (ADR-0049).** This is the *usage-only* mode, retained for compatibility.
> The whole-store mode — `SANDHI_STORE_DSN` moves the enforcement ledger, evidence, all
> eight operator stores, the native-reference registry **and** the usage history onto one
> PostgreSQL database — is documented in [postgres-store](postgres-store.md). When both
> variables are set, `SANDHI_STORE_DSN` wins.

Phase 2 of the Postgres program: the **usage history** (the `usage_events` table behind the
dashboard, `/admin/usage*` and bounded diagnostics) can live in PostgreSQL instead of the
SQLite store. Everything else stays exactly where it was:

- `SANDHI_STORE` remains **mandatory** — the enforcement ledger (budgets/holds), the credential
  vault, virtual keys, backend/route/profile/alert stores all stay on that SQLite file (ADR-0043
  keeps the durable ledger single-file this phase).
- Units stay neutral tokens — no dollars, tiers or SKUs cross this boundary (the measure-vs-price
  line, ADR-0001).

## Setup

1. Build the proxy with the feature (release binaries that serve PG usage histories):

   ```sh
   cargo build -p sandhi-proxy --features postgres --bin sandhi-proxy
   ```

   The feature is opt-in and the library keeps no hard sqlx/PostgreSQL dependency. A binary
   built without it **refuses to start** if `SANDHI_USAGE_DSN` is set — a configured usage
   store never silently downgrades to SQLite.

2. Point the proxy at PostgreSQL:

   ```sh
   export SANDHI_USAGE_DSN=postgres://sandhi:…@db-host:5432/sandhi
   export SANDHI_STORE=/var/lib/sandhi/usage.db   # still required (ledger + operator tables)
   ```

   An unset or whitespace-only `SANDHI_USAGE_DSN` selects the default SQLite usage store —
   identical behavior to every release before this one.

3. Start the proxy. On connect it runs the schema init **idempotently**
   (`CREATE TABLE IF NOT EXISTS` + indexes); no migration tool is needed for a fresh database.
   Connect failures, permission failures or an unreadable DSN are **fatal at startup** (exit 1
   with a clear stderr message) — the proxy never serves with the usage history missing.

When Postgres is active, the Postgres store **replaces** SQLite as the usage-event sink and
read path: the buffered sink emits into Postgres and every usage query reads from it. The
SQLite `usage_events` table simply stops receiving writes (the ledger and operator tables on
the same file are unaffected — they never read `usage_events`).

Startup logs one line when the Postgres usage store is active; the DSN itself is never logged
(it carries the credential).

## Schema

Table `usage_events` — the same 26 flat column names as the SQLite table (the migration script
and any externally managed table depend on that), typed `text` / `bigint` /
`double precision`, with `occurred_at` as `timestamptz` (RFC-3339 string bounds bind directly;
query bounds are inclusive-`from`/exclusive-`to` exactly as before), plus one addition:

- `id bigserial PRIMARY KEY` — the explicit insertion-ordering key. SQLite diagnostics order by
  the implicit `rowid`; Postgres rows need a named key for the same newest-first order. The
  SQLite schema is **not** changed.

Indexes mirror SQLite's: `occurred_at` plus every attribution dimension (`subject_id`,
`group_id`, `provider`, `model`, `virtual_key_id`, `session_id`, `run_id`, `request_id`).

## Lakehouse tier (pgdatalake, operator-managed)

`usage_events` is strictly INSERT-only with flat columns inside pgdatalake's supported type
set (timestamptz/int8/text/float8), so the table can be managed by the
[`anvai-labs/pgdatalake`](https://github.com/anvai-labs/pgdatalake) fork as an Iceberg
lakehouse table. **No lakehouse DDL lives in Sandhi code** — the fork's staged-write pipeline,
publisher and catalog are operator concerns. The recipe (server-mode; embedded mode does not
exist yet — see the co-design handoff `HANDOFF-pgdatalake-embedded-mode-codesign-20261124.md`,
retained with the pgdatalake fork's session notes):

```sql
CREATE EXTENSION lakehouse;
CREATE FOREIGN DATA WRAPPER lakehouse_w HANDLER lakehouse.lakehouse_fdw_handler;
CREATE SERVER lakehouse_srv FOREIGN DATA WRAPPER lakehouse_w
  OPTIONS (endpoint 'http://catalog:8181', warehouse 'wh', s3_endpoint 'http://s3:9000');
CREATE USER MAPPING FOR sandhi SERVER lakehouse_srv
  OPTIONS (s3_access_key '…', s3_secret_key '…');
-- Create the table with the SAME column names as the store's usage_events (26 flat columns)
-- BEFORE first proxy start: auto-init skips creation when the table already exists.
CREATE TABLE usage_events ( ... ) USING lakehouse;
SELECT lakehouse.attach_table('usage_events'::regclass, 'analytics.sandhi_usage', 'lakehouse_srv');
SELECT lakehouse.create_remote('usage_events'::regclass);
ALTER TABLE usage_events SET (write_mode = 'staged');
-- Thresholds (the recipe's deliberate choices):
--   lakehouse.target_file_size  — fork default 128 MiB, already the logical default; keep it.
--   lakehouse.publish_max_lag   — fork default 30 s; set 1800 (s) for 30-minute Iceberg cuts,
--                                 which matches the metering write rate (~tens of events/s/edge)
--                                 without fragmenting the catalog snapshot log.
-- Publisher: shared_preload_libraries='lakehouse' or SELECT lakehouse.start_publisher();
```

Two lakehouse-specific caveats:

- A `USING lakehouse` table cannot carry a `bigserial`; include an `id bigint` column and
  populate it if you need `/admin/usage/diagnostics` (it orders by `id DESC`). Without the
  column, diagnostics return an error while every aggregate endpoint keeps working.
- The proxy is a single writer (`SANDHI_REPLICA_COUNT=1`), matching the fork's single-publisher
  model — no multi-writer fencing is needed.

## Migrating an existing SQLite history

`scripts/migrate_sqlite_to_postgres.py` bulk-copies a SQLite store's tables into PostgreSQL
(run with the proxy stopped; rerun with `--truncate` for a clean reload). Schema creation is
the backend's job — start the proxy once against the empty database (auto-init creates
`usage_events`), then migrate.

## Tests

The SQLite path is covered by the ordinary workspace gate. The Postgres backend's live tests
run only when a real database is available:

```sh
SANDHI_TEST_PG_DSN=postgres://sandhi:…@localhost/sandhi_test \
  cargo test -p sandhi-store --features postgres -- --ignored
```

They exercise emit + failure counting, windowed aggregation/month bucketing, the run cost tree
and diagnostics against a real server.
