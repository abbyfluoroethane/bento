//! A one-file HTTP server for the image allowlist.
//!
//! `bentod fetch-images` downloads over HTTP (SPEC 5.1), so the end-to-end
//! run needs something to download from. This serves exactly one body on
//! every path, which is all the fetch pipeline asks for, and it keeps the
//! test off the network.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A running image server. Dropping it stops the listener.
pub struct ImageServer {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ImageServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ImageServer {
    /// Binds a loopback port and serves `body` at `/image.qcow2`.
    pub async fn start(body: Vec<u8>) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/image.qcow2", listener.local_addr()?);
        let body = Arc::new(body);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve(stream, body.clone()));
            }
        });
        Ok(Self { url, task })
    }

    /// The URL to put in the `[[images]]` allowlist entry.
    pub fn url(&self) -> &str {
        &self.url
    }
}

async fn serve(mut stream: TcpStream, body: Arc<Vec<u8>>) {
    // Read to the end of the request head. The requests are GETs with no
    // body, so nothing follows it.
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte).await {
            Ok(1) => request.push(byte[0]),
            _ => return,
        }
        if request.len() > 8192 {
            return;
        }
    }
    let head = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/octet-stream\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let _ = stream.write_all(&body).await;
    let _ = stream.flush().await;
}
