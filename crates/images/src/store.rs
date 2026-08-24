use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use bento_types::{Image, ImageVersion};
use reqwest::{Request, Response};
use tokio::sync::{Mutex, OwnedMutexGuard};

/// The image directory from SPEC section 5.1. It must never move after
/// the first instance exists: a qcow2 overlay records the absolute path
/// of its backing file.
pub const DEFAULT_DIR: &str = "/var/lib/bento/images";

/// The flock file inside the image directory. It guards image version
/// creation and deletion across processes (SPEC section 19).
const LOCK_FILE_NAME: &str = ".lock";
/// Serializes Podman storage access without blocking overlay creation.
const OCI_LOCK_FILE_NAME: &str = ".oci.lock";

/// A thread-safe dynamic error returned through an injection seam.
pub type DynError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Any failure from image fetching, storage, reporting, or overlay creation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("images: invalid sha256 checksum {0:?}")]
    InvalidChecksum(String),
    #[error("images: {context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    #[error("images: {context}: {source}")]
    Dependency {
        context: String,
        #[source]
        source: DynError,
    },
    #[error("images: {0}")]
    Invalid(String),
    #[error("images: {0}")]
    Http(String),
    #[error("images: {context}: {source}: {output}")]
    Command {
        context: &'static str,
        #[source]
        source: DynError,
        output: String,
    },
    #[error("{0}")]
    Multiple(MultipleErrors),
}

/// The result type used throughout this crate.
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub struct MultipleErrors(Vec<Error>);

impl fmt::Display for MultipleErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index != 0 {
                f.write_str("\n")?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for MultipleErrors {}

pub(crate) fn joined(mut errors: Vec<Error>) -> Result<()> {
    match errors.len() {
        0 => Ok(()),
        1 => Err(errors.pop().expect("length checked")),
        _ => Err(Error::Multiple(MultipleErrors(errors))),
    }
}

pub(crate) fn io_error(context: impl Into<String>, source: io::Error) -> Error {
    Error::Io {
        context: context.into(),
        source,
    }
}

pub(crate) fn dependency(context: impl Into<String>, source: DynError) -> Error {
    Error::Dependency {
        context: context.into(),
        source,
    }
}

/// Sends one HTTP request. [`ReqwestClient`] is the real implementation;
/// tests inject a fake so no network is touched.
#[async_trait]
pub trait Doer: Send + Sync {
    async fn do_request(&self, request: Request) -> std::result::Result<Response, DynError>;
}

/// The production HTTP request client.
#[derive(Debug, Clone)]
pub struct ReqwestClient(reqwest::Client);

impl ReqwestClient {
    pub fn new(client: reqwest::Client) -> Self {
        Self(client)
    }
}

impl Default for ReqwestClient {
    fn default() -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Self(reqwest::Client::new())
    }
}

#[async_trait]
impl Doer for ReqwestClient {
    async fn do_request(&self, request: Request) -> std::result::Result<Response, DynError> {
        Ok(self.0.execute(request).await?)
    }
}

#[async_trait]
impl Doer for reqwest::Client {
    async fn do_request(&self, request: Request) -> std::result::Result<Response, DynError> {
        Ok(self.execute(request).await?)
    }
}

/// A command failure and all output produced before it failed.
#[derive(Debug)]
pub struct RunError {
    source: DynError,
    output: Vec<u8>,
}

impl RunError {
    pub fn new(source: impl Into<DynError>, output: impl Into<Vec<u8>>) -> Self {
        Self {
            source: source.into(),
            output: output.into(),
        }
    }

    pub fn output(&self) -> &[u8] {
        &self.output
    }

    pub(crate) fn into_parts(self) -> (DynError, Vec<u8>) {
        (self.source, self.output)
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(f)
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Executes a host command such as qemu-img. Tests inject a fake so
/// nothing runs on the development machine.
#[async_trait]
pub trait Runner: Send + Sync {
    async fn run(&self, name: &str, args: &[OsString]) -> std::result::Result<Vec<u8>, RunError>;
}

/// The production host command runner.
#[derive(Debug, Clone, Copy, Default)]
pub struct CommandRunner;

#[async_trait]
impl Runner for CommandRunner {
    async fn run(&self, name: &str, args: &[OsString]) -> std::result::Result<Vec<u8>, RunError> {
        let stdout_file = tempfile::tempfile().map_err(|error| RunError::new(error, Vec::new()))?;
        let stderr_file = tempfile::tempfile().map_err(|error| RunError::new(error, Vec::new()))?;
        let stdout = stdout_file
            .try_clone()
            .map_err(|error| RunError::new(error, Vec::new()))?;
        let stderr = stderr_file
            .try_clone()
            .map_err(|error| RunError::new(error, Vec::new()))?;
        let status = tokio::process::Command::new(name)
            .args(args)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .status()
            .await
            .map_err(|error| RunError::new(error, Vec::new()))?;
        let stdout = read_command_output(stdout_file)
            .await
            .map_err(|error| RunError::new(error, Vec::new()))?;
        let stderr = read_command_output(stderr_file)
            .await
            .map_err(|error| RunError::new(error, Vec::new()))?;
        if status.success() {
            Ok(stdout)
        } else {
            let mut combined = stdout;
            if !combined.is_empty() && !combined.ends_with(b"\n") && !stderr.is_empty() {
                combined.push(b'\n');
            }
            combined.extend(stderr);
            Err(RunError::new(
                io::Error::other(format!("command exited with {status}")),
                combined,
            ))
        }
    }
}

async fn read_command_output(file: File) -> io::Result<Vec<u8>> {
    let mut file = tokio::fs::File::from_std(file);
    tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(0)).await?;
    let mut output = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut file, &mut output).await?;
    Ok(output)
}

