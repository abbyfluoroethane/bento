//! The SSH frontend (SPEC 4, 10, 15): public-key authentication, instance
//! forwarding, key linking, and the command-line interface.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use bento_network::{NftApplier, PortRange};

use crate::adapters::{Backend, CliBackend, CliRunner, Linker, Starter};
use crate::firewall::Firewall;
use crate::keys::{FRONTEND_KEY_FILE, HOST_KEY_FILE, authorized_key_line, ensure_key, key_path};
use crate::setup::{App, default_image, shutdown_signal};

pub(crate) async fn run_sshd(config: &Path, _args: &[OsString]) -> Result<()> {
    let app = App::new(config).await?;
    let result = sshd_inner(&app).await;
    app.close().await;
    result
}

async fn sshd_inner(app: &App) -> Result<()> {
    let hypervisor = app.connect_libvirt().await?;
    let manager = app.manager(hypervisor.clone())?;

    // One host key for every connection means rename and name reuse do not
    // produce known_hosts warnings (SPEC 10).
    let host_key = ensure_key(&key_path(app, HOST_KEY_FILE), "bento-host")?;
    let frontend_key = ensure_key(&key_path(app, FRONTEND_KEY_FILE), "bento-frontend")?;
    let frontend_public = authorized_key_line(frontend_key.public_key(), "bento-frontend")?;
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_else(|_| "localhost".to_owned())
        .trim()
        .to_owned();
    let host = app
        .store
        .ensure_host(hostname, &app.cfg.libvirt_uri)
        .await
        .map_err(|error| anyhow::anyhow!("hosts row: {error}"))?;

    // CLI mutations reload the whole table immediately (SPEC 6.3), rather
    // than waiting for the control-plane convergence tick.
    let firewall = Arc::new(Firewall::new(
        app.store.clone(),
        app.plan,
        Arc::new(NftApplier::default()),
        PortRange { from: 0, to: 0 },
    ));
    let lifecycle = Arc::new(CliBackend(Backend {
        manager: manager.clone(),
        store: app.store.clone(),
        host_id: host.id,
        frontend_key: frontend_public,
        firewall: Some(firewall.clone()),
    }));
    let cli = Arc::new(bento_cli::Cli::new(
        Arc::new(app.store.clone()),
        lifecycle,
        bento_cli::Options {
            domain: app.cfg.base_domain.clone(),
            default_image: default_image(&app.cfg),
            default_vcpu: app.cfg.defaults.vcpu,
            default_memory_mib: app.cfg.defaults.memory_mib,
            default_disk_gib: app.cfg.defaults.disk_gib,
            name_cooldown: app.cfg.cooldown(),
            ..Default::default()
        },
    ));
    let mut server = bento_sshfront::Server::new(
        Arc::new(app.store.clone()),
        Arc::new(app.store.clone()),
        Arc::new(Starter(hypervisor.clone())),
        Arc::new(CliRunner(cli)),
        host_key,
        frontend_key,
    );
    // An unknown key gets a link to sign in with, not an account: nothing
    // here writes a users row, a subnet, or a network (SPEC 13).
    server.linker = Some(Arc::new(Linker {
        store: app.store.clone(),
        base_domain: app.cfg.base_domain.clone(),
    }));
    server.guest_user = bento_lifecycle::GUEST_USER.to_owned();

    let address =
        bento_config::resolve_listen_addr(&app.cfg.listen.ssh).map_err(anyhow::Error::msg)?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(
        addr = %app.cfg.listen.ssh,
        domain = %app.cfg.base_domain,
        "ssh frontend listening"
    );
    tokio::select! {
        result = server.serve(listener) => result?,
        () = shutdown_signal() => {}
    }
    drop(server);
    drop(manager);
    drop(firewall);
    hypervisor.close().await?;
    Ok(())
}
