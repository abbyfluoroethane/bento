use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bento_cloudinit::Seed;
use bento_hypervisor::{AutostartClearer, Definer, DomainSpec, Hypervisor};
use bento_network::{AddressStore, Ipv4Prefix, Plan, UserNetwork};
use bento_types::{DesiredState, Image, Instance, State, User};
use time::OffsetDateTime;

use crate::{Error, QemuImgResizer};

/// A thread-safe dynamic error returned through a consumer-side seam.
pub type DynError = Box<dyn std::error::Error + Send + Sync + 'static>;
/// The lifecycle result type.
pub type Result<T> = std::result::Result<T, Error>;

/// The consumer-side view of the data layer that lifecycle needs.
#[async_trait]
pub trait Store: Send + Sync {
    /// Runs name cooldown, quota checking, and insertion in one transaction
    /// (SPEC sections 6.1 and 7.2).
    async fn create_instance(
        &self,
        instance: Instance,
        cooldown: Duration,
    ) -> std::result::Result<(), DynError>;
    /// Deletes the row, cascades shares, and releases the name in one
    /// transaction (SPEC 11.1 steps 3 and 4).
    async fn delete_instance(&self, uuid: &str) -> std::result::Result<Instance, DynError>;
    async fn instance(&self, uuid: &str) -> std::result::Result<Instance, DynError>;
    async fn instances(&self) -> std::result::Result<Vec<Instance>, DynError>;
    /// Lists desired-running, observed-stopped instances (SPEC 11.2).
    async fn instances_to_restore(&self) -> std::result::Result<Vec<Instance>, DynError>;
    async fn image(&self, name: &str) -> std::result::Result<Image, DynError>;
    async fn user_by_id(&self, id: i64) -> std::result::Result<User, DynError>;
    /// Changes the row name and releases the old one atomically (SPEC 7.2).
    async fn rename_instance(
        &self,
        uuid: &str,
        new_name: &str,
        cooldown: Duration,
    ) -> std::result::Result<(), DynError>;
    /// Reruns quota checking with the instance's current usage excluded.
    async fn resize(
        &self,
        uuid: &str,
        vcpu: u32,
        memory_mib: i64,
        disk_gib: i64,
        nested: bool,
    ) -> std::result::Result<(), DynError>;
    async fn set_desired_state(
        &self,
        uuid: &str,
        state: DesiredState,
    ) -> std::result::Result<(), DynError>;
    async fn set_observed_state(
        &self,
        uuid: &str,
        state: State,
    ) -> std::result::Result<(), DynError>;
    async fn update_observed_states(
        &self,
        states: HashMap<String, State>,
    ) -> std::result::Result<(), DynError>;
}

/// The consumer-side view of the content-addressed image store.
#[async_trait]
pub trait ImageStore: Send + Sync {
    /// Creates and grows a qcow2 overlay backed by `checksum` (SPEC 5.2).
    async fn create_overlay(
        &self,
        checksum: &str,
        overlay_path: &Path,
        disk_gib: i64,
    ) -> std::result::Result<(), DynError>;
}

/// Builds the NoCloud seed ISO (SPEC 5.2).
#[async_trait]
pub trait ISOBuilder: Send + Sync {
    async fn build(&self, seed: &Seed, iso_path: &Path) -> std::result::Result<(), DynError>;
}

#[async_trait]
impl ISOBuilder for bento_cloudinit::Builder {
    async fn build(&self, seed: &Seed, iso_path: &Path) -> std::result::Result<(), DynError> {
        self.build(seed, iso_path)
            .await
            .map_err(|error| Box::new(error) as DynError)
    }
}

/// Grows an existing overlay.
#[async_trait]
pub trait OverlayResizer: Send + Sync {
    async fn resize_overlay(
        &self,
        overlay_path: &Path,
        disk_gib: i64,
    ) -> std::result::Result<(), DynError>;
}

/// Injectable asynchronous wait primitive.
#[async_trait]
pub trait Sleep: Send + Sync {
    async fn sleep(&self, duration: Duration);
}

/// Injectable progress/warning sink.
pub trait LifecycleLogger: Send + Sync {
    fn info(&self, message: &str);
    fn warn(&self, message: &str);
    fn error(&self, message: &str);
}

#[derive(Debug)]
struct TokioSleep;

