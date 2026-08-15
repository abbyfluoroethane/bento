use std::fs::OpenOptions;
use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;
use rusqlite::backup::Backup;

use crate::{Error, Result, Store, path_text};

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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::Store;
    use crate::tests::{new_test_store, seed_store, test_instance};

    #[tokio::test]
    async fn dump_db_round_trip() {
        let store = new_test_store().await;
        let (owner, host) = seed_store(&store).await;
        let instance = test_instance(1, "web", &owner, &host);
        store
            .create_instance(instance.clone(), Duration::ZERO)
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
    async fn dump_db_refuses_existing_destination() {
        let store = new_test_store().await;
        let output_directory = tempfile::tempdir().unwrap();
        let destination = output_directory.path().join("backup.db");
        store.dump_db(&destination).await.unwrap();
        let error = store.dump_db(&destination).await.unwrap_err();
        assert!(error.to_string().contains("exists"));
    }
}
