//! What the controller and a runner say to each other (MULTI-NODE 11.2).
//!
//! The protocol is a tagged enum, not a set of paths. One endpoint takes
//! one [`Envelope`] and answers one [`Reply`]. Adding an operation adds a
//! variant, and the compiler then finds every place that must handle it.
//! That is the whole reason the wire format is JSON over HTTP rather than
//! a schema language: both ends are the same binary at the same version,
//! so the Rust type is already the contract (MULTI-NODE 11.2).
//!
//! Nothing here carries a path, a shell command, or nftables text. The
//! runner renders those itself from its own configuration.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// The protocol this build speaks.
///
/// A runner refuses a version it does not know rather than guessing.
/// Version 1 is the first, so the accepted set is exactly `{1}`; a later
/// build widens it and states how far back it reads.
pub const PROTOCOL_VERSION: u32 = 1;

/// One request from the controller to one runner.
///
/// Every field outside `op` exists to answer one question: may this
/// request act at all? They are checked in a fixed order before the
/// operation is looked at (see `fence::Fence::admit`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol_version: u32,
    /// Which machine the controller believes it is talking to.
    ///
    /// This is `/etc/machine-id`, not the `hosts` row id (MULTI-NODE 16).
    /// A runner knows its own machine ID without a database, and the
    /// controller stores it on the host row, so the two agree without the
    /// runner ever opening the controller's database. A request addressed
    /// to another machine is refused, so a misconfigured endpoint fails
    /// loudly instead of acting on the wrong machine.
    ///
    /// `None` means "whoever answers this address". The controller needs
    /// it exactly once: an operator writes a name and an endpoint, and
    /// only the machine itself can say which machine it is. A runner
    /// accepts an unaddressed request for a read, and never for a change,
    /// so nothing is ever altered on a machine the controller could not
    /// name.
    pub target_machine_id: Option<String>,
    /// The controller epoch (MULTI-NODE 11.3). A runner refuses an epoch
    /// lower than the highest it has accepted.
    pub epoch: i64,
    /// Which controller process holds the lease.
    pub holder_id: String,
    /// When that lease runs out. A runner refuses an expired lease.
    #[serde(with = "time::serde::rfc3339")]
    pub lease_expires_at: OffsetDateTime,
    /// The controller's clock when it sent this request.
    ///
    /// A runner judges the lease against its own clock, so it must know
    /// how far the two clocks disagree. Without this it could only guess
    /// from the lease, and a runner clock running fast would read every
    /// live lease as expired and report the wrong reason (MULTI-NODE 11.3).
    #[serde(with = "time::serde::rfc3339")]
    pub sent_at: OffsetDateTime,
    /// Unique for each attempt. A retried mutation repeats this value, and
    /// the runner answers with the outcome it already recorded.
    pub request_id: String,
    /// The object this request changes, and the controller's generation
    /// for it (MULTI-NODE 11.3). Required for a change; absent for a read.
    pub object: Option<ObjectFence>,
    pub op: Operation,
}

/// One object's place in the controller's history (MULTI-NODE 11.3).
///
/// `request_id` protects against a message arriving twice. This protects
/// against something different: a message arriving *late*, carrying an
/// older idea of what the object should be. The two are separate because
/// a retry and a stale order look the same on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectFence {
    /// What is being changed. For an instance this is its UUID, which is
    /// the identifier; the name is a label (SPEC 7.2).
    pub object_id: String,
    /// Increases every time the controller changes what it wants for this
    /// object. A runner refuses a lower one.
    pub generation: i64,
    /// A digest of the desired state at that generation. Two requests at
    /// the same generation must agree about what they want; if they do
    /// not, one of them is wrong and the runner refuses both.
    pub digest: String,
}

/// The typed operations (MULTI-NODE 11.5).
///
/// A read is admitted without recording anything. A change must name
/// the object it changes, and the runner remembers that it did it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    /// Who this runner is and what epoch it has accepted.
    Health,
    /// What the machine has: architecture, processors, memory, storage.
    Capabilities,
    /// The domains libvirt knows about on this machine.
    Inventory,
    /// What this machine and its running domains are using right now
    /// (SPEC 14.4).
    ///
    /// The controller charts every machine, not only its own, so it has
    /// to ask each one. This is a read: it records nothing and changes
    /// nothing, and the controller sends it on every sampling tick.
    Sample,
    /// Make sure one image version is on this machine, fetching it if it
    /// is not (MULTI-NODE 13.2).
    EnsureImage {
        image: ImageRequest,
    },
    /// Bring this machine's whole network to the described state
    /// (MULTI-NODE 8).
    ///
    /// It carries facts, not commands: users, slots, and where each
    /// remote slot lives. The runner renders its own bridges, routes,
    /// proxy ARP, and firewall from them. Applying the same state twice
    /// changes nothing the second time, so a runner converges by
    /// re-applying rather than by replaying a history it may have
    /// missed.
    ApplyNetwork {
        network: bento_network::MachineNetwork,
    },
    /// Build an instance on this machine and define its domain
    /// (MULTI-NODE 13.1).
    ///
    /// This is the operation that lets a machine other than the
    /// controller hold a guest. It carries facts, never paths: the runner
    /// puts the overlay and the seed image where its own configuration
    /// says, because only it knows where its storage is.
    ProvisionInstance {
        provision: Box<ProvisionRequest>,
    },
    /// Start a domain that is already defined.
    StartInstance {
        instance: InstanceRef,
    },
    /// Ask the guest to shut down, and destroy it only after the timeout.
    StopInstance {
        instance: InstanceRef,
    },
    RebootInstance {
        instance: InstanceRef,
    },
    /// Destroy a running domain and undefine it with its NVRAM file, and
    /// delete the overlay and seed image it was built from.
    RemoveInstance {
        instance: InstanceRef,
    },
}

