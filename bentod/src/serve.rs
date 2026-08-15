//! The control plane (SPEC 4): the only writer of the database, the policy
//! layer, and the dashboard. Startup follows SPEC 4.2 and 11.2: host checks,
//! libvirt, user networks, firewall, reboot restore, then HTTP.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use axum::Router;
use axum::body::Body;
use axum::extract::Path as AxumPath;
use axum::http::{HeaderMap, Response, Uri};
use axum::routing::{get, post};
use bento_hypervisor::{CheckConfig, NetworkManager};
use bento_network::{NftApplier, PortRange};
use tokio_util::sync::CancellationToken;

use crate::adapters::{
    ApiBackend, ApiStore, AuthAccess, AuthTokens, AuthUsers, Authenticator, Backend,
    NetworkEnsurer, access_status, operator_predicate, user_network,
};
use crate::firewall::Firewall;
use crate::keys::{FRONTEND_KEY_FILE, authorized_key_line, ensure_key, key_path};
use crate::ops::sync_image_allowlist;
use crate::setup::{App, shutdown_signal, socket_path};

pub(crate) async fn run_serve(config: &Path, _args: &[OsString]) -> Result<()> {
    let app = App::new(config).await?;
    let result = serve_inner(&app).await;
    app.close().await;
    result
}

async fn serve_inner(app: &App) -> Result<()> {
    host_checks(app).await?;
    let hypervisor = app.connect_libvirt().await?;
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_else(|_| "localhost".to_owned())
        .trim()
        .to_owned();
    let host = app
        .store
        .ensure_host(hostname, &app.cfg.libvirt_uri)
        .await
        .map_err(|error| anyhow::anyhow!("hosts row: {error}"))?;

    // The frontend public key rides in every seed so the frontend can reach
    // guests (SPEC 10 step 9). Creating it here keeps serve and sshd aligned.
    let frontend_key = ensure_key(&key_path(app, FRONTEND_KEY_FILE), "bento-frontend")?;
    let frontend_public = authorized_key_line(frontend_key.public_key(), "bento-frontend")?;
    let manager = app.manager(hypervisor.clone())?;
    sync_image_allowlist(app).await?;

    // Per-user networks and one whole-table nftables reload (SPEC 6.2, 6.3).
    let firewall = Arc::new(Firewall::new(
        app.store.clone(),
        app.plan,
        Arc::new(NftApplier::default()),
        PortRange {
            from: i32::from(app.cfg.listen.proxy_port_min),
            to: i32::from(app.cfg.listen.proxy_port_max),
        },
    ));
    ensure_user_networks(app, hypervisor.as_ref()).await?;
    firewall
        .reload()
        .await
        .map_err(|error| anyhow::anyhow!("nftables: {error}"))?;

    let router = control_plane_router(
        app,
        manager.clone(),
        firewall.clone(),
        frontend_public,
        host.id,
    )
    .await?;
    let address =
        bento_config::resolve_listen_addr(&app.cfg.listen.http).map_err(anyhow::Error::msg)?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(
        addr = %app.cfg.listen.http,
        domain = %app.cfg.base_domain,
        "control plane listening"
    );

    let cancellation = CancellationToken::new();
    let restore = tokio::spawn({
        let manager = manager.clone();
        async move {
            if let Err(error) = manager.restore().await {
                tracing::error!(%error, "restore failed");
            }
        }
    });
    let poller = tokio::spawn({
        let manager = manager.clone();
        let cancellation = cancellation.clone();
        async move {
            if let Err(error) = manager.run_poller(cancellation.cancelled()).await {
                tracing::error!(%error, "poller stopped");
            }
        }
    });
    let convergence = tokio::spawn({
        let store = app.store.clone();
        let plan = app.plan;
        let hypervisor = hypervisor.clone();
        let firewall = firewall.clone();
        let cancellation = cancellation.clone();
        async move {
            converge(store, plan, hypervisor, firewall, cancellation).await;
        }
    });

    let shutdown = cancellation.clone();
    let serve_result = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            shutdown.cancel();
        })
        .await;
    cancellation.cancel();
    restore.abort();
    let _ = restore.await;
    let _ = poller.await;
    let _ = convergence.await;
    drop(manager);
    drop(firewall);
    hypervisor.close().await?;
    Ok(serve_result?)
}

/// Runs the SPEC 4.2 checks. Fatal requirements refuse startup; KSM and
/// nested-virtualization failures only warn.
async fn host_checks(app: &App) -> Result<()> {
    let nested_wanted = app
        .store
        .instances()
        .await
        .unwrap_or_default()
        .iter()
        .any(|instance| instance.nested);
    let report = bento_hypervisor::check(
        CheckConfig {
            socket_path: socket_path(&app.cfg.libvirt_uri),
            image_dir: PathBuf::from(&app.cfg.image_dir),
            storage_dir: PathBuf::from(&app.cfg.storage_dir),
            nested_wanted,
            ..Default::default()
        },
        &bento_hypervisor::default_check_deps(),
    );
    for warning in report.warnings() {
        tracing::warn!(check = %warning.name, detail = %warning.detail, "host check");
    }
    if !report.ok() {
        let failures = report
            .results
            .iter()
            .filter(|result| result.fatal && !result.ok)
            .map(|result| format!("{}: {}", result.name, result.detail))
            .collect::<Vec<_>>()
            .join("\n  ");
        bail!("host requirements not met (SPEC 4.2):\n  {failures}");
    }
    Ok(())
}

