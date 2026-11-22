//! The HTTP proxy (SPEC 4, 9): optional TLS termination, hostname routing to
//! instances, and the base domain forwarded to the control plane. It reads
//! database state and asks the control plane to authorize sessions.

use std::convert::Infallible;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use bytes::Bytes;
use http::{Response, StatusCode, Uri};
use http_body_util::{BodyExt, Empty};

use crate::adapters::{ProxySource, RemoteSession};
use crate::setup::{App, bind_host, control_url, main_port, shutdown_signal};

pub(crate) async fn run_proxy(config: &Path, _args: &[OsString]) -> Result<()> {
    let app = App::new(config).await?;
    let result = proxy_inner(&app).await;
    app.close().await;
    result
}

async fn proxy_inner(app: &App) -> Result<()> {
    // SPEC 8 has the proxy own the wildcard certificate. `listen.tls = off`
    // delegates termination to a private frontend and serves plain HTTP.
    let mut certificate_manager = None;
    let tls_config = if app.cfg.listen.tls == bento_config::TlsMode::Off {
        tracing::warn!(
            note = "something else must terminate TLS in front of Bento; bind these listeners privately",
            "serving plain HTTP: listen.tls is off"
        );
        None
    } else {
        if app.cfg.acme.cloudflare_token.is_empty() {
            bail!(
                "acme.cloudflare_token is required: the wildcard certificate needs the DNS-01 challenge (SPEC 8). Set listen.tls = \"off\" when another proxy terminates TLS"
            );
        }
        let manager = bento_tlscert::new(bento_tlscert::Config {
            base_domain: app.cfg.base_domain.clone(),
            instance_domain: app.cfg.instance_domain.clone(),
            email: app.cfg.acme.email.clone(),
            provider: Some(bento_tlscert::cloudflare(&app.cfg.acme.cloudflare_token)),
            storage_dir: Path::new(&app.cfg.db_path)
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("acme"),
            directory: app.cfg.acme.directory.clone(),
            propagation_timeout: Duration::ZERO,
        })?;
        tracing::info!(domains = ?manager.domains(), "obtaining the wildcard certificate");
        let renewal = start_renewal(&manager).await?;
        let config = manager.tls_config();
        certificate_manager = Some((manager, renewal));
        Some(config)
    };

    let control = control_url(&app.cfg.listen.http);
    let source = Arc::new(ProxySource(app.store.clone()));
    let sessions = Arc::new(RemoteSession {
        base: control.clone(),
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?,
    });
    let proxy = Arc::new(
        bento_proxy::Proxy::builder(&app.cfg.base_domain, source.clone())
            .with_instance_domain(&app.cfg.instance_domain)
            .with_sessions(sessions)
            .with_last_seen(source)
            .with_control(control_proxy(&control)?)
            .with_ports(
                main_port(&app.cfg.listen.https),
                app.cfg.listen.proxy_port_min,
                app.cfg.listen.proxy_port_max,
            )
            .build()?,
    );
    let ports = proxy.ports();
    tracing::info!(
        bind = %bind_host(&app.cfg.listen.https),
        tls = %app.cfg.listen.tls.as_str(),
        main_port = ports[0],
        high_ports = %format!("{}-{}", ports[1], ports[ports.len() - 1]),
        control = %control,
        "proxy listening"
    );
    let result = proxy
        .serve(
            &bind_host(&app.cfg.listen.https),
            tls_config,
            None,
            shutdown_signal(),
        )
        .await;
    if let Some((manager, renewal)) = certificate_manager {
        manager.close();
        renewal.stop().await;
    }
    match result {
        Ok(()) | Err(bento_proxy::Error::Shutdown) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// The background task that renews the certificate (SPEC 8).
struct Renewal {
    cancellation: bento_tlscert::CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Renewal {
    /// Cancels the task and waits for it, so an order in flight finishes
    /// and removes its DNS record before the process exits.
    async fn stop(self) {
        self.cancellation.cancel();
        let _ = self.task.await;
    }
}

/// Obtains the certificate, then starts the renewal loop. Without the loop
/// a long-lived proxy lets its certificate expire (SPEC 8). A failed renewal
/// at startup is fatal only when no valid certificate exists; the loop
/// retries otherwise.
async fn start_renewal(manager: &bento_tlscert::Manager) -> Result<Renewal> {
    manager.manage_sync_tolerant().await?;
    let cancellation = bento_tlscert::CancellationToken::new();
    let task = manager.manage(cancellation.clone());
    Ok(Renewal { cancellation, task })
}

fn control_proxy(base: &str) -> Result<bento_proxy::ControlHandler> {
    let base: Uri = base.parse()?;
    let authority = base
        .authority()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("control URL has no authority"))?;
    let scheme = base.scheme().cloned().unwrap_or(http::uri::Scheme::HTTP);
    // The upgrade-aware send, not a bare client request: the web terminal is
    // a WebSocket on the base domain, and a bare request returns the 101
    // but never joins the two connections (SPEC 14.6).
    let transport = bento_proxy::http_transport();
    Ok(bento_proxy::control_handler(move |request| {
        let transport = transport.clone();
        let authority = authority.clone();
        let scheme = scheme.clone();
        async move {
            let path = request
                .uri()
                .path_and_query()
                .cloned()
                .unwrap_or_else(|| http::uri::PathAndQuery::from_static("/"));
            let uri = Uri::builder()
                .scheme(scheme)
                .authority(authority)
                .path_and_query(path)
                .build();
            let Ok(uri) = uri else {
                return empty_response(StatusCode::BAD_GATEWAY);
            };
            bento_proxy::send_upgradable(transport.as_ref(), request, |request| {
                *request.uri_mut() = uri;
            })
            .await
            .unwrap_or_else(|_| empty_response(StatusCode::BAD_GATEWAY))
        }
    }))
}

fn empty_response(status: StatusCode) -> Response<bento_proxy::ProxyBody> {
    let body = Empty::<Bytes>::new()
        .map_err(|never: Infallible| match never {})
        .boxed();
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_proxy_builds_without_network_io() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        control_proxy("http://127.0.0.1:10080").unwrap();
        let _client = reqwest::Client::new();
        let _path = std::path::PathBuf::from("acme");
    }

    /// Serves `service` on a loopback port, with upgrades, one connection
    /// per task.
    async fn serve_upgrades<S, F>(service: S) -> std::net::SocketAddr
    where
        S: Fn(http::Request<hyper::body::Incoming>) -> F + Clone + Send + Sync + 'static,
        F: std::future::Future<Output = Response<bento_proxy::ProxyBody>> + Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let service = service.clone();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |request| {
                        let response = service(request);
                        async move { Ok::<_, Infallible>(response.await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
                });
            }
        });
        address
    }

    /// The web terminal is a WebSocket on the base domain (SPEC 14.6). The
    /// control proxy must join the two upgraded connections, not only pass
    /// the 101 back. A generic `echo` upgrade stands in for the WebSocket.
    #[tokio::test]
    async fn control_proxy_joins_an_upgraded_connection() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let upstream = serve_upgrades(
            |mut request: http::Request<hyper::body::Incoming>| async move {
                let upgrade = hyper::upgrade::on(&mut request);
                tokio::spawn(async move {
                    let mut io = hyper_util::rt::TokioIo::new(upgrade.await.unwrap());
                    let mut buffer = [0_u8; 4];
                    io.read_exact(&mut buffer).await.unwrap();
                    io.write_all(&buffer).await.unwrap();
                });
                let mut response = empty_response(StatusCode::SWITCHING_PROTOCOLS);
                response
                    .headers_mut()
                    .insert(http::header::CONNECTION, "upgrade".parse().unwrap());
                response
                    .headers_mut()
                    .insert(http::header::UPGRADE, "echo".parse().unwrap());
                response
            },
        )
        .await;

        let control = control_proxy(&format!("http://{upstream}")).unwrap();
        let front = serve_upgrades(move |request: http::Request<hyper::body::Incoming>| {
            let control = control.clone();
            async move {
                let request = request.map(|body| {
                    body.map_err(|error| -> bento_proxy::BoxError { Box::new(error) })
                        .boxed()
                });
                control(request).await
            }
        })
        .await;

        let mut client = tokio::net::TcpStream::connect(front).await.unwrap();
        client
            .write_all(
                b"GET /vm/uuid-web/terminal/ws HTTP/1.1\r\nHost: bento.example.org\r\n\
                  Connection: upgrade\r\nUpgrade: echo\r\n\r\n",
            )
            .await
            .unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0_u8; 1];
            client.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");

        client.write_all(b"ping").await.unwrap();
        let mut echoed = [0_u8; 4];
        tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut echoed))
            .await
            .expect("the upgraded connection carries bytes")
            .unwrap();
        assert_eq!(&echoed, b"ping");
    }

    /// With no certificate on disk, an unreachable CA stops startup (SPEC 8).
    #[tokio::test]
    async fn start_renewal_fails_without_a_usable_certificate() {
        let storage = tempfile::tempdir().unwrap();
        let manager = bento_tlscert::new(bento_tlscert::Config {
            base_domain: "bento.example.org".into(),
            instance_domain: String::new(),
            email: String::new(),
            provider: Some(bento_tlscert::cloudflare("token")),
            storage_dir: storage.path().to_path_buf(),
            // Nothing listens on the discard port, so the order fails.
            directory: "http://127.0.0.1:9/directory".into(),
            propagation_timeout: Duration::ZERO,
        })
        .unwrap();
        assert!(start_renewal(&manager).await.is_err());
        assert!(!manager.has_valid_certificate());
    }
}
