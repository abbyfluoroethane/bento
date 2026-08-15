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
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

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
        manager.manage_sync().await?;
        let config = manager.tls_config();
        certificate_manager = Some(manager);
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
    if let Some(manager) = certificate_manager {
        manager.close();
    }
    match result {
        Ok(()) | Err(bento_proxy::Error::Shutdown) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn control_proxy(base: &str) -> Result<bento_proxy::ControlHandler> {
    let base: Uri = base.parse()?;
    let authority = base
        .authority()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("control URL has no authority"))?;
    let scheme = base.scheme().cloned().unwrap_or(http::uri::Scheme::HTTP);
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(Duration::from_secs(5)));
    let client: Client<_, bento_proxy::ProxyBody> =
        Client::builder(TokioExecutor::new()).build(connector);
    Ok(bento_proxy::control_handler(move |mut request| {
        let client = client.clone();
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
            *request.uri_mut() = uri;
            match client.request(request).await {
                Ok(response) => response.map(|body| {
                    body.map_err(|error| -> bento_proxy::BoxError { Box::new(error) })
                        .boxed()
                }),
                Err(_) => empty_response(StatusCode::BAD_GATEWAY),
            }
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
}
