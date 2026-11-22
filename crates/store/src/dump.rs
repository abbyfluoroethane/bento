use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use crate::{Error, Result, Store, migrate, path_text};

impl Store {
    /// Writes a consistent database snapshot with SQLite's online backup API
    /// (SPEC 12.1). WAL makes a raw file copy unsafe. The destination must
    /// not exist and is never overwritten.
    pub async fn dump_db(&self, destination: impl AsRef<Path>) -> Result<()> {
        let destination = destination.as_ref().to_path_buf();
        self.with_conn(move |source| {
            let display = path_text(&destination);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
            {
                Ok(file) => drop(file),
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(Error::DumpDestinationExists { path: display });
                }
                Err(source) => {
                    return Err(Error::DumpDestination {
                        path: display,
                        source,
                    });
                }
            }

            let mut target = Connection::open(&destination)?;
            let backup = Backup::new(source, &mut target)?;
            backup.run_to_completion(128, Duration::from_millis(1), None)?;
            Ok(())
        })
        .await
    }
}

impl Store {
    /// Replaces this database with the one at `source`, brought up to the
    /// current schema (MULTI-NODE 16, SPEC 12.1).
    ///
    /// The work happens on a temporary copy first, in the directory of
    /// the live database so that both sit on one file system:
    ///
    /// 1. copy `source` to the temporary file;
    /// 2. run the baseline and every migration on it, and apply the
    ///    epoch and generation rules;
    /// 3. check it (`integrity_check`, `foreign_key_check`);
    /// 4. only then copy its pages into the live connection.
    ///
    /// A failure in steps 1 to 3 leaves the live database as it was and
    /// returns [`Error::RestoreNotApplied`]. The temporary file, with any
    /// `-wal`, `-shm`, and `-journal` file, is removed on every path.
    ///
    /// Step 4 uses SQLite's backup API into the open live connection, with
    /// all pages in one step. That is one write transaction on the live
    /// database: a reader sees the old content or the new, and a failure
    /// rolls back. A file swap would leave the open connection on the
    /// replaced inode and would need the live `-wal` and `-shm` files
    /// removed in step with it. Backup into the connection keeps the live
    /// database in WAL mode and its WAL consistent. The temporary copy
    /// stays in rollback-journal mode, so it has no WAL to carry over.
    ///
    /// The units must be stopped. This is an operator command, not a
    /// running-service one, and it does not coordinate with other writers.
    pub async fn restore_db(&self, source: impl AsRef<Path>) -> Result<()> {
        self.restore_db_with(source.as_ref().to_path_buf(), crate::prepare_schema)
            .await
    }

    /// [`Store::restore_db`] with the schema step given, so a test can
    /// make it fail.
    async fn restore_db_with(
        &self,
        source: PathBuf,
        prepare: fn(&mut Connection, crate::Clock) -> Result<()>,
    ) -> Result<()> {
        let now = self.clock();
        self.with_conn_mut(move |target| restore_into(target, &source, now, prepare))
            .await
    }
}

fn restore_into(
    target: &mut Connection,
    source: &Path,
    now: crate::Clock,
    prepare: fn(&mut Connection, crate::Clock) -> Result<()>,
) -> Result<()> {
    let display = path_text(source);
    // Read-only, so a source that is not a database is refused here
    // rather than created as an empty one.
    let from =
        Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|error| {
            match error {
                rusqlite::Error::SqliteFailure(_, _) if !source.is_file() => Error::RestoreSource {
                    path: display,
                    source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
                },
                error => Error::Sqlite(error),
            }
        })?;
    // A copy that a later build migrated is refused before anything is
    // written (SPEC 12.1).
    migrate::check_source(&from)?;
    let (live_epoch, live_generations) = read_fences(target)?;
    let live_path = target
        .path()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or(rusqlite::Error::InvalidPath(PathBuf::new()))?;

    let temporary = TemporaryDatabase::create(&live_path)?;
    let prepared = prepare_copy(
        &from,
        &temporary.path,
        live_epoch,
        live_generations,
        now,
        prepare,
    )
    .map_err(|error| Error::RestoreNotApplied(Box::new(error)))?;
    drop(from);

    // The swap. Nothing above wrote to the live database.
    {
        let backup = Backup::new(&prepared, target)?;
        backup.run_to_completion(i32::MAX, Duration::from_millis(1), None)?;
    }
    drop(prepared);
    drop(temporary);
    Ok(())
}