/// The consumer-side view of the queries the image store needs. The
/// real store package implements it.
#[async_trait]
pub trait DB: Send + Sync {
    /// Appends a new persistent allowlist entry without replacement.
    async fn insert_image(&self, image: Image) -> std::result::Result<bool, DynError>;
    /// Removes an allowlist entry whose first runtime build failed.
    async fn delete_unbuilt_image(&self, name: &str) -> std::result::Result<bool, DynError>;
    /// Inserts or updates one persistent allowlist entry.
    async fn upsert_image(&self, image: Image) -> std::result::Result<(), DynError>;
    /// Returns the operator allowlist with current checksums.
    async fn images(&self) -> std::result::Result<Vec<Image>, DynError>;
    /// Reports whether a version with the checksum exists.
    async fn has_image_version(&self, checksum: &str) -> std::result::Result<bool, DynError>;
    /// Inserts one image_versions row.
    async fn insert_image_version(
        &self,
        version: ImageVersion,
    ) -> std::result::Result<(), DynError>;
    /// Marks a version as the current one of an image.
    async fn set_current_checksum(
        &self,
        image_name: &str,
        checksum: &str,
    ) -> std::result::Result<(), DynError>;
    /// Returns every image_versions row.
    async fn image_versions(&self) -> std::result::Result<Vec<ImageVersion>, DynError>;
    /// Finds a previous conversion by the named OCI source digest.
    async fn image_version_for_source(
        &self,
        image_name: &str,
        source_digest: &str,
    ) -> std::result::Result<Option<ImageVersion>, DynError>;
    /// Associates an OCI source digest with its physical disk version.
    async fn record_image_source(
        &self,
        image_name: &str,
        source_digest: &str,
        checksum: &str,
    ) -> std::result::Result<(), DynError>;
    /// Deletes one image_versions row.
    async fn delete_image_version(&self, checksum: &str) -> std::result::Result<(), DynError>;
    /// Reports whether any instances row carries the checksum in
    /// base_checksum (SPEC sections 5.1 and 12).
    async fn checksum_in_use(&self, checksum: &str) -> std::result::Result<bool, DynError>;
}

#[async_trait]
impl<T: DB + ?Sized> DB for Arc<T> {
    async fn insert_image(&self, image: Image) -> std::result::Result<bool, DynError> {
        (**self).insert_image(image).await
    }

    async fn delete_unbuilt_image(&self, name: &str) -> std::result::Result<bool, DynError> {
        (**self).delete_unbuilt_image(name).await
    }

    async fn upsert_image(&self, image: Image) -> std::result::Result<(), DynError> {
        (**self).upsert_image(image).await
    }
    async fn images(&self) -> std::result::Result<Vec<Image>, DynError> {
        (**self).images().await
    }

    async fn has_image_version(&self, checksum: &str) -> std::result::Result<bool, DynError> {
        (**self).has_image_version(checksum).await
    }

    async fn insert_image_version(
        &self,
        version: ImageVersion,
    ) -> std::result::Result<(), DynError> {
        (**self).insert_image_version(version).await
    }

    async fn set_current_checksum(
        &self,
        image_name: &str,
        checksum: &str,
    ) -> std::result::Result<(), DynError> {
        (**self).set_current_checksum(image_name, checksum).await
    }

    async fn image_versions(&self) -> std::result::Result<Vec<ImageVersion>, DynError> {
        (**self).image_versions().await
    }

    async fn image_version_for_source(
        &self,
        image_name: &str,
        source_digest: &str,
    ) -> std::result::Result<Option<ImageVersion>, DynError> {
        (**self)
            .image_version_for_source(image_name, source_digest)
            .await
    }

