use std::fs::OpenOptions;
use std::path::Path;
use std::time::Duration;

use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags};

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
    /// Replaces this database with the one at `source`, then brings it up
    /// to the current schema (MULTI-NODE 16).
    ///
    /// The backup API copies pages into the live connection, so the
    /// result is a whole database rather than a file swapped underneath
    /// an open handle. Running the migrations afterwards is what makes an
    /// older copy usable: a backup taken two schema versions ago comes
    /// back at the current shape instead of confusing the reader.
    ///
    /// The units must be stopped. This is an operator command, not a
    /// running-service one, and it does not coordinate with other writers.
    pub async fn restore_db(&self, source: impl AsRef<Path>) -> Result<()> {
        let source = source.as_ref().to_path_buf();
        let now = self.clock();
        self.with_conn_mut(move |target| {
            let display = path_text(&source);
            // Read-only, so a source that is not a database is refused
            // here rather than created as an empty one.
            let from = Connection::open_with_flags(&source, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|error| match error {
                    rusqlite::Error::SqliteFailure(_, _) if !source.is_file() => {
                        Error::RestoreSource {
                            path: display,
                            source: std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                "no such file",
                            ),
                        }
                    }
                    error => Error::Sqlite(error),
                })?;
            let backup = Backup::new(&from, target)?;
            backup.run_to_completion(128, Duration::from_millis(1), None)?;
            drop(backup);
            migrate::apply(target, now)
        })
        .await
    }
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
}