/// Copies `from` to `path`, migrates and fixes the copy, and checks it.
/// The returned connection is the checked copy.
fn prepare_copy(
    from: &Connection,
    path: &Path,
    live_epoch: i64,
    live_generations: Vec<(String, i64)>,
    now: crate::Clock,
    prepare: fn(&mut Connection, crate::Clock) -> Result<()>,
) -> Result<Connection> {
    let mut copy = Connection::open(path)?;
    copy.pragma_update(None, "foreign_keys", true)?;
    {
        let backup = Backup::new(from, &mut copy)?;
        backup.run_to_completion(128, Duration::from_millis(1), None)?;
    }
    prepare(&mut copy, now)?;

    // The copy holds an older controller epoch and older dispatch
    // generations. A runner refuses every epoch below the one it
    // accepted, so an epoch that went backwards would fence the
    // controller out of every runner until it caught up
    // (MULTI-NODE 11.3). Keep the higher value. The copy may also carry
    // a live holder from the moment it was taken; the units are
    // stopped, so no holder is valid (SPEC 12.1).
    let tx = copy.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE controller_lease \
         SET epoch = max(epoch, ?), holder_id = NULL, expires_at = NULL WHERE id = 1",
        [live_epoch],
    )?;
    for (object_id, generation) in live_generations {
        tx.execute(
            "INSERT INTO dispatch_generations (object_id, generation) VALUES (?, ?) \
             ON CONFLICT(object_id) DO UPDATE SET \
               generation = max(generation, excluded.generation)",
            rusqlite::params![object_id, generation],
        )?;
    }
    tx.commit()?;

    let integrity: String = copy.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(Error::RestoreCheck(format!("integrity_check: {integrity}")));
    }
    let broken: i64 =
        copy.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if broken > 0 {
        return Err(Error::RestoreCheck(format!(
            "foreign_key_check: {broken} rows break a foreign key"
        )));
    }
    Ok(copy)
}

/// A scratch database file next to the live one. Dropping it removes the
/// file and the `-wal`, `-shm`, and `-journal` files SQLite may have made.
/// Drop the connections to it first.
struct TemporaryDatabase {
    path: PathBuf,
}

impl TemporaryDatabase {
    fn create(live: &Path) -> Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let mut name = live.as_os_str().to_os_string();
        name.push(format!(".restore-{}-{nanos}.tmp", std::process::id()));
        let path = PathBuf::from(name);
        // `create_new` so that this never opens a file that exists.
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| Error::DumpDestination {
                path: path_text(&path),
                source,
            })?;
        Ok(Self { path })
    }
}

impl Drop for TemporaryDatabase {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut name = self.path.as_os_str().to_os_string();
            name.push(suffix);
            let _ = std::fs::remove_file(name);
        }
    }
}

/// The live controller epoch and dispatch generations, before a restore
/// replaces them. A live database that predates either table has none.
fn read_fences(conn: &Connection) -> Result<(i64, Vec<(String, i64)>)> {
    let has = |table: &str| -> rusqlite::Result<bool> {
        conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?)",
            [table],
            |row| row.get(0),
        )
    };
    let epoch = if has("controller_lease")? {
        conn.query_row(
            "SELECT COALESCE(MAX(epoch), 0) FROM controller_lease",
            [],
            |row| row.get(0),
        )?
    } else {
        0
    };
    let generations = if has("dispatch_generations")? {
        let mut statement =
            conn.prepare("SELECT object_id, generation FROM dispatch_generations")?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    Ok((epoch, generations))
}

#[cfg(test)]
mod tests {
    use bento_types::Capacity;
    use std::time::Duration;

    use crate::Store;
    use crate::tests::{new_test_store, seed_store, test_instance};