    async fn record_image_source(
        &self,
        image_name: &str,
        source_digest: &str,
        checksum: &str,
    ) -> std::result::Result<(), DynError> {
        (**self)
            .record_image_source(image_name, source_digest, checksum)
            .await
    }

    async fn delete_image_version(&self, checksum: &str) -> std::result::Result<(), DynError> {
        (**self).delete_image_version(checksum).await
    }

    async fn checksum_in_use(&self, checksum: &str) -> std::result::Result<bool, DynError> {
        (**self).checksum_in_use(checksum).await
    }
}

/// The content-addressed image store. One version lives at
/// `dir()/sha256-<hex>.qcow2`; the path never changes, and the path never
/// holds different content.
#[derive(Clone)]
pub struct Store {
    pub(crate) dir: PathBuf,
    pub(crate) db: Arc<dyn DB>,
    pub(crate) client: Arc<dyn Doer>,
    pub(crate) runner: Arc<dyn Runner>,
    pub(crate) qemu_img: OsString,
    pub(crate) podman: OsString,
    pub(crate) builder_image: String,
    pub(crate) bootc_rootfs: String,
    pub(crate) container_storage: PathBuf,
    pub(crate) build_timeout: std::time::Duration,

    // Serializes version creation and deletion inside this process. The
    // flock on LOCK_FILE_NAME does the same across processes. Together
    // they close the open item in SPEC section 19: a fetch-images
    // collection cannot delete a version while a create reads it.
    lock: Arc<Mutex<()>>,
    oci_lock: Arc<Mutex<()>>,
}

impl Store {
    /// Returns a store rooted at `dir`. An empty path selects [`DEFAULT_DIR`].
    pub fn new<D>(dir: impl Into<PathBuf>, db: D) -> Self
    where
        D: DB + 'static,
    {
        let mut dir = dir.into();
        if dir.as_os_str().is_empty() {
            dir = PathBuf::from(DEFAULT_DIR);
        }
        Self {
            dir,
            db: Arc::new(db),
            client: Arc::new(ReqwestClient::default()),
            runner: Arc::new(CommandRunner),
            qemu_img: OsString::from("qemu-img"),
            podman: OsString::from("podman"),
            builder_image: String::new(),
            bootc_rootfs: "ext4".to_owned(),
            container_storage: PathBuf::from("/var/lib/containers/storage"),
            build_timeout: std::time::Duration::from_secs(30 * 60),
            lock: Arc::new(Mutex::new(())),
            oci_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Sets the HTTP client used to download images.
    pub fn with_http_client<D: Doer + 'static>(mut self, client: D) -> Self {
        self.client = Arc::new(client);
        self
    }

    /// Sets the command runner used for qemu-img.
    pub fn with_runner<R: Runner + 'static>(mut self, runner: R) -> Self {
        self.runner = Arc::new(runner);
        self
    }

    /// Sets the qemu-img binary path.
    pub fn with_qemu_img(mut self, path: impl Into<OsString>) -> Self {
        self.qemu_img = path.into();
        self
    }

    /// Sets the Podman binary used to pull and convert OCI images.
    #[must_use]
    pub fn with_podman(mut self, path: impl Into<OsString>) -> Self {
        self.podman = path.into();
        self
    }

    /// Overrides the privileged image-builder container image.
    #[must_use]
    pub fn with_builder_image(mut self, image: impl Into<String>) -> Self {
        self.builder_image = image.into();
        self
    }

    /// Sets the fallback root filesystem passed to image-builder.
    #[must_use]
    pub fn with_bootc_rootfs(mut self, rootfs: impl Into<String>) -> Self {
        self.bootc_rootfs = rootfs.into();
        self
    }

    /// Sets the rootful Podman storage shared with image-builder.
    #[must_use]
    pub fn with_container_storage(mut self, path: impl Into<PathBuf>) -> Self {
        self.container_storage = path.into();
        self
    }