#[async_trait]
impl Sleep for TokioSleep {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

#[derive(Debug)]
struct TracingLogger;

impl LifecycleLogger for TracingLogger {
    fn info(&self, message: &str) {
        tracing::info!("{message}");
    }
    fn warn(&self, message: &str) {
        tracing::warn!("{message}");
    }
    fn error(&self, message: &str) {
        tracing::error!("{message}");
    }
}

pub type NestedProbe = Arc<dyn Fn() -> (bool, String) + Send + Sync>;
pub type UuidMint = Arc<dyn Fn() -> String + Send + Sync>;
pub type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;
pub type DeleteIsoFuture = Pin<Box<dyn Future<Output = std::result::Result<(), DynError>> + Send>>;
pub type DeleteIso = Arc<dyn Fn(PathBuf) -> DeleteIsoFuture + Send + Sync>;
pub type IsoExists = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

/// Wires a [`Manager`]. Hypervisor, store, images, ISO builder, network
/// plan, and storage directory are required; every other field defaults.
#[derive(Default)]
pub struct Config {
    pub hypervisor: Option<Arc<dyn Hypervisor>>,
    /// Optional persistent XML replacement capability.
    pub definer: Option<Arc<dyn Definer>>,
    /// Optional libvirt autostart clearing capability.
    pub autostart_clearer: Option<Arc<dyn AutostartClearer>>,
    pub store: Option<Arc<dyn Store>>,
    pub images: Option<Arc<dyn ImageStore>>,
    pub iso: Option<Arc<dyn ISOBuilder>>,
    pub resizer: Option<Arc<dyn OverlayResizer>>,
    pub plan: Option<Plan>,
    pub storage_dir: PathBuf,
    pub name_cooldown: Duration,
    pub batch_size: usize,
    pub dns: Vec<IpAddr>,
    pub logger: Option<Arc<dyn LifecycleLogger>>,
    pub nested_enabled: Option<NestedProbe>,
    pub poll_interval: Duration,
    pub start_poll_interval: Duration,
    pub start_timeout: Duration,
    pub sleep: Option<Arc<dyn Sleep>>,
    pub new_uuid: Option<UuidMint>,
    pub now: Option<Clock>,
    pub delete_iso: Option<DeleteIso>,
    pub iso_exists: Option<IsoExists>,
}

/// Orchestrates instance lifecycle operations.
pub struct Manager {
    pub(crate) hyp: Arc<dyn Hypervisor>,
    pub(crate) definer: Option<Arc<dyn Definer>>,
    pub(crate) autostart_clearer: Option<Arc<dyn AutostartClearer>>,
    pub(crate) store: Arc<dyn Store>,
    pub(crate) images: Arc<dyn ImageStore>,
    pub(crate) iso: Arc<dyn ISOBuilder>,
    pub(crate) resizer: Arc<dyn OverlayResizer>,
    pub(crate) plan: Plan,
    pub(crate) storage_dir: PathBuf,
    pub(crate) cooldown: Duration,
    pub(crate) batch_size: usize,
    pub(crate) dns: Vec<IpAddr>,
    pub(crate) log: Arc<dyn LifecycleLogger>,
    pub(crate) nested: NestedProbe,
    pub(crate) poll_every: Duration,
    pub(crate) start_poll: Duration,
    pub(crate) start_wait: Duration,
    pub(crate) sleep: Arc<dyn Sleep>,
    pub(crate) new_uuid: UuidMint,
    pub(crate) now: Clock,
    pub(crate) delete_iso: DeleteIso,
    pub(crate) iso_exists: IsoExists,
}

impl Manager {
    /// Validates configuration and applies injectable defaults.
    pub fn new(config: Config) -> Result<Self> {
        let hyp = config.hypervisor.ok_or(Error::Config("a Hypervisor"))?;
        let store = config.store.ok_or(Error::Config("a Store"))?;
        let images = config.images.ok_or(Error::Config("an ImageStore"))?;
        let iso = config.iso.ok_or(Error::Config("an ISOBuilder"))?;
        let plan = config.plan.ok_or(Error::Config("a network Plan"))?;
        if config.storage_dir.as_os_str().is_empty() {
            return Err(Error::Config("a StorageDir"));
        }
        let nested = config.nested_enabled.unwrap_or_else(|| {
            Arc::new(|| {
                let (enabled, path) = bento_hypervisor::nested_enabled(
                    bento_hypervisor::CheckConfig::default(),
                    &bento_hypervisor::default_check_deps(),
                );
                (
                    enabled,
                    path.map_or_else(String::new, |path| path.display().to_string()),
                )
            })
        });
        let delete_iso = config.delete_iso.unwrap_or_else(|| {
            Arc::new(|path| {
                Box::pin(async move {
                    bento_cloudinit::delete(path).map_err(|error| Box::new(error) as DynError)
                })
            })
        });
        let iso_exists = config.iso_exists.unwrap_or_else(|| Arc::new(Path::exists));
        Ok(Self {
            hyp,
            definer: config.definer,
            autostart_clearer: config.autostart_clearer,
            store,
            images,
            iso,
            resizer: config
                .resizer
                .unwrap_or_else(|| Arc::new(QemuImgResizer::default())),
            plan,
            storage_dir: config.storage_dir,
            cooldown: nonzero(config.name_cooldown, Duration::from_secs(24 * 60 * 60)),
            batch_size: if config.batch_size == 0 {
                4
            } else {
                config.batch_size
            },
            dns: if config.dns.is_empty() {
                bento_network::DEFAULT_DNS.to_vec()
            } else {
                config.dns
            },
            log: config.logger.unwrap_or_else(|| Arc::new(TracingLogger)),
            nested,
            poll_every: nonzero(config.poll_interval, Duration::from_secs(30)),
            start_poll: nonzero(config.start_poll_interval, Duration::from_millis(500)),
            start_wait: nonzero(config.start_timeout, Duration::from_secs(5 * 60)),
            sleep: config.sleep.unwrap_or_else(|| Arc::new(TokioSleep)),
            new_uuid: config.new_uuid.unwrap_or_else(|| Arc::new(random_uuid)),
            now: config
                .now
                .unwrap_or_else(|| Arc::new(OffsetDateTime::now_utc)),
            delete_iso,
            iso_exists,
        })
    }

