use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

use crate::{DynError, OverlayResizer};

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
}

impl fmt::Display for RunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Executes a host command such as `qemu-img`. Tests inject a fake so no
/// process runs on a development host.
#[async_trait]
pub trait Runner: Send + Sync {
    async fn run(&self, name: &OsStr, args: &[OsString]) -> std::result::Result<Vec<u8>, RunError>;
}

#[derive(Debug)]
struct ExecRunner;

#[async_trait]
impl Runner for ExecRunner {
    async fn run(&self, name: &OsStr, args: &[OsString]) -> std::result::Result<Vec<u8>, RunError> {
        let mut child = Command::new(name)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| RunError::new(error, Vec::new()))?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let mut output = Vec::new();
        let mut err_output = Vec::new();
        let (read_out, read_err, status) = tokio::join!(
            stdout.read_to_end(&mut output),
            stderr.read_to_end(&mut err_output),
            child.wait()
        );
        if let Err(error) = read_out {
            return Err(RunError::new(error, output));
        }
        if let Err(error) = read_err {
            output.extend(err_output);
            return Err(RunError::new(error, output));
        }
        output.extend(err_output);
        let status = status.map_err(|error| RunError::new(error, output.clone()))?;
        if status.success() {
            Ok(output)
        } else {
            Err(RunError::new(io::Error::other(status.to_string()), output))
        }
    }
}

/// Grows qcow2 overlays using `qemu-img resize` (SPEC 11.1).
pub struct QemuImgResizer {
    runner: Arc<dyn Runner>,
    qemu_img: PathBuf,
}

impl Default for QemuImgResizer {
    fn default() -> Self {
        Self {
            runner: Arc::new(ExecRunner),
            qemu_img: PathBuf::from("qemu-img"),
        }
    }
}

impl QemuImgResizer {
    #[must_use]
    pub fn with_runner(mut self, runner: Arc<dyn Runner>) -> Self {
        self.runner = runner;
        self
    }
    #[must_use]
    pub fn with_qemu_img(mut self, path: impl Into<PathBuf>) -> Self {
        self.qemu_img = path.into();
        self
    }
}

#[async_trait]
impl OverlayResizer for QemuImgResizer {
    async fn resize_overlay(
        &self,
        overlay_path: &Path,
        disk_gib: i64,
    ) -> std::result::Result<(), DynError> {
        if disk_gib <= 0 {
            return Err(format!(
                "lifecycle: resize overlay {}: disk size {disk_gib} GiB is not positive",
                overlay_path.display()
            )
            .into());
        }
        let args = [
            OsString::from("resize"),
            overlay_path.as_os_str().to_owned(),
            OsString::from(format!("{disk_gib}G")),
        ];
        if let Err(error) = self.runner.run(self.qemu_img.as_os_str(), &args).await {
            return Err(format!(
                "lifecycle: qemu-img resize {} to {disk_gib} GiB: {error}: {}",
                overlay_path.display(),
                String::from_utf8_lossy(error.output())
            )
            .into());
        }
        Ok(())
    }
}
