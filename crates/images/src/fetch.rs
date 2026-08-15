use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;

use bento_types::{Image, ImageVersion};
use futures::StreamExt;
use reqwest::{Method, Request, Url};
use sha2::{Digest, Sha256};
use tempfile::Builder;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;

use crate::store::{Error, Result, Store, dependency, io_error, joined, normalize_checksum};

impl Store {
    /// Runs the fetch-images pipeline from SPEC section 5.1 for every
    /// image on the allowlist, then garbage-collects unreferenced versions.
    /// The store lock guards only image version creation and deletion
    /// (SPEC section 19) — never a download: a multi-gigabyte download must
    /// not stall a concurrent overlay create that blocks on the same lock.
    ///
    /// Per image: download, checksum, reject on pin mismatch, return without
    /// action when the version already exists, store at the content-addressed
    /// path, insert the image_versions row, mark it current. An unpinned image
    /// whose content changed is stored under its new checksum and a warning
    /// names both checksums (trust on first use).
    pub async fn fetch_images(&self) -> Result<()> {
        // The downloads stream into the image directory before any lock is
        // held, so make sure it exists first.
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|error| io_error("create image directory", error))?;
        let images = self
            .db
            .images()
            .await
            .map_err(|error| dependency("list allowlist", error))?;
        let mut errors = Vec::new();
        for image in images {
            if let Err(error) = self.fetch_one(image.clone()).await {
                errors.push(Error::Invalid(format!("fetch {}: {error}", image.name)));
            }
        }
        if let Err(error) = self.collect().await {
            errors.push(error);
        }
        joined(errors)
    }

    /// Runs steps 1-7 of SPEC section 5.1 for one image. The download and
    /// checksum run without the store lock; the lock is taken only for the
    /// version-creation steps 4-7 (SPEC section 19).
    async fn fetch_one(&self, image: Image) -> Result<()> {
        // 1. Download the file from the URL, streaming into a temporary file
        // in the image directory so the final rename is atomic.
        let url = Url::parse(&image.url)
            .map_err(|error| Error::Http(format!("build request: {error}")))?;
        let request = Request::new(Method::GET, url);
        let response = self
            .client
            .do_request(request)
            .await
            .map_err(|error| dependency(format!("download {}", image.url), error))?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(Error::Http(format!(
                "download {}: unexpected status {}",
                image.url,
                response.status().as_u16()
            )));
        }

        let temporary = Builder::new()
            .prefix("download-")
            .tempfile_in(&self.dir)
            .map_err(|error| io_error("create temporary file", error))?;
        let (file, temporary_path) = temporary.into_parts();
        let mut file = tokio::fs::File::from_std(file);

        // 2. Compute the checksum while writing.
        let mut hash = Sha256::new();
        let mut size = 0_i64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|error| Error::Http(format!("download {}: {error}", image.url)))?;
            file.write_all(&chunk)
                .await
                .map_err(|error| io_error(format!("download {}", image.url), error))?;
            hash.update(&chunk);
            size = size
                .checked_add(i64::try_from(chunk.len()).unwrap_or(i64::MAX))
                .ok_or_else(|| Error::Invalid("download exceeds i64 size".to_owned()))?;
        }
        file.flush()
            .await
            .map_err(|error| io_error("write temporary file", error))?;
        drop(file);
        let checksum = hex::encode(hash.finalize());

        // 3. Reject the file if the allowlist pins a checksum and the two do
        // not match.
        if let Some(pinned_checksum) = &image.pinned_checksum {
            let pinned = normalize_checksum(pinned_checksum)
                .map_err(|error| Error::Invalid(format!("pinned checksum: {error}")))?;
            if pinned != checksum {
                return Err(Error::Invalid(format!(
                    "checksum mismatch: pinned {pinned}, downloaded {checksum}"
                )));
            }
        }

        // Steps 4-7 create an image version; only they need the store lock
        // (SPEC section 19).
        let _guard = self.acquire_lock().await?;

        // 4. Return without action if a version with that checksum already
        // exists.
        let exists = self
            .db
            .has_image_version(&checksum)
            .await
            .map_err(|error| dependency("check existing version", error))?;
        if exists {
            let path = self.path_normalized(&checksum);
            if matches!(tokio::fs::metadata(&path).await, Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            {
                tracing::warn!(
                    image = %image.name,
                    checksum,
                    path = %path.display(),
                    "image version row exists but the stored file is missing"
                );
            }
            return Ok(());
        }

        // 5. Store the file at the content-addressed path. The stored file is
        // never written again, so drop the write bits.
        tokio::fs::set_permissions(&temporary_path, std::fs::Permissions::from_mode(0o444))
            .await
            .map_err(|error| io_error("chmod", error))?;
        let path = self.path_normalized(&checksum);
        temporary_path
            .persist(&path)
            .map_err(|error| io_error(format!("store at {}", path.display()), error.error))?;

        // 6. Insert a row in image_versions.
        self.db
            .insert_image_version(ImageVersion {
                checksum: checksum.clone(),
                image_name: image.name.clone(),
                path: path.display().to_string(),
                size,
                fetched_at: OffsetDateTime::now_utc(),
            })
            .await
            .map_err(|error| dependency("insert image version", error))?;

        // An unpinned image is trusted on first use. A content change is not
        // an error, but the warning must name both checksums (SPEC 5.1).
        if image.pinned_checksum.is_none()
            && let Some(previous_checksum) = &image.current_checksum
            && previous_checksum != &checksum
        {
            tracing::warn!(
                image = %image.name,
                previous_checksum,
                new_checksum = checksum,
                "unpinned image content changed"
            );
        }

        // 7. Mark the new row as the current version of the image.
        self.db
            .set_current_checksum(&image.name, &checksum)
            .await
            .map_err(|error| dependency("mark current", error))?;
        Ok(())
    }

    /// Deletes image versions that no instance depends on. The condition
    /// from SPEC sections 5.1 and 12 is exact: a version is deletable only
    /// when no instances row carries its checksum in base_checksum. The
    /// current version of each image is always kept, because the next new
    /// instance boots from it. The whole collection holds the store lock,
    /// so a concurrent overlay create cannot lose its backing file
    /// mid-create (SPEC section 19).
    async fn collect(&self) -> Result<()> {
        let _guard = self.acquire_lock().await?;
        let images = self
            .db
            .images()
            .await
            .map_err(|error| dependency("collect: list allowlist", error))?;
        let current: HashSet<String> = images
            .into_iter()
            .filter_map(|image| image.current_checksum)
            .collect();
        let versions = self
            .db
            .image_versions()
            .await
            .map_err(|error| dependency("collect: list versions", error))?;
        let mut errors = Vec::new();
        for version in versions {
            if current.contains(&version.checksum) {
                continue;
            }
            let in_use = match self.db.checksum_in_use(&version.checksum).await {
                Ok(in_use) => in_use,
                Err(error) => {
                    errors.push(dependency(format!("collect {}", version.checksum), error));
                    continue;
                }
            };
            if in_use {
                continue;
            }
            if let Err(error) = self.db.delete_image_version(&version.checksum).await {
                errors.push(dependency(
                    format!("collect {}: delete row", version.checksum),
                    error,
                ));
                continue;
            }
            let path = self.path_normalized(&version.checksum);
            if let Err(error) = tokio::fs::remove_file(&path).await
                && error.kind() != std::io::ErrorKind::NotFound
            {
                errors.push(io_error(
                    format!("collect {}: delete file", version.checksum),
                    error,
                ));
                continue;
            }
            tracing::info!(
                image = %version.image_name,
                checksum = %version.checksum,
                "deleted unreferenced image version"
            );
        }
        joined(errors)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    use bento_types::{Image, ImageVersion};

    use super::*;
    use crate::test_support::{FakeClient, FakeDb, FakeResponse, FakeRunner, sha256_hex};

    fn new_test_store(db: FakeDb, client: FakeClient) -> (Store, tempfile::TempDir) {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = Store::new(temp.path(), db).with_http_client(client);
        (store, temp)
    }

    #[tokio::test]
    async fn fetch_images_stores_new_version() {
        let content = "debian-13-content";
        let checksum = sha256_hex(content);
        let db = FakeDb::new();
        db.set_images(vec![Image {
            name: "debian-13".into(),
            url: "https://img.example/d13.qcow2".into(),
            pinned_checksum: None,
            current_checksum: None,
        }]);
        let client =
            FakeClient::with_response("https://img.example/d13.qcow2", FakeResponse::ok(content));
        let (store, _temp) = new_test_store(db.clone(), client);

        store.fetch_images().await.expect("fetch images");

        let path = store.path(&checksum).expect("path");
        assert_eq!(
            std::fs::read(&path).expect("stored file"),
            content.as_bytes()
        );
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o222, 0, "stored versions must never be writable");
        let inserted = db.inserted();
        assert_eq!(inserted.len(), 1);
        let version = &inserted[0];
        assert_eq!(version.checksum, checksum);
        assert_eq!(version.image_name, "debian-13");
        assert_eq!(version.path, path.display().to_string());
        assert_eq!(
            version.size,
            i64::try_from(content.len()).expect("small content")
        );
        assert_ne!(version.fetched_at, OffsetDateTime::UNIX_EPOCH);
        assert_eq!(db.images_snapshot()[0].current_checksum, Some(checksum));
        assert!(
            std::fs::read_dir(store.dir())
                .expect("read dir")
                .all(|entry| !entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with("download-"))
        );
    }

    #[tokio::test]
    async fn fetch_images_pin_mismatch() {
        let content = "tampered";
        let pinned = "11".repeat(32);
        let db = FakeDb::new();
        db.set_images(vec![Image {
            name: "pinned-img".into(),
            url: "https://img.example/p.qcow2".into(),
            pinned_checksum: Some(pinned.clone()),
            current_checksum: None,
        }]);
        let (store, _temp) = new_test_store(
            db.clone(),
            FakeClient::with_response("https://img.example/p.qcow2", FakeResponse::ok(content)),
        );

        let error = store.fetch_images().await.expect_err("pin mismatch");
        let message = error.to_string();
        assert!(message.contains(&pinned));
        assert!(message.contains(&sha256_hex(content)));
        assert!(db.inserted().is_empty());
        assert_eq!(db.images_snapshot()[0].current_checksum, None);
        assert!(!store.path(&sha256_hex(content)).expect("path").exists());
    }

    #[tokio::test]
    async fn fetch_images_pin_match() {
        let content = "trusted";
        let checksum = sha256_hex(content);
        let db = FakeDb::new();
        db.set_images(vec![Image {
            name: "pinned-img".into(),
            url: "https://img.example/p.qcow2".into(),
            pinned_checksum: Some(format!("sha256:{}", checksum.to_uppercase())),
            current_checksum: None,
        }]);
        let (store, _temp) = new_test_store(
            db.clone(),
            FakeClient::with_response("https://img.example/p.qcow2", FakeResponse::ok(content)),
        );
        store.fetch_images().await.expect("fetch images");
        assert_eq!(db.images_snapshot()[0].current_checksum, Some(checksum));
    }

    #[tokio::test]
    async fn fetch_images_no_op_when_version_exists() {
        let content = "same-content";
        let checksum = sha256_hex(content);
        let db = FakeDb::new();
        db.set_images(vec![Image {
            name: "debian-13".into(),
            url: "https://img.example/d13.qcow2".into(),
            pinned_checksum: None,
            current_checksum: Some(checksum.clone()),
        }]);
        db.add_version(ImageVersion {
            checksum: checksum.clone(),
            image_name: "debian-13".into(),
            path: String::new(),
            size: 0,
            fetched_at: OffsetDateTime::UNIX_EPOCH,
        });
        let (store, _temp) = new_test_store(
            db.clone(),
            FakeClient::with_response("https://img.example/d13.qcow2", FakeResponse::ok(content)),
        );
        std::fs::create_dir_all(store.dir()).expect("create dir");
        std::fs::write(store.path(&checksum).expect("path"), content).expect("write backing");
        store.fetch_images().await.expect("fetch images");
        assert!(db.inserted().is_empty());
    }

    #[tokio::test]
    async fn fetch_images_unpinned_change_warns() {
        let old_checksum = sha256_hex("v1");
        let new_checksum = sha256_hex("v2");
        let db = FakeDb::new();
        db.set_images(vec![Image {
            name: "debian-13".into(),
            url: "https://img.example/d13.qcow2".into(),
            pinned_checksum: None,
            current_checksum: Some(old_checksum.clone()),
        }]);
        db.add_version(ImageVersion {
            checksum: old_checksum.clone(),
            image_name: "debian-13".into(),
            path: String::new(),
            size: 0,
            fetched_at: OffsetDateTime::UNIX_EPOCH,
        });
        db.set_in_use(&old_checksum, true);
        let logs = Arc::new(Mutex::new(Vec::new()));
        let subscriber = crate::test_support::RecordingSubscriber::new(Arc::clone(&logs));
        let dispatch = tracing::Dispatch::new(subscriber);
        let _default = tracing::dispatcher::set_default(&dispatch);
        let (store, _temp) = new_test_store(
            db.clone(),
            FakeClient::with_response("https://img.example/d13.qcow2", FakeResponse::ok("v2")),
        );

        store.fetch_images().await.expect("fetch images");
        assert_eq!(
            db.images_snapshot()[0].current_checksum,
            Some(new_checksum.clone())
        );
        let logs = logs.lock().expect("logs").join("\n");
        assert!(logs.contains(&old_checksum), "{logs}");
        assert!(logs.contains(&new_checksum), "{logs}");
        assert!(logs.contains("WARN"), "{logs}");
    }

    #[tokio::test]
    async fn fetch_images_download_error_does_not_stop_others() {
        let db = FakeDb::new();
        db.set_images(vec![
            Image {
                name: "broken".into(),
                url: "https://img.example/broken.qcow2".into(),
                pinned_checksum: None,
                current_checksum: None,
            },
            Image {
                name: "fine".into(),
                url: "https://img.example/fine.qcow2".into(),
                pinned_checksum: None,
                current_checksum: None,
            },
        ]);
        let client = FakeClient::new(HashMap::from([
            (
                "https://img.example/broken.qcow2".into(),
                FakeResponse::status(500),
            ),
            (
                "https://img.example/fine.qcow2".into(),
                FakeResponse::ok("fine"),
            ),
        ]));
        let (store, _temp) = new_test_store(db.clone(), client);
        let error = store.fetch_images().await.expect_err("broken image");
        assert!(error.to_string().contains("broken"));
        assert_eq!(
            db.images_snapshot()[1].current_checksum,
            Some(sha256_hex("fine"))
        );
    }

    // Pins the SPEC 19 lock scope: the store lock guards image version
    // creation and deletion only, never a download, so a `new` command's
    // create_overlay must succeed while a fetch-images download is in
    // flight. Holding the lock across the download deadlocks this callback.
    #[tokio::test]
    async fn fetch_images_does_not_hold_lock_during_download() {
        let existing_checksum = sha256_hex("already-stored");
        let db = FakeDb::new();
        db.set_images(vec![Image {
            name: "debian-13".into(),
            url: "https://img.example/d13.qcow2".into(),
            pinned_checksum: None,
            current_checksum: None,
        }]);
        db.add_version(ImageVersion {
            checksum: existing_checksum.clone(),
            image_name: "debian-13".into(),
            path: String::new(),
            size: 0,
            fetched_at: OffsetDateTime::UNIX_EPOCH,
        });
        db.set_in_use(&existing_checksum, true);
        let temp = tempfile::tempdir().expect("tempdir");
        let runner = FakeRunner::default();
        let store_slot = Arc::new(Mutex::new(None::<Arc<Store>>));
        let slot = Arc::clone(&store_slot);
        let checksum_for_callback = existing_checksum.clone();
        let client = FakeClient::with_callback(
            "https://img.example/d13.qcow2",
            FakeResponse::ok("new-image"),
            move || {
                let store = slot.lock().expect("slot").clone().expect("store installed");
                let checksum = checksum_for_callback.clone();
                async move {
                    store
                        .create_overlay(&checksum, store.dir().join("overlay.qcow2"), 10)
                        .await
                }
            },
        );
        let store = Arc::new(
            Store::new(temp.path(), db.clone())
                .with_http_client(client)
                .with_runner(runner),
        );
        *store_slot.lock().expect("slot") = Some(Arc::clone(&store));
        std::fs::create_dir_all(store.dir()).expect("create dir");
        std::fs::write(store.path(&existing_checksum).expect("path"), "base")
            .expect("write backing");

        tokio::time::timeout(std::time::Duration::from_secs(2), store.fetch_images())
            .await
            .expect("lock scope deadlock")
            .expect("fetch images");
        assert_eq!(
            db.images_snapshot()[0].current_checksum,
            Some(sha256_hex("new-image"))
        );
    }

    #[tokio::test]
    async fn collect_deletes_only_unreferenced_non_current_versions() {
        let current = sha256_hex("cur");
        let older = sha256_hex("old");
        let orphan = sha256_hex("gone");
        let db = FakeDb::new();
        db.set_images(vec![Image {
            name: "debian-13".into(),
            url: "https://img.example/d13.qcow2".into(),
            pinned_checksum: None,
            current_checksum: Some(current.clone()),
        }]);
        for checksum in [&current, &older, &orphan] {
            db.add_version(ImageVersion {
                checksum: checksum.clone(),
                image_name: "debian-13".into(),
                path: String::new(),
                size: 0,
                fetched_at: OffsetDateTime::UNIX_EPOCH,
            });
        }
        db.set_in_use(&older, true);
        let (store, _temp) = new_test_store(
            db.clone(),
            FakeClient::with_response("https://img.example/d13.qcow2", FakeResponse::ok("cur")),
        );
        std::fs::create_dir_all(store.dir()).expect("create dir");
        for checksum in [&current, &older, &orphan] {
            let path = store.path(checksum).expect("path");
            std::fs::write(&path, checksum).expect("write version");
            std::fs::set_permissions(path, Permissions::from_mode(0o444)).expect("chmod");
        }

        store.fetch_images().await.expect("fetch images");
        assert_eq!(db.deleted(), vec![orphan.clone()]);
        assert!(store.path(&current).expect("path").exists());
        assert!(store.path(&older).expect("path").exists());
        assert!(!store.path(&orphan).expect("path").exists());
        assert!(db.has_version(&older));
        assert!(db.has_version(&current));
    }
}
