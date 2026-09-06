//! The runner's own small database (MULTI-NODE 11.3).
//!
//! A runner keeps no instance rows: the controller owns those. It keeps
//! only what it must remember across a process restart and a host reboot,
//! which is the highest controller epoch it has accepted and the outcome
//! of each mutation it finished. Both are worthless in memory, because a
//! runner that forgets them on reboot would accept an old controller.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;
use time::OffsetDateTime;

use crate::fence::{Error, FenceStore};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS fence (
    id             INTEGER PRIMARY KEY CHECK (id = 1),
    accepted_epoch INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO fence (id) VALUES (1);

-- The highest generation this runner accepted for each object, and what
-- that generation wanted (MULTI-NODE 11.3).
CREATE TABLE IF NOT EXISTS object_generations (
    object_id  TEXT    PRIMARY KEY,
    generation INTEGER NOT NULL,
    digest     TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS request_outcomes (
    request_id  TEXT PRIMARY KEY,
    outcome     TEXT NOT NULL,
    finished_at TEXT NOT NULL
);
";

pub struct SqliteFence {
    conn: Mutex<Connection>,
}

impl SqliteFence {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // Durability over speed. This file exists to survive a power cut;
        // a faster setting would defeat the only reason it is on disk.
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(SqliteFence {
            conn: Mutex::new(conn),
        })
    }

    pub fn in_memory() -> Result<Self, Error> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(SqliteFence {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl FenceStore for SqliteFence {
    fn accepted_epoch(&self) -> Result<i64, Error> {
        Ok(self
            .conn()
            .query_row("SELECT accepted_epoch FROM fence WHERE id = 1", [], |row| {
                row.get(0)
            })?)
    }

    fn accept_epoch(&self, epoch: i64) -> Result<(), Error> {
        // `MAX` is the guard: a controller cannot talk this runner
        // backwards, whatever it sends (MULTI-NODE 11.4).
        self.conn().execute(
            "UPDATE fence SET accepted_epoch = MAX(accepted_epoch, ?) WHERE id = 1",
            [epoch],
        )?;
        Ok(())
    }

    fn outcome(&self, request_id: &str) -> Result<Option<String>, Error> {
        let conn = self.conn();
        let mut statement =
            conn.prepare("SELECT outcome FROM request_outcomes WHERE request_id = ?")?;
        let mut rows = statement.query([request_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    fn object_generation(&self, object_id: &str) -> Result<Option<(i64, String)>, Error> {
        let conn = self.conn();
        let mut statement =
            conn.prepare("SELECT generation, digest FROM object_generations WHERE object_id = ?")?;
        let mut rows = statement.query([object_id])?;
        match rows.next()? {
            Some(row) => Ok(Some((row.get(0)?, row.get(1)?))),
            None => Ok(None),
        }
    }

    fn accept_object(&self, object_id: &str, generation: i64, digest: &str) -> Result<(), Error> {
        // The `WHERE` is the guard: a later order cannot be undone by an
        // earlier one arriving afterwards (MULTI-NODE 11.4).
        self.conn().execute(
            "INSERT INTO object_generations (object_id, generation, digest) VALUES (?, ?, ?) \
             ON CONFLICT(object_id) DO UPDATE SET \
               generation = excluded.generation, digest = excluded.digest \
             WHERE excluded.generation > object_generations.generation",
            rusqlite::params![object_id, generation, digest],
        )?;
        Ok(())
    }

    fn record_outcome(&self, request_id: &str, outcome: &str) -> Result<(), Error> {
        let finished_at = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|error| Error::Time(error.to_string()))?;
        self.conn().execute(
            "INSERT OR REPLACE INTO request_outcomes (request_id, outcome, finished_at) \
             VALUES (?, ?, ?)",
            rusqlite::params![request_id, outcome, finished_at],
        )?;
        Ok(())
    }
}
