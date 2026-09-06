//! The runner service for this machine (MULTI-NODE 10.2, 11).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use bento_hypervisor::{Client, DomainSampler, Hypervisor};
use bento_runner::{
    Capabilities, CpuTimeSample, Domain, DomainUsage, Fence, Health, Host, HostError, HostSample,
    ImageRequest, InstanceRef, Inventory, PROTOCOL_VERSION, Reply, Samples, SqliteFence,
};
use bento_types::State;

use crate::setup::{App, host_checks, shutdown_signal};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * 1024 * 1024;

/// The host operations used by the runner protocol.
struct LocalHost {
    hypervisor: Arc<Client>,
    fence: Arc<Fence>,
    machine_id: String,
    hostname: String,
    storage_dir: PathBuf,
    image_dir: PathBuf,
    /// Applies the whole nftables table in one transaction (SPEC 6.3).
    firewall: Arc<dyn bento_network::Applier>,
    /// Applies guest routes and the proxy ARP flag (MULTI-NODE 8.2, 8.3).
    routes: Arc<dyn bento_network::RouteApplier>,
    /// Makes overlays from the image versions this machine holds. It has
    /// no allowlist: a runner knows its own machine, not the deployment
    /// (MULTI-NODE 11.2).
    overlays: Arc<bento_images::Store>,
    /// Writes the cloud-init seed image (SPEC 5.2).
    seeds: Arc<bento_cloudinit::Builder>,
}

impl LocalHost {
    fn overlay_path(&self, uuid: &str) -> PathBuf {
        self.storage_dir.join(format!("{uuid}.qcow2"))
    }

    /// The seed image, which exists only until the first boot has read it
    /// (SPEC 5.2).
    fn seed_iso_path(&self, uuid: &str) -> PathBuf {
        self.storage_dir.join(format!("{uuid}-seed.iso"))
    }
}

#[async_trait::async_trait]
impl Host for LocalHost {
    async fn health(&self) -> Result<Health, HostError> {
        let accepted_epoch = self.fence.accepted_epoch().map_err(HostError::new)?;
        Ok(Health {
            protocol_version: PROTOCOL_VERSION,
            machine_id: self.machine_id.clone(),
            hostname: self.hostname.clone(),
            accepted_epoch,
        })
    }

    async fn capabilities(&self) -> Result<Capabilities, HostError> {
        let memory = bento_hostinfo::read_memory()
            .map_err(|error| HostError::new(format!("read host memory: {error}")))?;
        let storage = bento_hostinfo::disk_usage(&self.storage_dir).map_err(|error| {
            HostError::new(format!(
                "read storage volume at {}: {error}",
                self.storage_dir.display()
            ))
        })?;
        Ok(Capabilities {
            arch: std::env::consts::ARCH.to_owned(),
            cpu_count: i64::try_from(bento_hostinfo::cpu_count())
                .map_err(|error| HostError::new(format!("read processor count: {error}")))?,
            memory_total_mib: byte_units(memory.total, MIB, "memory")?,
            storage_total_gib: byte_units(storage.total, GIB, "storage total")?,
            storage_available_gib: byte_units(storage.available, GIB, "storage available")?,
            // The libvirt client does not expose a version call. This text
            // states which existing client path the runner uses.
            hypervisor_version: "libvirt local RPC".to_owned(),
        })
    }

    async fn inventory(&self) -> Result<Inventory, HostError> {
        let domains = self
            .hypervisor
            .list()
            .await
            .map_err(HostError::new)?
            .into_iter()
            .map(|domain| Domain {
                uuid: domain.uuid,
                name: domain.name,
                state: domain.state,
            })
            .collect();
        Ok(Inventory { domains })
    }