    /// Bounds each Podman operation, including image-builder.
    #[must_use]
    pub fn with_build_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.build_timeout = timeout;
        self
    }

    /// Returns the image directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns the content-addressed path for a checksum:
    /// `<dir>/sha256-<hex>.qcow2`. The checksum may carry a `sha256:` or
    /// `sha256-` prefix and any letter case.
    pub fn path(&self, checksum: &str) -> Result<PathBuf> {
        Ok(self.path_normalized(&normalize_checksum(checksum)?))
    }

    /// Returns the content-addressed path an overlay with the given base
    /// checksum is backed by. It is the same derivation as [`Store::path`].
    pub fn backing_path(&self, checksum: &str) -> Result<PathBuf> {
        self.path(checksum)
    }

    pub(crate) fn path_normalized(&self, checksum: &str) -> PathBuf {
        self.dir.join(format!("sha256-{checksum}.qcow2"))
    }

    /// Takes the in-process mutex and then an exclusive flock on the lock
    /// file in the image directory. The guard releases both when dropped.
    pub(crate) async fn acquire_lock(&self) -> Result<StoreLock> {
        self.acquire_named_lock(Arc::clone(&self.lock), LOCK_FILE_NAME)
            .await
    }

    /// Serializes the complete pull/validate/build pipeline across processes.
    pub(crate) async fn acquire_oci_lock(&self) -> Result<StoreLock> {
        self.acquire_named_lock(Arc::clone(&self.oci_lock), OCI_LOCK_FILE_NAME)
            .await
    }

    async fn acquire_named_lock(
        &self,
        process_lock: Arc<Mutex<()>>,
        file_name: &'static str,
    ) -> Result<StoreLock> {
        let process_guard = process_lock.lock_owned().await;
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir)
                .map_err(|error| io_error("create image directory", error))?;
            let file = File::options()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(dir.join(file_name))
                .map_err(|error| io_error("open lock file", error))?;
            flock(&file, LOCK_EX).map_err(|error| io_error("flock", error))?;
            Ok(StoreLock {
                _process_guard: process_guard,
                file,
            })
        })
        .await
        .map_err(|error| Error::Invalid(format!("lock task failed: {error}")))?
    }
}

pub(crate) fn normalize_checksum(checksum: &str) -> Result<String> {
    let lowercase = checksum.trim().to_ascii_lowercase();
    let bare = lowercase
        .strip_prefix("sha256:")
        .or_else(|| lowercase.strip_prefix("sha256-"))
        .unwrap_or(&lowercase);
    if bare.len() != 64 || !bare.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::InvalidChecksum(checksum.to_owned()));
    }
    Ok(bare.to_owned())
}

pub(crate) struct StoreLock {
    _process_guard: OwnedMutexGuard<()>,
    file: File,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = flock(&self.file, LOCK_UN);
    }
}

const LOCK_EX: i32 = 2;
const LOCK_UN: i32 = 8;

fn flock(file: &File, operation: i32) -> io::Result<()> {
    unsafe extern "C" {
        #[link_name = "flock"]
        fn libc_flock(fd: i32, operation: i32) -> i32;
    }
    // SAFETY: `file` owns a valid descriptor for the duration of this call,
    // and flock neither retains the descriptor nor accesses Rust memory.
    if unsafe { libc_flock(file.as_raw_fd(), operation) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::FakeDb;

    #[test]
    fn path_derivation() {
        let checksum = "ab".repeat(32);
        let cases = [
            ("bare hex", checksum.clone(), false),
            ("sha256 colon prefix", format!("sha256:{checksum}"), false),
            ("sha256 dash prefix", format!("sha256-{checksum}"), false),
            ("uppercase", checksum.to_uppercase(), false),
            ("too short", checksum[..10].to_owned(), true),
            ("not hex", "zz".repeat(32), true),
            ("empty", String::new(), true),
            ("path traversal", "../../etc/passwd".to_owned(), true),
        ];
        let store = Store::new("/var/lib/bento/images", FakeDb::new());
        let expected = PathBuf::from(format!("/var/lib/bento/images/sha256-{checksum}.qcow2"));
        for (name, input, want_error) in cases {
            let result = store.path(&input);
            assert_eq!(result.is_err(), want_error, "{name}: {result:?}");
            if !want_error {
                assert_eq!(result.expect(name), expected, "{name}");
            }
        }
    }

    #[test]
    fn default_dir() {
        let store = Store::new("", FakeDb::new());
        assert_eq!(store.dir(), Path::new(DEFAULT_DIR));
        assert_eq!(DEFAULT_DIR, "/var/lib/bento/images");
    }

    #[test]
    fn backing_path_matches_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let checksum = "cd".repeat(32);
        let store = Store::new(temp.path(), FakeDb::new());
        let path = store.path(&checksum).expect("path");
        assert_eq!(store.backing_path(&checksum).expect("backing path"), path);
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(format!("sha256-{checksum}.qcow2").as_str())
        );
    }

    #[tokio::test]
    async fn command_runner_separates_success_stderr_and_preserves_failure_output() {
        let runner = CommandRunner;
        let output = runner
            .run(
                "/bin/sh",
                &[
                    OsString::from("-c"),
                    OsString::from("printf out; printf warning >&2"),
                ],
            )
            .await
            .expect("successful command");
        assert_eq!(output, b"out");

        let error = runner
            .run(
                "/bin/sh",
                &[
                    OsString::from("-c"),
                    OsString::from("printf out; printf error >&2; exit 7"),
                ],
            )
            .await
            .expect_err("failed command");
        assert_eq!(error.output(), b"out\nerror");
    }
}
