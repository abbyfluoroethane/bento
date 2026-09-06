//! The controller view of runner health (MULTI-NODE 11.2, 20).

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use bento_runner::{Envelope, ImageRequest, ObjectFence, Operation, PROTOCOL_VERSION, Reply};
use bento_store::{HostHealth, HostSeen, Store};
use bento_types::{Host, Image, Lease};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinSet;

/// How often the controller checks each configured runner (MULTI-NODE 20).
pub(crate) const INTERVAL: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const IMAGE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The network seam for one runner call. Tests use an in-memory fake.
#[async_trait]
trait Client: Send + Sync {
    async fn call(&self, endpoint: &str, envelope: Envelope) -> Result<Reply, String>;
}

struct HttpClient;

#[async_trait]
impl Client for HttpClient {
    async fn call(&self, endpoint: &str, envelope: Envelope) -> Result<Reply, String> {
        bento_runner::client::RunnerClient::new(endpoint)
            .call(envelope)
            .await
            .map_err(|error| error.to_string())
    }
}

/// One image version reported by one runner.
#[derive(Debug)]
pub(crate) struct HostImage {
    pub(crate) checksum: String,
}

/// Sends fenced image work and records the version that each runner reports.
#[derive(Clone)]
pub(crate) struct ImageSync {
    store: Store,
    client: Arc<dyn Client>,
    lease: watch::Receiver<Lease>,
    request_timeout: Duration,
}

impl ImageSync {
    pub(crate) fn new(store: Store, lease: watch::Receiver<Lease>) -> Self {
        Self {
            store,
            client: Arc::new(HttpClient),
            lease,
            request_timeout: IMAGE_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_client(store: Store, lease: watch::Receiver<Lease>, client: Arc<dyn Client>) -> Self {
        Self {
            store,
            client,
            lease,
            request_timeout: IMAGE_TIMEOUT,
        }
    }

    /// Syncs the allowlist to one runner without changing its current version.
    async fn sync_host(&self, host: &Host) -> Result<()> {
        let held = self.store.host_images(host.id).await?;
        for image in self.store.images().await? {
            let have = preferred_version(&image, &held);
            if let Err(error) = self.ensure(host, &image, have).await {
                tracing::warn!(
                    runner_id = host.id,
                    runner = %host.name,
                    image = %image.name,
                    %error,
                    "runner image sync failed"
                );
            }
        }
        Ok(())
    }

    /// Makes one runner fetch or retain an image and records its answer.
    pub(crate) async fn ensure(
        &self,
        host: &Host,
        image: &Image,
        have: Option<String>,
    ) -> Result<HostImage> {
        if host.machine_id.is_none() {
            anyhow::bail!("runner {} has not reported its machine identity", host.name);
        }
        let held = self.store.host_images(host.id).await?;
        let request = ImageRequest {
            name: image.name.clone(),
            url: image.url.clone(),
            have,
        };
        let current = self.lease.borrow().clone();
        let object = self.object_fence(&request, current.epoch).await?;
        let reply = send(
            self.client.as_ref(),
            host.endpoint
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("runner {} has no endpoint", host.name))?,
            host,
            &current,
            Operation::EnsureImage { image: request },
            Some(object),
            self.request_timeout,
        )
        .await
        .map_err(anyhow::Error::msg)?;
        let Reply::ImageReady {
            name,
            checksum,
            already_present: _,
            size: _,
        } = reply
        else {
            anyhow::bail!("runner answered {reply:?} to an image request");
        };
        if name != image.name {
            anyhow::bail!(
                "runner answered with image {name:?} to a request for {:?}",
                image.name
            );
        }
        let gained = !held
            .iter()
            .any(|(held_name, held_checksum)| held_name == &name && held_checksum == &checksum);
        self.store
            .record_host_image(host.id, name.clone(), checksum.clone())
            .await?;
        if gained {
            tracing::info!(
                runner_id = host.id,
                runner = %host.name,
                image = %name,
                %checksum,
                "runner gained image version"
            );
        }
        Ok(HostImage { checksum })
    }

    async fn object_fence(&self, request: &ImageRequest, epoch: i64) -> Result<ObjectFence> {
        let mut hasher = Sha256::new();
        digest_field(&mut hasher, &request.name);
        digest_field(&mut hasher, &request.url);
        match &request.have {
            Some(have) => {
                hasher.update([1]);
                digest_field(&mut hasher, have);
            }
            None => hasher.update([0]),
        }
        let mut digest = String::from("sha256-");
        for byte in hasher.finalize() {
            write!(&mut digest, "{byte:02x}").expect("writing to a string cannot fail");
        }
        let generation = self
            .store
            .next_dispatch_generation(request.name.clone(), epoch)
            .await?;
        Ok(ObjectFence {
            object_id: request.name.clone(),
            generation,
            digest,
        })
    }
}

fn digest_field(hasher: &mut Sha256, value: &str) {
    hasher.update(value.len().to_be_bytes());
    hasher.update(value.as_bytes());
}

fn preferred_version(image: &Image, held: &[(String, String)]) -> Option<String> {
    if let Some(current) = &image.current_checksum
        && held
            .iter()
            .any(|(name, checksum)| name == &image.name && checksum == current)
    {
        return Some(current.clone());
    }
    held.iter()
        .find(|(name, _)| name == &image.name)
        .map(|(_, checksum)| checksum.clone())
}

/// Sends each machine the network it should have (MULTI-NODE 8).
///
/// The state is a full description, not a set of increments, and applying
/// it twice changes nothing the second time. A machine therefore converges
/// by being told the whole answer again, which is what lets one that was
/// unreachable catch up without replaying what it missed.
#[derive(Clone)]
pub(crate) struct NetworkSync {
    store: Store,
    client: Arc<dyn Client>,
    lease: watch::Receiver<Lease>,
    plan: bento_network::Plan,
    high_ports: bento_network::PortRange,
    /// The machine running the frontends. Every other machine permits it
    /// to reach a guest's published ports (MULTI-NODE 8.4).
    controller_host_id: i64,
    /// The last state each machine acknowledged, by digest. It saves a
    /// call and a log line when nothing changed; it is not correctness,
    /// because applying the same state again is harmless.
    applied: Arc<Mutex<HashMap<i64, String>>>,
    request_timeout: Duration,
}

impl NetworkSync {
    pub(crate) fn new(
        store: Store,
        lease: watch::Receiver<Lease>,
        plan: bento_network::Plan,
        high_ports: bento_network::PortRange,
        controller_host_id: i64,
    ) -> Self {
        Self {
            store,
            client: Arc::new(HttpClient),
            lease,
            plan,
            high_ports,
            controller_host_id,
            applied: Arc::new(Mutex::new(HashMap::new())),
            request_timeout: REQUEST_TIMEOUT,
        }
    }