    #[tokio::test]
    async fn dump_db_round_trip() {
        let store = new_test_store().await;
        let (owner, host) = seed_store(&store).await;
        let instance = test_instance(1, "web", &owner, &host);
        store
            .create_instance(instance.clone(), Duration::ZERO, Capacity::unbounded())
            .await
            .unwrap();
        store.touch_last_seen(&instance.uuid).await.unwrap();

        let output_directory = tempfile::tempdir().unwrap();
        let destination = output_directory.path().join("backup.db");
        store.dump_db(&destination).await.unwrap();
        store.delete_instance(&instance.uuid).await.unwrap();

        let restored = Store::open(&destination).await.unwrap();
        let user = restored.user_by_name("alice").await.unwrap();
        assert_eq!(user.id, owner.id);
        assert_eq!(user.subnet, owner.subnet);
        let restored_instance = restored.instance(&instance.uuid).await.unwrap();
        assert_eq!(restored_instance.name, "web");
        assert_eq!(restored_instance.address, instance.address);
        assert!(restored_instance.last_seen_at.is_some());
    }

    #[tokio::test]
    async fn restore_db_brings_back_what_the_dump_held() {
        let store = new_test_store().await;
        let (owner, host) = seed_store(&store).await;
        let instance = test_instance(1, "web", &owner, &host);
        store
            .create_instance(instance.clone(), Duration::ZERO, Capacity::unbounded())
            .await
            .unwrap();

        let output_directory = tempfile::tempdir().unwrap();
        let backup = output_directory.path().join("backup.db");
        store.dump_db(&backup).await.unwrap();

        // Whatever happened after the backup is what the restore undoes.
        store.delete_instance(&instance.uuid).await.unwrap();
        assert!(store.instance(&instance.uuid).await.is_err());

        store.restore_db(&backup).await.unwrap();
        assert_eq!(store.instance(&instance.uuid).await.unwrap().name, "web");
        assert_eq!(store.user_by_name("alice").await.unwrap().id, owner.id);
    }

    #[tokio::test]
    async fn restore_db_refuses_a_source_that_is_not_there() {
        let store = new_test_store().await;
        let output_directory = tempfile::tempdir().unwrap();
        let error = store
            .restore_db(output_directory.path().join("absent.db"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("restore source"), "{error}");
    }

    #[tokio::test]
    async fn dump_db_refuses_existing_destination() {
        let store = new_test_store().await;
        let output_directory = tempfile::tempdir().unwrap();
        let destination = output_directory.path().join("backup.db");
        store.dump_db(&destination).await.unwrap();
        let error = store.dump_db(&destination).await.unwrap_err();
        assert!(error.to_string().contains("exists"));
    }

    #[tokio::test]
    async fn restore_db_never_lowers_the_controller_epoch_or_generations() {
        let store = new_test_store().await;
        let ttl = Duration::from_secs(30);
        for _ in 0..2 {
            store.acquire_lease("a", ttl).await.unwrap();
            store.release_lease("a").await.unwrap();
        }
        let early = store.next_dispatch_generation("web", 2).await.unwrap();
        // The copy is taken while a controller holds the lease.
        store.acquire_lease("a", ttl).await.unwrap();
        let output_directory = tempfile::tempdir().unwrap();
        let backup = output_directory.path().join("backup.db");
        store.dump_db(&backup).await.unwrap();
        store.release_lease("a").await.unwrap();
        for _ in 0..3 {
            store.acquire_lease("a", ttl).await.unwrap();
            store.release_lease("a").await.unwrap();
        }
        let late = store.next_dispatch_generation("web", 5).await.unwrap();
        assert!(late > early);

        store.restore_db(&backup).await.unwrap();

        // A runner accepted epoch 5 and refuses a lower one.
        let lease = store.acquire_lease("b", ttl).await.unwrap();
        assert!(lease.epoch > 5, "epoch went back to {}", lease.epoch);
        // The copy's holder does not block the new controller either: the
        // acquire above succeeded without waiting for a TTL.
        let next = store.next_dispatch_generation("web", 1).await.unwrap();
        assert!(next > late, "generation {next} is not above {late}");
    }

    #[tokio::test]
    async fn restore_db_refuses_a_copy_from_a_newer_build_and_keeps_the_live_data() {
        let store = new_test_store().await;
        let (owner, _host) = seed_store(&store).await;
        let output_directory = tempfile::tempdir().unwrap();
        let backup = output_directory.path().join("newer.db");
        store.dump_db(&backup).await.unwrap();
        let copy = rusqlite::Connection::open(&backup).unwrap();
        copy.execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (999, 'future', 'x')",
            [],
        )
        .unwrap();
        drop(copy);

        let error = store.restore_db(&backup).await.unwrap_err();
        assert!(
            matches!(error, crate::Error::SchemaTooNew { recorded: 999, .. }),
            "{error}"
        );
        assert_eq!(store.user_by_name("alice").await.unwrap().id, owner.id);
    }

