//! Durable SQLite store for Sandhi — the usage-event sink + aggregation queries, plus the
//! operator tables introduced by TD-0003 (the [`vault`] credential index, [`vkeys`]
//! virtual-key store, and [`alerts`] threshold rules). Kept in its own crate (not `sandhi-core`)
//! so the language bindings' wheels never pull in bundled SQLite.

pub mod alerts;
pub mod ledger;
pub mod vault;
pub mod vkeys;

use std::collections::BTreeMap;
use std::sync::Mutex;

use rusqlite::{params, Connection};
use sandhi_core::{Backend, LatencySummary, Sink, UsageAggregateV1, UsageEvent};

pub use alerts::{AlertRuleRecord, AlertStore, CreateAlertRequest};
pub use ledger::{BudgetRow, ReserveOutcome, SqliteLedger};
pub use vault::{
    hash_secret, CredentialScheme, InMemoryVault, KeyringVault, SentinelPassVault, Vault,
    VaultEntry, VaultError, VaultStore,
};
pub use vkeys::{MintRequest, MintedKey, VirtualKeyRecord, VirtualKeyStore};

/// Per-row mirror of [`sandhi_core::billable_parts`], summed. The `CASE` reproduces the ADR-0005
/// D4 reasoning fold **per call**, which is exactly why this cannot be written as
/// `SUM(reasoning) > SUM(tokens_out)`. `store_matches_core_billable` pins this expression against
/// the Rust one so the two cannot drift.
/// Raw latency samples for one group key: `(duration_ms, time_to_first_token_ms)`.
/// TTFT is shorter than duration whenever a call was non-streaming.
type LatencySamples = (Vec<u64>, Vec<u64>);

/// Most-recent calls sampled per query when summarising latency (TD-0009 D3).
const LATENCY_SAMPLE_LIMIT: usize = 10_000;

const BILLABLE_SQL: &str = "COALESCE(SUM(tokens_in + cache_creation_tokens + cache_read_tokens \
     + tokens_out + CASE WHEN COALESCE(reasoning_tokens,0) > tokens_out \
     THEN COALESCE(reasoning_tokens,0) ELSE 0 END),0)";

/// One aggregation row (or the grand total).
///
/// **This is the contract type, not a store-private struct** (TD-0009 D1): the aggregate crosses
/// into the dashboard, the `sandhi` CLI, both language bindings, and later the control plane, so
/// it is defined and schema'd once in `sandhi-core` and merely *computed* here. The SQL below is
/// an index over [`sandhi_core::UsageAggregator`]'s fold — an optimization for large histories,
/// never a second definition. `store_matches_core_fold` proves the two agree.
pub type Bucket = UsageAggregateV1;