    /// Brings one machine's network to what the database says it is.
    pub(crate) async fn sync_host(&self, host: &Host) -> Result<()> {
        if host.machine_id.is_none() {
            anyhow::bail!("runner {} has not reported its machine identity", host.name);
        }
        let network = crate::netstate::machine_network(
            &self.store,
            self.plan,
            self.high_ports,
            host.id,
            self.controller_host_id,
        )
        .await?;
        // Refused here rather than on the machine, so a plan that would
        // black-hole traffic never leaves the controller (MULTI-NODE 8.3).
        network.check()?;

        let body = serde_json::to_string(&network)?;
        let digest = digest_of(&body);
        if self.applied.lock().await.get(&host.id) == Some(&digest) {
            return Ok(());
        }

        let current = self.lease.borrow().clone();
        let object = self.object_fence(host.id, &digest, current.epoch).await?;
        let reply = send(
            self.client.as_ref(),
            host.endpoint
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("runner {} has no endpoint", host.name))?,
            host,
            &current,
            Operation::ApplyNetwork { network },
            Some(object),
            self.request_timeout,
        )
        .await
        .map_err(anyhow::Error::msg)?;
        let Reply::NetworkApplied {
            bridges,
            routes_added,
            routes_removed,
            routes_unchanged,
        } = reply
        else {
            anyhow::bail!("runner answered {reply:?} to a network request");
        };
        self.applied.lock().await.insert(host.id, digest);
        if routes_added + routes_removed > 0 {
            tracing::info!(
                runner_id = host.id,
                runner = %host.name,
                bridges,
                added = routes_added,
                removed = routes_removed,
                unchanged = routes_unchanged,
                "runner network changed"
            );
        }
        Ok(())
    }

    async fn object_fence(&self, host_id: i64, digest: &str, epoch: i64) -> Result<ObjectFence> {
        let object_id = format!("network:{host_id}");
        let generation = self
            .store
            .next_dispatch_generation(object_id.clone(), epoch)
            .await?;
        Ok(ObjectFence {
            object_id,
            generation,
            digest: digest.to_owned(),
        })
    }
}

fn digest_of(body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    let mut digest = String::from("sha256-");
    for byte in hasher.finalize() {
        write!(&mut digest, "{byte:02x}").expect("writing to a string cannot fail");
    }
    digest
}

/// Sends instance work to the machine that will run it
/// (MULTI-NODE 13.1).
///
/// Provisioning is fenced like every other change: it names the object it
/// changes and the generation it belongs to, so a request that arrives
/// late cannot build an instance the controller has since given up on
/// (MULTI-NODE 11.3).
/// Where a process gets the lease it speaks under (MULTI-NODE 11.3).
///
/// One deployment has one controller, and it is the only process that
/// holds the lease. The SSH frontend is not that process, but it still
/// creates instances, so it names the lease the controller holds rather
/// than taking a second one. The runner orders their work by epoch and
/// object generation either way.
#[derive(Clone)]
enum LeaseSource {
    Held(watch::Receiver<Lease>),
    Current(Store),
}

impl LeaseSource {
    async fn lease(&self) -> Result<Lease> {
        match self {
            LeaseSource::Held(receiver) => Ok(receiver.borrow().clone()),
            LeaseSource::Current(store) => store.lease().await?.ok_or_else(|| {
                anyhow::anyhow!(
                    "no controller holds the lease, so nothing may change another machine"
                )
            }),
        }
    }
}

#[derive(Clone)]
pub(crate) struct InstanceSync {
    client: Arc<dyn Client>,
    lease: LeaseSource,
    /// Where the per-object generation comes from. It is the database,
    /// not a counter in this process, because more than one process
    /// dispatches (MULTI-NODE 11.3).
    store: Store,
    request_timeout: Duration,
}

impl InstanceSync {
    /// For the controller, which holds and renews the lease.
    pub(crate) fn new(store: Store, lease: watch::Receiver<Lease>) -> Self {
        Self::with_source(store, LeaseSource::Held(lease))
    }

    /// For a process of the same deployment that is not the controller,
    /// such as the SSH frontend. It reads the lease rather than holding
    /// one.
    pub(crate) fn following(store: Store) -> Self {
        Self::with_source(store.clone(), LeaseSource::Current(store))
    }

    fn with_source(store: Store, lease: LeaseSource) -> Self {
        Self {
            client: Arc::new(HttpClient),
            lease,
            store,
            // Building an instance copies no image, but it does create a
            // qcow2 overlay and resize it, which is slower than a poll.
            request_timeout: Duration::from_secs(5 * 60),
        }
    }

