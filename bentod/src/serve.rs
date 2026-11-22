//! The control plane (SPEC 4): the only writer of the database, the policy
//! layer, and the dashboard. Startup follows SPEC 4.2 and 11.2: host checks,
//! libvirt, user networks, firewall, reboot restore, then HTTP.

use std::ffi::OsString;
use std::future::{Future, IntoFuture};
use std::ops::ControlFlow;
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
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::adapters::{
    AccountProvisioner, ApiBackend, ApiStore, AuthAccess, AuthInstanceNames, AuthPairings,
    AuthTokens, AuthUsers, Authenticator, Backend, Console, NetworkEnsurer, RuntimeImages, Starter,
    access_status, operator_predicate, user_network,
};
use crate::firewall::Firewall;
use crate::keys::{FRONTEND_KEY_FILE, authorized_key_line, ensure_key, key_path};
use crate::ops::sync_image_allowlist;
use crate::setup::{App, host_checks, shutdown_signal};

const CONTROLLER_LEASE_TTL: Duration = Duration::from_secs(30);

/// Runs `step` once per `period` until `shutdown` fires or `step` breaks.
/// The first run comes one period after the call. A late tick is delayed,
/// not repeated: a burst of catch-up runs would see the same instant.
async fn every<F, Fut>(period: Duration, shutdown: &CancellationToken, mut step: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ControlFlow<()>>,
{
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = ticker.tick() => {
                if step().await.is_break() {
                    return;
                }
            }
        }
    }
}

/// How long open connections may delay a shutdown. A web terminal
/// WebSocket does not end by itself, so a drain without a bound would keep
/// the process from the lease release (MULTI-NODE 11.3).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// How often expired dashboard sessions leave memory.
const SESSION_SWEEP: Duration = Duration::from_secs(300);

/// Removes expired sessions every `SESSION_SWEEP`. A session whose cookie
/// never comes back is not removed by lookup (SPEC 13).
async fn sweep_sessions(
    sessions: Arc<bento_auth::MemorySessionStore>,
    now: impl Fn() -> OffsetDateTime,
    shutdown: &CancellationToken,
) {
    every(SESSION_SWEEP, shutdown, || async {
        sessions.delete_expired(now()).await;
        ControlFlow::Continue(())
    })
    .await;
}

/// Serves until `cancel` fires, then lets open requests finish for `grace`
/// and stops waiting. Connections still open then end with the process.
async fn serve_until(
    listener: tokio::net::TcpListener,
    router: Router,
    cancel: CancellationToken,
    grace: Duration,
) -> std::io::Result<()> {
    let drain = cancel.clone();
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(async move { drain.cancelled().await })
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result,
        () = async {
            cancel.cancelled().await;
            tokio::time::sleep(grace).await;
        } => {
            tracing::warn!(?grace, "connections still open after the grace period; shutting down");
            Ok(())
        }
    }
}