    async fn sample(&self) -> Result<Samples, HostError> {
        // A machine that cannot read one figure still reports the rest.
        // A chart with a gap is worth more than no chart at all, and the
        // controller already treats a missing processor reading as "no
        // point yet" (SPEC 14.4).
        let memory = bento_hostinfo::read_memory().unwrap_or_default();
        let storage = bento_hostinfo::disk_usage(&self.storage_dir).unwrap_or_default();
        let host = HostSample {
            cpu: bento_hostinfo::read_cpu()
                .ok()
                .flatten()
                .map(|cpu| CpuTimeSample {
                    total: cpu.total,
                    idle: cpu.idle,
                }),
            memory_total_bytes: memory.total,
            memory_available_bytes: memory.available,
            storage_total_bytes: storage.total,
            storage_available_bytes: storage.available,
            cpu_count: i64::try_from(bento_hostinfo::cpu_count()).unwrap_or(0),
        };

        let mut domains = Vec::new();
        for domain in self.hypervisor.list().await.map_err(HostError::new)? {
            // A domain that is not running has no sample, and one
            // libvirt failure must not cost the reading of the rest.
            let sample = match self.hypervisor.sample(&domain.name).await {
                Ok(Some(sample)) => sample,
                Ok(None) => continue,
                Err(error) => {
                    tracing::debug!(name = %domain.name, %error, "sample: reading a domain");
                    continue;
                }
            };
            domains.push(DomainUsage {
                uuid: domain.uuid.clone(),
                cpu_time_ns: sample.cpu_time_ns,
                vcpus: sample.vcpus,
                rss_kib: sample.rss_kib,
                storage_used_bytes: overlay_bytes(&self.overlay_path(&domain.uuid)),
            });
        }
        Ok(Samples { host, domains })
    }

    async fn ensure_image(&self, image: &ImageRequest) -> Result<Reply, HostError> {
        // A version this machine already holds is kept. Its overlays are
        // backed by it, so it may not be replaced by a newer build
        // (SPEC 5.1).
        if let Some(have) = &image.have {
            let hex = sha256_hex(have)?;
            let path = self.image_dir.join(format!("sha256-{hex}.qcow2"));
            // The name is a content address, and this runner only writes
            // that name after checking the content, so its presence is
            // the whole check.
            if let Ok(metadata) = tokio::fs::metadata(&path).await
                && metadata.is_file()
            {
                return Ok(Reply::ImageReady {
                    name: image.name.clone(),
                    checksum: format!("sha256-{hex}"),
                    already_present: true,
                    size: metadata.len() as i64,
                });
            }
        }

        // Otherwise fetch what the URL serves now. The runner names the
        // result by what it hashed to, not by what anybody expected, so
        // a newer build lands beside the older one rather than over it.
        let (checksum, size) = fetch_into(&image.url, &self.image_dir).await?;
        Ok(Reply::ImageReady {
            name: image.name.clone(),
            checksum,
            already_present: false,
            size,
        })
    }

