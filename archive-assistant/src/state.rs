use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

pub struct StateDb {
    conn: Connection,
}

impl StateDb {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS processed (
                path         TEXT    NOT NULL PRIMARY KEY,
                mtime        INTEGER NOT NULL,
                processed_at INTEGER NOT NULL
            );",
        )?;
        Ok(Self { conn })
    }

    /// Returns true if this (path, mtime) pair is already recorded.
    pub fn is_current(&self, path: &str, mtime: u64) -> Result<bool> {
        let stored: Option<i64> = self
            .conn
            .query_row(
                "SELECT mtime FROM processed WHERE path = ?1",
                [path],
                |row| row.get(0),
            )
            .optional()?;
        Ok(stored == Some(mtime as i64))
    }

    /// Record that a file has been processed (or checked and skipped).
    pub fn record(&self, path: &str, mtime: u64) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        self.conn.execute(
            "INSERT OR REPLACE INTO processed (path, mtime, processed_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![path, mtime as i64, now as i64],
        )?;
        Ok(())
    }
}
