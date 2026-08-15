use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tempfile::TempDir;
use tokio::io::AsyncReadExt as _;
use tokio::net::UnixStream;
use tokio::process::Command;

use crate::Seed;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A command failure and the command's combined output.
#[derive(Debug)]
pub struct CommandError {
    source: BoxError,
    output: Vec<u8>,
}

impl CommandError {
    /// Records a command error together with any output emitted before
    /// the command failed.
    pub fn new(
        source: impl std::error::Error + Send + Sync + 'static,
        output: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            source: Box::new(source),
            output: output.into(),
        }
    }

    /// The command output captured before failure.
    pub fn output(&self) -> &[u8] {
        &self.output
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for CommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Anything that stops a seed from being rendered, built, or deleted.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cloudinit: {0}")]
    Invalid(String),
    #[error("cloudinit: create staging directory: {0}")]
    CreateStaging(#[source] io::Error),
    #[error("cloudinit: write {name}: {source}")]
    Write {
        name: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("cloudinit: xorriso: {source}: {output}")]
    Xorriso {
        #[source]
        source: CommandError,
        output: String,
    },
    #[error("cloudinit: delete seed ISO: {0}")]
    Delete(#[source] io::Error),
}

impl Error {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

/// Executes a host command such as `xorriso`. Tests inject a fake so
/// nothing runs on the development machine.
#[async_trait]
pub trait Runner: Send + Sync {
    async fn run(&self, name: &OsStr, args: &[OsString]) -> Result<Vec<u8>, CommandError>;
}

#[derive(Debug)]
struct ExecRunner;

#[async_trait]
impl Runner for ExecRunner {
    async fn run(&self, name: &OsStr, args: &[OsString]) -> Result<Vec<u8>, CommandError> {
        let (reader, writer) =
            StdUnixStream::pair().map_err(|error| CommandError::new(error, Vec::new()))?;
        let stderr = writer
            .try_clone()
            .map_err(|error| CommandError::new(error, Vec::new()))?;
        reader
            .set_nonblocking(true)
            .map_err(|error| CommandError::new(error, Vec::new()))?;
        let mut reader =
            UnixStream::from_std(reader).map_err(|error| CommandError::new(error, Vec::new()))?;
        let mut child = Command::new(name)
            .args(args)
            .kill_on_drop(true)
            .stdout(Stdio::from(OwnedFd::from(writer)))
            .stderr(Stdio::from(OwnedFd::from(stderr)))
            .spawn()
            .map_err(|error| CommandError::new(error, Vec::new()))?;
        let mut output = Vec::new();
        let (read_result, status_result) =
            tokio::join!(reader.read_to_end(&mut output), child.wait());
        if let Err(error) = read_result {
            return Err(CommandError::new(error, output));
        }
        let status = status_result.map_err(|error| CommandError::new(error, output.clone()))?;
        if status.success() {
            return Ok(output);
        }
        let error = io::Error::other(
            status
                .code()
                .map_or_else(|| status.to_string(), |code| format!("exit status {code}")),
        );
        Err(CommandError::new(error, output))
    }
}

/// Builds NoCloud seed ISOs with `xorriso`.
pub struct Builder {
    runner: Arc<dyn Runner>,
    xorriso: PathBuf,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    /// Returns a builder that runs the real `xorriso`.
    pub fn new() -> Self {
        Self {
            runner: Arc::new(ExecRunner),
            xorriso: PathBuf::from("xorriso"),
        }
    }

    /// Sets the command runner used for `xorriso`.
    #[must_use]
    pub fn with_runner<R>(mut self, runner: R) -> Self
    where
        R: Runner + 'static,
    {
        self.runner = Arc::new(runner);
        self
    }

    /// Sets the `xorriso` binary path.
    #[must_use]
    pub fn with_xorriso(mut self, path: impl Into<PathBuf>) -> Self {
        self.xorriso = path.into();
        self
    }

    /// Renders the seed files and writes the NoCloud ISO at `iso_path`.
    /// The volume label `cidata` is what makes cloud-init recognize the
    /// disk as a NoCloud seed. The ISO holds the public keys of the owner;
    /// the caller detaches it and calls [`delete`] after the first
    /// successful boot (SPEC section 5.2).
    pub async fn build(&self, seed: &Seed, iso_path: impl AsRef<Path>) -> Result<(), Error> {
        let meta = seed.meta_data()?;
        let user = seed.user_data()?;
        let network = seed.network_config()?;

        let directory = tempfile::Builder::new()
            .prefix("bento-seed-")
            .tempdir()
            .map_err(Error::CreateStaging)?;
        write_seed_file(&directory, "meta-data", &meta)?;
        write_seed_file(&directory, "user-data", &user)?;
        write_seed_file(&directory, "network-config", &network)?;

        let args = vec![
            OsString::from("-as"),
            OsString::from("mkisofs"),
            OsString::from("-output"),
            iso_path.as_ref().as_os_str().to_owned(),
            OsString::from("-volid"),
            OsString::from("cidata"),
            OsString::from("-joliet"),
            OsString::from("-rational-rock"),
            directory.path().as_os_str().to_owned(),
        ];
        self.runner
            .run(self.xorriso.as_os_str(), &args)
            .await
            .map_err(|source| {
                let output = String::from_utf8_lossy(source.output()).into_owned();
                Error::Xorriso { source, output }
            })?;
        Ok(())
    }
}

fn write_seed_file(directory: &TempDir, name: &'static str, contents: &str) -> Result<(), Error> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.path().join(name))
        .and_then(|mut file| std::io::Write::write_all(&mut file, contents.as_bytes()))
        .map_err(|source| Error::Write { name, source })
}

/// Removes a seed ISO. An ISO that is already gone is not an error: the
/// goal state is "the keys are not on disk", and it is reached.
pub fn delete(iso_path: impl AsRef<Path>) -> Result<(), Error> {
    match std::fs::remove_file(iso_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Delete(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::seed::tests::test_seed;

    #[derive(Debug, Default)]
    struct FakeState {
        calls: Vec<Vec<OsString>>,
        staged: HashMap<String, String>,
    }

    /// Captures the xorriso invocation and snapshots the staging
    /// directory contents at call time, before `Builder::build` removes
    /// it.
    #[derive(Debug, Clone, Default)]
    struct FakeRunner {
        state: Arc<Mutex<FakeState>>,
        fail: bool,
    }

    #[async_trait]
    impl Runner for FakeRunner {
        async fn run(&self, name: &OsStr, args: &[OsString]) -> Result<Vec<u8>, CommandError> {
            let mut state = self.state.lock().unwrap();
            let mut call = vec![name.to_owned()];
            call.extend_from_slice(args);
            state.calls.push(call);
            if let Some(directory) = args.last() {
                state.staged.clear();
                if let Ok(entries) = std::fs::read_dir(directory) {
                    for entry in entries.flatten() {
                        if let Ok(contents) = std::fs::read_to_string(entry.path()) {
                            state
                                .staged
                                .insert(entry.file_name().to_string_lossy().into_owned(), contents);
                        }
                    }
                }
            }
            if self.fail {
                return Err(CommandError::new(FakeFailure, b"xorriso says no".to_vec()));
            }
            Ok(Vec::new())
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("exit status 1")]
    struct FakeFailure;

    fn golden(name: &str) -> String {
        std::fs::read_to_string(format!("{}/testdata/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[tokio::test]
    async fn build() {
        let runner = FakeRunner::default();
        let state = Arc::clone(&runner.state);
        let builder = Builder::new().with_runner(runner);
        let temp = tempfile::tempdir().unwrap();
        let iso_path = temp.path().join("seed.iso");

        builder.build(&test_seed(), &iso_path).await.unwrap();

        let state = state.lock().unwrap();
        assert_eq!(state.calls.len(), 1);
        let call = &state.calls[0];
        assert_eq!(call[0], "xorriso");
        let args = call[1..]
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        for expected in [
            "-as mkisofs".to_string(),
            format!("-output {}", iso_path.display()),
            "-volid cidata".to_string(),
        ] {
            assert!(
                args.contains(&expected),
                "xorriso args missing {expected:?}: {args}"
            );
        }
        for name in ["meta-data", "user-data", "network-config"] {
            assert_eq!(
                state.staged.get(name),
                Some(&golden(&format!("{name}.golden")))
            );
        }

        let staging_directory = Path::new(state.calls[0].last().unwrap());
        assert!(
            !staging_directory.exists(),
            "staging directory must be removed"
        );
    }

    #[tokio::test]
    async fn build_invalid_seed() {
        let runner = FakeRunner::default();
        let state = Arc::clone(&runner.state);
        let builder = Builder::new().with_runner(runner);
        let mut seed = test_seed();
        seed.authorized_keys.clear();

        let result = builder.build(&seed, "seed.iso").await;
        assert!(result.is_err());
        assert!(state.lock().unwrap().calls.is_empty());
    }

    #[tokio::test]
    async fn build_xorriso_failure() {
        let runner = FakeRunner {
            fail: true,
            ..FakeRunner::default()
        };
        let state = Arc::clone(&runner.state);
        let builder = Builder::new()
            .with_runner(runner)
            .with_xorriso("/opt/xorriso");
        let temp = tempfile::tempdir().unwrap();

        let error = builder
            .build(&test_seed(), temp.path().join("seed.iso"))
            .await
            .expect_err("build succeeded");
        assert!(error.to_string().contains("xorriso says no"));
        assert_eq!(state.lock().unwrap().calls[0][0], "/opt/xorriso");
    }

    #[test]
    fn delete_iso() {
        let temp = tempfile::tempdir().unwrap();
        let iso = temp.path().join("seed.iso");
        std::fs::write(&iso, b"iso").unwrap();
        delete(&iso).unwrap();
        assert!(!iso.exists());
        delete(&iso).unwrap();
    }
}