    async fn apply_network(
        &self,
        network: &bento_network::MachineNetwork,
    ) -> Result<Reply, HostError> {
        // Checked before anything is applied. A wrong next hop or a slot
        // owned twice produces a black hole that looks exactly like a
        // healthy machine, so it must not reach the kernel
        // (MULTI-NODE 8.3).
        network.check().map_err(HostError::new)?;

        // The order matters and is the order of MULTI-NODE 8.3: the
        // bridge exists, then the route to a remote slot exists, and only
        // then does proxy ARP answer for an address in that slot. A proxy
        // ARP answer without a usable route is a convincing black hole:
        // the guest gets a MAC and sends the frame, and this machine has
        // nowhere to forward it.
        let networks = network.networks().map_err(HostError::new)?;
        for user in &networks {
            let xml = user.xml().map_err(HostError::new)?;
            crate::adapters::NetworkEnsurer::ensure_network(
                self.hypervisor.as_ref(),
                &user.name,
                &xml,
            )
            .await
            .map_err(|error| HostError::new(format!("network {}: {error}", user.name)))?;
        }

        let desired = network.routes().map_err(HostError::new)?;
        let converged = bento_network::converge(self.routes.as_ref(), &desired)
            .await
            .map_err(HostError::new)?;

        for bridge in network.proxy_arp_bridges().map_err(HostError::new)? {
            // Forwarding and proxy ARP go on the user bridges only, never
            // host-wide: answering for an address Bento does not route is
            // the failure this avoids (MULTI-NODE 8.3).
            for (flag, value) in [("forwarding", "1"), ("proxy_arp", "1")] {
                self.routes
                    .set_interface_flag(&bridge, flag, value)
                    .await
                    .map_err(|error| HostError::new(format!("{bridge} {flag}: {error}")))?;
            }
        }

        let ruleset = network.ruleset().map_err(HostError::new)?;
        bento_network::reload(self.firewall.as_ref(), &ruleset)
            .await
            .map_err(HostError::new)?;

        // Only once guest traffic crosses machines. Before that, nothing
        // is forwarded from off-machine and another table's forward
        // policy cannot hide anything (MULTI-NODE 8.4).
        if !network.remote_slots.is_empty() {
            match bento_network::foreign_forward_filters("").await {
                Ok(tables) if !tables.is_empty() => tracing::warn!(
                    tables = tables.join(", "),
                    "another nftables table filters the forward hook; Bento cannot \
                     accept what it rejects. A guest of another machine that cannot \
                     be reached looks like a missing route. See DEPLOYING.md, \
                     \"the host firewall\""
                ),
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(%error, "could not read the other forward filters")
                }
            }
        }

        tracing::info!(
            bridges = networks.len(),
            added = converged.added.len(),
            removed = converged.removed.len(),
            unchanged = converged.unchanged,
            "runner: network applied"
        );
        Ok(Reply::NetworkApplied {
            bridges: networks.len(),
            routes_added: converged.added.len(),
            routes_removed: converged.removed.len(),
            routes_unchanged: converged.unchanged,
        })
    }

    async fn provision(
        &self,
        request: &bento_runner::ProvisionRequest,
    ) -> Result<Reply, HostError> {
        let uuid = &request.instance.uuid;
        let overlay = self.overlay_path(uuid);
        let seed_iso = self.seed_iso_path(uuid);

        // The same order and the same unwinding as a create on the
        // controller's own machine (SPEC 11.1): each step undoes what the
        // ones before it made, so a failure leaves no half-built instance
        // for the next attempt to trip over.
        self.overlays
            .create_overlay(&request.base_checksum, &overlay, request.disk_gib)
            .await
            .map_err(|error| HostError::new(format!("overlay for {uuid}: {error}")))?;

        if request.with_seed_iso
            && let Err(error) = self.seeds.build(&request.seed, &seed_iso).await
        {
            self.unwind_provision(&overlay, None).await;
            return Err(HostError::new(format!("seed image for {uuid}: {error}")));
        }

        let xml = bento_hypervisor::domain_xml(&bento_hypervisor::DomainSpec {
            name: request.instance.name.clone(),
            uuid: uuid.clone(),
            vcpu: request.vcpu,
            memory_mib: request.memory_mib,
            disk_path: overlay.display().to_string(),
            iso_path: if request.with_seed_iso {
                seed_iso.display().to_string()
            } else {
                String::new()
            },
            network: request.network.clone(),
            mac: request.mac.clone(),
            nested: request.nested,
            ksm: request.ksm,
            // Empty selects this machine's architecture. The controller
            // never names one: it places only onto a machine of the right
            // architecture (MULTI-NODE 12).
            arch: String::new(),
        });
        let xml = match xml {
            Ok(xml) => xml,
            Err(error) => {
                self.unwind_provision(&overlay, request.with_seed_iso.then_some(&seed_iso))
                    .await;
                return Err(HostError::new(format!("domain xml for {uuid}: {error}")));
            }
        };

        // `create` defines and starts in one call; `define` leaves the
        // domain stopped. A create asks for a running instance, and a
        // restore of a desired-stopped one does not (SPEC 11.2).
        let defined = if request.start {
            bento_hypervisor::Hypervisor::create(self.hypervisor.as_ref(), &xml).await
        } else {
            bento_hypervisor::Definer::define(self.hypervisor.as_ref(), &xml).await
        };
        if let Err(error) = defined {
            self.unwind_provision(&overlay, request.with_seed_iso.then_some(&seed_iso))
                .await;
            return Err(HostError::new(format!("define {uuid}: {error}")));
        }

        let state = self.observed(&request.instance).await?;
        tracing::info!(
            instance = %request.instance.name,
            uuid = %uuid,
            state = state.as_str(),
            "runner: instance provisioned"
        );
        Ok(Reply::Provisioned { state })
    }

    async fn start(&self, instance: &InstanceRef) -> Result<State, HostError> {
        self.hypervisor
            .start(&instance.name)
            .await
            .map_err(HostError::new)?;
        self.observed(instance).await
    }

    async fn stop(&self, instance: &InstanceRef) -> Result<State, HostError> {
        // The stop result says how it stopped, gracefully or forced. The
        // controller stores observed state, not the manner of it, so the
        // observed state is what travels back (SPEC 11.2).
        self.hypervisor
            .stop(&instance.name)
            .await
            .map_err(HostError::new)?;
        self.observed(instance).await
    }

    async fn reboot(&self, instance: &InstanceRef) -> Result<State, HostError> {
        self.hypervisor
            .reboot(&instance.name)
            .await
            .map_err(HostError::new)?;
        self.observed(instance).await
    }

    async fn remove(&self, instance: &InstanceRef) -> Result<State, HostError> {
        self.hypervisor
            .remove(&instance.name)
            .await
            .map_err(HostError::new)?;
        // The disk and the seed image live on this machine, so this is the
        // only place that can delete them. Leaving them behind would fill
        // the machine with the disks of instances that no longer exist.
        // The overlay goes only after the domain is undefined, so nothing
        // can still be writing to it.
        self.unwind_provision(
            &self.overlay_path(&instance.uuid),
            Some(&self.seed_iso_path(&instance.uuid)),
        )
        .await;
        // The domain is gone, so there is nothing left to observe. A
        // removed instance reads as stopped, which is what the controller
        // records for a domain that no longer runs.
        Ok(State::Stopped)
    }
}