/// A SQLite-backed usage store.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open (creating if needed) a store at `path`.
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// An ephemeral in-memory store (tests / demos).
    pub fn in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn init(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS usage_events (
                request_id TEXT, occurred_at TEXT, provider TEXT, model TEXT, backend TEXT,
                virtual_key_id TEXT, subject_id TEXT, group_id TEXT, route TEXT, session_id TEXT,
                tokens_in INTEGER, tokens_out INTEGER,
                cache_creation_tokens INTEGER, cache_read_tokens INTEGER, gpu_seconds REAL,
                duration_ms INTEGER, time_to_first_token_ms INTEGER, reasoning_tokens INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_usage_subject ON usage_events(subject_id);
            CREATE INDEX IF NOT EXISTS idx_usage_group ON usage_events(group_id);
            CREATE INDEX IF NOT EXISTS idx_usage_provider ON usage_events(provider);
            CREATE INDEX IF NOT EXISTS idx_usage_model ON usage_events(model);
            CREATE INDEX IF NOT EXISTS idx_usage_vkey ON usage_events(virtual_key_id);
            CREATE INDEX IF NOT EXISTS idx_usage_session ON usage_events(session_id);
            CREATE INDEX IF NOT EXISTS idx_usage_occurred ON usage_events(occurred_at);",
        )?;
        // Additive columns for databases created before the latency/reasoning fields
        // existed. SQLite has no ADD COLUMN IF NOT EXISTS — attempt and ignore the
        // duplicate-column error so init stays idempotent.
        for ddl in [
            "ALTER TABLE usage_events ADD COLUMN duration_ms INTEGER",
            "ALTER TABLE usage_events ADD COLUMN time_to_first_token_ms INTEGER",
            "ALTER TABLE usage_events ADD COLUMN reasoning_tokens INTEGER",
        ] {
            match conn.execute(ddl, []) {
                Ok(_) => {}
                Err(e) if e.to_string().contains("duplicate column name") => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn insert(&self, e: &UsageEvent) -> rusqlite::Result<()> {
        let backend = match e.backend {
            Backend::External => "external",
            Backend::SelfHosted => "self_hosted",
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO usage_events (
                request_id, occurred_at, provider, model, backend,
                virtual_key_id, subject_id, group_id, route, session_id,
                tokens_in, tokens_out, cache_creation_tokens, cache_read_tokens, gpu_seconds,
                duration_ms, time_to_first_token_ms, reasoning_tokens
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            params![
                e.request_id,
                e.occurred_at,
                e.provider,
                e.model,
                backend,
                e.virtual_key_id,
                e.subject_id,
                e.group_id,
                e.route,
                e.session_id,
                e.tokens_in as i64,
                e.tokens_out as i64,
                e.cache_creation_tokens as i64,
                e.cache_read_tokens as i64,
                e.gpu_seconds,
                e.duration_ms.map(|v| v as i64),
                e.time_to_first_token_ms.map(|v| v as i64),
                e.reasoning_tokens.map(|v| v as i64),
            ],
        )?;
        Ok(())
    }

    /// Totals grouped by a fixed column (`subject_id` / `group_id` / `provider` / `model` /
    /// `virtual_key_id` / `session_id`), busiest first. An optional RFC 3339 `since` lower-bounds
    /// `occurred_at`.
    fn totals_grouped_since(
        &self,
        col: &str,
        since: Option<&str>,
    ) -> rusqlite::Result<Vec<Bucket>> {
        let conn = self.conn.lock().unwrap();
        let (where_clause, params): (String, Vec<Box<dyn rusqlite::ToSql>>) = match since {
            Some(s) => (
                "WHERE occurred_at >= ?1".into(),
                vec![Box::new(s.to_string())],
            ),
            None => ("WHERE 1=1".into(), vec![]),
        };
        let sql = format!(
            "SELECT COALESCE({col}, '(none)') AS k, COUNT(*), \
                COALESCE(SUM(tokens_in),0), COALESCE(SUM(tokens_out),0), \
                COALESCE(SUM(cache_creation_tokens),0), COALESCE(SUM(cache_read_tokens),0), \
                COALESCE(SUM(reasoning_tokens),0), {BILLABLE_SQL} \
             FROM usage_events {where_clause} GROUP BY k \
             ORDER BY {BILLABLE_SQL} DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), |r| {
            Ok(Bucket {
                key: r.get(0)?,
                calls: r.get::<_, i64>(1)? as u64,
                tokens_in: r.get::<_, i64>(2)? as u64,
                tokens_out: r.get::<_, i64>(3)? as u64,
                cache_creation_tokens: r.get::<_, i64>(4)? as u64,
                cache_read_tokens: r.get::<_, i64>(5)? as u64,
                reasoning_tokens: r.get::<_, i64>(6)? as u64,
                billable_tokens: r.get::<_, i64>(7)? as u64,
                latency: None,
            })
        })?;
        let mut buckets: Vec<Bucket> = rows.collect::<rusqlite::Result<_>>()?;
        let samples = Self::latency_samples(&conn, col, since)?;
        for b in &mut buckets {
            if let Some((d, t)) = samples.get(&b.key) {
                b.latency = LatencySummary::from_samples(d, t);
            }
        }
        Ok(buckets)
    }

    /// Latency samples per group key, most recent first and bounded.
    ///
    /// SQLite has no percentile function, so the percentile itself is computed by
    /// [`LatencySummary::from_samples`] — the same code the in-process aggregator uses, because a
    /// second percentile implementation is a second definition (TD-0009 D6). The bound is what
    /// makes this affordable on a large history and is why the summary carries `samples`: a
    /// consumer can see the weight behind the number instead of assuming a full scan.
    fn latency_samples(
        conn: &Connection,
        col: &str,
        since: Option<&str>,
    ) -> rusqlite::Result<BTreeMap<String, LatencySamples>> {
        let (where_clause, params): (String, Vec<Box<dyn rusqlite::ToSql>>) = match since {
            Some(s) => (
                "WHERE occurred_at >= ?1 AND duration_ms IS NOT NULL".into(),
                vec![Box::new(s.to_string())],
            ),
            None => ("WHERE duration_ms IS NOT NULL".into(), vec![]),
        };
        let sql = format!(
            "SELECT COALESCE({col}, '(none)') AS k, duration_ms, time_to_first_token_ms \
             FROM usage_events {where_clause} \
             ORDER BY occurred_at DESC LIMIT {LATENCY_SAMPLE_LIMIT}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut out: BTreeMap<String, LatencySamples> = BTreeMap::new();
        let rows = stmt.query_map(param_refs.as_slice(), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, Option<i64>>(2)?,
            ))
        })?;
        for row in rows {
            let (k, d, t) = row?;
            let entry = out.entry(k).or_default();
            entry.0.push(d);
            if let Some(t) = t {
                entry.1.push(t as u64);
            }
        }
        Ok(out)
    }

    /// Totals grouped by a fixed column, busiest first (no time window).
    fn totals_grouped(&self, col: &str) -> rusqlite::Result<Vec<Bucket>> {
        self.totals_grouped_since(col, None)
    }

    pub fn totals_by_subject(&self) -> rusqlite::Result<Vec<Bucket>> {
        self.totals_grouped("subject_id")
    }

    pub fn totals_by_group(&self) -> rusqlite::Result<Vec<Bucket>> {
        self.totals_grouped("group_id")
    }

    pub fn totals_by_provider(&self) -> rusqlite::Result<Vec<Bucket>> {
        self.totals_grouped("provider")
    }

    /// TD-0003 P1 attribution: per-model totals.
    pub fn totals_by_model(&self) -> rusqlite::Result<Vec<Bucket>> {
        self.totals_grouped("model")
    }

    /// TD-0003 P1 attribution: per-virtual-key totals.
    pub fn totals_by_virtual_key(&self) -> rusqlite::Result<Vec<Bucket>> {
        self.totals_grouped("virtual_key_id")
    }

    /// TD-0003 P1 attribution: per-session totals.
    pub fn totals_by_session(&self) -> rusqlite::Result<Vec<Bucket>> {
        self.totals_grouped("session_id")
    }

    /// Windowed variant: totals since an RFC 3339 timestamp, grouped by `dimension`
    /// (`subject` | `group` | `provider` | `model` | `key` | `session`). Returns `None` for an
    /// unknown dimension (the caller maps the short name).
    pub fn totals_since(
        &self,
        dimension: &str,
        since: &str,
    ) -> rusqlite::Result<Option<Vec<Bucket>>> {
        let col = match dimension {
            "subject" | "user" => "subject_id",
            "group" => "group_id",
            "provider" => "provider",
            "model" => "model",
            "key" | "virtual_key" => "virtual_key_id",
            "session" => "session_id",
            _ => return Ok(None),
        };
        Ok(Some(self.totals_grouped_since(col, Some(since))?))
    }

    /// The grand total across every event.
    pub fn grand_total(&self) -> rusqlite::Result<Bucket> {
        let conn = self.conn.lock().unwrap();
        // `'total'` as the grouping expression funnels every row into one key, so the same
        // sampling path serves the grand total — no second latency query to keep in step.
        let samples = Self::latency_samples(&conn, "'total'", None)?;
        let latency = samples
            .get("total")
            .and_then(|(d, t)| LatencySummary::from_samples(d, t));
        conn.query_row(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(tokens_in),0), COALESCE(SUM(tokens_out),0), \
                    COALESCE(SUM(cache_creation_tokens),0), COALESCE(SUM(cache_read_tokens),0), \
                    COALESCE(SUM(reasoning_tokens),0), {BILLABLE_SQL} FROM usage_events"
            ),
            [],
            |r| {
                Ok(Bucket {
                    key: "total".to_string(),
                    calls: r.get::<_, i64>(0)? as u64,
                    tokens_in: r.get::<_, i64>(1)? as u64,
                    tokens_out: r.get::<_, i64>(2)? as u64,
                    cache_creation_tokens: r.get::<_, i64>(3)? as u64,
                    cache_read_tokens: r.get::<_, i64>(4)? as u64,
                    reasoning_tokens: r.get::<_, i64>(5)? as u64,
                    billable_tokens: r.get::<_, i64>(6)? as u64,
                    latency: latency.clone(),
                })
            },
        )
    }
}