/// Logs a task that panicked. A cancelled task is not an error.
fn log_join(name: &str, result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result
        && error.is_panic()
    {
        tracing::error!(task = name, %error, "background task panicked");
    }
}

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
    log_join("lease renewal", renewal.await);
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
        // The lease already has a full TTL, so the first renewal comes one
        // period after the start (MULTI-NODE 11.3).
        every(CONTROLLER_LEASE_TTL / 3, &cancellation, || async {
            match store.renew_lease(holder_id.clone(), CONTROLLER_LEASE_TTL).await {
                Ok(lease) => {
                    lease_sender.send_replace(lease);
                    ControlFlow::Continue(())
                }
                Err(bento_store::Error::LeaseLost) => {
                    tracing::error!(
                        "controller lease lost; this process has been replaced and must not dispatch"
                    );
                    cancellation.cancel();
                    ControlFlow::Break(())
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
                    ControlFlow::Break(())
                }
            }
        })
        .await;
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
    let firewall = Arc::new(Firewall::for_config(
        app.store.clone(),
        app.plan,
        &app.cfg,
        Arc::new(NftApplier::default()),
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
    let (router, sessions) = control_plane_router(
        app,
        manager.clone(),
        hypervisor.clone(),
        firewall.clone(),
        frontend_key,
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

    let mut tasks = tokio::task::JoinSet::new();
    let restore = tokio::spawn({
        let manager = manager.clone();
        async move {
            if let Err(error) = manager.restore().await {
                tracing::error!(%error, "restore failed");
            }
        }
    });
    tasks.spawn({
        let manager = manager.clone();
        let cancellation = cancellation.clone();
        async move {
            if let Err(error) = manager.run_poller(cancellation.cancelled()).await {
                tracing::error!(%error, "poller stopped");
            }
        }
    });
    tasks.spawn({
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
            // The first reading comes at once, the others one period apart.
            task.tick().await;
            every(crate::metrics::INTERVAL, &cancellation, || async {
                task.tick().await;
                ControlFlow::Continue(())
            })
            .await;
        }
    });
    tasks.spawn({
        let network_sync = crate::runners::NetworkSync::new(
            app.store.clone(),
            lease.clone(),
            app.plan,
            PortRange {
                from: app.cfg.listen.proxy_port_min,
                to: app.cfg.listen.proxy_port_max,
            },
            host.id,
        );
        let task = crate::runners::PollTask::new(
            app.store.clone(),
            lease,
            network_sync,
            host.id,
            Arc::new(crate::adapters::RunnerRestore(manager.clone())),
        );
        let cancellation = cancellation.clone();
        async move {
            task.run(cancellation.cancelled()).await;
        }
    });
    tasks.spawn({
        let store = app.store.clone();
        let plan = app.plan;
        let hypervisor = hypervisor.clone();
        let firewall = firewall.clone();
        let cancellation = cancellation.clone();
        async move {
            converge(store, plan, hypervisor, firewall, cancellation).await;
        }
    });

    tasks.spawn({
        let cancellation = cancellation.clone();
        async move {
            sweep_sessions(sessions, OffsetDateTime::now_utc, &cancellation).await;
        }
    });

    let signal = tokio::spawn({
        let shutdown = cancellation.clone();
        async move {
            tokio::select! {
                _ = shutdown_signal() => shutdown.cancel(),
                _ = shutdown.cancelled() => {}
            }
        }
    });
    let serve_result = serve_until(listener, router, cancellation.clone(), SHUTDOWN_GRACE).await;
    signal.abort();
    cancellation.cancel();
    restore.abort();
    log_join("restore", restore.await);
    // Every task ends on cancellation. A panic is logged, not dropped.
    while let Some(result) = tasks.join_next().await {
        log_join("background", result);
    }
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
    every(Duration::from_secs(30), &cancellation, || async {
        for user in store.users().await.unwrap_or_default() {
            let result = async {
                let network = user_network(plan, &user.subnet)?;
                NetworkManager::ensure_network(networks.as_ref(), &network.name, &network.xml()?)
                    .await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(user = %user.name, %error, "network convergence");
            }
        }
        if let Err(error) = firewall.reload().await {
            tracing::warn!(%error, "firewall convergence");
        }
        ControlFlow::Continue(())
    })
    .await;
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
    frontend_key: Arc<russh::keys::PrivateKey>,
    host_id: i64,
    sampler: Arc<crate::metrics::Sampler>,
    // The caller sweeps the session store: lookup removes an expired
    // session only when its cookie comes back (SPEC 13).
) -> Result<(Router, Arc<bento_auth::MemorySessionStore>)> {
    let sessions = Arc::new(bento_auth::MemorySessionStore::new());
    let mut auth = bento_auth::Service::new(
        &app.cfg.base_domain,
        Arc::new(AuthUsers(app.store.clone())),
        Arc::new(AuthAccess(app.store.clone())),
        Arc::new(AuthTokens(app.store.clone())),
    )
    .with_session_store(sessions.clone())
    .with_pairings(Arc::new(AuthPairings(app.store.clone())))
    .with_instance_domain(&app.cfg.instance_domain)
    .with_instance_names(Arc::new(AuthInstanceNames(app.store.clone())))
    .with_provider_name(provider_name(
        app.cfg.oidc.issuer.as_deref().unwrap_or_default(),
    ));
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
    let auth = Arc::new(auth);
    if let Some(issuer) = &app.cfg.oidc.issuer {
        spawn_oidc_discovery(
            auth.clone(),
            issuer.clone(),
            app.cfg.oidc.client_id.clone(),
            app.cfg.oidc.client_secret.clone(),
            format!("https://{}/callback", app.cfg.base_domain),
        );
    } else {
        tracing::warn!("no OIDC issuer configured; dashboard login disabled");
    }
    let frontend_public = authorized_key_line(frontend_key.public_key(), "bento-frontend")?;
    // This process already holds the frontend key, so the web terminal
    // needs no new secret (SPEC 14.6).
    let mut guests = bento_sshfront::Server::for_guests(
        Arc::new(app.store.clone()),
        Arc::new(Starter(manager.clone())),
        frontend_key,
    );
    guests.guest_user = bento_lifecycle::GUEST_USER.to_owned();
    let operators = Arc::new(operator_predicate(&app.cfg.operators));
    let http = Arc::new(bento_api::Config {
        store: Arc::new(ApiStore(app.store.clone())),
        lifecycle: Arc::new(ApiBackend(Backend {
            manager,
            store: app.store.clone(),
            host_id,
            frontend_key: frontend_public,
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
        console: Arc::new(Console(guests)),
        base_domain: app.cfg.base_domain.clone(),
        instance_domain: app.cfg.instance_domain.clone(),
        reserved_names: app.cfg.reserved(),
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
    let router = Router::new()
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
        );
    // One guard over every route, so a route added later is covered
    // (SPEC 13). It wraps the merged router, not each part.
    let router = bento_api::guard_origin(router, &app.cfg.base_domain);
    Ok((router, sessions))
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

    #[tokio::test(start_paused = true)]
    async fn every_runs_each_period_and_stops_on_shutdown() {
        let shutdown = CancellationToken::new();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let task = tokio::spawn({
            let shutdown = shutdown.clone();
            let count = count.clone();
            async move {
                every(Duration::from_secs(10), &shutdown, || async {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    ControlFlow::Continue(())
                })
                .await;
            }
        });
        tokio::time::sleep(Duration::from_secs(35)).await;
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 3);
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn every_stops_when_the_step_breaks() {
        let shutdown = CancellationToken::new();
        let mut runs = 0;
        every(Duration::from_secs(1), &shutdown, || {
            runs += 1;
            let flow = if runs == 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            };
            async move { flow }
        })
        .await;
        assert_eq!(runs, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn the_sweep_removes_a_session_that_never_returns() {
        use bento_auth::{Session, SessionStore};
        let epoch = OffsetDateTime::UNIX_EPOCH;
        let sessions = Arc::new(bento_auth::MemorySessionStore::new());
        for (id, expires_at) in [("dead", epoch), ("live", epoch + time::Duration::DAY)] {
            sessions
                .put(Session {
                    id: id.into(),
                    user_id: 1,
                    created_at: epoch,
                    expires_at,
                })
                .await
                .unwrap();
        }
        let shutdown = CancellationToken::new();
        // The clock stands one hour after the epoch.
        let task = tokio::spawn({
            let sessions = sessions.clone();
            let shutdown = shutdown.clone();
            async move {
                sweep_sessions(sessions, || epoch + time::Duration::HOUR, &shutdown).await;
            }
        });
        tokio::time::sleep(SESSION_SWEEP + Duration::from_secs(1)).await;
        assert!(sessions.get("dead").await.is_none());
        assert!(sessions.get("live").await.is_some());
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_does_not_wait_for_a_connection_that_never_ends() {
        use tokio::io::AsyncWriteExt;
        let entered = Arc::new(tokio::sync::Notify::new());
        let router = Router::new().route(
            "/hang",
            get({
                let entered = entered.clone();
                move || {
                    let entered = entered.clone();
                    async move {
                        entered.notify_one();
                        std::future::pending::<&'static str>().await
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let server = tokio::spawn(serve_until(
            listener,
            router,
            cancel.clone(),
            Duration::from_secs(10),
        ));
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET /hang HTTP/1.1\r\nHost: example.org\r\n\r\n")
            .await
            .unwrap();
        entered.notified().await;
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(60), server)
            .await
            .expect("serve_until returns after the grace period");
        result.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_panicking_task_surfaces_as_a_join_error() {
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async { panic!("boom") });
        let error = tasks.join_next().await.unwrap().unwrap_err();
        assert!(error.is_panic());
        // log_join must accept it without panicking itself.
        log_join("test", Err(error));
    }

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