impl LocalHost {
    /// What libvirt says about a domain after the runner acted on it.
    ///
    /// The runner reports what it saw rather than what it intended, so
    /// the controller never records a state that did not happen
    /// (SPEC 11.2). A domain that vanished between the action and this
    /// read is reported as stopped, not as an error: the action itself
    /// succeeded.
    async fn observed(&self, instance: &InstanceRef) -> Result<State, HostError> {
        match self.hypervisor.state(&instance.name).await {
            Ok(state) => Ok(state),
            Err(_) => Ok(State::Stopped),
        }
    }

    /// Removes what a half-built or removed instance left on disk.
    ///
    /// Every failure here is logged and none is returned. The caller is
    /// already reporting why the instance could not be built, and a
    /// second error about the cleanup would replace the reason with a
    /// consequence. A file that survives is visible in `reconcile`.
    async fn unwind_provision(&self, overlay: &Path, seed_iso: Option<&Path>) {
        for path in [Some(overlay), seed_iso].into_iter().flatten() {
            match tokio::fs::remove_file(path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => tracing::warn!(
                    path = %path.display(),
                    %error,
                    "runner: could not remove the file of an instance that is gone"
                ),
            }
        }
    }
}

/// Reads a `sha256-<hex>` or `sha256:<hex>` content address.
/// The space an overlay really occupies, not its virtual size. A qcow2
/// overlay is thin, so the two differ by a lot (SPEC 19). A file that is
/// not there yet reads as nothing rather than as a failure.
fn overlay_bytes(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).map_or(0, |meta| meta.blocks() * 512)
}