/// Everything a machine needs to build one instance.
///
/// The controller allocates the identity, the address, and the MAC, and
/// picks the image version, because those are deployment-wide decisions
/// (SPEC 6.2, MULTI-NODE 12). The machine turns them into an overlay, a
/// seed image, and a domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionRequest {
    pub instance: InstanceRef,
    /// The image version to back the overlay with. The machine holds it
    /// already: the fleet gate refuses a create until every machine does
    /// (MULTI-NODE 13.2).
    pub base_checksum: String,
    pub disk_gib: i64,
    pub vcpu: u32,
    pub memory_mib: i64,
    pub nested: bool,
    pub ksm: bool,
    /// The libvirt network to attach to, for example `bento-user-1`. The
    /// machine already has it from [`Operation::ApplyNetwork`].
    pub network: String,
    pub mac: String,
    /// What cloud-init writes into the guest on first boot (SPEC 5.2).
    /// It carries the address, the gateway, and the owner's keys, so the
    /// machine needs no user table to build it.
    pub seed: bento_cloudinit::Seed,
    /// A bootc image bakes its packages in, so it takes no seed ISO
    /// drive for the guest agent (SPEC 5.2).
    pub with_seed_iso: bool,
    /// Whether to start the domain once it is defined.
    pub start: bool,
}

/// An image the controller wants a runner to have.
///
/// The allowlist URL says where a known-good image comes from. It does
/// not promise the bytes never change: a distribution publishes a new
/// build whenever it fixes something, and that build is the one an
/// operator wants (SPEC 5.1). So a runner fetches what the URL serves
/// now and reports the checksum it got. The controller records what each
/// runner holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRequest {
    /// The allowlist entry this belongs to.
    pub name: String,
    /// Where to fetch it.
    pub url: String,
    /// A version this runner may already hold. When it is present on
    /// disk, the runner keeps it and fetches nothing. `None` asks the
    /// runner to fetch what the URL serves now, which is how an operator
    /// pulls a new version onto every machine.
    pub have: Option<String>,
}

/// Which instance an operation acts on.
///
/// The UUID is the identifier and the name is the libvirt domain label
/// (SPEC 7.2). Both travel, so the runner never has to look one up from
/// the other, and a rename in flight cannot make it act on the wrong
/// domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceRef {
    pub uuid: String,
    pub name: String,
}

impl Operation {
    /// Whether the operation changes runner state.
    ///
    /// A read-only operation is admitted without recording its epoch. That
    /// is what lets a controller restoring an older database read fencing
    /// state before it may mutate anything (MULTI-NODE 11.4).
    pub fn mutates(&self) -> bool {
        match self {
            Operation::Health
            | Operation::Capabilities
            | Operation::Inventory
            | Operation::Sample => false,
            Operation::EnsureImage { .. }
            | Operation::ApplyNetwork { .. }
            | Operation::ProvisionInstance { .. }
            | Operation::StartInstance { .. }
            | Operation::StopInstance { .. }
            | Operation::RebootInstance { .. }
            | Operation::RemoveInstance { .. } => true,
        }
    }
}

