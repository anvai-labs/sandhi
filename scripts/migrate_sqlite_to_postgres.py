#!/usr/bin/env python3
"""One-time SQLite → PostgreSQL migration for Sandhi stores (development tool).

Reads a Sandhi SQLite store file (plus any ledger-shard siblings) and bulk-copies
every requested table into PostgreSQL, preserving primary keys and then resetting
PostgreSQL sequences so later inserts cannot collide. Run with writers quiesced
(the proxy stopped); rerun with --truncate for a clean reload.

Column values pass through unchanged: the PostgreSQL DDL (created by the
Postgres store backend) owns the storage types — text columns stay text,
epoch-integer columns stay bigint — so this script converts nothing. Schema
creation is the backend's job; the script only moves rows.

Usage:
  python3 scripts/migrate_sqlite_to_postgres.py \
      --sqlite /var/lib/sandhi-edge/usage.db \
      --postgres postgresql://sandhi:…@host/sandhi \
      [--shard /var/lib/sandhi-edge/usage.db-ledger-shard-1.db] \
      [--truncate] [--tables usage_events,virtual_keys,…]

Requires psycopg 3 (`pip install "psycopg[binary]"`). Standard library only
otherwise; no pandas/pyarrow needed at this row volume.
"""
from __future__ import annotations

import argparse
import sqlite3
import sys

# Order matters only for human readability; foreign keys are not enforced
# across these tables, and ids are copied verbatim.
DEFAULT_TABLES = [
    "usage_events",
    "budget_limit",
    "budget_reservation",
    "idempotency_dedup",
    "budget_execution_intent",
    "budget_request_correlation",
    "budget_dispatch_fence",
    "budget_settlement_outbox",
    "budget_terminal_observation",
    "budget_execution_back_leg",
    "budget_physical_attempt",
    "budget_execution_context",
    "vault",
    "virtual_keys",
    "oidc_key_delegations",
    "backends",
    "profiles",
    "profile_tokens",
    "alert_rules",
    "logical_routes",
    "route_affinity",
    "issued_native_references",
    "policy_receipts",
]

# Tables whose AUTOINCREMENT-style id must have its PostgreSQL sequence reset.
SEQUENCE_TABLES = ["budget_reservation"]


def quote_ident(name: str) -> str:
    return '"' + name.replace('"', '""') + '"'


def table_columns(sqlite_conn: sqlite3.Connection, table: str) -> list[str]:
    rows = sqlite_conn.execute(f"PRAGMA table_info({quote_ident(table)})").fetchall()
    if not rows:
        raise SystemExit(f"table {table!r} does not exist in the SQLite source")
    return [row[1] for row in rows]


def copy_table(sqlite_conn: sqlite3.Connection, pg, table: str, batch: int) -> int:
    columns = table_columns(sqlite_conn, table)
    collist = ", ".join(quote_ident(c) for c in columns)
    placeholders = ", ".join(["%s"] * len(columns))
    total = 0
    cur = sqlite_conn.execute(f"SELECT {collist} FROM {quote_ident(table)}")
    with pg.cursor() as pgcur:
        while True:
            rows = cur.fetchmany(batch)
            if not rows:
                break
            args = [(tuple(row) for row in rows)]
            for chunk in args:
                pgcur.executemany(
                    f"INSERT INTO {quote_ident(table)} ({collist}) "
                    f"VALUES ({placeholders}) ON CONFLICT DO NOTHING",
                    chunk,
                )
            total += len(rows[0]) and len(rows)
    return total


def reset_sequences(pg) -> None:
    with pg.cursor() as pgcur:
        for table in SEQUENCE_TABLES:
            pgcur.execute(
                "SELECT COALESCE(MAX(id), 0) FROM " + quote_ident(table)
            )
            (max_id,) = pgcur.fetchone()
            pgcur.execute(
                "SELECT setval(pg_get_serial_sequence(%s, 'id'), %s, %s)",
                (table, max_id, max_id > 0),
            )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sqlite", required=True, help="path to the SQLite store file")
    parser.add_argument(
        "--shard", action="append", default=[],
        help="ledger-shard sibling file (repeatable: --shard …-ledger-shard-1.db)")
    parser.add_argument("--postgres", required=True, help="PostgreSQL DSN")
    parser.add_argument("--tables", default=",".join(DEFAULT_TABLES),
                        help="comma-separated tables to copy")
    parser.add_argument("--truncate", action="store_true",
                        help="truncate each target table before copying")
    parser.add_argument("--batch", type=int, default=5000)
    args = parser.parse_args()

    try:
        import psycopg
    except ImportError:
        print("psycopg 3 is required: pip install 'psycopg[binary]'", file=sys.stderr)
        return 2

    sources = [args.sqlite, *args.shard]
    grand_total = 0
    with psycopg.connect(args.postgres) as pg:
        with pg.transaction():
            for path in sources:
                conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
                for table in args.tables.split(","):
                    table = table.strip()
                    if not table:
                        continue
                    if args.truncate:
                        with pg.cursor() as pgcur:
                            pgcur.execute(
                                f"TRUNCATE {quote_ident(table)} CASCADE"
                            )
                    copied = copy_table(conn, pg, table, args.batch)
                    print(f"{path}: {table}: {copied} rows")
                    grand_total += copied
                conn.close()
            reset_sequences(pg)
    print(f"done: {grand_total} rows copied; sequences reset")
    return 0


if __name__ == "__main__":
    sys.exit(main())