    /// Builds one instance on one machine and returns what it observed.
    pub(crate) async fn provision(
        &self,
        host: &Host,
        spec: &bento_lifecycle::ProvisionSpec,
    ) -> Result<bento_types::State> {
        let endpoint = host
            .endpoint
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("machine {} has no endpoint", host.name))?;
        if host.machine_id.is_none() {
            anyhow::bail!("machine {} has not reported its identity", host.name);
        }
        let request = bento_runner::ProvisionRequest {
            instance: bento_runner::InstanceRef {
                uuid: spec.instance.uuid.clone(),
                name: spec.instance.name.clone(),
            },
            base_checksum: spec.instance.base_checksum.clone(),
            disk_gib: spec.instance.disk_gib,
            vcpu: spec.instance.vcpu,
            memory_mib: spec.instance.memory_mib,
            nested: spec.instance.nested,
            ksm: spec.instance.ksm,
            network: spec.network.clone(),
            mac: spec.instance.mac.clone(),
            seed: spec.seed.clone(),
            with_seed_iso: spec.with_seed_iso,
            start: spec.start,
        };
        let current = self.lease.lease().await?;
        let body = serde_json::to_string(&request)?;
        let object = self
            .object_fence(&spec.instance.uuid, &digest_of(&body), current.epoch)
            .await?;
        let reply = send(
            self.client.as_ref(),
            endpoint,
            host,
            &current,
            Operation::ProvisionInstance {
                provision: Box::new(request),
            },
            Some(object),
            self.request_timeout,
        )
        .await
        .map_err(anyhow::Error::msg)?;
        match reply {
            Reply::Provisioned { state } => Ok(state),
            other => anyhow::bail!("machine answered {other:?} to a provision request"),
        }
    }

    /// Sends a read, which needs no object and records no generation.
    pub(crate) async fn read(&self, host: &Host, op: Operation) -> Result<Reply> {
        let endpoint = host
            .endpoint
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("machine {} has no endpoint", host.name))?;
        let current = self.lease.lease().await?;
        send(
            self.client.as_ref(),
            endpoint,
            host,
            &current,
            op,
            None,
            Duration::from_secs(10),
        )
        .await
        .map_err(anyhow::Error::msg)
    }

    /// Sends a change and returns the state the machine observed after it.
    pub(crate) async fn change(&self, host: &Host, op: Operation) -> Result<bento_types::State> {
        let endpoint = host
            .endpoint
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("machine {} has no endpoint", host.name))?;
        let uuid = match &op {
            Operation::StartInstance { instance }
            | Operation::StopInstance { instance }
            | Operation::RebootInstance { instance }
            | Operation::RemoveInstance { instance } => instance.uuid.clone(),
            other => anyhow::bail!("{other:?} is not an instance change"),
        };
        let current = self.lease.lease().await?;
        let digest = digest_of(&serde_json::to_string(&op)?);
        let object = self.object_fence(&uuid, &digest, current.epoch).await?;
        let reply = send(
            self.client.as_ref(),
            endpoint,
            host,
            &current,
            op,
            Some(object),
            self.request_timeout,
        )
        .await
        .map_err(anyhow::Error::msg)?;
        match reply {
            Reply::Changed { state } => Ok(state),
            other => anyhow::bail!("machine answered {other:?} to an instance change"),
        }
    }

    /// The UUID of a domain on one machine, by its name.
    ///
    /// The runner protocol names an instance by both, because the UUID
    /// is the identifier and the name is a label (SPEC 7.2). The
    /// lifecycle code above acts on names, so this looks the UUID up
    /// from the machine's own inventory rather than guessing.
    pub(crate) async fn uuid_of(&self, host: &Host, name: &str) -> Result<String> {
        match self.read(host, Operation::Inventory).await? {
            Reply::Inventory(inventory) => inventory
                .domains
                .into_iter()
                .find(|domain| domain.name == name)
                .map(|domain| domain.uuid)
                .ok_or_else(|| anyhow::anyhow!("machine {} has no domain {name}", host.name)),
            other => anyhow::bail!("machine answered {other:?} to an inventory request"),
        }
    }

    async fn object_fence(&self, uuid: &str, digest: &str, epoch: i64) -> Result<ObjectFence> {
        // The UUID is the identifier of an instance; the name is a label
        // (SPEC 7.2).
        let generation = self.store.next_dispatch_generation(uuid, epoch).await?;
        Ok(ObjectFence {
            object_id: uuid.to_owned(),
            generation,
            digest: digest.to_owned(),
        })
    }
}

/// Reads what another machine and its guests are using, for the
/// dashboard charts (SPEC 14.4, MULTI-NODE 20).
///
/// It is a read, so it takes no object generation and records nothing on
/// the runner. It exists so the sampler in `metrics` needs to know
/// nothing about the runner protocol.
pub(crate) struct RunnerSampler {
    sync: InstanceSync,
}

impl RunnerSampler {
    pub(crate) fn new(sync: InstanceSync) -> Self {
        Self { sync }
    }
}

