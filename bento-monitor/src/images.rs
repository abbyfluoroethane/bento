//! Whether every allowlist entry has a downloaded version
//! (DEPLOYING.md section 4).
//!
//! An allowlist row is not enough to make an instance: the image needs a
//! current version, which only `fetch-images` records. The database is
//! read the way the Fleet screen reads it: opened read-only for one
//! snapshot, never migrated, and closed again (DEPLOYING.md 6, "The
//! Fleet screen").

use std::path::Path;

use bento_store::Store;

/// What the database says about the allowlist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Images {
    /// No configuration loaded, so nothing names the database or the
    /// allowlist.
    #[default]
    NotRead,
    /// The configuration names a database that does not exist yet.
    /// `fetch-images` and `serve` make it.
    NoDatabase(String),
    /// The database is there but did not open or read, for example
    /// because it belongs to root and the monitor is not root.
    Unreadable(String),
    /// The allowlist entries that have no current version, and how many
    /// entries there are.
    Read { missing: Vec<String>, wanted: usize },
}

impl Images {
    /// Reads one snapshot of `db_path` for the entries named in `wanted`.
    pub fn read(runtime: &tokio::runtime::Runtime, db_path: &str, wanted: &[String]) -> Images {
        if !Path::new(db_path).is_file() {
            return Images::NoDatabase(db_path.to_string());
        }
        runtime.block_on(async move {
            let store = match Store::open_read_only(db_path).await {
                Ok(store) => store,
                Err(error) => return Images::Unreadable(format!("{db_path}: {error}")),
            };
            let images = store.images().await;
            let _ = store.close().await;
            match images {
                Ok(images) => Images::Read {
                    missing: wanted
                        .iter()
                        .filter(|name| {
                            !images.iter().any(|image| {
                                &image.name == *name && image.current_checksum.is_some()
                            })
                        })
                        .cloned()
                        .collect(),
                    wanted: wanted.len(),
                },
                Err(error) => Images::Unreadable(format!("{db_path}: {error}")),
            }
        })
    }

    /// Whether every entry has a current version.
    pub fn complete(&self) -> bool {
        matches!(self, Images::Read { missing, .. } if missing.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bento_types::{Image, ImageKind, ImageVersion};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn no_database_is_not_a_complete_allowlist() {
        let state = Images::read(&runtime(), "/no/such/bento.db", &["debian-13".to_string()]);
        assert_eq!(state, Images::NoDatabase("/no/such/bento.db".to_string()));
        assert!(!state.complete());
    }

    #[test]
    fn an_entry_without_a_current_version_is_missing() {
        let runtime = runtime();
        let directory = tempfile::tempdir().expect("tempdir");
        let db = directory.path().join("bento.db").display().to_string();
        runtime.block_on(async {
            // The test is the writer here; the monitor never is.
            let store = Store::open(&db).await.expect("open");
            for (name, current) in [("debian-13", Some("sha256-aa")), ("fedora-44", None)] {
                store
                    .upsert_image(Image {
                        name: name.to_string(),
                        url: format!("https://example.org/{name}.qcow2"),
                        kind: ImageKind::default(),
                        pinned_checksum: None,
                        current_checksum: None,
                    })
                    .await
                    .expect("image");
                // The allowlist write never sets the version; only a
                // fetch does (SPEC 5.1).
                if let Some(checksum) = current {
                    store
                        .add_image_version(ImageVersion {
                            checksum: checksum.to_string(),
                            image_name: name.to_string(),
                            path: format!("/var/lib/bento/images/{checksum}.qcow2"),
                            size: 1,
                            kind: ImageKind::default(),
                            source_digest: None,
                            fetched_at: time::macros::datetime!(2026-01-01 0:00 UTC),
                        })
                        .await
                        .expect("version row");
                    store
                        .set_current_checksum(name, checksum)
                        .await
                        .expect("version");
                }
            }
            store.close().await.expect("close");
        });

        let wanted = ["debian-13", "fedora-44", "alpine"].map(String::from);
        let state = Images::read(&runtime, &db, &wanted);
        assert_eq!(
            state,
            Images::Read {
                missing: vec!["fedora-44".to_string(), "alpine".to_string()],
                wanted: 3,
            }
        );
        assert!(!state.complete());

        let state = Images::read(&runtime, &db, &["debian-13".to_string()]);
        assert!(state.complete(), "{state:?}");
    }
}
