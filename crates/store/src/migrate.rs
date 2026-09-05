//! Ordered schema migrations (MULTI-NODE section 16).
//!
//! Version 1 evolved the schema with `CREATE TABLE IF NOT EXISTS` and one
//! ad hoc column check. That is enough while every deployment runs one
//! host and one binary. It is not enough once a controller and its
//! runners can hold different builds, because nothing records which
//! shape a database already has.
//!
//! So the schema now has a version. `schema.sql` remains the baseline
//! that creates a new database, and every change after that baseline is
//! one numbered migration applied in order and recorded in
//! `schema_migrations`.
//!
//! Two rules keep this safe for a database that predates the table:
//!
//! 1. `schema.sql` still runs first and is still idempotent, so an
//!    existing database is unchanged by it.
//! 2. Every migration inspects before it mutates. A migration whose work
//!    the old ad hoc path already did finds nothing to do and only
//!    records its version.
//!
//! A migration never rewrites an earlier migration. To correct one, add
//! the next number.

use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::Result;

/// One numbered step from the baseline schema to the current shape.
struct Migration {
    /// Strictly increasing, starting at 1. Never reused, never reordered.
    version: i64,
    /// Short identifier recorded beside the version, for operators
    /// reading the table by hand.
    name: &'static str,
    /// The work. It runs inside one immediate transaction with the row
    /// that records it, so a failure leaves neither behind.
    run: fn(&Transaction<'_>) -> rusqlite::Result<()>,
}

/// Every migration, in the order they apply.
const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "image_source_columns",
    run: image_source_columns,
}];

/// Records which migrations a database has. Created before any of them
/// runs, and never dropped.
const MIGRATIONS_TABLE: &str = "CREATE TABLE IF NOT EXISTS schema_migrations (
    version    INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL,
    applied_at TEXT    NOT NULL
);";

/// Applies every migration the database does not have, oldest first.
///
/// Call this after `schema.sql`, on a connection no other writer holds.
pub(crate) fn apply(conn: &mut Connection, now: crate::Clock) -> Result<()> {
    debug_assert!(
        MIGRATIONS
            .windows(2)
            .all(|pair| pair[0].version < pair[1].version),
        "migration versions must be strictly increasing",
    );
    debug_assert!(
        MIGRATIONS.first().is_none_or(|first| first.version == 1),
        "migration versions must start at 1",
    );

    conn.execute_batch(MIGRATIONS_TABLE)?;
    let current = current_version(conn)?;

    for migration in MIGRATIONS.iter().filter(|m| m.version > current) {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        (migration.run)(&tx)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?, ?, ?)",
            rusqlite::params![
                migration.version,
                migration.name,
                crate::format_time(now())?
            ],
        )?;
        tx.commit()?;
    }
    Ok(())
}

/// The highest recorded version, or 0 for a database with none.
fn current_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )
}

/// Whether a table already has a column. SQLite has no portable
/// `ADD COLUMN IF NOT EXISTS`, so every column migration asks first.
fn has_column(tx: &Transaction<'_>, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut statement = tx.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Adds the version 1.1 image-source columns to databases created by 0.9.
///
/// This was the one ad hoc migration, and it ran on every open. A
/// database that already took it finds all three columns present and
/// only records the version.
fn image_source_columns(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    if !has_column(tx, "images", "kind")? {
        tx.execute_batch(
            "ALTER TABLE images ADD COLUMN kind TEXT NOT NULL DEFAULT 'qcow2' \
             CHECK (kind IN ('qcow2', 'oci'));",
        )?;
    }
    if !has_column(tx, "image_versions", "source_digest")? {
        tx.execute_batch("ALTER TABLE image_versions ADD COLUMN source_digest TEXT;")?;
    }
    if !has_column(tx, "image_versions", "kind")? {
        tx.execute_batch(
            "ALTER TABLE image_versions ADD COLUMN kind TEXT NOT NULL DEFAULT 'qcow2' \
             CHECK (kind IN ('qcow2', 'oci')); \
             UPDATE image_versions SET kind = 'oci' WHERE source_digest IS NOT NULL;",
        )?;
    }
    tx.execute_batch(
        "INSERT OR IGNORE INTO image_source_versions (image_name, source_digest, checksum) \
         SELECT image_name, source_digest, checksum FROM image_versions \
         WHERE source_digest IS NOT NULL;",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> crate::Clock {
        std::sync::Arc::new(|| time::OffsetDateTime::UNIX_EPOCH)
    }

    fn baseline() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::SCHEMA_SQL).unwrap();
        conn
    }

    #[test]
    fn applies_every_migration_once() {
        let mut conn = baseline();
        apply(&mut conn, clock()).unwrap();
        let after_first = current_version(&conn).unwrap();
        assert_eq!(after_first, MIGRATIONS.last().unwrap().version);

        // A second open must be a no-op, not a second application.
        apply(&mut conn, clock()).unwrap();
        assert_eq!(current_version(&conn).unwrap(), after_first);
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, MIGRATIONS.len() as i64);
    }

    #[test]
    fn adopts_a_database_that_predates_the_table() {
        // A 0.9 database: no `kind`, no `source_digest`, and no record of
        // any migration. The columns must appear exactly once.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE images (
                 name             TEXT PRIMARY KEY,
                 url              TEXT NOT NULL,
                 pinned_checksum  TEXT,
                 current_checksum TEXT
             );
             CREATE TABLE image_versions (
                 checksum   TEXT    PRIMARY KEY,
                 image_name TEXT    NOT NULL,
                 path       TEXT    NOT NULL UNIQUE,
                 size       INTEGER NOT NULL,
                 fetched_at TEXT    NOT NULL
             );
             CREATE TABLE image_source_versions (
                 image_name    TEXT NOT NULL,
                 source_digest TEXT NOT NULL,
                 checksum      TEXT NOT NULL,
                 PRIMARY KEY (image_name, source_digest)
             );",
        )
        .unwrap();
        let mut conn = conn;
        apply(&mut conn, clock()).unwrap();

        let tx = conn.transaction().unwrap();
        assert!(has_column(&tx, "images", "kind").unwrap());
        assert!(has_column(&tx, "image_versions", "source_digest").unwrap());
        assert!(has_column(&tx, "image_versions", "kind").unwrap());
        drop(tx);
        assert_eq!(current_version(&conn).unwrap(), 1);
    }

    #[test]
    fn a_failing_migration_records_nothing() {
        fn fails(_: &Transaction<'_>) -> rusqlite::Result<()> {
            Err(rusqlite::Error::InvalidQuery)
        }
        let mut conn = baseline();
        conn.execute_batch(MIGRATIONS_TABLE).unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let result = fails(&tx);
        assert!(result.is_err());
        drop(tx);
        assert_eq!(current_version(&conn).unwrap(), 0);
    }
}