#[async_trait]
impl crate::metrics::RemoteSampler for RunnerSampler {
    async fn sample(&self, host: &Host) -> Result<bento_runner::Samples, String> {
        match self.sync.read(host, Operation::Sample).await {
            Ok(Reply::Samples(samples)) => Ok(samples),
            Ok(other) => Err(format!("machine answered {other:?} to a sample request")),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// One machine's hypervisor, reached through its runner
/// (MULTI-NODE 13.1).
///
/// Starting, stopping, rebooting, and removing an instance all act on
/// the domain, and the domain is on the machine that runs it. This makes
/// that machine look like a local libvirt connection, so the lifecycle
/// code above it does not branch on where an instance lives.
pub(crate) struct RunnerHypervisor {
    sync: InstanceSync,
    host: Host,
}

impl RunnerHypervisor {
    pub(crate) fn new(sync: InstanceSync, host: Host) -> Self {
        Self { sync, host }
    }

    async fn act(
        &self,
        op: impl Fn(bento_runner::InstanceRef) -> Operation,
        name: &str,
    ) -> std::result::Result<bento_types::State, bento_hypervisor::Error> {
        let uuid = self
            .sync
            .uuid_of(&self.host, name)
            .await
            .map_err(|error| bento_hypervisor::Error::Operation(error.to_string()))?;
        let instance = bento_runner::InstanceRef {
            uuid,
            name: name.to_owned(),
        };
        self.sync
            .change(&self.host, op(instance))
            .await
            .map_err(|error| bento_hypervisor::Error::Operation(error.to_string()))
    }
}

#[async_trait]
impl bento_hypervisor::Hypervisor for RunnerHypervisor {
    async fn create(&self, _xml: &str) -> std::result::Result<(), bento_hypervisor::Error> {
        // Building an instance on another machine sends facts, not XML:
        // the machine renders its own paths (MULTI-NODE 11.2). That is
        // `Fleet::provision`, and nothing reaches this.
        Err(bento_hypervisor::Error::Operation(
            "a domain on another machine is defined by provisioning it, not by XML".into(),
        ))
    }

    async fn start(&self, name: &str) -> std::result::Result<(), bento_hypervisor::Error> {
        self.act(|instance| Operation::StartInstance { instance }, name)
            .await
            .map(|_| ())
    }

    async fn stop(
        &self,
        name: &str,
    ) -> std::result::Result<bento_hypervisor::StopResult, bento_hypervisor::Error> {
        // The machine reports the state it observed, not the manner of
        // the stop. The controller stores observed state (SPEC 11.2), so
        // a graceful answer is the honest one to give here.
        self.act(|instance| Operation::StopInstance { instance }, name)
            .await
            .map(|_| bento_hypervisor::StopResult::Graceful)
    }

    async fn reboot(&self, name: &str) -> std::result::Result<(), bento_hypervisor::Error> {
        self.act(|instance| Operation::RebootInstance { instance }, name)
            .await
            .map(|_| ())
    }

    async fn remove(&self, name: &str) -> std::result::Result<(), bento_hypervisor::Error> {
        self.act(|instance| Operation::RemoveInstance { instance }, name)
            .await
            .map(|_| ())
    }

    async fn list(
        &self,
    ) -> std::result::Result<Vec<bento_hypervisor::DomainInfo>, bento_hypervisor::Error> {
        let reply = self
            .sync
            .read(&self.host, Operation::Inventory)
            .await
            .map_err(|error| bento_hypervisor::Error::Operation(error.to_string()))?;
        match reply {
            Reply::Inventory(inventory) => Ok(inventory
                .domains
                .into_iter()
                .map(|domain| bento_hypervisor::DomainInfo {
                    name: domain.name,
                    uuid: domain.uuid,
                    state: domain.state,
                })
                .collect()),
            other => Err(bento_hypervisor::Error::Operation(format!(
                "machine answered {other:?} to an inventory request"
            ))),
        }
    }

    async fn state(
        &self,
        name: &str,
    ) -> std::result::Result<bento_types::State, bento_hypervisor::Error> {
        self.list()
            .await?
            .into_iter()
            .find(|domain| domain.name == name)
            .map(|domain| domain.state)
            .ok_or_else(|| bento_hypervisor::Error::DomainNotFound(name.to_owned()))
    }
}

/// Polls all runner endpoints and records their last known state.
pub(crate) struct PollTask {
    store: Store,
    client: Arc<dyn Client>,
    lease: watch::Receiver<Lease>,
    request_timeout: Duration,
    /// Present when the controller also converges runner networks. Tests
    /// that only exercise health polling leave it out.
    network: Option<NetworkSync>,
    /// This machine. Its instances are observed by the lifecycle poller
    /// against local libvirt, so the runner poll leaves them alone.
    local_host_id: Option<i64>,
}

impl PollTask {
    pub(crate) fn new(
        store: Store,
        lease: watch::Receiver<Lease>,
        network: NetworkSync,
        local_host_id: i64,
    ) -> Self {
        Self {
            store,
            client: Arc::new(HttpClient),
            lease,
            request_timeout: REQUEST_TIMEOUT,
            network: Some(network),
            local_host_id: Some(local_host_id),
        }
    }

    #[cfg(test)]
    fn with_client(store: Store, lease: watch::Receiver<Lease>, client: Arc<dyn Client>) -> Self {
        Self {
            store,
            client,
            lease,
            request_timeout: REQUEST_TIMEOUT,
            network: None,
            local_host_id: None,
        }
    }

    /// Runs one sweep at startup and every 30 seconds after that.
    pub(crate) async fn run<F>(self, shutdown: F)
    where
        F: Future<Output = ()>,
    {
        tokio::pin!(shutdown);
        let mut ticker = tokio::time::interval(INTERVAL);
        let image_sync = ImageSync {
            store: self.store.clone(),
            client: Arc::clone(&self.client),
            lease: self.lease.clone(),
            request_timeout: IMAGE_TIMEOUT,
        };
        let mut image_tasks = JoinSet::new();
        let mut syncing = HashSet::new();
        // A late sweep must not cause a burst of calls to every runner.
        // Such a burst adds load but gives no new state (MULTI-NODE 20).
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    image_tasks.abort_all();
                    return;
                },
                Some(result) = image_tasks.join_next(), if !image_tasks.is_empty() => {
                    match result {
                        Ok((host_id, Ok(()))) => {
                            syncing.remove(&host_id);
                        }
                        Ok((host_id, Err(error))) => {
                            syncing.remove(&host_id);
                            tracing::warn!(runner_id = host_id, %error, "runner image sync stopped");
                        }
                        Err(error) => tracing::warn!(%error, "runner image sync task failed"),
                    }
                }
                _ = ticker.tick() => {
                    match self.tick().await {
                        Ok(hosts) => {
                            for host in hosts {
                                // The network is converged before the
                                // image sync is started, because a slow
                                // image fetch must not hold back a route
                                // an existing guest needs
                                // (MULTI-NODE 8.2).
                                if let Some(network) = &self.network
                                    && let Err(error) = network.sync_host(&host).await
                                {
                                    tracing::warn!(
                                        runner_id = host.id,
                                        runner = %host.name,
                                        %error,
                                        "runner network sync failed"
                                    );
                                }
                                if syncing.insert(host.id) {
                                    let sync = image_sync.clone();
                                    image_tasks.spawn(async move {
                                        let host_id = host.id;
                                        (host_id, sync.sync_host(&host).await)
                                    });
                                }
                            }
                        }
                        Err(error) => tracing::warn!(%error, "runner poll failed"),
                    }
                }
            }
        }
    }