    #[tokio::test]
    async fn restore_db_brings_a_pre_migration_copy_to_the_current_schema() {
        let store = new_test_store().await;
        let output_directory = tempfile::tempdir().unwrap();
        let backup = output_directory.path().join("old.db");
        let old = rusqlite::Connection::open(&backup).unwrap();
        // A 0.9 database: no machine id, no migration record, and none
        // of the later tables.
        old.execute_batch(
            "CREATE TABLE images (
                 name TEXT PRIMARY KEY, url TEXT NOT NULL,
                 pinned_checksum TEXT, current_checksum TEXT);
             CREATE TABLE image_versions (
                 checksum TEXT PRIMARY KEY, image_name TEXT NOT NULL,
                 path TEXT NOT NULL UNIQUE, size INTEGER NOT NULL, fetched_at TEXT NOT NULL);
             CREATE TABLE image_source_versions (
                 image_name TEXT NOT NULL, source_digest TEXT NOT NULL, checksum TEXT NOT NULL,
                 PRIMARY KEY (image_name, source_digest));
             CREATE TABLE hosts (
                 id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE,
                 libvirt_uri TEXT NOT NULL, created_at TEXT NOT NULL);
             CREATE TABLE instances (
                 uuid TEXT PRIMARY KEY, name TEXT NOT NULL, owner_id INTEGER NOT NULL,
                 host_id INTEGER NOT NULL REFERENCES hosts(id));
             INSERT INTO hosts (id, name, libvirt_uri, created_at)
                 VALUES (1, 'linux', 'qemu:///system', '2026-01-01T00:00:00Z');",
        )
        .unwrap();
        drop(old);

