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

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

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
    /// True when `run` drops a table that other tables reference. With
    /// `foreign_keys` on, `DROP TABLE` deletes the rows first and fires
    /// `ON DELETE CASCADE` on the children, which would erase them.
    /// SQLite ignores `PRAGMA foreign_keys` inside a transaction, so
    /// `apply_after` switches it off before the transaction begins and
    /// back to its old value after. Such a migration runs
    /// `foreign_key_check` itself before it returns.
    foreign_keys_off: bool,
}

/// Every migration, in the order they apply.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "image_source_columns",
        run: image_source_columns,
        foreign_keys_off: false,
    },
    Migration {
        version: 2,
        name: "host_machine_id",
        run: host_machine_id,
        foreign_keys_off: false,
    },
    Migration {
        version: 3,
        name: "runners_and_slots",
        run: runners_and_slots,
        foreign_keys_off: false,
    },
    Migration {
        version: 4,
        name: "host_observations",
        run: host_observations,
        foreign_keys_off: false,
    },
    Migration {
        version: 5,
        name: "host_images",
        run: host_images,
        foreign_keys_off: false,
    },
    Migration {
        version: 6,
        name: "host_underlay",
        run: host_underlay,
        foreign_keys_off: false,
    },
    Migration {
        version: 7,
        name: "dispatch_generations",
        run: dispatch_generations,
        foreign_keys_off: false,
    },
    Migration {
        version: 8,
        name: "unique_ssh_key_fingerprints",
        run: unique_ssh_key_fingerprints,
        foreign_keys_off: false,
    },
    Migration {
        version: 9,
        name: "nullable_token_expiry",
        run: nullable_token_expiry,
        foreign_keys_off: false,
    },
    Migration {
        version: 10,
        name: "instance_provisioning_state",
        run: instance_provisioning_state,
        foreign_keys_off: true,
    },
    Migration {
        version: 11,
        name: "host_network_result",
        run: host_network_result,
        foreign_keys_off: false,
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

    apply_list(conn, MIGRATIONS, now)
}

/// [`apply`] over an explicit list, so a test can give it a migration
/// that fails.
fn apply_list(conn: &mut Connection, list: &[Migration], now: crate::Clock) -> Result<()> {
    conn.execute_batch(MIGRATIONS_TABLE)?;
    let current = current_version(conn)?;
    refuse_newer(current, list)?;
    apply_after(conn, list, current, now)
}

/// Runs every migration above `current`. `current` may be stale: another
/// process that starts at the same time (`serve`, `proxy`, `sshd` each
/// open the store) can record a version between the read and the write
/// lock. Each migration therefore asks again inside its own write
/// transaction and skips a version that is already recorded.
fn apply_after(
    conn: &mut Connection,
    list: &[Migration],
    current: i64,
    now: crate::Clock,
) -> Result<()> {
    for migration in list.iter().filter(|m| m.version > current) {
        if migration.foreign_keys_off {
            let was_on: bool = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
            conn.pragma_update(None, "foreign_keys", false)?;
            let result = apply_one(conn, migration, now.clone());
            // Restore even when the migration failed: the connection
            // outlives this call.
            let restored = conn.pragma_update(None, "foreign_keys", was_on);
            result?;
            restored?;
        } else {
            apply_one(conn, migration, now.clone())?;
        }
    }
    Ok(())
}

/// One migration in its own immediate transaction, with the row that
/// records it.
fn apply_one(conn: &mut Connection, migration: &Migration, now: crate::Clock) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let recorded: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM schema_migrations WHERE version = ?)",
        [migration.version],
        |row| row.get(0),
    )?;
    if recorded {
        return Ok(());
    }
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
    Ok(())
}

/// Refuses a database that a later build has migrated. An older binary
/// would run its old code against a schema it does not know (SPEC 12.1).
fn refuse_newer(recorded: i64, list: &[Migration]) -> Result<()> {
    let known = list.last().map_or(0, |m| m.version);
    if recorded > known {
        return Err(crate::Error::SchemaTooNew { recorded, known });
    }
    Ok(())
}

