//! The runner service: what one machine does on behalf of the controller
//! (MULTI-NODE 10.2, 11).
//!
//! A Bento deployment has one controller and one runner service for each
//! machine that holds guests, including the controller's own machine. The
//! controller opens every connection; a runner never dials back
//! (MULTI-NODE 11.2).
//!
//! This crate holds the parts that do not touch the host: the wire types,
//! and the fence that decides whether a request may act at all. What a
//! runner actually does to libvirt sits behind the [`Host`] trait, so
//! every rule here is tested without a hypervisor.

pub mod client;
pub mod fence;
pub mod protocol;
pub mod server;
pub mod store;

pub use client::{ClientError, RunnerClient};
pub use fence::{Admission, Fence, FenceStore};
pub use protocol::{
    Capabilities, CpuTimeSample, Domain, DomainUsage, Envelope, Health, HostSample, ImageRequest,
    InstanceRef, Inventory, ObjectFence, Operation, PROTOCOL_VERSION, ProvisionRequest, Refusal,
    Reply, Samples,
};
pub use store::SqliteFence;

/// What a runner can do to the machine it runs on.
///
/// The seam exists so the protocol, the fence, and the dispatch are
/// tested against a fake, with no libvirt and no host (CLAUDE.md).
#[async_trait::async_trait]
pub trait Host: Send + Sync {
    async fn health(&self) -> Result<Health, HostError>;
    async fn capabilities(&self) -> Result<Capabilities, HostError>;
    async fn inventory(&self) -> Result<Inventory, HostError>;
    /// Reads this machine and every running domain on it (SPEC 14.4).
    async fn sample(&self) -> Result<Samples, HostError>;
    /// Makes sure one image version is on this machine.
    async fn ensure_image(&self, image: &ImageRequest) -> Result<Reply, HostError>;
    /// Brings the whole network to the described state (MULTI-NODE 8).
    /// It is idempotent: the same state applied twice changes nothing.
    async fn apply_network(
        &self,
        network: &bento_network::MachineNetwork,
    ) -> Result<Reply, HostError>;
    /// Builds an instance on this machine: overlay, seed image, domain
    /// (MULTI-NODE 13.1).
    async fn provision(&self, request: &ProvisionRequest) -> Result<Reply, HostError>;
    /// Starts a domain that is already defined, and answers with what it
    /// observed afterwards.
    async fn start(&self, instance: &InstanceRef) -> Result<bento_types::State, HostError>;
    async fn stop(&self, instance: &InstanceRef) -> Result<bento_types::State, HostError>;
    async fn reboot(&self, instance: &InstanceRef) -> Result<bento_types::State, HostError>;
    async fn remove(&self, instance: &InstanceRef) -> Result<bento_types::State, HostError>;
}

#[derive(Debug, thiserror::Error)]
#[error("runner host: {0}")]
pub struct HostError(pub String);

impl HostError {
    pub fn new(message: impl std::fmt::Display) -> Self {
        HostError(message.to_string())
    }
}

/// Runs one request against the fence and then the host.
///
/// This is the whole of a runner's decision making. It is deliberately
/// one function: every request takes the same path, in the same order,
/// whatever the operation.
pub async fn serve_one(
    fence: &Fence,
    host: &dyn Host,
    envelope: Envelope,
) -> Result<Outcome, HostError> {
    match fence.admit(&envelope).map_err(HostError::new)? {
        Admission::Refused(refusal) => Ok(Outcome::Refused(refusal)),
        Admission::AlreadyDone(recorded) => Ok(Outcome::Replayed(recorded)),
        Admission::Allowed => {
            let reply = match &envelope.op {
                Operation::Health => Reply::Health(host.health().await?),
                Operation::Capabilities => Reply::Capabilities(host.capabilities().await?),
                Operation::Inventory => Reply::Inventory(host.inventory().await?),
                Operation::Sample => Reply::Samples(host.sample().await?),
                Operation::EnsureImage { image } => host.ensure_image(image).await?,
                Operation::ApplyNetwork { network } => host.apply_network(network).await?,
                Operation::ProvisionInstance { provision } => host.provision(provision).await?,
                Operation::StartInstance { instance } => Reply::Changed {
                    state: host.start(instance).await?,
                },
                Operation::StopInstance { instance } => Reply::Changed {
                    state: host.stop(instance).await?,
                },
                Operation::RebootInstance { instance } => Reply::Changed {
                    state: host.reboot(instance).await?,
                },
                Operation::RemoveInstance { instance } => Reply::Changed {
                    state: host.remove(instance).await?,
                },
            };
            // A change is recorded before the answer leaves, so a retry
            // that arrives after a lost reply is answered from memory
            // rather than run again (MULTI-NODE 11.3).
            if envelope.op.mutates() {
                let recorded = serde_json::to_string(&reply).map_err(HostError::new)?;
                fence
                    .finish(&envelope.request_id, &recorded)
                    .map_err(HostError::new)?;
            }
            Ok(Outcome::Done(reply))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done(Reply),
    /// The same request already ran; this is what it answered.
    Replayed(String),
    Refused(Refusal),
}

#[cfg(test)]
mod tests;
