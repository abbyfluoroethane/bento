//! The control plane (SPEC 4): the only writer of the database, the policy
//! layer, and the dashboard. Startup follows SPEC 4.2 and 11.2: host checks,
//! libvirt, user networks, firewall, reboot restore, then HTTP.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::body::Body;
use axum::extract::Path as AxumPath;
use axum::http::{HeaderMap, Response, Uri};
use axum::routing::{get, post};
use bento_hypervisor::NetworkManager;
use bento_network::{NftApplier, PortRange};
use bento_types::Lease;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::adapters::{
    AccountProvisioner, ApiBackend, ApiStore, AuthAccess, AuthPairings, AuthTokens, AuthUsers,
    Authenticator, Backend, NetworkEnsurer, RuntimeImages, access_status, operator_predicate,
    user_network,
};
use crate::firewall::Firewall;
use crate::keys::{FRONTEND_KEY_FILE, authorized_key_line, ensure_key, key_path};
use crate::ops::sync_image_allowlist;
use crate::setup::{App, host_checks, shutdown_signal};

const CONTROLLER_LEASE_TTL: Duration = Duration::from_secs(30);

pub(crate) async fn run_serve(config: &Path, _args: &[OsString]) -> Result<()> {
    let app = App::new(config).await?;
    let holder_id = bento_lifecycle::random_uuid();
    let lease = acquire_controller_lease(&app.store, holder_id.clone()).await?;
    tracing::info!(
        controller_epoch = lease.epoch,
        holder_id = %lease.holder_id,
        expires_at = %lease.expires_at,
        "controller lease acquired"
    );

    let cancellation = CancellationToken::new();
    let (lease_sender, lease_receiver) = watch::channel(lease);
    let renewal = spawn_lease_renewal(
        app.store.clone(),
        holder_id.clone(),
        lease_sender,
        cancellation.clone(),
    );
    let result = serve_inner(&app, lease_receiver, cancellation.clone()).await;
    cancellation.cancel();
    let _ = renewal.await;
    let release = app.store.release_lease(holder_id).await;
    if let Err(error) = &release {
        tracing::warn!(%error, "controller lease release failed");
    }
    app.close().await;
    result?;
    release.context("release controller lease (MULTI-NODE 11.3)")?;
    Ok(())
}

async fn acquire_controller_lease(store: &bento_store::Store, holder_id: String) -> Result<Lease> {
    match store.acquire_lease(holder_id, CONTROLLER_LEASE_TTL).await {
        Ok(lease) => Ok(lease),
        Err(bento_store::Error::LeaseHeld { holder, expires_at }) => bail!(
            "control plane cannot start: controller lease is held by {holder} until {expires_at} (MULTI-NODE 11.3)"
        ),
        Err(error) => Err(error).context("acquire controller lease (MULTI-NODE 11.3)"),
    }
}

fn spawn_lease_renewal(
    store: bento_store::Store,
    holder_id: String,
    lease_sender: watch::Sender<Lease>,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(CONTROLLER_LEASE_TTL / 3);
        // The lease already has a full TTL. Wait one renewal period before
        // writing it again (MULTI-NODE 11.3).
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                _ = ticker.tick() => {
                    match store.renew_lease(holder_id.clone(), CONTROLLER_LEASE_TTL).await {
                        Ok(lease) => {
                            lease_sender.send_replace(lease);
                        }
                        Err(bento_store::Error::LeaseLost) => {
                            tracing::error!(
                                "controller lease lost; this process has been replaced and must not dispatch"
                            );
                            cancellation.cancel();
                            return;
                        }
                        Err(error) => {
                            // The controller cannot prove that it still owns
                            // the lease. Stop before it can dispatch stale work
                            // (MULTI-NODE 11.3).
                            tracing::error!(
                                %error,
                                "controller lease renewal failed; this process must not dispatch"
                            );
                            cancellation.cancel();
                            return;
                        }
                    }
                }
            }
        }
    })
}