/// Checks a database file that is about to replace the live one. It reads
/// only, and tolerates a copy that has no `schema_migrations` table (a
/// 0.9 copy).
pub(crate) fn check_source(source: &Connection) -> Result<()> {
    let has_table: bool = source.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master \
         WHERE type = 'table' AND name = 'schema_migrations')",
        [],
        |row| row.get(0),
    )?;
    if has_table {
        refuse_newer(current_version(source)?, MIGRATIONS)?;
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

/// Makes an SSH key fingerprint unique (SPEC 13, SPEC 12).
///
/// Until now nothing stopped two rows sharing a fingerprint, and the SSH
/// lookup took the oldest. A user could register another person's public
/// key first and receive that person's logins.
///
/// A database may already hold such rows. The policy for them:
///
/// * Rows of one fingerprint that belong to one user: keep the oldest.
///   They name the same account, so nothing is lost.
/// * Rows of one fingerprint that belong to more than one user: delete
///   all of them. The oldest row is what logins already used, but it may
///   be the squatter, and no data in the database says which user holds
///   the private key. Keeping either would hand a login to a guess. The
///   real owner links the key again over SSH (SPEC 13), which proves
///   possession. The cost is one re-link for a user who did nothing
///   wrong; the alternative is a login to the wrong account.
///
/// The index keeps the name the baseline gives the plain index, so the
/// baseline's `CREATE INDEX IF NOT EXISTS` finds it on the next start and
/// does not add a second, non-unique one. A database that already has the
/// unique index only records the version.
fn unique_ssh_key_fingerprints(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    let already_unique = {
        let mut statement = tx.prepare("PRAGMA index_list(ssh_keys)")?;
        // Columns: seq, name, unique, origin, partial.
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
        })?;
        let mut found = false;
        for row in rows {
            let (name, unique) = row?;
            found |= name == "idx_ssh_keys_fingerprint" && unique == 1;
        }
        found
    };
    if already_unique {
        return Ok(());
    }

    let contested: Vec<String> = {
        let mut statement = tx.prepare(
            "SELECT fingerprint FROM ssh_keys GROUP BY fingerprint \
             HAVING COUNT(DISTINCT user_id) > 1",
        )?;
        let rows = statement.query_map([], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for fingerprint in &contested {
        let removed = tx.execute("DELETE FROM ssh_keys WHERE fingerprint = ?", [fingerprint])?;
        tracing::warn!(
            fingerprint,
            removed,
            "ssh key was registered by more than one user: removed every copy, the owner must link it again"
        );
    }
    tx.execute_batch(
        "DELETE FROM ssh_keys WHERE id NOT IN \
             (SELECT MIN(id) FROM ssh_keys GROUP BY fingerprint);
         DROP INDEX IF EXISTS idx_ssh_keys_fingerprint;
         CREATE UNIQUE INDEX idx_ssh_keys_fingerprint ON ssh_keys(fingerprint);",
    )
}

/// Lets `tokens.expires_at` be NULL, and stores NULL for a token that never
/// expires (SPEC 13). Earlier versions wrote the Unix epoch for such a
/// token, because the column was NOT NULL. SQLite cannot drop NOT NULL in
/// place, so the table is rebuilt. A database whose column is already
/// nullable skips the rebuild; the epoch rows are still converted, which
/// is safe to repeat.
fn nullable_token_expiry(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    let not_null = {
        let mut statement = tx.prepare("PRAGMA table_info(tokens)")?;
        // Columns: cid, name, type, notnull, dflt_value, pk.
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(3)?))
        })?;
        let mut found = false;
        for row in rows {
            let (name, not_null) = row?;
            found |= name == "expires_at" && not_null == 1;
        }
        found
    };
    if not_null {
        tx.execute_batch(
            "CREATE TABLE tokens_new (
                 id         INTEGER PRIMARY KEY,
                 user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                 hash       TEXT    NOT NULL UNIQUE,
                 expires_at TEXT
             );
             INSERT INTO tokens_new (id, user_id, hash, expires_at)
                 SELECT id, user_id, hash, expires_at FROM tokens;
             DROP TABLE tokens;
             ALTER TABLE tokens_new RENAME TO tokens;",
        )?;
    }
    tx.execute(
        "UPDATE tokens SET expires_at = NULL WHERE expires_at = '1970-01-01T00:00:00Z'",
        [],
    )?;
    Ok(())
}

