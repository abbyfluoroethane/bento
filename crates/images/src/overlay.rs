use std::ffi::OsString;
use std::path::PathBuf;

use crate::store::{Error, Result, Store, io_error, normalize_checksum};

impl Store {
    /// Creates the root volume of a new instance as a copy-on-write qcow2
    /// overlay backed by the stored image version with the given checksum,
    /// then resizes it to the requested disk size (SPEC section 5.2). It
    /// holds the store lock for the whole operation, so a concurrent
    /// fetch-images collection cannot delete the backing version mid-create
    /// (SPEC section 19).
    ///
    /// Recording the backing checksum in the instances row is the caller's
    /// job: the caller passes the checksum in, and [`Store::backing_path`]
    /// derives the path it was built on.
    pub async fn create_overlay(
        &self,
        checksum: &str,
        overlay_path: impl Into<PathBuf>,
        disk_gib: i64,
    ) -> Result<()> {
        let checksum = normalize_checksum(checksum)?;
        if disk_gib <= 0 {
            return Err(Error::Invalid(format!(
                "overlay disk size must be positive, got {disk_gib} GiB"
            )));
        }
        let overlay_path = overlay_path.into();
        let _guard = self.acquire_lock().await?;
        let backing = self.path_normalized(&checksum);
        match tokio::fs::metadata(&backing).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::Invalid(format!(
                    "image version {checksum} is not in the store (expected {})",
                    backing.display()
                )));
            }
            Err(error) => return Err(io_error("stat backing file", error)),
        }

        // 1. Create a qcow2 file with the image version as its backing file.
        // The backing format is stated explicitly; qemu-img refuses to guess.
        let create_args = [
            OsString::from("create"),
            OsString::from("-f"),
            OsString::from("qcow2"),
            OsString::from("-F"),
            OsString::from("qcow2"),
            OsString::from("-b"),
            backing.into_os_string(),
            overlay_path.clone().into_os_string(),
        ];
        let program = self.qemu_img.to_string_lossy();
        if let Err(error) = self.runner.run(&program, &create_args).await {
            return Err(command_error("qemu-img create", error));
        }

        // 2. Resize the overlay to the requested disk size. The G suffix is
        // binary (GiB) in qemu-img.
        let resize_args = [
            OsString::from("resize"),
            overlay_path.clone().into_os_string(),
            OsString::from(format!("{disk_gib}G")),
        ];
        if let Err(error) = self.runner.run(&program, &resize_args).await {
            let _ = tokio::fs::remove_file(&overlay_path).await;
            return Err(command_error("qemu-img resize", error));
        }
        Ok(())
    }
}

fn command_error(context: &'static str, error: crate::RunError) -> Error {
    let (source, output) = error.into_parts();
    Error::Command {
        context,
        source,
        output: String::from_utf8_lossy(&output).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::RunError;
    use crate::test_support::{FakeDb, FakeRunner};

    #[tokio::test]
    async fn create_overlay() {
        let temp = tempfile::tempdir().expect("tempdir");
        let checksum = "ab".repeat(32);
        let runner = FakeRunner::default();
        let store = Store::new(temp.path(), FakeDb::new()).with_runner(runner.clone());
        std::fs::create_dir_all(store.dir()).expect("create dir");
        let backing = store.path(&checksum).expect("path");
        std::fs::write(&backing, "base").expect("write backing");
        let overlay = store.dir().join("overlay.qcow2");

        store
            .create_overlay(&checksum, &overlay, 20)
            .await
            .expect("create overlay");

        assert_eq!(
            runner.calls(),
            vec![
                vec![
                    "qemu-img",
                    "create",
                    "-f",
                    "qcow2",
                    "-F",
                    "qcow2",
                    "-b",
                    &backing.to_string_lossy(),
                    &overlay.to_string_lossy(),
                ],
                vec!["qemu-img", "resize", &overlay.to_string_lossy(), "20G"],
            ]
        );
    }

    #[tokio::test]
    async fn create_overlay_missing_backing_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let checksum = "cd".repeat(32);
        let runner = FakeRunner::default();
        let store = Store::new(temp.path(), FakeDb::new()).with_runner(runner.clone());
        let error = store
            .create_overlay(&checksum, store.dir().join("o.qcow2"), 10)
            .await
            .expect_err("missing backing");
        assert!(error.to_string().contains(&checksum));
        assert!(runner.calls().is_empty());
    }

    #[tokio::test]
    async fn create_overlay_invalid_input() {
        let temp = tempfile::tempdir().expect("tempdir");
        let runner = FakeRunner::default();
        let store = Store::new(temp.path(), FakeDb::new()).with_runner(runner.clone());
        assert!(
            store
                .create_overlay("nothex", "/x/o.qcow2", 10)
                .await
                .is_err()
        );
        assert!(
            store
                .create_overlay(&"ab".repeat(32), "/x/o.qcow2", 0)
                .await
                .is_err()
        );
        assert!(runner.calls().is_empty());
    }

    #[tokio::test]
    async fn create_overlay_resize_failure_cleans_up() {
        let temp = tempfile::tempdir().expect("tempdir");
        let checksum = "ef".repeat(32);
        let runner =
            FakeRunner::failing(1, RunError::new(io::Error::other("resize failed"), b"boom"));
        let store = Store::new(temp.path(), FakeDb::new()).with_runner(runner);
        std::fs::create_dir_all(store.dir()).expect("create dir");
        std::fs::write(store.path(&checksum).expect("path"), "base").expect("write backing");
        let overlay = store.dir().join("overlay.qcow2");
        std::fs::write(&overlay, "overlay").expect("write overlay");

        let error = store
            .create_overlay(&checksum, &overlay, 10)
            .await
            .expect_err("resize failure");
        let message = error.to_string();
        assert!(message.contains("resize failed"), "{message}");
        assert!(message.contains("boom"), "{message}");
        assert!(!overlay.exists());
    }
}