        store.restore_db(&backup).await.unwrap();
        let hosts = store.hosts().await.unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "linux");
    }

    /// Every file in the directory of the live database, by name.
    fn files_beside(live: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(live.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    async fn live_path(store: &Store) -> std::path::PathBuf {
        store
            .with_conn(|conn| Ok(std::path::PathBuf::from(conn.path().unwrap())))
            .await
            .unwrap()
    }

    fn fail_in_the_schema_step(
        conn: &mut rusqlite::Connection,
        _now: crate::Clock,
    ) -> crate::Result<()> {
        // Change the copy first, so a restore that skipped the copy
        // would show it.
        conn.execute_batch("DROP TABLE users;")?;
        Err(crate::Error::Sqlite(rusqlite::Error::InvalidQuery))
    }

    #[tokio::test]
    async fn restore_db_with_a_failing_migration_leaves_the_live_database_and_no_temporary_file() {
        let store = new_test_store().await;
        let (owner, _host) = seed_store(&store).await;
        let backup_directory = tempfile::tempdir().unwrap();
        let backup = backup_directory.path().join("backup.db");
        store.dump_db(&backup).await.unwrap();
        store.set_runner_prefix(25).await.unwrap();
        let live = live_path(&store).await;
        let files_before = files_beside(&live);

        let error = store
            .restore_db_with(backup.clone(), fail_in_the_schema_step)
            .await
            .unwrap_err();

        assert!(
            matches!(error, crate::Error::RestoreNotApplied(_)),
            "{error}"
        );
        assert!(error.to_string().contains("live database is unchanged"));
        // Live data is as it was, including what changed after the backup.
        assert_eq!(store.user_by_name("alice").await.unwrap().id, owner.id);
        let prefix: i64 = store
            .with_conn(|conn| {
                Ok(conn.query_row("SELECT runner_prefix FROM deployment", [], |r| r.get(0))?)
            })
            .await
            .unwrap();
        assert_eq!(prefix, 25);
        assert_eq!(files_beside(&live), files_before);
    }

    #[tokio::test]
    async fn restore_db_refuses_a_corrupt_source_and_keeps_the_live_database() {
        let store = new_test_store().await;
        let (owner, _host) = seed_store(&store).await;
        let backup_directory = tempfile::tempdir().unwrap();
        let backup = backup_directory.path().join("garbage.db");
        std::fs::write(&backup, vec![0xAB_u8; 8192]).unwrap();
        let live = live_path(&store).await;
        let files_before = files_beside(&live);

        assert!(store.restore_db(&backup).await.is_err());

        assert_eq!(store.user_by_name("alice").await.unwrap().id, owner.id);
        assert_eq!(files_beside(&live), files_before);
    }

    #[tokio::test]
    async fn restore_db_refuses_a_copy_that_breaks_a_foreign_key() {
        let store = new_test_store().await;
        let (owner, host) = seed_store(&store).await;
        let instance = test_instance(1, "web", &owner, &host);
        store
            .create_instance(instance.clone(), Duration::ZERO, Capacity::unbounded())
            .await
            .unwrap();
        let backup_directory = tempfile::tempdir().unwrap();
        let backup = backup_directory.path().join("backup.db");
        store.dump_db(&backup).await.unwrap();
        let copy = rusqlite::Connection::open(&backup).unwrap();
        copy.execute_batch("PRAGMA foreign_keys = OFF; UPDATE instances SET owner_id = 999;")
            .unwrap();
        drop(copy);
        let live = live_path(&store).await;
        let files_before = files_beside(&live);

        let error = store.restore_db(&backup).await.unwrap_err();

        assert!(error.to_string().contains("unchanged"), "{error}");
        assert_eq!(
            store.instance(&instance.uuid).await.unwrap().owner_id,
            owner.id
        );
        assert_eq!(files_beside(&live), files_before);
    }

    #[tokio::test]
    async fn restore_db_of_an_old_copy_ends_current_with_the_epoch_rules_true() {
        let store = new_test_store().await;
        let ttl = Duration::from_secs(30);
        for _ in 0..3 {
            store.acquire_lease("a", ttl).await.unwrap();
            store.release_lease("a").await.unwrap();
        }
        let backup_directory = tempfile::tempdir().unwrap();
        let backup = backup_directory.path().join("old.db");
        let old = rusqlite::Connection::open(&backup).unwrap();
        old.execute_batch(
            "CREATE TABLE hosts (
                 id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE,
                 libvirt_uri TEXT NOT NULL, created_at TEXT NOT NULL);
             INSERT INTO hosts (id, name, libvirt_uri, created_at)
                 VALUES (1, 'linux', 'qemu:///system', '2026-01-01T00:00:00Z');",
        )
        .unwrap();
        drop(old);
        let live = live_path(&store).await;
        let files_before = files_beside(&live);

        store.restore_db(&backup).await.unwrap();

        let version: i64 = store
            .with_conn(|conn| {
                Ok(
                    conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                        row.get(0)
                    })?,
                )
            })
            .await
            .unwrap();
        assert_eq!(version, 11);
        assert_eq!(store.hosts().await.unwrap()[0].name, "linux");
        let lease = store.acquire_lease("b", ttl).await.unwrap();
        assert!(lease.epoch > 3, "epoch went back to {}", lease.epoch);
        // The live database keeps its WAL mode and no scratch file stays.
        let mode: String = store
            .with_conn(|conn| Ok(conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?))
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        assert_eq!(files_beside(&live), files_before);
    }
}