    async fn tick(&self) -> Result<Vec<Host>> {
        let mut polls = JoinSet::new();
        for host in self.store.hosts().await? {
            let Some(endpoint) = host.endpoint.clone() else {
                continue;
            };
            let store = self.store.clone();
            let client = Arc::clone(&self.client);
            let lease = self.lease.clone();
            let request_timeout = self.request_timeout;
            let local = self.local_host_id == Some(host.id);
            polls.spawn(async move {
                let before = store
                    .host_observation(host.id)
                    .await?
                    .map_or(HostHealth::Unknown, |seen| seen.health);
                let seen =
                    poll_host(client.as_ref(), &endpoint, &host, &lease, request_timeout).await;
                let healthy = seen.error.is_none();
                let after = store.observe_host(host.id, seen).await?;
                if before != after {
                    // A stable poll stays quiet. Operators need only state
                    // changes in the normal log stream (MULTI-NODE 20).
                    tracing::info!(
                        runner_id = host.id,
                        runner = %host.name,
                        old_health = before.as_str(),
                        health = after.as_str(),
                        "runner health changed"
                    );
                }
                // A guest on another machine is not in this machine's
                // libvirt, so the local poller never sees it. Without
                // this its row would keep whatever state it was created
                // with, and a guest that had stopped would read as
                // running for ever (MULTI-NODE 20).
                if healthy && !local {
                    match observe_instances(
                        client.as_ref(),
                        &endpoint,
                        &host,
                        &lease,
                        request_timeout,
                        &store,
                    )
                    .await
                    {
                        Ok(0) => {}
                        Ok(count) => tracing::debug!(
                            runner = %host.name,
                            instances = count,
                            "observed instance states on another machine"
                        ),
                        Err(error) => tracing::warn!(
                            runner_id = host.id,
                            runner = %host.name,
                            %error,
                            "could not observe instance states"
                        ),
                    }
                }
                if healthy {
                    Ok::<_, bento_store::Error>(Some(store.host(host.id).await?))
                } else {
                    Ok(None)
                }
            });
        }

        let mut healthy = Vec::new();
        while let Some(result) = polls.join_next().await {
            match result {
                Ok(Ok(Some(host))) => healthy.push(host),
                Ok(Ok(None)) => {}
                Ok(Err(error)) => tracing::warn!(%error, "runner observation failed"),
                Err(error) => tracing::warn!(%error, "runner poll task failed"),
            }
        }
        Ok(healthy)
    }
}

/// Records the observed state of the instances one machine runs.
///
/// It updates only the rows that machine holds. A row for another
/// machine is left alone, because this answer says nothing about it.
async fn observe_instances(
    client: &dyn Client,
    endpoint: &str,
    host: &Host,
    lease: &watch::Receiver<Lease>,
    request_timeout: Duration,
    store: &Store,
) -> Result<usize> {
    // A machine that holds no instance has nothing to say about one, so
    // it is not asked. A new machine is polled for health long before it
    // is given anything to run.
    let rows = store.instances_on_host(host.id).await?;
    if rows.is_empty() {
        return Ok(0);
    }

    let current = lease.borrow().clone();
    let reply = send(
        client,
        endpoint,
        host,
        &current,
        Operation::Inventory,
        None,
        request_timeout,
    )
    .await
    .map_err(anyhow::Error::msg)?;
    let Reply::Inventory(inventory) = reply else {
        anyhow::bail!("runner answered {reply:?} to an inventory request");
    };
    let seen: HashMap<String, bento_types::State> = inventory
        .domains
        .into_iter()
        .map(|domain| (domain.uuid, domain.state))
        .collect();

    // A row this machine holds with no domain has stopped, or was never
    // built. Either way it is not running, and saying so is what lets the
    // dashboard show the truth.
    let mut states = HashMap::new();
    for instance in rows {
        let state = seen
            .get(&instance.uuid)
            .copied()
            .unwrap_or(bento_types::State::Stopped);
        if state != instance.state {
            states.insert(instance.uuid, state);
        }
    }
    let changed = states.len();
    store.update_observed_states(states).await?;
    Ok(changed)
}

