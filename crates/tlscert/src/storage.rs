use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Error;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), Error> {
    let parent = path.parent().ok_or_else(|| Error::Storage {
        operation: "find parent of",
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent"),
    })?;
    fs::create_dir_all(parent).map_err(|source| Error::Storage {
        operation: "create",
        path: parent.to_path_buf(),
        source,
    })?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("secret");
    let temp_path = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));

    let result = write_and_rename(&temp_path, path, contents);
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn write_and_rename(temp_path: &Path, path: &Path, contents: &[u8]) -> Result<(), Error> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);

    let mut file = options.open(temp_path).map_err(|source| Error::Storage {
        operation: "create temporary file for",
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(contents).map_err(|source| Error::Storage {
        operation: "write",
        path: path.to_path_buf(),
        source,
    })?;
    file.sync_all().map_err(|source| Error::Storage {
        operation: "sync",
        path: path.to_path_buf(),
        source,
    })?;
    drop(file);
    fs::rename(temp_path, path).map_err(|source| Error::Storage {
        operation: "replace",
        path: path.to_path_buf(),
        source,
    })?;
    sync_parent(path)
}

fn sync_parent(path: &Path) -> Result<(), Error> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let directory = std::fs::File::open(parent).map_err(|source| Error::Storage {
        operation: "open parent directory of",
        path: path.to_path_buf(),
        source,
    })?;
    directory.sync_all().map_err(|source| Error::Storage {
        operation: "sync parent directory of",
        path: path.to_path_buf(),
        source,
    })
}

pub(crate) fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, Error> {
    match fs::read(path) {
        Ok(data) => Ok(Some(data)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Storage {
            operation: "read",
            path: PathBuf::from(path),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::atomic_write;

    #[test]
    fn atomic_write_replaces_with_private_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("account.json");

        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"second");
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let entries = fs::read_dir(dir.path())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path(), path);
    }
}