    /// Root volume path. It derives from UUID so rename never moves a disk.
    pub fn overlay_path(&self, uuid: &str) -> PathBuf {
        self.storage_dir.join(format!("{uuid}.qcow2"))
    }

    /// Seed path, present only until the first successful boot (SPEC 5.2).
    pub fn seed_iso_path(&self, uuid: &str) -> PathBuf {
        self.storage_dir.join(format!("{uuid}-seed.iso"))
    }

    pub(crate) fn check_nested(&self) -> Result<()> {
        let (enabled, detail) = (self.nested)();
        if enabled {
            return Ok(());
        }
        let mut message =
            "load the KVM module with kvm_intel.nested=1 (Intel) or kvm_amd.nested=1 (AMD)"
                .to_string();
        if !detail.is_empty() {
            message.push_str(": ");
            message.push_str(&detail);
        }
        Err(Error::NestedUnavailable(message))
    }

    pub(crate) fn domain_xml(
        &self,
        instance: &Instance,
        owner: &User,
        with_iso: bool,
    ) -> Result<String> {
        let subnet = parse_prefix(&owner.subnet).map_err(|error| {
            Error::operation(format!(
                "lifecycle: user {} has a bad subnet {:?}: {error}",
                owner.name, owner.subnet
            ))
        })?;
        let index = self.plan.index(subnet).map_err(|error| {
            Error::operation(format!(
                "lifecycle: subnet {}/{} outside the private range: {error}",
                subnet.addr, subnet.bits
            ))
        })?;
        let network = UserNetwork::new(self.plan, index as isize)
            .map_err(|error| Error::operation(error.to_string()))?;
        bento_hypervisor::domain_xml(&DomainSpec {
            name: instance.name.clone(),
            uuid: instance.uuid.clone(),
            vcpu: instance.vcpu,
            memory_mib: instance.memory_mib,
            disk_path: self.overlay_path(&instance.uuid).display().to_string(),
            iso_path: if with_iso {
                self.seed_iso_path(&instance.uuid).display().to_string()
            } else {
                String::new()
            },
            network: network.name,
            mac: instance.mac.clone(),
            nested: instance.nested,
            ksm: instance.ksm,
            arch: String::new(),
        })
        .map_err(|error| Error::operation(error.to_string()))
    }

    pub(crate) async fn domain_xml_by_uuid(
        &self,
        instance: &Instance,
        with_iso: bool,
    ) -> Result<String> {
        let owner = self
            .store
            .user_by_id(instance.owner_id)
            .await
            .map_err(|error| {
                Error::operation(format!("lifecycle: owner of {}: {error}", instance.name))
            })?;
        self.domain_xml(instance, &owner, with_iso)
    }
}

fn nonzero(value: Duration, default: Duration) -> Duration {
    if value.is_zero() { default } else { value }
}

pub(crate) fn random_uuid() -> String {
    let mut bytes: [u8; 16] = rand::random();
    bytes[6] = bytes[6] & 0x0f | 0x40;
    bytes[8] = bytes[8] & 0x3f | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

pub(crate) fn parse_prefix(value: &str) -> std::result::Result<Ipv4Prefix, String> {
    let (address, bits) = value
        .rsplit_once('/')
        .ok_or_else(|| "missing prefix length".to_string())?;
    let addr = address
        .parse::<Ipv4Addr>()
        .map_err(|error| error.to_string())?;
    let bits = bits.parse::<u8>().map_err(|error| error.to_string())?;
    if bits > 32 {
        return Err("prefix length out of range".to_string());
    }
    Ok(Ipv4Prefix { addr, bits })
}

pub(crate) struct AddressView(pub(crate) Arc<dyn Store>);

#[async_trait]
impl AddressStore for AddressView {
    async fn used_addresses(
        &self,
        subnet: Ipv4Prefix,
    ) -> std::result::Result<Vec<Ipv4Addr>, bento_network::DynError> {
        let rows = self.0.instances().await?;
        Ok(rows
            .into_iter()
            .filter_map(|instance| instance.address.parse().ok())
            .filter(|address: &Ipv4Addr| prefix_contains(subnet, *address))
            .collect())
    }
}

fn prefix_contains(prefix: Ipv4Prefix, address: Ipv4Addr) -> bool {
    let mask = if prefix.bits == 0 {
        0
    } else {
        u32::MAX << (32 - prefix.bits)
    };
    u32::from(prefix.addr) & mask == u32::from(address) & mask
}