/// Defines and starts every registered user's libvirt network (SPEC 6.2).
/// The convergence loop repeats this so registrations heal automatically.
pub(crate) async fn ensure_user_networks(app: &App, networks: &dyn NetworkEnsurer) -> Result<()> {
    for user in app.store.users().await? {
        let network = match user_network(app.plan, &user.subnet) {
            Ok(network) => network,
            Err(error) => {
                tracing::warn!(user = %user.name, %error, "user network skipped");
                continue;
            }
        };
        networks
            .ensure_network(&network.name, &network.xml()?)
            .await
            .map_err(|error| {
                anyhow::anyhow!("network {} of {}: {error}", network.name, user.name)
            })?;
    }
    Ok(())
}

async fn converge(
    store: bento_store::Store,
    plan: bento_network::Plan,
    networks: Arc<bento_hypervisor::Client>,
    firewall: Arc<Firewall>,
    cancellation: CancellationToken,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.tick().await;
    loop {
        tokio::select! {
            () = cancellation.cancelled() => return,
            _ = interval.tick() => {
                for user in store.users().await.unwrap_or_default() {
                    let result = async {
                        let network = user_network(plan, &user.subnet)?;
                        NetworkManager::ensure_network(networks.as_ref(), &network.name, &network.xml()?).await?;
                        Ok::<_, anyhow::Error>(())
                    }.await;
                    if let Err(error) = result {
                        tracing::warn!(user = %user.name, %error, "network convergence");
                    }
                }
                if let Err(error) = firewall.reload().await {
                    tracing::warn!(%error, "firewall convergence");
                }
            }
        }
    }
}

async fn control_plane_router(
    app: &App,
    manager: Arc<bento_lifecycle::Manager>,
    firewall: Arc<Firewall>,
    frontend_key: String,
    host_id: i64,
) -> Result<Router> {
    let mut auth = bento_auth::Service::new(
        &app.cfg.base_domain,
        Arc::new(AuthUsers(app.store.clone())),
        Arc::new(AuthAccess(app.store.clone())),
        Arc::new(AuthTokens(app.store.clone())),
    );
    if !app.cfg.oidc.issuer.is_empty() {
        let redirect = format!("https://{}/callback", app.cfg.base_domain);
        match bento_auth::ProviderClient::discover(
            &app.cfg.oidc.issuer,
            &app.cfg.oidc.client_id,
            &app.cfg.oidc.client_secret,
            &redirect,
        )
        .await
        {
            Ok(provider) => {
                let provider = Arc::new(provider);
                auth = auth.with_oidc(provider.clone(), provider);
            }
            Err(error) => tracing::warn!(
                issuer = %app.cfg.oidc.issuer,
                %error,
                "OIDC discovery failed; dashboard login disabled until restart"
            ),
        }
    } else {
        tracing::warn!("no OIDC issuer configured; dashboard login disabled");
    }
    let auth = Arc::new(auth);
    let operators = Arc::new(operator_predicate(&app.cfg.operators));
    let api = bento_api::router(bento_api::Config {
        store: Arc::new(ApiStore(app.store.clone())),
        lifecycle: Arc::new(ApiBackend(Backend {
            manager,
            store: app.store.clone(),
            host_id,
            frontend_key,
            firewall: Some(firewall),
        })),
        auth: Arc::new(Authenticator {
            service: auth.clone(),
            store: app.store.clone(),
        }),
        is_operator: Some(Arc::new(move |user| operators.contains(&user.name))),
        db_path: app.cfg.db_path.clone(),
    });

    let access_auth = auth.clone();
    let login_auth = auth.clone();
    let callback_auth = auth.clone();
    let logout_auth = auth;
    Ok(Router::new()
        .merge(api)
        .route(
            "/access/{uuid}",
            get(
                move |AxumPath(uuid): AxumPath<String>, headers: HeaderMap| {
                    let auth = access_auth.clone();
                    async move { access_status(&auth, &headers, &uuid).await }
                },
            ),
        )
        .route(
            "/login",
            get(move |uri: Uri| {
                let auth = login_auth.clone();
                async move { auth_response(auth.login_response(&uri)) }
            }),
        )
        .route(
            "/callback",
            get(move |headers: HeaderMap, uri: Uri| {
                let auth = callback_auth.clone();
                async move { auth_response(auth.callback_response(&headers, &uri).await) }
            }),
        )
        .route(
            "/logout",
            post(move |headers: HeaderMap| {
                let auth = logout_auth.clone();
                async move { auth_response(auth.logout_response(&headers).await) }
            }),
        )
        .merge(bento_dashboard::router()))
}

fn auth_response(response: http::Response<String>) -> Response<Body> {
    response.map(Body::from)
}

#[cfg(test)]
mod tests {
    #[test]
    fn status_code_type_is_the_http_one() {
        assert_eq!(http::StatusCode::NO_CONTENT.as_u16(), 204);
    }
}