impl Sink for SqliteStore {
    fn emit(&self, event: &UsageEvent) {
        // Best-effort — a storage failure must never break the caller (ADR-0047 D7).
        let _ = self.insert(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandhi_core::UsageEvent;

    fn ev(provider: &str, subject: &str, group: &str, tin: u64, tout: u64) -> UsageEvent {
        UsageEvent::new(
            "r",
            "2026-07-19T00:00:00Z",
            provider,
            "m",
            Backend::External,
        )
        .with_attribution(Some("vk".into()), Some(subject.into()), Some(group.into()))
        .with_tokens(tin, tout)
        .with_cache(0, 5)
    }

    #[test]
    fn persists_and_aggregates() {
        let store = SqliteStore::in_memory().unwrap();
        store.emit(&ev("openai", "alice", "team-a", 100, 20));
        store.emit(&ev("openai", "alice", "team-a", 50, 10));
        store.emit(&ev("anthropic", "bob", "team-b", 200, 40));

        let total = store.grand_total().unwrap();
        assert_eq!(total.calls, 3);
        assert_eq!(total.tokens_in, 350);
        assert_eq!(total.tokens_out, 70);
        assert_eq!(total.cache_read_tokens, 15);

        let by_subject = store.totals_by_subject().unwrap();
        // Billable is the ADR-0005 D4 quantity, so the cache split counts: bob is
        // 200 in + 5 cache-read + 40 out = 245, alice is (100+5+20) + (50+5+10) = 190.
        // Under the old narrow in+out helper these read 240 and 180 — less than the ledger
        // actually charged for the same calls.
        assert_eq!(by_subject[0].key, "bob");
        assert_eq!(by_subject[0].billable_tokens, 245);
        assert_eq!(by_subject[0].cache_read_tokens, 5);
        let alice = by_subject.iter().find(|b| b.key == "alice").unwrap();
        assert_eq!(alice.calls, 2);
        assert_eq!(alice.tokens_in, 150);

        let by_provider = store.totals_by_provider().unwrap();
        assert_eq!(by_provider.len(), 2);
        let openai = by_provider.iter().find(|b| b.key == "openai").unwrap();
        assert_eq!(openai.calls, 2);
    }

    #[test]
    fn persists_latency_and_reasoning_columns() {
        let store = SqliteStore::in_memory().unwrap();
        let event = ev("openai", "alice", "team-a", 100, 20)
            .with_latency(Some(1234), Some(210))
            .with_reasoning(Some(33));
        store.emit(&event);

        let conn = store.conn.lock().unwrap();
        let (duration, ttft, reasoning): (Option<i64>, Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT duration_ms, time_to_first_token_ms, reasoning_tokens FROM usage_events",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(duration, Some(1234));
        assert_eq!(ttft, Some(210));
        assert_eq!(reasoning, Some(33));
    }

    #[test]
    fn init_migrates_pre_latency_databases() {
        // Simulate a database created before the latency/reasoning columns existed.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE usage_events (
                request_id TEXT, occurred_at TEXT, provider TEXT, model TEXT, backend TEXT,
                virtual_key_id TEXT, subject_id TEXT, group_id TEXT, route TEXT, session_id TEXT,
                tokens_in INTEGER, tokens_out INTEGER,
                cache_creation_tokens INTEGER, cache_read_tokens INTEGER, gpu_seconds REAL
            );",
        )
        .unwrap();

        SqliteStore::init(&conn).unwrap();
        // Idempotent: a second init must not fail on the now-present columns.
        SqliteStore::init(&conn).unwrap();

        let store = SqliteStore {
            conn: Mutex::new(conn),
        };
        let event = ev("openai", "alice", "team-a", 1, 2).with_latency(Some(5), None);
        store.emit(&event);
        assert_eq!(store.grand_total().unwrap().calls, 1);
    }

    #[test]
    fn store_matches_core_fold() {
        // TD-0009 D6: the core fold is the definition and SQL is an index over it, so the two
        // must agree row for row — including the per-call reasoning fold and the ranking.
        use sandhi_core::{Dimension, UsageAggregator};

        let store = SqliteStore::in_memory().unwrap();
        let mut agg = UsageAggregator::new(Dimension::Subject);
        let corpus = [
            ev("openai", "alice", "team-a", 100, 20),
            ev("openai", "alice", "team-a", 50, 10),
            ev("anthropic", "bob", "team-b", 200, 40),
            ev("openai", "bob", "team-b", 10, 10).with_reasoning(Some(4)), // folded
            ev("openai", "carol", "team-b", 10, 3).with_reasoning(Some(8)), // unfolded
        ];
        for e in &corpus {
            store.emit(e);
            agg.add(e);
        }

        let sql_rows = store.totals_by_subject().unwrap();
        let fold_rows = agg.rows();
        assert_eq!(sql_rows.len(), fold_rows.len());
        for (sql, fold) in sql_rows.iter().zip(fold_rows.iter()) {
            assert_eq!(sql, fold, "SQL row must equal the core fold row");
        }
        assert_eq!(store.grand_total().unwrap(), agg.total());
    }

    #[test]
    fn store_reads_back_latency_it_records() {
        // #68 added duration/TTFT and nothing ever read them. Prove the round trip.
        let store = SqliteStore::in_memory().unwrap();
        for ms in [10_u64, 20, 30, 40, 100] {
            store
                .emit(&ev("openai", "alice", "team-a", 10, 5).with_latency(Some(ms), Some(ms / 2)));
        }
        let l = store
            .grand_total()
            .unwrap()
            .latency
            .expect("durations were recorded");
        assert_eq!(l.samples, 5);
        assert_eq!(l.p50_ms, 30);
        assert_eq!(l.p95_ms, 100);
        assert_eq!(l.ttft_p50_ms, Some(15));
        // Same values through the grouped path.
        let by_subject = store.totals_by_subject().unwrap();
        assert_eq!(by_subject[0].latency.as_ref().unwrap().p50_ms, 30);
    }

    #[test]
    fn latency_is_absent_when_nothing_reported_it() {
        let store = SqliteStore::in_memory().unwrap();
        store.emit(&ev("openai", "alice", "team-a", 10, 5));
        assert!(store.grand_total().unwrap().latency.is_none());
    }

    #[test]
    fn store_matches_core_billable() {
        // TD-0009 D6 applied here: one billable definition, two implementations (the Rust
        // `billable_parts` and the SQL mirror), proven equal rather than assumed.
        //
        // The fixtures are chosen so a naive aggregate — summing the columns and THEN applying
        // the reasoning fold — gives a different answer. One call folds reasoning into output
        // (4 <= 10, adds nothing), the other reports it separately (8 > 3, adds 8).
        let store = SqliteStore::in_memory().unwrap();
        let folded = ev("openai", "alice", "team-a", 10, 10)
            .with_cache(0, 0)
            .with_reasoning(Some(4));
        let unfolded = ev("openai", "alice", "team-a", 10, 3)
            .with_cache(0, 0)
            .with_reasoning(Some(8));
        store.emit(&folded);
        store.emit(&unfolded);

        let rust_sum = folded.billable_tokens() + unfolded.billable_tokens(); // 20 + 21
        let total = store.grand_total().unwrap();
        assert_eq!(
            total.billable_tokens, rust_sum,
            "SQL must mirror sandhi_core::billable_parts per row"
        );

        // Guard the guard: if the fold were applied to the summed columns instead of per call,
        // it would read 33 (reasoning 12 is not > output 13, so nothing is added) instead of 41.
        let naive = total.tokens_in
            + total.cache_creation_tokens
            + total.cache_read_tokens
            + total.tokens_out
            + if total.reasoning_tokens > total.tokens_out {
                total.reasoning_tokens
            } else {
                0
            };
        assert_ne!(
            naive, rust_sum,
            "fixtures must expose the per-call vs aggregate fold difference, else this proves nothing"
        );
    }

    #[test]
    fn empty_store_is_zero() {
        let store = SqliteStore::in_memory().unwrap();
        let total = store.grand_total().unwrap();
        assert_eq!(total.calls, 0);
        assert_eq!(total.billable_tokens, 0);
        assert!(store.totals_by_group().unwrap().is_empty());
    }
}