async fn poll_host(
    client: &dyn Client,
    endpoint: &str,
    host: &Host,
    lease: &watch::Receiver<Lease>,
    request_timeout: Duration,
) -> HostSeen {
    let current = lease.borrow().clone();
    let health = match send(
        client,
        endpoint,
        host,
        &current,
        Operation::Health,
        None,
        request_timeout,
    )
    .await
    {
        Ok(Reply::Health(health)) => health,
        Ok(reply) => return failed(format!("runner answered {reply:?} to a health request")),
        Err(error) => return failed(error),
    };

    let current = lease.borrow().clone();
    let capabilities = match send(
        client,
        endpoint,
        host,
        &current,
        Operation::Capabilities,
        None,
        request_timeout,
    )
    .await
    {
        Ok(Reply::Capabilities(capabilities)) => capabilities,
        Ok(reply) => {
            return failed(format!(
                "runner answered {reply:?} to a capabilities request"
            ));
        }
        Err(error) => return failed(error),
    };

    HostSeen {
        machine_id: Some(health.machine_id),
        accepted_epoch: health.accepted_epoch,
        arch: Some(capabilities.arch),
        cpu_count: Some(capabilities.cpu_count),
        memory_total_mib: Some(capabilities.memory_total_mib),
        storage_total_gib: Some(capabilities.storage_total_gib),
        storage_available_gib: Some(capabilities.storage_available_gib),
        hypervisor_version: Some(capabilities.hypervisor_version),
        error: None,
    }
}

async fn send(
    client: &dyn Client,
    endpoint: &str,
    host: &Host,
    lease: &Lease,
    op: Operation,
    object: Option<ObjectFence>,
    request_timeout: Duration,
) -> Result<Reply, String> {
    let envelope = Envelope {
        protocol_version: PROTOCOL_VERSION,
        target_machine_id: host.machine_id.clone(),
        epoch: lease.epoch,
        holder_id: lease.holder_id.clone(),
        lease_expires_at: lease.expires_at,
        sent_at: time::OffsetDateTime::now_utc(),
        request_id: bento_lifecycle::random_uuid(),
        object,
        op,
    };
    match tokio::time::timeout(request_timeout, client.call(endpoint, envelope)).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "runner request timed out after {} seconds",
            request_timeout.as_secs()
        )),
    }
}