/// Lets `instances.state` hold `provisioning`: a row that `create` or
/// `copy` inserted before its domain exists (SPEC 11.2).
///
/// SQLite cannot change a CHECK in place, so the table is rebuilt. The
/// new table is the old table's own SQL with the CHECK widened, so every
/// column that earlier migrations added (`slot`) comes along, and the
/// indexes and triggers (migration 3's slot-owner triggers) are read
/// from `sqlite_master` and created again. Shares reference the table
/// with `ON DELETE CASCADE`, which is why this migration runs with
/// `foreign_keys` off (see `Migration::foreign_keys_off`).
///
/// A database whose table already accepts `provisioning`, or has no
/// such CHECK, is left alone and only records the version.
fn instance_provisioning_state(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    const OLD_CHECK: &str = "'starting')";
    const NEW_CHECK: &str = "'starting', 'provisioning')";

    let sql: Option<String> = tx
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'instances'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(sql) = sql else {
        return Ok(());
    };
    if sql.contains("'provisioning'") || !sql.contains(OLD_CHECK) {
        return Ok(());
    }

    // Indexes and triggers go with the table when it is dropped. Take
    // their SQL first. Automatic indexes (UNIQUE, PRIMARY KEY) have no
    // SQL and come back with the new table.
    let broken_before = broken_references(tx)?;
    let dependents: Vec<String> = {
        let mut statement = tx.prepare(
            "SELECT sql FROM sqlite_master \
             WHERE tbl_name = 'instances' AND type IN ('index', 'trigger') AND sql IS NOT NULL \
             ORDER BY type DESC, name",
        )?;
        statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };

    // The first "instances" in the stored text is the table name.
    let rebuilt = sql
        .replacen("instances", "instances_new", 1)
        .replacen(OLD_CHECK, NEW_CHECK, 1);
    tx.execute_batch(&rebuilt)?;
    tx.execute_batch(
        "INSERT INTO instances_new SELECT * FROM instances;
         DROP TABLE instances;
         ALTER TABLE instances_new RENAME TO instances;",
    )?;
    for statement in dependents {
        tx.execute_batch(&statement)?;
    }

    // Foreign keys were off, so nothing checked the copy. Only a row the
    // rebuild itself broke fails it: a violation that the database
    // already had is not this migration's to refuse.
    let broken = broken_references(tx)?.saturating_sub(broken_before);
    if broken > 0 {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
            Some(format!(
                "rebuilding instances left {broken} rows that break a foreign key"
            )),
        ));
    }
    Ok(())
}

/// Rows in the two tables that the instances rebuild touches that break
/// a foreign key: instances (as child) and shares (as the child of
/// instances).
fn broken_references(tx: &Transaction<'_>) -> rusqlite::Result<i64> {
    tx.query_row(
        "SELECT (SELECT COUNT(*) FROM pragma_foreign_key_check('instances')) \
              + (SELECT COUNT(*) FROM pragma_foreign_key_check('shares'))",
        [],
        |row| row.get(0),
    )
}

