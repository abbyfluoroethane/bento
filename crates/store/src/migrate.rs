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
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "image_source_columns",
        run: image_source_columns,
    },
    Migration {
        version: 2,
        name: "host_machine_id",
        run: host_machine_id,
    },
    Migration {
        version: 3,
        name: "runners_and_slots",
        run: runners_and_slots,
    },
    Migration {
        version: 4,
        name: "host_observations",
        run: host_observations,
    },
    Migration {
        version: 5,
        name: "host_images",
        run: host_images,
    },
    Migration {
        version: 6,
        name: "host_underlay",
        run: host_underlay,
    },
    Migration {
        version: 7,
        name: "dispatch_generations",
        run: dispatch_generations,
    },
];

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

/// Keys a host row on the machine it names (MULTI-NODE 16).
///
/// Version 1 read the transient kernel hostname and keyed the row on that
/// string, so each rename minted a row and the instances of one machine
/// spread across rows that all described it. A version-1 deployment ran
/// one host by construction, so every row here is that machine, however
/// many names it answered to. Keep the oldest row, move the instances of
/// the others onto it, and let the machine claim it at the next startup.
fn host_machine_id(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    if !has_column(tx, "hosts", "machine_id")? {
        tx.execute_batch("ALTER TABLE hosts ADD COLUMN machine_id TEXT;")?;
    }

    // Instances move first. They reference the rows that are about to go.
    let keep: Option<i64> = tx.query_row("SELECT MIN(id) FROM hosts", [], |row| row.get(0))?;
    if let Some(keep) = keep {
        tx.execute(
            "UPDATE instances SET host_id = ? WHERE host_id <> ?",
            rusqlite::params![keep, keep],
        )?;
        tx.execute("DELETE FROM hosts WHERE id <> ?", [keep])?;
    }

    // Partial, because a row this migration collapsed carries no machine
    // ID until the machine it describes claims it at the next startup.
    tx.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_hosts_machine_id \
         ON hosts(machine_id) WHERE machine_id IS NOT NULL;",
    )?;
    Ok(())
}

/// Adds the runner and slot model (MULTI-NODE 16).
///
/// A version-1 deployment ran one host and gave each user a whole `/24`.
/// That is exactly one slot, numbered 0, owned by that host, so the
/// migration writes it rather than asking anybody. Existing addresses
/// stay valid, because a `/24` slot has the same bounds the `/24` had.
fn runners_and_slots(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    // What the controller needs to reach a runner and to decide whether
    // it may place there (MULTI-NODE 16). A version-1 host has no
    // endpoint: the controller talks to its own libvirt socket.
    if !has_column(tx, "hosts", "endpoint")? {
        tx.execute_batch("ALTER TABLE hosts ADD COLUMN endpoint TEXT;")?;
    }
    if !has_column(tx, "hosts", "enabled")? {
        tx.execute_batch("ALTER TABLE hosts ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;")?;
    }
    if !has_column(tx, "hosts", "placement")? {
        tx.execute_batch(
            "ALTER TABLE hosts ADD COLUMN placement TEXT NOT NULL DEFAULT 'active' \
             CHECK (placement IN ('active', 'draining', 'removed'));",
        )?;
    }

    // The slot that supplied the address of an instance (MULTI-NODE 16).
    if !has_column(tx, "instances", "slot")? {
        tx.execute_batch(
            "ALTER TABLE instances ADD COLUMN slot INTEGER REFERENCES runner_slots(slot);",
        )?;
    }

    // One host, one slot, every instance on it. Migration 2 left one host
    // row, so this reads that row rather than choosing between rows.
    let host: Option<i64> = tx.query_row("SELECT MIN(id) FROM hosts", [], |row| row.get(0))?;
    if let Some(host) = host {
        tx.execute(
            "INSERT OR IGNORE INTO runner_slots (slot, owner_host_id) VALUES (0, ?)",
            [host],
        )?;
        tx.execute("UPDATE instances SET slot = 0 WHERE slot IS NULL", [])?;
    }

    // An instance may only hold an address from a slot its own host owns
    // (MULTI-NODE 16). The controller checks this in the transaction that
    // claims the address; the triggers are what make the rule true of the
    // table itself, whatever writes to it.
    for (name, event) in [
        ("instances_slot_owner_insert", "INSERT"),
        ("instances_slot_owner_update", "UPDATE"),
    ] {
        tx.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS {name} \
             BEFORE {event} ON instances FOR EACH ROW WHEN NEW.slot IS NOT NULL \
             BEGIN \
               SELECT RAISE(ABORT, 'instance host does not own its slot') \
               WHERE NOT EXISTS ( \
                 SELECT 1 FROM runner_slots \
                 WHERE slot = NEW.slot AND owner_host_id = NEW.host_id \
               ); \
             END;"
        ))?;
    }
    Ok(())
}

