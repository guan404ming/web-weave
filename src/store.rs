use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::monitor::MetricsSnapshot;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).context("open sqlite")?;
        Self::init_pragmas(&conn)?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn init_pragmas(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA synchronous = NORMAL;
             PRAGMA cache_size = -64000;",
        )
        .context("set pragmas")
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS urls (
                url TEXT PRIMARY KEY,
                domain TEXT NOT NULL,
                depth INTEGER NOT NULL,
                status INTEGER,
                source TEXT NOT NULL,
                discovered_at TEXT NOT NULL DEFAULT (datetime('now')),
                fetched_at TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_urls_domain ON urls(domain);

            CREATE TABLE IF NOT EXISTS domains (
                domain TEXT PRIMARY KEY,
                first_seen TEXT NOT NULL DEFAULT (datetime('now')),
                url_count INTEGER NOT NULL DEFAULT 0,
                fetched_count INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS frontier_checkpoint (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                checkpoint_at TEXT NOT NULL DEFAULT (datetime('now')),
                frontier_data BLOB NOT NULL,
                bloom_data BLOB NOT NULL
            );

            CREATE TABLE IF NOT EXISTS metrics_snapshot (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                snapshot_at TEXT NOT NULL DEFAULT (datetime('now')),
                urls_discovered INTEGER NOT NULL,
                urls_fetched INTEGER NOT NULL,
                active_domains INTEGER NOT NULL,
                current_qps REAL NOT NULL,
                success_rate REAL NOT NULL,
                p50_latency_ms INTEGER NOT NULL,
                p95_latency_ms INTEGER NOT NULL,
                p99_latency_ms INTEGER NOT NULL,
                errors_last_minute INTEGER NOT NULL
            );",
        )
        .context("create schema")
    }

    /// Batch insert discovered URLs. Uses INSERT OR IGNORE to skip duplicates.
    pub fn insert_urls_batch(
        &self,
        urls: &[(String, String, u32, String)],
    ) -> Result<()> {
        if urls.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO urls (url, domain, depth, source) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (url, domain, depth, source) in urls {
                stmt.execute(params![url, domain, depth, source])?;
            }

            // Update domain counts
            let mut domain_stmt = tx.prepare_cached(
                "INSERT INTO domains (domain, url_count) VALUES (?1, 1)
                 ON CONFLICT(domain) DO UPDATE SET url_count = url_count + 1",
            )?;
            for (_, domain, _, _) in urls {
                domain_stmt.execute(params![domain])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Mark a URL as fetched with the given HTTP status code.
    #[allow(dead_code)]
    pub fn mark_fetched(&self, url: &str, status: u16) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE urls SET status = ?1, fetched_at = datetime('now') WHERE url = ?2",
            params![status as i64, url],
        )?;
        Ok(())
    }

    /// Batch mark URLs as fetched.
    pub fn mark_fetched_batch(&self, items: &[(String, u16)]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE urls SET status = ?1, fetched_at = datetime('now') WHERE url = ?2",
            )?;
            let mut domain_stmt = tx.prepare_cached(
                "UPDATE domains SET fetched_count = fetched_count + 1 WHERE domain = ?1",
            )?;
            for (url, status) in items {
                stmt.execute(params![*status as i64, url])?;
                // Extract domain from URL for the domain counter update
                if let Some(domain) = crate::parser::extract_domain(url) {
                    domain_stmt.execute(params![domain])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Save a checkpoint with serialized frontier and bloom filter data.
    pub fn save_checkpoint(&self, frontier_data: &[u8], bloom_data: &[u8]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO frontier_checkpoint (frontier_data, bloom_data) VALUES (?1, ?2)",
            params![frontier_data, bloom_data],
        )?;
        // Keep only the last 3 checkpoints
        conn.execute(
            "DELETE FROM frontier_checkpoint WHERE id NOT IN (
                SELECT id FROM frontier_checkpoint ORDER BY id DESC LIMIT 3
            )",
            [],
        )?;
        Ok(())
    }

    /// Load the most recent checkpoint.
    pub fn load_latest_checkpoint(&self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT frontier_data, bloom_data FROM frontier_checkpoint ORDER BY id DESC LIMIT 1",
        )?;
        let result = stmt
            .query_row([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                ))
            })
            .optional()?;
        Ok(result)
    }

    /// Check if a checkpoint exists (for resume logic).
    #[allow(dead_code)]
    pub fn has_checkpoint(&self) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM frontier_checkpoint",
            [],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Save a metrics snapshot.
    pub fn save_metrics(&self, snapshot: &MetricsSnapshot) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO metrics_snapshot (
                urls_discovered, urls_fetched, active_domains,
                current_qps, success_rate,
                p50_latency_ms, p95_latency_ms, p99_latency_ms,
                errors_last_minute
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                snapshot.urls_discovered as i64,
                snapshot.urls_fetched as i64,
                snapshot.active_domains as i64,
                snapshot.current_qps,
                snapshot.success_rate,
                snapshot.p50_latency_ms as i64,
                snapshot.p95_latency_ms as i64,
                snapshot.p99_latency_ms as i64,
                snapshot.errors_last_minute as i64,
            ],
        )?;
        Ok(())
    }
}

/// Extension trait for optional query results.
trait OptionalExt<T> {
    fn optional(self) -> Result<Option<T>>;
}

impl<T> OptionalExt<T> for rusqlite::Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