fn failed(error: String) -> HostSeen {
    // Empty figures preserve the last good report in the store. The error
    // changes only reachability (MULTI-NODE 20).
    HostSeen {
        error: Some(error),
        ..HostSeen::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bento_runner::{Capabilities, Health};
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use time::OffsetDateTime;

    const MACHINE_ID: &str = "167eeb6836c44115aa084e7780e4328c";
    const ENDPOINT: &str = "http://10.0.0.97:10443";
    const UNDERLAY: &str = "10.0.0.97";

    struct FakeClient {
        replies: Mutex<VecDeque<Result<Reply, String>>>,
        calls: Mutex<Vec<(String, Envelope)>>,
    }

    impl FakeClient {
        fn new(replies: Vec<Result<Reply, String>>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Client for FakeClient {
        async fn call(&self, endpoint: &str, envelope: Envelope) -> Result<Reply, String> {
            self.calls
                .lock()
                .expect("fake calls")
                .push((endpoint.to_owned(), envelope));
            self.replies
                .lock()
                .expect("fake replies")
                .pop_front()
                .expect("one fake reply for each call")
        }
    }

    fn health() -> Reply {
        Reply::Health(Health {
            protocol_version: PROTOCOL_VERSION,
            machine_id: MACHINE_ID.into(),
            hostname: "runner-a.example.org".into(),
            accepted_epoch: 6,
        })
    }

    fn capabilities() -> Reply {
        Reply::Capabilities(Capabilities {
            arch: "aarch64".into(),
            cpu_count: 8,
            memory_total_mib: 7549,
            storage_total_gib: 164,
            storage_available_gib: 153,
            hypervisor_version: "libvirt local RPC".into(),
        })
    }

    fn lease() -> watch::Receiver<Lease> {
        let (_, receiver) = watch::channel(Lease {
            holder_id: "controller-a".into(),
            epoch: 7,
            expires_at: OffsetDateTime::now_utc() + Duration::from_secs(60),
        });
        receiver
    }

    async fn store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("bento.db"))
            .await
            .unwrap();
        (directory, store)
    }

    #[tokio::test]
    async fn a_successful_poll_records_health_and_capabilities() {
        let (_directory, store) = store().await;
        let host = store
            .register_runner("runner-a.example.org", ENDPOINT, UNDERLAY)
            .await
            .unwrap();
        let client = Arc::new(FakeClient::new(vec![Ok(health()), Ok(capabilities())]));
        let task = PollTask::with_client(store.clone(), lease(), client.clone());

        task.tick().await.unwrap();

        let observed = store.host_observation(host.id).await.unwrap().unwrap();
        assert_eq!(observed.health, HostHealth::Ok);
        assert_eq!(observed.accepted_epoch, 6);
        assert_eq!(observed.arch.as_deref(), Some("aarch64"));
        assert_eq!(observed.cpu_count, Some(8));
        assert_eq!(observed.memory_total_mib, Some(7549));
        assert_eq!(observed.storage_total_gib, Some(164));
        assert_eq!(observed.storage_available_gib, Some(153));
        assert_eq!(
            observed.hypervisor_version.as_deref(),
            Some("libvirt local RPC")
        );
        assert_eq!(
            store.host(host.id).await.unwrap().machine_id.as_deref(),
            Some(MACHINE_ID)
        );

        let calls = client.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|(endpoint, _)| endpoint == ENDPOINT));
        assert!(
            calls
                .iter()
                .all(|(_, call)| call.protocol_version == PROTOCOL_VERSION)
        );
        assert!(
            calls
                .iter()
                .all(|(_, call)| call.target_machine_id.is_none())
        );
        assert!(calls.iter().all(|(_, call)| call.epoch == 7));
        assert!(
            calls
                .iter()
                .all(|(_, call)| call.holder_id == "controller-a")
        );
        assert_ne!(calls[0].1.request_id, calls[1].1.request_id);
        assert_eq!(calls[0].1.op, Operation::Health);
        assert_eq!(calls[1].1.op, Operation::Capabilities);
    }

    fn provision_spec(host_id: i64) -> bento_lifecycle::ProvisionSpec {
        bento_lifecycle::ProvisionSpec {
            host_id,
            instance: bento_types::Instance {
                uuid: "11111111-2222-4333-8444-555555555555".into(),
                name: "web".into(),
                owner_id: 1,
                host_id,
                image_name: "debian-13".into(),
                base_checksum: "aa11".into(),
                state: bento_types::State::Stopped,
                desired_state: bento_types::DesiredState::Running,
                address: "10.100.0.129".into(),
                mac: "52:54:00:00:00:01".into(),
                vcpu: 2,
                memory_mib: 2048,
                disk_gib: 20,
                nested: false,
                ksm: true,
                http_port: 0,
                visibility: bento_types::Visibility::Off,
                created_at: time::OffsetDateTime::UNIX_EPOCH,
                last_seen_at: None,
                slot: Some(1),
            },
            network: "bento-user-0".into(),
            seed: bento_cloudinit::Seed {
                instance_id: "11111111-2222-4333-8444-555555555555".into(),
                hostname: "web".into(),
                user_name: "bento".into(),
                authorized_keys: vec!["ssh-ed25519 AAAA owner@example.org".into()],
                mac: "52:54:00:00:00:01".into(),
                address_cidr: "10.100.0.129/24".into(),
                gateway: "10.100.0.1".into(),
                dns: "1.1.1.1".into(),
                install_guest_agent: true,
            },
            with_seed_iso: true,
            start: true,
        }
    }

    #[tokio::test]
    async fn a_provision_carries_facts_and_never_a_path() {
        // The machine renders its own paths from its own configuration
        // (MULTI-NODE 11.2). What travels is the identity, the image
        // version, the size, the network, and the seed.
        let (_directory, store) = store().await;
        let host = store
            .register_runner("runner-a.example.org", ENDPOINT, UNDERLAY)
            .await
            .unwrap();
        store
            .observe_host(
                host.id,
                HostSeen {
                    machine_id: Some("167eeb6836c44115aa084e7780e4328c".into()),
                    ..HostSeen::default()
                },
            )
            .await
            .unwrap();
        let host = store.host(host.id).await.unwrap();
        let client = Arc::new(FakeClient::new(vec![Ok(Reply::Provisioned {
            state: bento_types::State::Running,
        })]));
        let sync = InstanceSync {
            client: client.clone(),
            lease: LeaseSource::Held(lease()),
            store: store.clone(),
            request_timeout: REQUEST_TIMEOUT,
        };

        let state = sync
            .provision(&host, &provision_spec(host.id))
            .await
            .unwrap();
        assert_eq!(state, bento_types::State::Running);

        let calls = client.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (endpoint, envelope) = &calls[0];
        assert_eq!(endpoint, ENDPOINT);
        // Addressed to the machine, fenced by the object it changes.
        assert_eq!(
            envelope.target_machine_id.as_deref(),
            Some("167eeb6836c44115aa084e7780e4328c")
        );
        let object = envelope.object.as_ref().expect("a change names its object");
        assert_eq!(object.object_id, "11111111-2222-4333-8444-555555555555");

        let Operation::ProvisionInstance { provision } = &envelope.op else {
            panic!("wrong operation: {:?}", envelope.op);
        };
        assert_eq!(provision.instance.name, "web");
        assert_eq!(provision.base_checksum, "aa11");
        assert_eq!(provision.disk_gib, 20);
        assert_eq!(provision.network, "bento-user-0");
        assert_eq!(provision.seed.address_cidr, "10.100.0.129/24");
        assert!(provision.start);

        // Nothing in the request names a path on the far machine.
        let text = serde_json::to_string(&envelope.op).unwrap();
        assert!(
            !text.contains("/var/lib") && !text.contains(".qcow2") && !text.contains(".iso"),
            "the request carries a path: {text}"
        );
    }

    #[tokio::test]
    async fn two_processes_never_send_one_generation_for_one_instance() {
        // `serve` and the SSH frontend both change instances, under the
        // one lease the controller holds. A runner refuses two orders
        // that claim one generation but want different things, because it
        // cannot tell which is right (MULTI-NODE 11.3). Counting in
        // memory would make both processes start at one and collide on
        // their first order for the same instance.
        let (_directory, store) = store().await;
        let host = store
            .register_runner("runner-a.example.org", ENDPOINT, UNDERLAY)
            .await
            .unwrap();
        store
            .observe_host(
                host.id,
                HostSeen {
                    machine_id: Some("167eeb6836c44115aa084e7780e4328c".into()),
                    ..HostSeen::default()
                },
            )
            .await
            .unwrap();
        let host = store.host(host.id).await.unwrap();
        // The controller takes the lease. The frontend reads that row
        // rather than taking a second one.
        let held = store
            .acquire_lease("the-controller", Duration::from_secs(30))
            .await
            .unwrap();
        let (_sender, held) = watch::channel(held);

        // The controller, which holds the lease.
        let serve_client = Arc::new(FakeClient::new(vec![Ok(Reply::Changed {
            state: bento_types::State::Stopped,
        })]));
        let serve = InstanceSync {
            client: serve_client.clone(),
            lease: LeaseSource::Held(held),
            store: store.clone(),
            request_timeout: REQUEST_TIMEOUT,
        };
        // The SSH frontend, which reads the lease rather than holding it.
        let frontend_client = Arc::new(FakeClient::new(vec![Ok(Reply::Changed {
            state: bento_types::State::Running,
        })]));
        let frontend = InstanceSync::following(store.clone());
        let frontend = InstanceSync {
            client: frontend_client.clone(),
            ..frontend
        };

        let instance = bento_runner::InstanceRef {
            uuid: "11111111-2222-4333-8444-555555555555".into(),
            name: "web".into(),
        };
        // A stop from the dashboard, then a start from the SSH frontend.
        serve
            .change(
                &host,
                Operation::StopInstance {
                    instance: instance.clone(),
                },
            )
            .await
            .unwrap();
        frontend
            .change(&host, Operation::StartInstance { instance })
            .await
            .unwrap();

        let first = serve_client.calls.lock().unwrap()[0]
            .1
            .object
            .clone()
            .expect("a change names its object");
        let second = frontend_client.calls.lock().unwrap()[0]
            .1
            .object
            .clone()
            .expect("a change names its object");
        assert_eq!(first.object_id, second.object_id);
        assert_ne!(
            first.digest, second.digest,
            "a stop and a start must not look alike"
        );
        assert!(
            second.generation > first.generation,
            "the frontend reused generation {} that the controller already sent; \
             the runner would refuse it as a conflict",
            first.generation
        );
    }

    #[tokio::test]
    async fn a_provision_to_a_machine_that_has_not_reported_its_identity_is_refused() {
        // Addressing a change to a machine the controller cannot name
        // would let it act on the wrong one (MULTI-NODE 11.2).
        let (_directory, store) = store().await;
        let host = store
            .register_runner("runner-a.example.org", ENDPOINT, UNDERLAY)
            .await
            .unwrap();
        let client = Arc::new(FakeClient::new(vec![]));
        let sync = InstanceSync {
            client: client.clone(),
            lease: LeaseSource::Held(lease()),
            store: store.clone(),
            request_timeout: REQUEST_TIMEOUT,
        };

        let error = sync
            .provision(&host, &provision_spec(host.id))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("identity"), "{error}");
        assert!(client.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_machine_with_no_instances_is_not_asked_about_them() {
        // A new machine is polled for health long before it runs
        // anything. Asking it for an inventory would be a call with one
        // possible answer.
        let (_directory, store) = store().await;
        let host = store
            .register_runner("runner-a.example.org", ENDPOINT, UNDERLAY)
            .await
            .unwrap();
        let client = Arc::new(FakeClient::new(vec![]));

        let changed = observe_instances(
            client.as_ref(),
            ENDPOINT,
            &host,
            &lease(),
            REQUEST_TIMEOUT,
            &store,
        )
        .await
        .unwrap();

        assert_eq!(changed, 0);
        assert!(
            client.calls.lock().unwrap().is_empty(),
            "a machine holding nothing was asked for an inventory"
        );
    }

    #[tokio::test]
    async fn a_failed_poll_records_an_error_and_keeps_last_known_figures() {
        let (_directory, store) = store().await;
        let host = store
            .register_runner("runner-a.example.org", ENDPOINT, UNDERLAY)
            .await
            .unwrap();
        let client = Arc::new(FakeClient::new(vec![
            Ok(health()),
            Ok(capabilities()),
            Err("runner transport: connection refused".into()),
        ]));
        let task = PollTask::with_client(store.clone(), lease(), client);
        task.tick().await.unwrap();
        let well = store.host_observation(host.id).await.unwrap().unwrap();

        task.tick().await.unwrap();

        let failed = store.host_observation(host.id).await.unwrap().unwrap();
        assert_eq!(failed.health, HostHealth::Unreachable);
        assert_eq!(failed.arch, well.arch);
        assert_eq!(failed.cpu_count, well.cpu_count);
        assert_eq!(failed.memory_total_mib, well.memory_total_mib);
        assert_eq!(failed.storage_total_gib, well.storage_total_gib);
        assert_eq!(failed.storage_available_gib, well.storage_available_gib);
        assert_eq!(failed.hypervisor_version, well.hypervisor_version);
        assert_eq!(failed.last_contact_at, well.last_contact_at);
        assert_eq!(
            failed.last_error.as_deref(),
            Some("runner transport: connection refused")
        );
    }

    #[tokio::test]
    async fn image_sync_records_the_version_the_runner_reports() {
        let (_directory, store) = store().await;
        let pending = store
            .register_runner("runner-a.example.org", ENDPOINT, UNDERLAY)
            .await
            .unwrap();
        store
            .observe_host(
                pending.id,
                HostSeen {
                    machine_id: Some(MACHINE_ID.into()),
                    ..HostSeen::default()
                },
            )
            .await
            .unwrap();
        store
            .upsert_image(Image {
                name: "debian-13".into(),
                url: "https://images.example.org/debian-13.qcow2".into(),
                kind: bento_types::ImageKind::Qcow2,
                pinned_checksum: None,
                current_checksum: None,
            })
            .await
            .unwrap();
        let host = store.host(pending.id).await.unwrap();
        let client = Arc::new(FakeClient::new(vec![Ok(Reply::ImageReady {
            name: "debian-13".into(),
            checksum: "sha256-f580e185".into(),
            already_present: false,
            size: 734_003_200,
        })]));
        let sync = ImageSync::with_client(store.clone(), lease(), client.clone());
        let image = store.image("debian-13").await.unwrap();

        let reported = sync.ensure(&host, &image, None).await.unwrap();

        assert_eq!(reported.checksum, "sha256-f580e185");
        assert_eq!(
            store.host_images(host.id).await.unwrap(),
            // The protocol reports a checksum prefixed; the store keeps
            // it bare, so one version stays one row (MULTI-NODE 13.2).
            vec![("debian-13".into(), "f580e185".into())]
        );
        let calls = client.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let envelope = &calls[0].1;
        assert_eq!(envelope.target_machine_id.as_deref(), Some(MACHINE_ID));
        let fence = envelope.object.as_ref().expect("image work is fenced");
        assert_eq!(fence.object_id, "debian-13");
        assert!(fence.generation > 0);
        assert!(fence.digest.starts_with("sha256-"));
        assert!(matches!(
            &envelope.op,
            Operation::EnsureImage { image }
                if image.name == "debian-13" && image.have.is_none()
        ));
    }
}