/// Adds the table that records what a runner last said (MULTI-NODE 16).
///
/// `schema.sql` creates it for a new database. This migration exists so
/// an upgraded database gets it too, and so the version is recorded.
fn host_observations(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS host_observations (
             host_id               INTEGER PRIMARY KEY REFERENCES hosts(id) ON DELETE CASCADE,
             health                TEXT    NOT NULL DEFAULT 'unknown'
                                   CHECK (health IN ('unknown', 'ok', 'unreachable', 'mismatched')),
             last_contact_at       TEXT,
             accepted_epoch        INTEGER NOT NULL DEFAULT 0,
             arch                  TEXT,
             cpu_count             INTEGER,
             memory_total_mib      INTEGER,
             storage_total_gib     INTEGER,
             storage_available_gib INTEGER,
             hypervisor_version    TEXT,
             last_error            TEXT
         );",
    )
}

/// Adds the table that records which image versions each machine holds
/// (MULTI-NODE 13.2).
fn host_images(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS host_images (
             host_id     INTEGER NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
             image_name  TEXT    NOT NULL,
             checksum    TEXT    NOT NULL,
             verified_at TEXT    NOT NULL,
             PRIMARY KEY (host_id, image_name, checksum)
         );",
    )
}

/// Adds the next hop other machines use to reach a machine's guest slots
/// (MULTI-NODE 8.5).
///
/// It is separate from `endpoint` because management traffic and guest
/// data need not share a path: a deployment can keep management on the
/// LAN and move guest traffic onto a tunnel by changing this column
/// alone. It stays NULL until the operator configures the machine, and a
/// machine with no underlay gets no slot route from anyone.
fn host_underlay(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    if !has_column(tx, "hosts", "underlay")? {
        tx.execute_batch("ALTER TABLE hosts ADD COLUMN underlay TEXT;")?;
    }
    Ok(())
}

