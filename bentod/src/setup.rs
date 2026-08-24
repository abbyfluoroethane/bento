//! Shared bootstrap for the subcommands: configuration, logging, the store,
//! the libvirt connection, and the lifecycle manager.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use bento_config::Config;
use bento_hypervisor::Client;
use bento_images::Store as ImageStore;
use bento_lifecycle::Manager;
use bento_network::Plan;
use bento_store::Store;
use url::Url;

use crate::adapters::{ImageDb, LifecycleImages, LifecycleStore};

/// Bundles what every subcommand needs.
pub(crate) struct App {
    pub(crate) cfg: Config,
    pub(crate) plan: Plan,
    pub(crate) store: Store,
}

impl App {
    /// Loads configuration and opens the database. SPEC 12.1 wants its one
    /// documented path printed at control-plane startup.
    pub(crate) async fn new(config_path: &Path) -> Result<Self> {
        let cfg = Config::load(config_path)?;
        let plan = Plan::new(&cfg.private_range)?;
        let store = Store::open(&cfg.db_path)
            .await
            .with_context(|| format!("open database {}", cfg.db_path))?;
        tracing::info!(
            path = %cfg.db_path,
            note = "back it up with `bentod dump-db`, never with a file copy (SPEC 12.1)",
            "database open"
        );
        Ok(Self { cfg, plan, store })
    }

    pub(crate) async fn close(self) {
        if let Err(error) = self.store.close().await {
            tracing::warn!(%error, "closing database");
        }
    }

    /// Dials libvirtd over the local socket named by the configured URI
    /// (SPEC 4.1).
    pub(crate) async fn connect_libvirt(&self) -> Result<Arc<Client>> {
        Ok(Arc::new(
            Client::connect(socket_path(&self.cfg.libvirt_uri)).await?,
        ))
    }

    /// Returns the content-addressed image store over the database (SPEC 5.1).
    pub(crate) fn image_store(&self) -> Arc<ImageStore> {
        Arc::new(
            ImageStore::new(&self.cfg.image_dir, ImageDb(self.store.clone()))
                .with_builder_image(&self.cfg.bootc.builder_image)
                .with_bootc_rootfs(&self.cfg.bootc.rootfs)
                .with_container_storage(&self.cfg.bootc.container_storage)
                .with_build_timeout(self.cfg.bootc.build_timeout.std()),
        )
    }

    /// Builds the lifecycle manager over the given hypervisor connection.
    pub(crate) fn manager(&self, hypervisor: Arc<Client>) -> Result<Arc<Manager>> {
        let dns = self.dns_addrs()?;
        let images = self.image_store();
        Ok(Arc::new(Manager::new(bento_lifecycle::Config {
            hypervisor: Some(hypervisor.clone()),
            definer: Some(hypervisor.clone()),
            autostart_clearer: Some(hypervisor),
            store: Some(Arc::new(LifecycleStore(self.store.clone()))),
            images: Some(Arc::new(LifecycleImages(images))),
            iso: Some(Arc::new(bento_cloudinit::Builder::default())),
            plan: Some(self.plan),
            storage_dir: PathBuf::from(&self.cfg.storage_dir),
            name_cooldown: self.cfg.cooldown(),
            batch_size: self.cfg.restore_batch_size as usize,
            dns,
            ..Default::default()
        })?))
    }

    fn dns_addrs(&self) -> Result<Vec<IpAddr>> {
        self.cfg
            .dns
            .iter()
            .map(|value| {
                value
                    .parse::<IpAddr>()
                    .with_context(|| format!("config dns {value:?} is not an IP address"))
            })
            .collect()
    }
}

/// Extracts the Unix socket from a `qemu:///system` style URI. The default
/// URI and an empty string select the default socket; `?socket=` overrides it.
pub(crate) fn socket_path(uri: &str) -> PathBuf {
    if uri.is_empty() {
        return PathBuf::new();
    }
    Url::parse(uri)
        .ok()
        .and_then(|url| {
            url.query_pairs()
                .find(|(key, value)| key == "socket" && !value.is_empty())
                .map(|(_, value)| PathBuf::from(value.into_owned()))
        })
        .unwrap_or_default()
}

pub(crate) fn default_image(cfg: &Config) -> String {
    cfg.default_image().unwrap_or_default().to_owned()
}

/// Turns the control-plane listen address into a URL the proxy can dial: an
/// unspecified host becomes loopback.
pub(crate) fn control_url(listen: &str) -> String {
    if let Some(port) = listen.strip_prefix(':') {
        return format!("http://127.0.0.1:{port}");
    }
    match listen.parse::<SocketAddr>() {
        Ok(address) if address.ip().is_unspecified() => {
            format!("http://127.0.0.1:{}", address.port())
        }
        _ => format!("http://{listen}"),
    }
}

/// Extracts the host half of a listen address for the proxy's port fan-out.
pub(crate) fn bind_host(listen: &str) -> String {
    if let Some(port) = listen.strip_prefix(':')
        && port.parse::<u16>().is_ok()
    {
        return String::new();
    }
    listen
        .parse::<SocketAddr>()
        .map(|address| address.ip().to_string())
        .unwrap_or_else(|_| listen.trim_start_matches(':').to_owned())
}

/// Extracts the proxy's main port. An unusable address leaves SPEC 9's
/// default of 443 in place. The port moves when another TLS terminator fronts
/// Bento and forwards to it privately.
pub(crate) fn main_port(listen: &str) -> u16 {
    if let Some(port) = listen.strip_prefix(':') {
        return port.parse().unwrap_or(0);
    }
    listen
        .parse::<SocketAddr>()
        .map_or(0, |address| address.port())
}

pub(crate) async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut interrupt = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_parsing() {
        assert_eq!(socket_path("qemu:///system"), PathBuf::new());
        assert_eq!(
            socket_path("qemu:///system?socket=/run/libvirt/virtqemud-sock"),
            PathBuf::from("/run/libvirt/virtqemud-sock")
        );
        assert_eq!(control_url("127.0.0.1:8080"), "http://127.0.0.1:8080");
        assert_eq!(control_url(":8080"), "http://127.0.0.1:8080");
        assert_eq!(bind_host(":443"), "");
        assert_eq!(bind_host("192.0.2.1:443"), "192.0.2.1");
    }
}
