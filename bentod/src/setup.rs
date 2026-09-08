//! Shared bootstrap for the subcommands: configuration, logging, the store,
//! the libvirt connection, and the lifecycle manager.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use bento_config::Config;
use bento_hypervisor::{CheckConfig, Client};
use bento_images::Store as ImageStore;
use bento_lifecycle::Manager;
use bento_network::Plan;
use bento_store::Store;
use bento_types::Capacity;
use url::Url;

use crate::adapters::{ImageDb, LifecycleStore};

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
    pub(crate) fn manager(&self, hypervisor: Arc<Client>, host_id: i64) -> Result<Arc<Manager>> {
        self.manager_with_runners(hypervisor, host_id, None)
    }

    /// Builds the lifecycle manager, and gives it a way to send work to
    /// another machine when the deployment has one (MULTI-NODE 13.1).
    ///
    /// Only `serve` holds the controller lease, so only `serve` may send
    /// a change to another machine. Every other subcommand builds a
    /// manager that can act on this machine alone, and a create that
    /// placement sent elsewhere fails there with a sentence saying so.
    pub(crate) fn manager_with_runners(
        &self,
        hypervisor: Arc<Client>,
        host_id: i64,
        runners: Option<crate::runners::InstanceSync>,
    ) -> Result<Arc<Manager>> {
        let dns = self.dns_addrs()?;
        let images = self.image_store();
        let capacity = host_capacity(&self.cfg)?;
        tracing::info!(
            memory_mib = capacity.memory_mib,
            disk_gib = capacity.disk_gib,
            overcommit_ratio = self.cfg.overcommit_ratio,
            "host capacity: the ceiling on create and resize (SPEC 6.1)"
        );
        Ok(Arc::new(Manager::new(bento_lifecycle::Config {
            hypervisor: Some(hypervisor.clone()),
            definer: Some(hypervisor.clone()),
            autostart_clearer: Some(hypervisor.clone()),
            store: Some(Arc::new(LifecycleStore(
                self.store.clone(),
                crate::adapters::LocalCapacity {
                    host_id,
                    capacity,
                    overcommit_ratio: self.cfg.overcommit_ratio,
                },
            ))),
            host_id,
            fleet: Some(Arc::new(crate::adapters::RunnerFleet {
                store: self.store.clone(),
                images,
                seeds: Arc::new(bento_cloudinit::Builder::default()),
                hypervisor: hypervisor.clone(),
                storage_dir: PathBuf::from(&self.cfg.storage_dir),
                host_id,
                runners,
            })),
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

/// Runs the host checks that must finish before a service starts a listener.
/// Fatal requirements stop startup. Other requirements only warn. The runner
/// uses this order to keep its management listener off an invalid host
/// (MULTI-NODE 11.1).
pub(crate) async fn host_checks(app: &App) -> Result<()> {
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
            container_storage: PathBuf::from(&app.cfg.bootc.container_storage),
            podman_required: app.cfg.images.iter().any(|image| !image.oci.is_empty()),
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

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * 1024 * 1024;

/// The ceiling that a create or a resize is checked against (SPEC 6.1).
///
/// Memory is the host's own total times the operator's overcommit ratio
/// (SPEC 5.3). Disk is the size of the volume that holds the overlays,
/// against virtual disk size, so it is a worst case bound rather than a
/// measurement of real use.
///
/// Both numbers are read once. Neither changes while `bentod` runs, and
/// a failure to read either stops startup rather than silently leaving
/// the host unbounded.
fn host_capacity(cfg: &Config) -> Result<Capacity> {
    let memory = bento_hostinfo::read_memory().context("read /proc/meminfo (SPEC 6.1)")?;
    if memory.total == 0 {
        bail!("/proc/meminfo carries no MemTotal, so the host memory ceiling is unknown");
    }
    let storage = bento_hostinfo::disk_usage(Path::new(&cfg.storage_dir))
        .with_context(|| format!("read the storage volume at {}", cfg.storage_dir))?;
    Ok(capacity_from(
        memory.total,
        storage.total,
        cfg.overcommit_ratio,
    ))
}

/// The arithmetic of [`host_capacity`], apart from the host it reads.
fn capacity_from(memory_bytes: u64, storage_bytes: u64, overcommit_ratio: f64) -> Capacity {
    Capacity::from_host(
        (memory_bytes / MIB) as i64,
        (storage_bytes / GIB) as i64,
        overcommit_ratio,
    )
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

    #[test]
    fn capacity_applies_the_overcommit_ratio_to_memory_only() {
        let memory = 64 * 1024 * MIB; // 64 GiB
        let storage = 900 * GIB;

        // The default ratio of 1.0 is the host as it is (SPEC 5.3).
        assert_eq!(
            capacity_from(memory, storage, 1.0),
            Capacity {
                memory_mib: 65_536,
                disk_gib: 900,
            }
        );

        // A higher ratio raises memory. Disk is never overcommitted: the
        // ceiling is the volume, because a full volume stops every guest
        // on the host at once.
        assert_eq!(
            capacity_from(memory, storage, 1.5),
            Capacity {
                memory_mib: 98_304,
                disk_gib: 900,
            }
        );
    }

    #[test]
    fn capacity_rounds_down_rather_than_promising_room() {
        // A volume smaller than one GiB offers no whole GiB, and a
        // ceiling of zero bounds nothing, so the host check would pass
        // everything. `host_capacity` is the only caller and it runs on
        // a real storage volume; this records the edge rather than
        // claiming it is reachable.
        let capacity = capacity_from(MIB * 3 / 2, GIB / 2, 1.0);
        assert_eq!(capacity.memory_mib, 1);
        assert_eq!(capacity.disk_gib, 0);
    }
}
