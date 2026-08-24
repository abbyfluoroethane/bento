use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use bento_types::{Image, ImageKind, ImageVersion};
use futures::StreamExt;
use reqwest::{Method, Request, Url};
use sha2::{Digest, Sha256};
use tempfile::{Builder, TempDir, TempPath};
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

    /// Adds a bootc-compatible OCI image to the durable allowlist and builds
    /// its qcow2 version immediately. A failed first build removes the newly
    /// inserted row so the operator can correct either the name or source.
    pub async fn add_oci_image(&self, name: &str, reference: &str) -> Result<()> {
        validate_image_name(name)?;
        validate_oci_reference(reference)?;
        let image = Image {
            name: name.to_owned(),
            url: reference.to_owned(),
            kind: ImageKind::Oci,
            pinned_checksum: None,
            current_checksum: None,
        };
        let inserted = self
            .db
            .insert_image(image.clone())
            .await
            .map_err(|error| dependency("append OCI image to allowlist", error))?;
        if !inserted {
            let existing = self
                .db
                .images()
                .await
                .map_err(|error| dependency("read existing allowlist entry", error))?
                .into_iter()
                .find(|existing| existing.name == name);
            if let Some(existing) = existing
                && existing.kind == ImageKind::Oci
                && existing.url == reference
            {
                // A failed first build leaves the durable allowlist entry in
                // place. Re-submitting the same entry is therefore a retry.
                return self.fetch_one(existing).await;
            }
            return Err(Error::Invalid(format!(
                "image name {name:?} is already on the allowlist with a different source"
            )));
        }
        let result = self.fetch_one(image).await;
        if result.is_err()
            && let Err(cleanup) = self.db.delete_unbuilt_image(name).await
        {
            tracing::warn!(image = name, %cleanup, "failed to remove unsuccessful runtime image");
        }
        result
    }

    /// Refreshes one named allowlist entry.
    pub async fn fetch_named(&self, name: &str) -> Result<()> {
        let image = self
            .db
            .images()
            .await
            .map_err(|error| dependency("list allowlist", error))?
            .into_iter()
            .find(|image| image.name == name)
            .ok_or_else(|| Error::Invalid(format!("unknown image {name:?}")))?;
        self.fetch_one(image).await
    }

    /// Runs steps 1-7 of SPEC section 5.1 for one image. The download and
    /// checksum run without the store lock; the lock is taken only for the
    /// version-creation steps 4-7 (SPEC section 19).
    async fn fetch_one(&self, image: Image) -> Result<()> {
        if image.kind == ImageKind::Oci {
            return self.fetch_oci(image).await;
        }
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

        let store = self.clone();
        tokio::spawn(async move {
            store
                .commit_download(image, temporary_path, checksum, size)
                .await
        })
        .await
        .map_err(|error| Error::Invalid(format!("download commit task failed: {error}")))?
    }

    async fn commit_download(
        &self,
        image: Image,
        temporary_path: TempPath,
        checksum: String,
        size: i64,
    ) -> Result<()> {
        // Steps 4-7 create an image version; only they need the store lock.
        let _guard = self.acquire_lock().await?;
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
                let mut rollback = persist_temp_path(temporary_path, &path)?;
                self.db
                    .set_current_checksum(&image.name, &checksum)
                    .await
                    .map_err(|error| dependency("mark repaired version current", error))?;
                rollback.disarm();
            } else {
                self.db
                    .set_current_checksum(&image.name, &checksum)
                    .await
                    .map_err(|error| dependency("mark existing version current", error))?;
            }
            return Ok(());
        }

        // 5. Store the file at the content-addressed path. The stored file is
        // never written again, so drop the write bits.
        let path = self.path_normalized(&checksum);
        let mut rollback = persist_temp_path(temporary_path, &path)?;

        // 6. Insert a row in image_versions.
        self.db
            .insert_image_version(ImageVersion {
                checksum: checksum.clone(),
                image_name: image.name.clone(),
                path: path.display().to_string(),
                size,
                kind: ImageKind::Qcow2,
                source_digest: None,
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
        rollback.disarm();
        Ok(())
    }

    async fn fetch_oci(&self, image: Image) -> Result<()> {
        validate_oci_reference(&image.url)?;
        validate_builder_reference(&self.builder_image)?;
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|error| io_error("create image directory", error))?;
        // Podman and image-builder both mutate the rootful container store.
        // This separate lock spans the complete OCI pipeline but never blocks
        // an overlay create on the content-store lock.
        let _oci_guard = self.acquire_oci_lock().await?;
        let podman = self.podman.to_string_lossy();
        run_timed(
            self,
            &podman,
            "podman pull",
            &[
                OsString::from("pull"),
                OsString::from("--quiet"),
                OsString::from("--policy=always"),
                OsString::from("--"),
                OsString::from(&image.url),
            ],
        )
        .await?;
        let digest_output = run_timed(
            self,
            &podman,
            "podman inspect",
            &[
                OsString::from("image"),
                OsString::from("inspect"),
                OsString::from("--format={{.Digest}}"),
                OsString::from("--"),
                OsString::from(&image.url),
            ],
        )
        .await?;
        let digest_text = String::from_utf8_lossy(&digest_output);
        let digest = format!(
            "sha256:{}",
            normalize_checksum(digest_text.trim()).map_err(|error| {
                Error::Invalid(format!("podman returned no usable OCI digest: {error}"))
            })?
        );

        // A previous successful conversion of this exact OCI digest is the
        // cache. Moving tags are pulled and inspected before this comparison.
        if let Some(version) = self
            .db
            .image_version_for_source(&image.name, &digest)
            .await
            .map_err(|error| dependency("find OCI source version", error))?
            && tokio::fs::metadata(&version.path).await.is_ok()
        {
            self.db
                .set_current_checksum(&image.name, &version.checksum)
                .await
                .map_err(|error| dependency("mark cached OCI build current", error))?;
            return Ok(());
        }

        validate_bootc_contract(self, &podman, &image.url).await?;
        run_timed(
            self,
            &podman,
            "pull image-builder",
            &[
                OsString::from("pull"),
                OsString::from("--quiet"),
                OsString::from("--policy=always"),
                OsString::from("--"),
                OsString::from(&self.builder_image),
            ],
        )
        .await?;

        let output = Builder::new()
            .prefix("bootc-output-")
            .tempdir_in(&self.dir)
            .map_err(|error| io_error("create bootc output directory", error))?;
        let output_mount = format!("{}:/output", output.path().display());
        let storage_mount = format!(
            "{}:/var/lib/containers/storage",
            self.container_storage.display()
        );
        run_timed(
            self,
            &podman,
            "image-builder",
            &[
                OsString::from("run"),
                OsString::from("--rm"),
                OsString::from("--privileged"),
                OsString::from("--security-opt"),
                OsString::from("label=type:unconfined_t"),
                OsString::from("-v"),
                OsString::from(output_mount),
                OsString::from("-v"),
                OsString::from(storage_mount),
                OsString::from(&self.builder_image),
                OsString::from("build"),
                OsString::from("--bootc-ref"),
                OsString::from(&image.url),
                OsString::from("--bootc-default-fs"),
                OsString::from(&self.bootc_rootfs),
                OsString::from("qcow2"),
            ],
        )
        .await?;
        let disk = find_single_qcow2(output.path())?;
        let (checksum, size) = hash_file(&disk).await?;
        let store = self.clone();
        tokio::spawn(async move {
            store
                .commit_generated(image, output, disk, checksum, size, digest)
                .await
        })
        .await
        .map_err(|error| Error::Invalid(format!("generated commit task failed: {error}")))?
    }

    async fn commit_generated(
        &self,
        image: Image,
        _output: TempDir,
        disk: PathBuf,
        checksum: String,
        size: i64,
        source_digest: String,
    ) -> Result<()> {
        let _guard = self.acquire_lock().await?;
        let exists = self
            .db
            .has_image_version(&checksum)
            .await
            .map_err(|error| dependency("check existing generated version", error))?;
        let path = self.path_normalized(&checksum);
        let file_exists = std::fs::metadata(&path).is_ok();
        if !exists && file_exists {
            let (stored_checksum, _) = hash_file(&path).await?;
            if stored_checksum != checksum {
                return Err(Error::Invalid(format!(
                    "orphaned image file {} does not match its content-addressed checksum",
                    path.display()
                )));
            }
        }
        let mut rollback = if !file_exists {
            Some(rename_generated_disk(&disk, &path)?)
        } else {
            None
        };
        if !exists {
            self.db
                .insert_image_version(ImageVersion {
                    checksum: checksum.clone(),
                    image_name: image.name.clone(),
                    path: path.display().to_string(),
                    size,
                    kind: ImageKind::Oci,
                    source_digest: Some(source_digest.clone()),
                    fetched_at: OffsetDateTime::now_utc(),
                })
                .await
                .map_err(|error| dependency("insert generated image version", error))?;
        }
        self.db
            .record_image_source(&image.name, &source_digest, &checksum)
            .await
            .map_err(|error| dependency("record OCI source digest", error))?;
        self.db
            .set_current_checksum(&image.name, &checksum)
            .await
            .map_err(|error| dependency("mark generated version current", error))?;
        if let Some(rollback) = &mut rollback {
            rollback.disarm();
        }
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

fn validate_image_name(name: &str) -> Result<()> {
    let valid_edge = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 63
        || !valid_edge(bytes[0])
        || !valid_edge(bytes[bytes.len() - 1])
        || !bytes.iter().all(|byte| valid_edge(*byte) || *byte == b'-')
    {
        return Err(Error::Invalid(
            "image name must be a lower-case DNS label".to_owned(),
        ));
    }
    Ok(())
}

fn validate_oci_reference(reference: &str) -> Result<()> {
    const TRANSPORTS: [&str; 11] = [
        "atomic:",
        "containers-storage:",
        "dir:",
        "docker:",
        "docker-archive:",
        "docker-daemon:",
        "oci:",
        "oci-archive:",
        "ostree:",
        "sif:",
        "tarball:",
    ];
    let valid_character = |character: char| {
        character.is_ascii_alphanumeric()
            || matches!(
                character,
                '.' | '_' | '-' | '/' | ':' | '@' | '+' | '[' | ']'
            )
    };
    if reference.is_empty()
        || reference.len() > 2048
        || !reference
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric() || first == '[')
        || !reference.chars().all(valid_character)
        || TRANSPORTS.iter().any(|transport| {
            reference
                .get(..transport.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(transport))
        })
    {
        return Err(Error::Invalid(
            "OCI image reference is not a valid option-safe registry reference".to_owned(),
        ));
    }
    Ok(())
}