/// Adds the per-object generation the controller dispatches at
/// (MULTI-NODE 11.3).
///
/// A runner refuses two orders that claim one generation but want
/// different things, because it cannot tell which is right. The
/// generation therefore has to increase across every process that
/// dispatches, not within one of them: `serve` and the SSH frontend both
/// change instances, and two in-memory counters would both start at one
/// and collide on their first order for the same object.
///
/// This is the controller's side. A runner keeps its own record of the
/// highest generation it has accepted, in its own fence database.
fn dispatch_generations(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS dispatch_generations (
             object_id  TEXT    PRIMARY KEY,
             generation INTEGER NOT NULL
         );",
    )
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
             );
             CREATE TABLE hosts (
                 id          INTEGER PRIMARY KEY,
                 name        TEXT    NOT NULL UNIQUE,
                 libvirt_uri TEXT    NOT NULL,
                 created_at  TEXT    NOT NULL
             );
             CREATE TABLE instances (
                 uuid     TEXT    PRIMARY KEY,
                 name     TEXT    NOT NULL,
                 owner_id INTEGER NOT NULL,
                 host_id  INTEGER NOT NULL REFERENCES hosts(id)
             );
             INSERT INTO hosts (id, name, libvirt_uri, created_at) VALUES
                 (1, 'linux', 'qemu:///system', 'x'),
                 (2, 'localhost.localdomain', 'qemu:///system', 'x'),
                 (3, 'Mac-mini', 'qemu:///system', 'x');
             INSERT INTO instances (uuid, name, owner_id, host_id) VALUES
                 ('a', 'web', 1, 1), ('b', 'db', 1, 2), ('c', 'cache', 1, 3);",
        )
        .unwrap();
        let mut conn = conn;
        // The production sequence: the baseline runs first and leaves an
        // existing database alone, then the migrations carry it forward
        // (`open_connection`).
        conn.execute_batch(crate::SCHEMA_SQL).unwrap();
        apply(&mut conn, clock()).unwrap();

        let tx = conn.transaction().unwrap();
        assert!(has_column(&tx, "images", "kind").unwrap());
        assert!(has_column(&tx, "image_versions", "source_digest").unwrap());
        assert!(has_column(&tx, "image_versions", "kind").unwrap());
        assert!(has_column(&tx, "hosts", "machine_id").unwrap());
        drop(tx);
        assert_eq!(
            current_version(&conn).unwrap(),
            MIGRATIONS.last().unwrap().version
        );

        // The three names were one machine. Its instances come with it.
        let mut statement = conn.prepare("SELECT id FROM hosts ORDER BY id").unwrap();
        let hosts = statement
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<i64>>>()
            .unwrap();
        assert_eq!(hosts, vec![1]);
        let stranded: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM instances WHERE host_id <> 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stranded, 0);

        // A version-1 deployment is one host holding a whole /24, which
        // is slot 0 (MULTI-NODE 16). Every instance moves onto it.
        let (slot, owner): (i64, i64) = conn
            .query_row("SELECT slot, owner_host_id FROM runner_slots", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((slot, owner), (0, 1));
        let unplaced: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM instances WHERE slot IS NOT 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unplaced, 0);
        // The /24 became one slot, so the prefix stays 24.
        let prefix: i64 = conn
            .query_row("SELECT runner_prefix FROM deployment", [], |r| r.get(0))
            .unwrap();
        assert_eq!(prefix, 24);
    }

    #[test]
    fn an_instance_cannot_hold_an_address_from_a_slot_its_host_does_not_own() {
        // The controller checks this in the transaction that claims the
        // address. The trigger is what makes it true of the table
        // (MULTI-NODE 16).
        let mut conn = baseline();
        apply(&mut conn, clock()).unwrap();
        conn.execute_batch(
            "INSERT INTO hosts (id, machine_id, name, libvirt_uri, created_at) VALUES
                 (1, 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'konata', 'qemu:///system', 'x'),
                 (2, 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 'tsukasa', 'qemu:///system', 'x');
             INSERT INTO runner_slots (slot, owner_host_id) VALUES (0, 1), (1, 2);
             INSERT INTO users (id, name, email, subnet, created_at) VALUES
                 (1, 'alice', 'alice@example.org', '10.100.0.0/24', 'x');
             INSERT INTO images (name, url) VALUES ('debian-13', 'https://example.org/d.qcow2');
             INSERT INTO image_versions (checksum, image_name, path, size, fetched_at) VALUES
                 ('sha256-a', 'debian-13', '/img/a', 1, 'x');",
        )
        .unwrap();

        let insert = |host_id: i64, slot: i64, uuid: &str, address: &str, mac: &str| {
            conn.execute(
                "INSERT INTO instances (uuid, name, owner_id, host_id, image_name, \
                 base_checksum, address, mac, vcpu, memory, disk, created_at, slot) \
                 VALUES (?, ?, 1, ?, 'debian-13', 'sha256-a', ?, ?, 1, 512, 10, 'x', ?)",
                rusqlite::params![uuid, uuid, host_id, address, mac, slot],
            )
        };

        // konata owns slot 0, so this is allowed.
        insert(1, 0, "web", "10.100.0.2", "52:54:00:00:00:01").unwrap();
        // konata does not own slot 1.
        let error = insert(1, 1, "db", "10.100.0.66", "52:54:00:00:00:02").unwrap_err();
        assert!(
            error.to_string().contains("does not own its slot"),
            "{error}"
        );
        // Moving a placed instance onto a slot its host does not own is
        // refused the same way.
        let error = conn
            .execute("UPDATE instances SET slot = 1 WHERE uuid = 'web'", [])
            .unwrap_err();
        assert!(
            error.to_string().contains("does not own its slot"),
            "{error}"
        );
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
