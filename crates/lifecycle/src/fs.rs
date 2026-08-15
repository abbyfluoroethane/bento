use std::path::Path;

pub(crate) async fn remove_file(path: &Path) -> std::io::Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Copies to a freshly-created UUID path. Existing destinations are a logic
/// error and are never overwritten. Disk files are private to the daemon.
pub(crate) async fn copy_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut input = tokio::fs::File::open(source).await?;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut output = options.open(destination).await?;
    let result = async {
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = input.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            output.write_all(&buffer[..count]).await?;
        }
        output.flush().await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(destination).await;
    }
    result
}