/// Adds the outcome of the last network apply to a host's observation
/// (MULTI-NODE 14.3, 15).
///
/// This is not a value of `health`: `observe_host` rewrites `health` on
/// every poll and would erase it. These columns are written only by
/// `record_network_result`. NULL `network_error` means the last apply
/// succeeded or none has run.
fn host_network_result(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    if !has_column(tx, "host_observations", "network_error")? {
        tx.execute_batch("ALTER TABLE host_observations ADD COLUMN network_error TEXT;")?;
    }
    if !has_column(tx, "host_observations", "network_checked_at")? {
        tx.execute_batch("ALTER TABLE host_observations ADD COLUMN network_checked_at TEXT;")?;
    }
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
    fn a_failing_migration_rolls_back_and_records_nothing() {
        fn adds_a_table_then_fails(tx: &Transaction<'_>) -> rusqlite::Result<()> {
            tx.execute_batch("CREATE TABLE half_done (id INTEGER);")?;
            Err(rusqlite::Error::InvalidQuery)
        }
        let list = [Migration {
            version: 1,
            name: "fails",
            run: adds_a_table_then_fails,
            foreign_keys_off: false,
        }];
        let mut conn = baseline();
        assert!(apply_list(&mut conn, &list, clock()).is_err());
        assert_eq!(current_version(&conn).unwrap(), 0);
        let table: bool = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'half_done')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!table, "the failed migration's change was kept");
    }

    #[test]
    fn a_stale_read_does_not_run_a_recorded_migration_again() {
        fn counts(tx: &Transaction<'_>) -> rusqlite::Result<()> {
            tx.execute_batch("CREATE TABLE ran (n INTEGER); INSERT INTO ran VALUES (1);")
        }
        let list = [Migration {
            version: 1,
            name: "once",
            run: counts,
            foreign_keys_off: false,
        }];
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bento.db");
        let mut first = Connection::open(&path).unwrap();
        let mut second = Connection::open(&path).unwrap();
        first.execute_batch(crate::SCHEMA_SQL).unwrap();
        first.execute_batch(MIGRATIONS_TABLE).unwrap();
        // Both processes read version 0, then the first one finishes.
        let stale = current_version(&second).unwrap();
        assert_eq!(stale, 0);
        apply_list(&mut first, &list, clock()).unwrap();

        apply_after(&mut second, &list, stale, clock()).unwrap();

        let rows: i64 = first
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 1);
        let ran: i64 = first
            .query_row("SELECT COUNT(*) FROM ran", [], |row| row.get(0))
            .unwrap();
        assert_eq!(ran, 1, "the migration ran twice");
    }

    #[test]
    fn a_database_newer_than_the_binary_is_refused() {
        let mut conn = baseline();
        apply(&mut conn, clock()).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (999, 'later', 'x')",
            [],
        )
        .unwrap();
        let error = apply(&mut conn, clock()).unwrap_err();
        assert!(
            matches!(error, crate::Error::SchemaTooNew { recorded: 999, .. }),
            "{error}"
        );
    }

    /// A database at version 7 with the pre-fix, non-unique index and
    /// the given `(id, user_id, fingerprint)` rows.
    fn at_version_7(rows: &[(i64, i64, &str)]) -> Connection {
        let mut conn = baseline();
        conn.execute_batch(MIGRATIONS_TABLE).unwrap();
        for migration in MIGRATIONS.iter().filter(|m| m.version <= 7) {
            conn.execute(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?, ?, 'x')",
                rusqlite::params![migration.version, migration.name],
            )
            .unwrap();
        }
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             INSERT INTO users (id, name, email, subnet, created_at)
                 VALUES (1, 'alice', 'alice@example.org', '10.0.1.0/24', 'x'),
                        (2, 'bob', 'bob@example.org', '10.0.2.0/24', 'x');",
        )
        .unwrap();
        for (id, user, fingerprint) in rows {
            conn.execute(
                "INSERT INTO ssh_keys (id, user_id, public_key, fingerprint, created_at) \
                 VALUES (?, ?, 'key', ?, 'x')",
                rusqlite::params![id, user, fingerprint],
            )
            .unwrap();
        }
        apply(&mut conn, clock()).unwrap();
        conn
    }

    fn key_ids(conn: &Connection) -> Vec<i64> {
        let mut statement = conn.prepare("SELECT id FROM ssh_keys ORDER BY id").unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|id| id.unwrap())
            .collect()
    }

    #[test]
    fn a_fingerprint_held_by_two_users_is_dropped_from_both() {
        // Rows 1 and 2: alice and bob hold one fingerprint. The older row
        // may be the squatter, so neither is trusted; the real owner
        // links the key again. Row 3 is unrelated and stays.
        let conn = at_version_7(&[(1, 2, "SHA256:x"), (2, 1, "SHA256:x"), (3, 1, "SHA256:y")]);
        assert_eq!(key_ids(&conn), vec![3]);
    }

    #[test]
    fn a_fingerprint_repeated_by_one_user_keeps_the_oldest_row() {
        let conn = at_version_7(&[(1, 1, "SHA256:x"), (2, 1, "SHA256:x"), (3, 2, "SHA256:y")]);
        assert_eq!(key_ids(&conn), vec![1, 3]);
    }

    #[test]
    fn the_fingerprint_index_is_unique_after_migrating() {
        let conn = at_version_7(&[(1, 1, "SHA256:x")]);
        let refused = conn.execute(
            "INSERT INTO ssh_keys (user_id, public_key, fingerprint, created_at) \
             VALUES (2, 'key', 'SHA256:x', 'x')",
            [],
        );
        assert!(refused.is_err());
    }

    #[test]
    fn a_database_that_already_has_the_unique_index_only_records_the_version() {
        let mut conn = baseline();
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             DROP INDEX idx_ssh_keys_fingerprint;
             CREATE UNIQUE INDEX idx_ssh_keys_fingerprint ON ssh_keys(fingerprint);
             INSERT INTO ssh_keys (id, user_id, public_key, fingerprint, created_at)
                 VALUES (1, 1, 'key', 'SHA256:x', 'x');",
        )
        .unwrap();
        apply(&mut conn, clock()).unwrap();
        assert_eq!(key_ids(&conn), vec![1]);
        assert_eq!(
            current_version(&conn).unwrap(),
            MIGRATIONS.last().unwrap().version
        );
    }

    #[test]
    fn token_expiry_becomes_nullable_and_epoch_rows_become_null() {
        let mut conn = baseline();
        conn.execute_batch(MIGRATIONS_TABLE).unwrap();
        for migration in MIGRATIONS.iter().filter(|m| m.version <= 8) {
            conn.execute(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?, ?, 'x')",
                rusqlite::params![migration.version, migration.name],
            )
            .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO users (id, name, email, subnet, created_at)
                 VALUES (1, 'alice', 'alice@example.org', '10.0.1.0/24', 'x');
             INSERT INTO tokens (id, user_id, hash, expires_at) VALUES
                 (1, 1, 'forever', '1970-01-01T00:00:00Z'),
                 (2, 1, 'later', '2030-01-01T00:00:00Z');",
        )
        .unwrap();
        apply(&mut conn, clock()).unwrap();
        let expiry = |hash: &str| -> Option<String> {
            conn.query_row(
                "SELECT expires_at FROM tokens WHERE hash = ?",
                [hash],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(expiry("forever"), None);
        assert_eq!(expiry("later").as_deref(), Some("2030-01-01T00:00:00Z"));
        conn.execute(
            "INSERT INTO tokens (user_id, hash, expires_at) VALUES (1, 'new', NULL)",
            [],
        )
        .unwrap();
    }

    /// A database at version 9 with a host, a slot, two users, an image,
    /// two instances, and a share.
    fn at_version_9() -> Connection {
        let mut conn = baseline();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        apply_list(&mut conn, &MIGRATIONS[..9], clock()).unwrap();
        conn.execute_batch(
            "INSERT INTO hosts (id, machine_id, name, libvirt_uri, created_at) VALUES
                 (1, 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'konata', 'qemu:///system', 'x'),
                 (2, 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 'tsukasa', 'qemu:///system', 'x');
             INSERT INTO runner_slots (slot, owner_host_id) VALUES (0, 1), (1, 2);
             INSERT INTO users (id, name, email, subnet, created_at) VALUES
                 (1, 'alice', 'alice@example.org', '10.100.0.0/24', 'x'),
                 (2, 'bob', 'bob@example.org', '10.100.1.0/24', 'x');
             INSERT INTO images (name, url) VALUES ('debian-13', 'https://example.org/d.qcow2');
             INSERT INTO image_versions (checksum, image_name, path, size, fetched_at) VALUES
                 ('sha256-a', 'debian-13', '/img/a', 1, 'x');
             INSERT INTO instances (uuid, name, owner_id, host_id, image_name, base_checksum,
                 state, address, mac, vcpu, memory, disk, created_at, slot) VALUES
                 ('web', 'web', 1, 1, 'debian-13', 'sha256-a', 'running',
                  '10.100.0.2', '52:54:00:00:00:01', 1, 512, 10, 'x', 0),
                 ('db', 'db', 1, 2, 'debian-13', 'sha256-a', 'stopped',
                  '10.100.0.66', '52:54:00:00:00:02', 2, 1024, 20, 'x', 1);
             INSERT INTO shares (instance_uuid, user_id, created_at) VALUES ('web', 2, 'x');",
        )
        .unwrap();
        conn
    }

    fn insert_instance(conn: &Connection, state: &str) -> rusqlite::Result<usize> {
        conn.execute(
            "INSERT INTO instances (uuid, name, owner_id, host_id, image_name, base_checksum, \
             state, address, mac, vcpu, memory, disk, created_at, slot) \
             VALUES ('new', 'new', 1, 1, 'debian-13', 'sha256-a', ?, '10.100.0.3', \
             '52:54:00:00:00:03', 1, 512, 10, 'x', 0)",
            [state],
        )
    }

    /// Every index and trigger of one table, as `(type, name, sql)`.
    fn dependents(conn: &Connection, table: &str) -> Vec<(String, String, String)> {
        let mut statement = conn
            .prepare(
                "SELECT type, name, sql FROM sqlite_master \
                 WHERE tbl_name = ? AND type IN ('index', 'trigger') AND sql IS NOT NULL \
                 ORDER BY type, name",
            )
            .unwrap();
        statement
            .query_map([table], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    }

    fn instance_rows(conn: &Connection) -> Vec<(String, String, i64, i64, String)> {
        let mut statement = conn
            .prepare("SELECT uuid, state, slot, memory, address FROM instances ORDER BY uuid")
            .unwrap();
        statement
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    }

    #[test]
    fn provisioning_is_refused_at_version_9_and_accepted_at_10() {
        let mut conn = at_version_9();
        assert!(insert_instance(&conn, "provisioning").is_err());

        apply(&mut conn, clock()).unwrap();
        insert_instance(&conn, "provisioning").unwrap();
        assert!(insert_instance(&conn, "bogus").is_err());
    }

    #[test]
    fn the_instances_rebuild_keeps_rows_shares_indexes_and_triggers() {
        let mut conn = at_version_9();
        let rows_before = instance_rows(&conn);
        let dependents_before = dependents(&conn, "instances");
        assert!(dependents_before.len() >= 4, "{dependents_before:?}");
        let columns_before = {
            let mut s = conn.prepare("PRAGMA table_info(instances)").unwrap();
            s.query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .map(|r| r.unwrap())
                .collect::<Vec<_>>()
        };

        apply(&mut conn, clock()).unwrap();

        assert_eq!(instance_rows(&conn), rows_before);
        assert_eq!(dependents(&conn, "instances"), dependents_before);
        let columns_after = {
            let mut s = conn.prepare("PRAGMA table_info(instances)").unwrap();
            s.query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .map(|r| r.unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(columns_after, columns_before);
        // The cascade did not run: the share survived the drop.
        let shares: i64 = conn
            .query_row("SELECT COUNT(*) FROM shares", [], |r| r.get(0))
            .unwrap();
        assert_eq!(shares, 1);
        // Foreign keys are enforced again, and consistent.
        let enforced: bool = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert!(enforced);
        let broken: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(broken, 0);
        assert!(conn.execute("DELETE FROM users WHERE id = 1", []).is_err());
        // The shares table still resolves to the rebuilt table, and its
        // cascade still works.
        conn.execute("DELETE FROM instances WHERE uuid = 'web'", [])
            .unwrap();
        let shares: i64 = conn
            .query_row("SELECT COUNT(*) FROM shares", [], |r| r.get(0))
            .unwrap();
        assert_eq!(shares, 0);
    }

    #[test]
    fn the_rebuilt_instances_table_keeps_refusing_what_it_refused() {
        let mut conn = at_version_9();
        apply(&mut conn, clock()).unwrap();

        // Slot-owner triggers (MULTI-NODE 16): host 1 does not own slot 1.
        let error = conn
            .execute("UPDATE instances SET slot = 1 WHERE uuid = 'web'", [])
            .unwrap_err();
        assert!(
            error.to_string().contains("does not own its slot"),
            "{error}"
        );
        conn.execute("UPDATE instances SET slot = 1 WHERE uuid = 'db'", [])
            .unwrap();
        let error = conn
            .execute(
                "INSERT INTO instances (uuid, name, owner_id, host_id, image_name, \
                 base_checksum, address, mac, vcpu, memory, disk, created_at, slot) \
                 VALUES ('x', 'x', 1, 1, 'debian-13', 'sha256-a', '10.100.0.9', \
                 '52:54:00:00:00:09', 1, 512, 10, 'x', 1)",
                [],
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("does not own its slot"),
            "{error}"
        );
        // Unique name, address, and MAC.
        assert!(
            conn.execute("UPDATE instances SET name = 'web' WHERE uuid = 'db'", [])
                .is_err()
        );
        assert!(
            conn.execute(
                "UPDATE instances SET address = '10.100.0.2' WHERE uuid = 'db'",
                []
            )
            .is_err()
        );
        assert!(
            conn.execute(
                "UPDATE instances SET mac = '52:54:00:00:00:01' WHERE uuid = 'db'",
                []
            )
            .is_err()
        );
        // Foreign key to hosts.
        assert!(
            conn.execute("UPDATE instances SET host_id = 99 WHERE uuid = 'db'", [])
                .is_err()
        );
    }

    #[test]
    fn the_instances_rebuild_runs_once_and_skips_a_table_that_already_accepts_it() {
        let mut conn = at_version_9();
        apply(&mut conn, clock()).unwrap();
        let sql_after_first: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'instances'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        apply(&mut conn, clock()).unwrap();
        let sql_after_second: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'instances'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sql_after_first, sql_after_second);
        assert_eq!(current_version(&conn).unwrap(), 11);

        // A database that has the new CHECK but no record of version 10
        // only records it: the rows keep their rowid order and the
        // table is not rebuilt.
        conn.execute("DELETE FROM schema_migrations WHERE version >= 10", [])
            .unwrap();
        let marker = "CREATE TRIGGER marker AFTER DELETE ON instances BEGIN SELECT 1; END;";
        conn.execute_batch(marker).unwrap();
        apply(&mut conn, clock()).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 11);
        // A rebuild would have kept the trigger too, so check the table
        // SQL is byte for byte the same as before.
        let sql_after_third: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'instances'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sql_after_first, sql_after_third);
    }

    #[test]
    fn a_migration_that_needs_foreign_keys_off_gets_them_back_after_a_failure() {
        fn fails(tx: &Transaction<'_>) -> rusqlite::Result<()> {
            // The pragma is off inside the migration.
            let on: bool = tx.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
            assert!(!on, "foreign_keys was on inside the migration");
            Err(rusqlite::Error::InvalidQuery)
        }
        let list = [Migration {
            version: 1,
            name: "fails",
            run: fails,
            foreign_keys_off: true,
        }];
        let mut conn = baseline();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        assert!(apply_list(&mut conn, &list, clock()).is_err());
        let on: bool = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert!(on, "foreign_keys stayed off after a failure");
        assert_eq!(current_version(&conn).unwrap(), 0);
    }

    #[test]
    fn a_foreign_key_violation_the_database_already_had_does_not_block_the_rebuild() {
        let mut conn = at_version_9();
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn.execute("UPDATE instances SET owner_id = 99 WHERE uuid = 'db'", [])
            .unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        apply(&mut conn, clock()).unwrap();
        assert_eq!(instance_rows(&conn).len(), 2);
    }

    #[test]
    fn network_result_columns_are_added_once_and_a_present_column_only_records() {
        let mut conn = at_version_9();
        apply(&mut conn, clock()).unwrap();
        let tx = conn.transaction().unwrap();
        assert!(has_column(&tx, "host_observations", "network_error").unwrap());
        assert!(has_column(&tx, "host_observations", "network_checked_at").unwrap());
        drop(tx);

        // The column exists but the version is not recorded.
        conn.execute("DELETE FROM schema_migrations WHERE version = 11", [])
            .unwrap();
        apply(&mut conn, clock()).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 11);
    }
}