async fn serve_inner(
    app: &App,
    lease: watch::Receiver<Lease>,
    cancellation: CancellationToken,
) -> Result<()> {
    host_checks(app).await?;
    let hypervisor = app.connect_libvirt().await?;
    // The machine ID is the key and the hostname is a label. Reading the
    // transient kernel hostname as the key gave one machine a second row
    // every time NetworkManager renamed it (MULTI-NODE 16).
    let machine_id = bento_hostinfo::read_machine_id()
        .map_err(|error| anyhow::anyhow!("machine identity: {error}"))?;
    let host = app
        .store
        .ensure_host(
            machine_id,
            bento_hostinfo::read_hostname(),
            &app.cfg.libvirt_uri,
        )
        .await
        .map_err(|error| anyhow::anyhow!("hosts row: {error}"))?;
    for runner in &app.cfg.runners {
        // The configuration validated this address at startup, so the
        // only reachable failure here would be a code change that let an
        // unvalidated entry through (MULTI-NODE 8.5).
        let underlay = runner
            .underlay_address()
            .map_err(|error| anyhow::anyhow!("runner {:?}: {error}", runner.name))?;
        app.store
            .register_runner(&runner.name, &runner.endpoint, underlay.to_string())
            .await
            .with_context(|| format!("register runner {:?}", runner.name))?;
    }

    // The frontend public key rides in every seed so the frontend can reach
    // guests (SPEC 10 step 9). Creating it here keeps serve and sshd aligned.
    let frontend_key = ensure_key(&key_path(app, FRONTEND_KEY_FILE), "bento-frontend")?;
    let frontend_public = authorized_key_line(frontend_key.public_key(), "bento-frontend")?;
    // Only this process holds the controller lease, so only this process
    // may send a change to another machine (MULTI-NODE 11.3). A create
    // that placement sends elsewhere goes through here.
    let instances = crate::runners::InstanceSync::new(app.store.clone(), lease.clone());
    let manager = app.manager_with_runners(hypervisor.clone(), host.id, Some(instances.clone()))?;
    sync_image_allowlist(app).await?;

    // The controller fetches images to its own image directory, so the
    // versions it knows about are files on this machine. Recording them
    // is what lets the fleet gate pass on a deployment that has only ever
    // had one host (MULTI-NODE 13.2).
    let recorded = app
        .store
        .record_local_image_versions(host.id)
        .await
        .map_err(|error| anyhow::anyhow!("record local image versions: {error}"))?;
    if recorded > 0 {
        tracing::info!(
            host = %host.name,
            versions = recorded,
            "recorded the image versions this machine holds"
        );
    }

    // Per-user networks and one whole-table nftables reload (SPEC 6.2, 6.3).
    let firewall = Arc::new(Firewall::new(
        app.store.clone(),
        app.plan,
        Arc::new(NftApplier::default()),
        PortRange {
            from: i32::from(app.cfg.listen.proxy_port_min),
            to: i32::from(app.cfg.listen.proxy_port_max),
        },
        host.id,
    ));
    ensure_user_networks(app, hypervisor.as_ref()).await?;
    firewall
        .reload()
        .await
        .map_err(|error| anyhow::anyhow!("nftables: {error}"))?;

    // The sampler holds the series the dashboard charts read (SPEC
    // 14.4). The router answers from it, and the task below fills it.
    let sampler = Arc::new(crate::metrics::Sampler::new());
    let router = control_plane_router(
        app,
        manager.clone(),
        hypervisor.clone(),
        firewall.clone(),
        frontend_public,
        host.id,
        sampler.clone(),
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
    let sampling = tokio::spawn({
        let task = crate::metrics::SamplerTask {
            sampler: sampler.clone(),
            store: app.store.clone(),
            manager: manager.clone(),
            domains: hypervisor.clone(),
            storage_dir: app.cfg.storage_dir.clone(),
            local_host_id: host.id,
            // Every other machine is asked over the runner protocol. A
            // guest on another machine is not in this machine's libvirt,
            // so without this its charts would stay empty for ever
            // (MULTI-NODE 20).
            remote: Some(Arc::new(crate::runners::RunnerSampler::new(
                instances.clone(),
            ))),
        };
        let cancellation = cancellation.clone();
        async move {
            let mut ticker = tokio::time::interval(crate::metrics::INTERVAL);
            // A tick that arrives late must not start a burst of catch-up
            // readings: every one of them would measure the same instant.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    _ = ticker.tick() => task.tick().await,
                }
            }
        }
    });
    let runner_poll = tokio::spawn({
        let network_sync = crate::runners::NetworkSync::new(
            app.store.clone(),
            lease.clone(),
            app.plan,
            PortRange {
                from: i32::from(app.cfg.listen.proxy_port_min),
                to: i32::from(app.cfg.listen.proxy_port_max),
            },
            host.id,
        );
        let task = crate::runners::PollTask::new(app.store.clone(), lease, network_sync, host.id);
        let cancellation = cancellation.clone();
        async move {
            task.run(cancellation.cancelled()).await;
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
            tokio::select! {
                _ = shutdown_signal() => shutdown.cancel(),
                _ = shutdown.cancelled() => {}
            }
        })
        .await;
    cancellation.cancel();
    restore.abort();
    let _ = restore.await;
    let _ = poller.await;
    let _ = convergence.await;
    let _ = sampling.await;
    let _ = runner_poll.await;
    drop(manager);
    drop(firewall);
    hypervisor.close().await?;
    Ok(serve_result?)
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

/// The wait before the second discovery attempt, and the ceiling the wait
/// doubles up to.
const OIDC_RETRY_INITIAL: Duration = Duration::from_secs(2);
const OIDC_RETRY_MAX: Duration = Duration::from_secs(60);

/// Discovers the OIDC provider in the background, retrying until it answers.
///
/// Discovery is a network call to a service this unit cannot order itself
/// against: the provider is typically a rootless user unit, and a root system
/// unit has no way to wait for one. A single attempt at startup therefore
/// loses a race after every reboot, and because the provider is wired exactly
/// once, losing it left login answering 500 for the life of the process —
/// visible only as one warning in the journal. Retrying costs an idle task
/// and removes the whole failure mode.
fn spawn_oidc_discovery(
    auth: Arc<bento_auth::Service>,
    issuer: String,
    client_id: String,
    client_secret: String,
    redirect: String,
) {
    tokio::spawn(async move {
        let mut backoff = OIDC_RETRY_INITIAL;
        let mut attempt = 1_u32;
        loop {
            match bento_auth::ProviderClient::discover(
                &issuer,
                &client_id,
                &client_secret,
                &redirect,
            )
            .await
            {
                Ok(provider) => {
                    let provider = Arc::new(provider);
                    auth.install_oidc(provider.clone(), provider);
                    tracing::info!(%issuer, attempt, "OIDC discovered; dashboard login is up");
                    return;
                }
                Err(error) => {
                    // Kept at warn even once it is clearly a standing
                    // misconfiguration: login is down the whole time, and the
                    // capped backoff means this is at most one line a minute.
                    tracing::warn!(
                        %issuer,
                        %error,
                        attempt,
                        retry_in = ?backoff,
                        "OIDC discovery failed; dashboard login is disabled until it succeeds"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(OIDC_RETRY_MAX);
                    attempt += 1;
                }
            }
        }
    });
}

async fn control_plane_router(
    app: &App,
    manager: Arc<bento_lifecycle::Manager>,
    hypervisor: Arc<bento_hypervisor::Client>,
    firewall: Arc<Firewall>,
    frontend_key: String,
    host_id: i64,
    sampler: Arc<crate::metrics::Sampler>,
) -> Result<Router> {
    let mut auth = bento_auth::Service::new(
        &app.cfg.base_domain,
        Arc::new(AuthUsers(app.store.clone())),
        Arc::new(AuthAccess(app.store.clone())),
        Arc::new(AuthTokens(app.store.clone())),
    )
    .with_pairings(Arc::new(AuthPairings(app.store.clone())))
    .with_provider_name(provider_name(&app.cfg.oidc.issuer));
    if app.cfg.oidc.allow_signup {
        // Wiring the provisioner is what opens signups: without it a login
        // for an unknown identity is refused (SPEC 13).
        auth = auth.with_provisioner(Arc::new(AccountProvisioner {
            store: app.store.clone(),
            plan: app.plan,
            networks: Some(hypervisor.clone()),
            firewall: Some(firewall.clone()),
        }));
    } else {
        tracing::warn!("signups are off; only existing accounts can log in");
    }
    if app.cfg.oidc.issuer.is_empty() {
        tracing::warn!("no OIDC issuer configured; dashboard login disabled");
    }
    let auth = Arc::new(auth);
    if !app.cfg.oidc.issuer.is_empty() {
        spawn_oidc_discovery(
            auth.clone(),
            app.cfg.oidc.issuer.clone(),
            app.cfg.oidc.client_id.clone(),
            app.cfg.oidc.client_secret.clone(),
            format!("https://{}/callback", app.cfg.base_domain),
        );
    }
    let operators = Arc::new(operator_predicate(&app.cfg.operators));
    let http = Arc::new(bento_api::Config {
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
        image_admin: Some(Arc::new(RuntimeImages(app.image_store()))),
        db_path: app.cfg.db_path.clone(),
        metrics: sampler,
        base_domain: app.cfg.base_domain.clone(),
        defaults: bento_api::CreateDefaults {
            vcpu: app.cfg.defaults.vcpu,
            memory_mib: app.cfg.defaults.memory_mib,
            disk_gib: app.cfg.defaults.disk_gib,
        },
    });
    let api = bento_api::router(http.clone());
    let pages = bento_api::pages(http);

    let access_auth = auth.clone();
    let login_auth = auth.clone();
    let callback_auth = auth.clone();
    let link_page_auth = auth.clone();
    let link_confirm_auth = auth.clone();
    let dashboard_auth = auth.clone();
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
        // The SSH frontend's link. GET renders the fingerprint and a form;
        // only the POST attaches the key, so nothing that merely follows a
        // link can link one (SPEC 13).
        .route(
            "/link/{token}",
            get(
                move |AxumPath(token): AxumPath<String>, headers: HeaderMap| {
                    let auth = link_page_auth.clone();
                    async move { auth_response(auth.link_page_response(&headers, &token).await) }
                },
            )
            .post(
                move |AxumPath(token): AxumPath<String>, headers: HeaderMap| {
                    let auth = link_confirm_auth.clone();
                    async move { auth_response(auth.link_confirm_response(&headers, &token).await) }
                },
            ),
        )
        .route(
            "/logout",
            post(move |headers: HeaderMap| {
                let auth = logout_auth.clone();
                async move { auth_response(auth.logout_response(&headers).await) }
            }),
        )
        // The dashboard pages assume a session and have no sign-in of
        // their own, so a signed-out visitor is answered by the gate
        // instead (SPEC 13, 14). Assets are served to anyone. An HTMX
        // fragment request whose session ended gets a redirect header
        // rather than the splash page swapped into a corner of the page.
        .merge(
            pages
                .merge(bento_dashboard::router())
                .layer(axum::middleware::from_fn(
                    move |request: axum::extract::Request, next: axum::middleware::Next| {
                        let auth = dashboard_auth.clone();
                        async move {
                            if bento_auth::is_dashboard_asset(request.uri().path()) {
                                return next.run(request).await;
                            }
                            match auth.dashboard_gate(request.headers()).await {
                                Some(response) if request.headers().contains_key("hx-request") => {
                                    let _ = response;
                                    Response::builder()
                                        .status(http::StatusCode::UNAUTHORIZED)
                                        .header("hx-redirect", "/")
                                        .body(Body::empty())
                                        .expect("static response builds")
                                }
                                Some(response) => auth_response(response),
                                None => next.run(request).await,
                            }
                        }
                    },
                )),
        ))
}

/// The host of the OIDC issuer, for the sign-in button. An issuer that is
/// not a URL is shown as written; an empty one yields no name.
fn provider_name(issuer: &str) -> String {
    issuer
        .parse::<url::Url>()
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| issuer.trim().to_string())
}

fn auth_response(response: http::Response<String>) -> Response<Body> {
    response.map(Body::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn startup_refuses_a_lease_held_by_another_process() {
        let directory = tempfile::tempdir().unwrap();
        let store = bento_store::Store::open(directory.path().join("bento.db"))
            .await
            .unwrap();
        let held = store
            .acquire_lease("controller-a", CONTROLLER_LEASE_TTL)
            .await
            .unwrap();

        let error = acquire_controller_lease(&store, "controller-b".into())
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("control plane cannot start"), "{error}");
        assert!(error.contains("controller-a"), "{error}");
        assert!(error.contains(&held.expires_at.to_string()), "{error}");
    }

    #[test]
    fn status_code_type_is_the_http_one() {
        assert_eq!(http::StatusCode::NO_CONTENT.as_u16(), 204);
    }
}