fn validate_builder_reference(reference: &str) -> Result<()> {
    validate_oci_reference(reference)?;
    let Some((name, digest)) = reference.rsplit_once("@sha256:") else {
        return Err(Error::Invalid(
            "bootc builder image must be configured with an immutable @sha256 digest".to_owned(),
        ));
    };
    if name.is_empty() || digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(Error::Invalid(
            "bootc builder image must end with @sha256:<64 hex>".to_owned(),
        ));
    }
    Ok(())
}

async fn validate_bootc_contract(store: &Store, podman: &str, reference: &str) -> Result<()> {
    // This runs without privileges or host mounts. It checks the minimum
    // files Bento needs before handing the image to privileged image-builder.
    const SCRIPT: &str = r#"
test -x /usr/bin/cloud-init || { echo 'bootc contract: /usr/bin/cloud-init is missing' >&2; exit 20; }
test -x /usr/bin/qemu-ga || { echo 'bootc contract: /usr/bin/qemu-ga is missing' >&2; exit 21; }
test -d /usr/lib/modules || { echo 'bootc contract: /usr/lib/modules is missing' >&2; exit 22; }
find /usr/lib/modules -type f -name 'vmlinuz*' -print -quit | grep -q . || { echo 'bootc contract: no kernel was found in /usr/lib/modules' >&2; exit 23; }
find /usr/lib /usr/lib64 -type f -path '*/cloudinit/sources/DataSourceNoCloud.py' -print -quit 2>/dev/null | grep -q . || { echo 'bootc contract: cloud-init NoCloud data source is missing' >&2; exit 24; }
"#;
    run_timed(
        store,
        podman,
        "validate bootc image contract",
        &[
            OsString::from("run"),
            OsString::from("--rm"),
            OsString::from("--network=none"),
            OsString::from("--entrypoint=/bin/sh"),
            OsString::from("--"),
            OsString::from(reference),
            OsString::from("-eu"),
            OsString::from("-c"),
            OsString::from(SCRIPT),
        ],
    )
    .await
    .map(drop)
}