fn sha256_hex(checksum: &str) -> Result<String, HostError> {
    let hex = checksum
        .strip_prefix("sha256-")
        .or_else(|| checksum.strip_prefix("sha256:"))
        .unwrap_or(checksum)
        .to_ascii_lowercase();
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HostError::new(format!(
            "image checksum {checksum:?} is not a sha256 content address"
        )));
    }
    Ok(hex)
}

/// Fetches `url` into `directory` and names the result by its content.
///
/// The order here is the whole point. The download lands on a temporary
/// name, its digest is taken, and only then does it move to the name that
/// digest gives it. A file carrying a content-addressed name must contain
/// what that name says, because every later reader treats the name as
/// proof (SPEC 5.1). A failure removes the temporary file and leaves no
/// content-addressed name behind.
async fn fetch_into(url: &str, directory: &Path) -> Result<(String, i64), HostError> {
    use sha2::Digest;
    use tokio::io::AsyncWriteExt;

    tokio::fs::create_dir_all(directory)
        .await
        .map_err(|error| HostError::new(format!("create image directory: {error}")))?;
    // One temporary name for each fetch in flight, so two fetches into
    // the same directory cannot write over each other.
    let partial = directory.join(format!("fetch-{}.partial", bento_lifecycle::random_uuid()));
    let mut response = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|error| HostError::new(format!("fetch {url}: {error}")))?
        .error_for_status()
        .map_err(|error| HostError::new(format!("fetch {url}: {error}")))?;

    let mut file = tokio::fs::File::create(&partial)
        .await
        .map_err(|error| HostError::new(format!("create {}: {error}", partial.display())))?;
    let mut hasher = sha2::Sha256::new();
    let mut size: i64 = 0;
    let outcome = async {
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| HostError::new(format!("read {url}: {error}")))?
        {
            hasher.update(&chunk);
            size += chunk.len() as i64;
            file.write_all(&chunk)
                .await
                .map_err(|error| HostError::new(format!("write image: {error}")))?;
        }
        file.flush()
            .await
            .map_err(|error| HostError::new(format!("flush image: {error}")))?;
        // The file must be on the disk before the rename, or a power cut
        // leaves a complete name over incomplete content.
        file.sync_all()
            .await
            .map_err(|error| HostError::new(format!("sync image: {error}")))?;
        Ok::<_, HostError>(())
    }
    .await;
    drop(file);
    if let Err(error) = outcome {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(error);
    }

    let digest = hex_digest(&hasher.finalize());
    let destination = directory.join(format!("sha256-{digest}.qcow2"));
    if let Err(error) = tokio::fs::rename(&partial, &destination).await {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(HostError::new(format!(
            "move image into {}: {error}",
            destination.display()
        )));
    }
    Ok((format!("sha256-{digest}"), size))
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        use std::fmt::Write;
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn byte_units(bytes: u64, unit: u64, name: &str) -> Result<i64, HostError> {
    i64::try_from(bytes / unit)
        .map_err(|error| HostError::new(format!("{name} is too large: {error}")))
}

pub(crate) async fn run_runner(config: &Path, _args: &[OsString]) -> Result<()> {
    let app = App::new(config).await?;
    let result = runner_inner(&app).await;
    app.close().await;
    result
}