/// What a runner answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Health(Health),
    Capabilities(Capabilities),
    Inventory(Inventory),
    Samples(Samples),
    /// A change that finished. The observed state is what the runner saw
    /// after it acted, so the controller need not poll to learn it.
    Changed {
        state: bento_types::State,
    },
    /// The network state this machine now has. The counts let the
    /// controller log what changed without asking again.
    NetworkApplied {
        bridges: usize,
        routes_added: usize,
        routes_removed: usize,
        routes_unchanged: usize,
    },
    /// An instance built and defined on this machine. The observed state
    /// is what the machine saw after it acted.
    Provisioned {
        state: bento_types::State,
    },
    /// The image version this machine now holds. `checksum` is what the
    /// runner actually has, which is not always what the controller
    /// expected: the URL may have moved on since the controller looked.
    ImageReady {
        name: String,
        checksum: String,
        /// `false` when the runner had to fetch it.
        already_present: bool,
        size: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub protocol_version: u32,
    /// The durable identity of the machine. The controller maps this to
    /// its own host row; the runner does not know that row (MULTI-NODE 16).
    pub machine_id: String,
    pub hostname: String,
    /// The highest controller epoch this runner has accepted.
    pub accepted_epoch: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub arch: String,
    pub cpu_count: i64,
    pub memory_total_mib: i64,
    pub storage_total_gib: i64,
    pub storage_available_gib: i64,
    /// What libvirt reports, so the controller can refuse to place onto a
    /// runner whose hypervisor cannot serve the request.
    pub hypervisor_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    pub domains: Vec<Domain>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Domain {
    pub uuid: String,
    pub name: String,
    pub state: bento_types::State,
}

/// One reading of a machine and everything running on it (SPEC 14.4).
///
/// It carries counters and byte counts, never percentages or series.
/// The controller holds the previous reading, so only the controller can
/// turn a counter into a rate, and it does that the same way for every
/// machine including its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Samples {
    pub host: HostSample,
    /// Only the domains that are running. A stopped domain spends no
    /// processor time and reports no memory.
    pub domains: Vec<DomainUsage>,
}

/// Processor time since boot, in whatever unit the machine counts in.
///
/// Only the difference between two readings is used, so the unit does
/// not travel. A machine that cannot read its counters sends `None`
/// rather than zeroes, which would read as a machine that went idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuTimeSample {
    pub total: u64,
    pub idle: u64,
}

/// What one machine is using.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSample {
    pub cpu: Option<CpuTimeSample>,
    pub memory_total_bytes: u64,
    pub memory_available_bytes: u64,
    /// The volume the machine keeps its overlays on, not every volume.
    pub storage_total_bytes: u64,
    pub storage_available_bytes: u64,
    pub cpu_count: i64,
}

/// What one running domain is using.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainUsage {
    /// The UUID, because that is the identifier the controller keys on
    /// (SPEC 7.2). The name is a label and may have changed.
    pub uuid: String,
    /// Cumulative processor time since the domain started, in
    /// nanoseconds.
    pub cpu_time_ns: u64,
    pub vcpus: u32,
    /// Host memory really backing the guest, in KiB. `None` when the
    /// domain does not report it.
    pub rss_kib: Option<u64>,
    /// The bytes the overlay really occupies, not its virtual size
    /// (SPEC 19). Bytes rather than gibibytes so the wire type stays
    /// exact and the controller does the arithmetic.
    pub storage_used_bytes: u64,
}

/// Why a runner refused, in the words the controller acts on.
///
/// These are refusals, not failures. Each one tells the controller
/// something true about itself: it is too old, it is talking to the wrong
/// machine, or it no longer holds the lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "refused", rename_all = "snake_case")]
pub enum Refusal {
    #[error("runner speaks protocol {theirs}, the request is protocol {yours}")]
    ProtocolVersion { yours: u32, theirs: u32 },
    #[error("this runner is machine {theirs}, the request is addressed to machine {yours}")]
    WrongMachine { yours: String, theirs: String },
    /// A change was asked for without naming the machine it was meant
    /// for. A read may be unaddressed; a change may not.
    #[error("a change must name the machine it is meant for")]
    UnaddressedChange,
    /// A change arrived with no object named, so the runner cannot tell
    /// which generation it belongs to.
    #[error("a change must name the object it changes")]
    UnfencedChange,
    /// A late order from a controller that has been overtaken.
    #[error("generation {yours} for {object_id} is older than the accepted generation {theirs}")]
    StaleGeneration {
        object_id: String,
        yours: i64,
        theirs: i64,
    },
    /// Two requests claim the same generation but want different things.
    /// One of them is wrong, and the runner cannot tell which.
    #[error("generation {generation} for {object_id} was already accepted wanting something else")]
    GenerationConflict { object_id: String, generation: i64 },
    /// A later controller has already spoken to this runner. The sender is
    /// a controller that does not know it has been replaced.
    #[error("epoch {yours} is older than the accepted epoch {theirs}")]
    StaleEpoch { yours: i64, theirs: i64 },
    #[error("the controller lease expired at {at}")]
    LeaseExpired { at: String },
    /// The runner's clock disagrees with the controller by more than the
    /// configured skew, so it cannot judge the lease. It fails closed.
    /// The sign says which way: a positive value means the runner is
    /// ahead of the controller.
    #[error("the runner clock is {skew_seconds}s from the controller's; it cannot judge the lease")]
    ClockUntrusted { skew_seconds: i64 },
}