async fn run(
    store: &Store,
    program: &str,
    context: &'static str,
    args: &[OsString],
) -> Result<Vec<u8>> {
    store.runner.run(program, args).await.map_err(|error| {
        let (source, output) = error.into_parts();
        Error::Command {
            context,
            source,
            output: String::from_utf8_lossy(&output).into_owned(),
        }
    })
}

async fn run_timed(
    store: &Store,
    program: &str,
    context: &'static str,
    args: &[OsString],
) -> Result<Vec<u8>> {
    tokio::time::timeout(store.build_timeout, run(store, program, context, args))
        .await
        .map_err(|_| {
            Error::Invalid(format!(
                "{context} timed out after {} seconds",
                store.build_timeout.as_secs()
            ))
        })?
}

struct FileRollback {
    path: PathBuf,
    armed: bool,
}

impl FileRollback {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for FileRollback {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn persist_temp_path(temporary_path: TempPath, path: &Path) -> Result<FileRollback> {
    std::fs::set_permissions(&temporary_path, std::fs::Permissions::from_mode(0o444))
        .map_err(|error| io_error("chmod image", error))?;
    temporary_path
        .persist(path)
        .map_err(|error| io_error(format!("store at {}", path.display()), error.error))?;
    Ok(FileRollback {
        path: path.to_owned(),
        armed: true,
    })
}

fn rename_generated_disk(disk: &Path, path: &Path) -> Result<FileRollback> {
    std::fs::set_permissions(disk, std::fs::Permissions::from_mode(0o444))
        .map_err(|error| io_error("chmod generated image", error))?;
    std::fs::rename(disk, path)
        .map_err(|error| io_error(format!("store at {}", path.display()), error))?;
    Ok(FileRollback {
        path: path.to_owned(),
        armed: true,
    })
}

async fn hash_file(path: &Path) -> Result<(String, i64)> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| io_error("open generated qcow2", error))?;
    let mut hash = Sha256::new();
    let mut size = 0_i64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = tokio::io::AsyncReadExt::read(&mut file, &mut buffer)
            .await
            .map_err(|error| io_error("read generated qcow2", error))?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
        size = size
            .checked_add(i64::try_from(read).unwrap_or(i64::MAX))
            .ok_or_else(|| Error::Invalid("generated image exceeds i64 size".to_owned()))?;
    }
    Ok((hex::encode(hash.finalize()), size))
}