async fn runner_inner(app: &App) -> Result<()> {
    // Validation must finish before the management listener starts
    // (MULTI-NODE 11.1).
    host_checks(app).await?;
    let hypervisor = app.connect_libvirt().await?;
    // A runner knows which machine it is without asking anybody. It does
    // not open the controller's database: that database lives on the
    // controller, and a runner has no copy of it (MULTI-NODE 11.2). The
    // controller stores this same value on its host row, which is how the
    // two agree on identity (MULTI-NODE 16).
    let machine_id = bento_hostinfo::read_machine_id()
        .map_err(|error| anyhow::anyhow!("machine identity: {error}"))?;
    let hostname = bento_hostinfo::read_hostname();

    let fence_store = SqliteFence::open(&app.cfg.runner.fence_db)
        .with_context(|| format!("open runner fence database {}", app.cfg.runner.fence_db))?;
    let fence = Arc::new(
        Fence::new(machine_id.clone(), Box::new(fence_store))
            .with_max_skew(app.cfg.runner.max_clock_skew.std()),
    );
    let accepted_epoch = fence.accepted_epoch()?;
    let local_host = Arc::new(LocalHost {
        hypervisor: hypervisor.clone(),
        fence: fence.clone(),
        machine_id,
        hostname,
        storage_dir: PathBuf::from(&app.cfg.storage_dir),
        image_dir: PathBuf::from(&app.cfg.image_dir),
        overlays: Arc::new(bento_images::Store::for_overlays(&app.cfg.image_dir)),
        seeds: Arc::new(bento_cloudinit::Builder::default()),
        firewall: Arc::new(bento_network::NftApplier::default()),
        routes: Arc::new(bento_network::IpRouteApplier {
            path: String::new(),
            // Bounds what convergence may delete to Bento's own address
            // space, so a static route the operator installed for
            // something else is never removed (MULTI-NODE 8.2).
            private_range: app.plan.range(),
        }),
    });
    let logged_machine_id = local_host.machine_id.clone();
    let router = bento_runner::server::router(fence, local_host);

    let address =
        bento_config::resolve_listen_addr(&app.cfg.runner.listen).map_err(anyhow::Error::msg)?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    let bound = listener.local_addr()?;
    tracing::info!(
        addr = %bound,
        machine_id = %logged_machine_id,
        accepted_epoch,
        "runner listening"
    );

    let serve_result = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    hypervisor.close().await?;
    Ok(serve_result?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serves fixed bytes on loopback, so the fetch path is tested with
    /// no network beyond this machine.
    async fn serve(body: &'static [u8]) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut scratch = [0_u8; 1024];
                let _ = stream.read(&mut scratch).await;
                let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(body).await;
                let _ = stream.flush().await;
            }
        });
        (format!("http://{address}/image.qcow2"), handle)
    }

    /// `main` installs this for the real binary. A test builds its own
    /// client, so it installs the same one (CLAUDE.md: every TLS user is
    /// `ring`).
    fn install_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    const BODY: &[u8] = b"bento test image";

    fn body_digest() -> String {
        use sha2::Digest;
        hex_digest(&sha2::Sha256::digest(BODY))
    }

    #[tokio::test]
    async fn a_fetch_is_named_by_what_it_contains() {
        // The bug this test exists for: the file was renamed into place
        // and only then hashed, so the bytes could land under a name that
        // promised different content, and every later reader believed the
        // name.
        let directory = tempfile::tempdir().unwrap();
        install_provider();
        let (url, server) = serve(BODY).await;

        let (checksum, size) = fetch_into(&url, directory.path()).await.unwrap();
        assert_eq!(checksum, format!("sha256-{}", body_digest()));
        assert_eq!(size, BODY.len() as i64);

        let landed = directory.path().join(format!("{checksum}.qcow2"));
        assert_eq!(std::fs::read(&landed).unwrap(), BODY);

        // Only the content-addressed name is left; no partial file.
        let left: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec![format!("{checksum}.qcow2")]);
        server.abort();
    }

    #[tokio::test]
    async fn a_fetch_that_fails_leaves_no_content_addressed_name() {
        let directory = tempfile::tempdir().unwrap();
        install_provider();
        // Nothing listens here, so the fetch cannot succeed.
        let error = fetch_into("http://127.0.0.1:1/image.qcow2", directory.path())
            .await
            .expect_err("an unreachable URL must fail");
        assert!(error.to_string().contains("fetch"), "{error}");
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            0,
            "a failed fetch left a file behind"
        );
    }
}