fn find_single_qcow2(root: &Path) -> Result<PathBuf> {
    fn visit(directory: &Path, found: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                visit(&path, found)?;
            } else if path
                .extension()
                .is_some_and(|extension| extension == "qcow2")
            {
                found.push(path);
            }
        }
        Ok(())
    }
    let mut found = Vec::new();
    visit(root, &mut found).map_err(|error| io_error("scan image-builder output", error))?;
    match found.as_slice() {
        [disk] => Ok(disk.clone()),
        [] => Err(Error::Invalid(
            "image-builder produced no qcow2 artifact".to_owned(),
        )),
        _ => Err(Error::Invalid(format!(
            "image-builder produced {} qcow2 artifacts; expected one",
            found.len()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::fs::Permissions;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use bento_types::{Image, ImageVersion};

    use super::*;
    use crate::test_support::{FakeClient, FakeDb, FakeResponse, FakeRunner, sha256_hex};

    fn new_test_store(db: FakeDb, client: FakeClient) -> (Store, tempfile::TempDir) {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = Store::new(temp.path(), db).with_http_client(client);
        (store, temp)
    }

    type RunnerCalls = Vec<(String, Vec<OsString>)>;

    #[derive(Clone, Default)]
    struct OciRunner(Arc<Mutex<RunnerCalls>>);

    impl OciRunner {
        fn calls(&self) -> Vec<(String, Vec<OsString>)> {
            self.0.lock().expect("runner calls").clone()
        }
    }

    fn builder_reference() -> String {
        format!("builder.example/image-builder@sha256:{}", "12".repeat(32))
    }

    #[async_trait]
    impl crate::Runner for OciRunner {
        async fn run(
            &self,
            name: &str,
            args: &[OsString],
        ) -> std::result::Result<Vec<u8>, crate::RunError> {
            self.0
                .lock()
                .expect("runner calls")
                .push((name.to_owned(), args.to_vec()));
            if args.first().is_some_and(|arg| arg == "image") {
                return Ok(format!("sha256:{}\n", "ab".repeat(32)).into_bytes());
            }
            if args.iter().any(|arg| arg == "--entrypoint=/bin/sh") {
                return Ok(Vec::new());
            }
            if args.first().is_some_and(|arg| arg == "run") {
                let mount = args
                    .iter()
                    .find_map(|arg| arg.to_str().and_then(|text| text.strip_suffix(":/output")))
                    .ok_or_else(|| {
                        crate::RunError::new(io::Error::other("missing output mount"), vec![])
                    })?;
                let artifact = Path::new(mount).join("qcow2/disk.qcow2");
                std::fs::create_dir_all(artifact.parent().expect("artifact parent"))
                    .map_err(|error| crate::RunError::new(error, vec![]))?;
                std::fs::write(artifact, b"bootc-qcow2")
                    .map_err(|error| crate::RunError::new(error, vec![]))?;
            }
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn add_oci_image_persists_builds_and_caches_by_source_digest() {
        let db = FakeDb::new();
        let runner = OciRunner::default();
        let temp = tempfile::tempdir().expect("tempdir");
        let store = Store::new(temp.path(), db.clone())
            .with_runner(runner.clone())
            .with_podman("test-podman")
            .with_builder_image(builder_reference())
            .with_bootc_rootfs("xfs")
            .with_container_storage("/test/containers");

        store
            .add_oci_image("fedora-bootc", "quay.io/fedora/fedora-bootc:latest")
            .await
            .expect("add OCI image");

        let image = &db.images_snapshot()[0];
        assert_eq!(image.name, "fedora-bootc");
        assert_eq!(image.kind, ImageKind::Oci);
        assert!(image.current_checksum.is_some());
        let inserted = db.inserted();
        assert_eq!(inserted.len(), 1);
        assert_eq!(inserted[0].size, 11);
        assert_eq!(inserted[0].kind, ImageKind::Oci);
        assert_eq!(
            inserted[0].source_digest,
            Some(format!("sha256:{}", "ab".repeat(32)))
        );
        assert_eq!(
            std::fs::read(&inserted[0].path).expect("generated qcow2"),
            b"bootc-qcow2"
        );
        let calls = runner.calls();
        assert_eq!(calls.len(), 5);
        assert!(calls.iter().all(|call| call.0 == "test-podman"));
        let builder_pull = &calls[3].1;
        assert!(builder_pull.contains(&OsString::from("--policy=always")));
        assert!(builder_pull.contains(&OsString::from(builder_reference())));
        let build = &calls[4].1;
        assert!(build.contains(&OsString::from(builder_reference())));
        assert!(build.contains(&OsString::from(
            "/test/containers:/var/lib/containers/storage"
        )));
        assert!(
            build
                .windows(2)
                .any(|args| args == ["--bootc-default-fs", "xfs"])
        );

        store
            .fetch_named("fedora-bootc")
            .await
            .expect("reuse OCI build");
        assert_eq!(db.inserted().len(), 1);
        assert_eq!(
            runner.calls().len(),
            7,
            "cache skips validation and builder commands"
        );

        let generated_path = db.inserted()[0].path.clone();
        std::fs::remove_file(&generated_path).expect("remove cached artifact");
        store
            .fetch_named("fedora-bootc")
            .await
            .expect("rebuild missing OCI artifact");
        assert_eq!(db.inserted().len(), 1, "content row is reused");
        assert_eq!(
            runner.calls().len(),
            12,
            "missing artifact forces a rebuild"
        );
        assert_eq!(
            std::fs::read(&generated_path).expect("repaired artifact"),
            b"bootc-qcow2"
        );

        store
            .add_oci_image("fedora-alias", "quay.io/fedora/fedora-bootc:latest")
            .await
            .expect("map a second allowlist name to the same disk");
        assert_eq!(db.inserted().len(), 1, "same content has one physical row");
        assert!(db.images_snapshot()[1].current_checksum.is_some());
        assert_eq!(runner.calls().len(), 17);
        store
            .fetch_named("fedora-alias")
            .await
            .expect("reuse the second name's source mapping");
        assert_eq!(
            runner.calls().len(),
            19,
            "source mapping avoids a second build"
        );

        let error = store
            .add_oci_image("fedora-bootc", "quay.io/example/different:latest")
            .await
            .expect_err("duplicate allowlist name");
        assert!(error.to_string().contains("already on the allowlist"));
        assert_eq!(
            runner.calls().len(),
            19,
            "duplicate is rejected before pull"
        );
    }

    #[derive(Clone, Copy)]
    struct ContractFailureRunner;

    #[async_trait]
    impl crate::Runner for ContractFailureRunner {
        async fn run(
            &self,
            _name: &str,
            args: &[OsString],
        ) -> std::result::Result<Vec<u8>, crate::RunError> {
            if args.first().is_some_and(|arg| arg == "image") {
                return Ok(format!("sha256:{}\n", "ab".repeat(32)).into_bytes());
            }
            if args.iter().any(|arg| arg == "--entrypoint=/bin/sh") {
                return Err(crate::RunError::new(
                    io::Error::other("contract rejected"),
                    b"cloud-init NoCloud data source is missing".to_vec(),
                ));
            }
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn failed_runtime_addition_releases_name_for_retry() {
        let db = FakeDb::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let failing = Store::new(temp.path(), db.clone())
            .with_runner(ContractFailureRunner)
            .with_builder_image(builder_reference());
        let error = failing
            .add_oci_image("retryable", "quay.io/example/os:latest")
            .await
            .expect_err("contract failure");
        assert!(error.to_string().contains("NoCloud"));
        assert!(db.images_snapshot().is_empty(), "failed row is rolled back");

        Store::new(temp.path(), db.clone())
            .with_runner(OciRunner::default())
            .with_builder_image(builder_reference())
            .add_oci_image("retryable", "quay.io/example/os:latest")
            .await
            .expect("same name can be retried after correction");
        assert_eq!(db.images_snapshot().len(), 1);
    }

    #[derive(Clone, Copy)]
    struct PendingRunner;

    #[async_trait]
    impl crate::Runner for PendingRunner {
        async fn run(
            &self,
            _name: &str,
            _args: &[OsString],
        ) -> std::result::Result<Vec<u8>, crate::RunError> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn oci_commands_time_out_and_release_new_name() {
        let db = FakeDb::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let store = Store::new(temp.path(), db.clone())
            .with_runner(PendingRunner)
            .with_builder_image(builder_reference())
            .with_build_timeout(Duration::from_millis(5));
        let error = store
            .add_oci_image("timeout", "quay.io/example/os:latest")
            .await
            .expect_err("timeout");
        assert!(error.to_string().contains("timed out"));
        assert!(db.images_snapshot().is_empty());
    }

    #[derive(Clone)]
    struct SerialRunner {
        inner: OciRunner,
        active: Arc<AtomicUsize>,
        maximum: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl crate::Runner for SerialRunner {
        async fn run(
            &self,
            name: &str,
            args: &[OsString],
        ) -> std::result::Result<Vec<u8>, crate::RunError> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            let result = crate::Runner::run(&self.inner, name, args).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            result
        }
    }

    #[tokio::test]
    async fn concurrent_oci_additions_serialize_the_complete_pipeline() {
        let db = FakeDb::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let runner = SerialRunner {
            inner: OciRunner::default(),
            active: active.clone(),
            maximum: maximum.clone(),
        };
        // Use independent Stores to exercise the filesystem flock, not only
        // the in-memory mutex shared by Store clones.
        let first = Store::new(temp.path(), db.clone())
            .with_runner(runner.clone())
            .with_builder_image(builder_reference());
        let second = Store::new(temp.path(), db)
            .with_runner(runner)
            .with_builder_image(builder_reference());
        let (first_result, second_result) = tokio::join!(
            first.add_oci_image("first", "quay.io/example/first:latest"),
            second.add_oci_image("second", "quay.io/example/second:latest")
        );
        first_result.expect("first build");
        second_result.expect("second build");
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn add_oci_image_validates_before_persisting_or_running() {
        let db = FakeDb::new();
        let runner = OciRunner::default();
        let temp = tempfile::tempdir().expect("tempdir");
        let store = Store::new(temp.path(), db.clone()).with_runner(runner.clone());

        let error = store
            .add_oci_image("Not Valid", "quay.io/example/os:latest")
            .await
            .expect_err("invalid name");
        assert!(error.to_string().contains("DNS label"));
        assert!(db.images_snapshot().is_empty());
        assert!(runner.calls().is_empty());

        let error = store
            .add_oci_image("valid", "--authfile=/root/secret")
            .await
            .expect_err("option-like reference");
        assert!(error.to_string().contains("option-safe"));
        assert!(db.images_snapshot().is_empty());
        assert!(runner.calls().is_empty());

        let error = store
            .add_oci_image("valid", "oci-archive:/tmp/image.tar")
            .await
            .expect_err("local transport reference");
        assert!(error.to_string().contains("registry reference"));
        assert!(db.images_snapshot().is_empty());
        assert!(runner.calls().is_empty());
    }

    #[tokio::test]
    async fn fetch_images_stores_new_version() {
        let content = "debian-13-content";
        let checksum = sha256_hex(content);
        let db = FakeDb::new();
        db.set_images(vec![Image {
            name: "debian-13".into(),
            url: "https://img.example/d13.qcow2".into(),
            kind: Default::default(),
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
            kind: Default::default(),
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
            kind: Default::default(),
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
            kind: Default::default(),
            pinned_checksum: None,
            current_checksum: None,
        }]);
        db.add_version(ImageVersion {
            checksum: checksum.clone(),
            image_name: "debian-13".into(),
            path: String::new(),
            size: 0,
            kind: Default::default(),
            source_digest: None,
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
        assert_eq!(db.images_snapshot()[0].current_checksum, Some(checksum));
    }

    #[tokio::test]
    async fn fetch_images_repairs_a_missing_file_for_an_existing_version() {
        let content = "repair-content";
        let checksum = sha256_hex(content);
        let db = FakeDb::new();
        db.set_images(vec![Image {
            name: "repair".into(),
            url: "https://img.example/repair.qcow2".into(),
            kind: ImageKind::Qcow2,
            pinned_checksum: None,
            current_checksum: None,
        }]);
        db.add_version(ImageVersion {
            checksum: checksum.clone(),
            image_name: "repair".into(),
            path: String::new(),
            size: i64::try_from(content.len()).unwrap(),
            kind: ImageKind::Qcow2,
            source_digest: None,
            fetched_at: OffsetDateTime::UNIX_EPOCH,
        });
        let (store, _temp) = new_test_store(
            db.clone(),
            FakeClient::with_response(
                "https://img.example/repair.qcow2",
                FakeResponse::ok(content),
            ),
        );

        store.fetch_images().await.expect("repair image file");

        assert_eq!(
            std::fs::read(store.path(&checksum).unwrap()).expect("repaired file"),
            content.as_bytes()
        );
        assert_eq!(db.images_snapshot()[0].current_checksum, Some(checksum));
    }

    #[tokio::test]
    async fn fetch_images_unpinned_change_warns() {
        let old_checksum = sha256_hex("v1");
        let new_checksum = sha256_hex("v2");
        let db = FakeDb::new();
        db.set_images(vec![Image {
            name: "debian-13".into(),
            url: "https://img.example/d13.qcow2".into(),
            kind: Default::default(),
            pinned_checksum: None,
            current_checksum: Some(old_checksum.clone()),
        }]);
        db.add_version(ImageVersion {
            checksum: old_checksum.clone(),
            image_name: "debian-13".into(),
            path: String::new(),
            size: 0,
            kind: Default::default(),
            source_digest: None,
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
                kind: Default::default(),
                pinned_checksum: None,
                current_checksum: None,
            },
            Image {
                name: "fine".into(),
                url: "https://img.example/fine.qcow2".into(),
                kind: Default::default(),
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
            kind: Default::default(),
            pinned_checksum: None,
            current_checksum: None,
        }]);
        db.add_version(ImageVersion {
            checksum: existing_checksum.clone(),
            image_name: "debian-13".into(),
            path: String::new(),
            size: 0,
            kind: Default::default(),
            source_digest: None,
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
            kind: Default::default(),
            pinned_checksum: None,
            current_checksum: Some(current.clone()),
        }]);
        for checksum in [&current, &older, &orphan] {
            db.add_version(ImageVersion {
                checksum: checksum.clone(),
                image_name: "debian-13".into(),
                path: String::new(),
                size: 0,
                kind: Default::default(),
                source_digest: None,
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
