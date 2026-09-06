//! Instance lifecycle orchestration, desired-state tracking, and host reboot
//! restoration (SPEC section 11).
//!
//! This is the control-plane ordering layer. It drives injected data, image,
//! cloud-init, and hypervisor consumers and owns every unwind path between
//! them. Host-dependent behavior stays behind an injected seam.

mod actions;
mod copy;
mod fs;
mod manager;
mod new;
mod poller;
mod reconcile;
mod rename;
mod restore;
mod runner;

pub use actions::{ResizeRequest, ResizeResult};
pub use manager::{
    Clock, Config, DeleteIso, DeleteIsoFuture, DynError, Fleet, ISOBuilder, IsoExists,
    LifecycleLogger, Manager, NestedProbe, OverlayResizer, ProvisionSpec, Result, Sleep, Store,
    UuidMint, random_uuid,
};
pub use new::{GUEST_USER, NewRequest};
pub use reconcile::ReconcileReport;
pub use runner::{CommandRunner, QemuImgResizer, RunError};

use thiserror::Error as ThisError;

/// Errors returned by lifecycle actions.
#[derive(Debug, ThisError)]
pub enum Error {
    /// A required manager dependency was omitted.
    #[error("lifecycle: config needs {0}")]
    Config(&'static str),
    /// Nested virtualization was requested while the KVM module has it off.
    #[error("lifecycle: nested virtualization is off on this host: {0}")]
    NestedUnavailable(String),
    /// Version 1 only grows overlays (SPEC 11.1).
    #[error("lifecycle: disk size cannot shrink: {0}")]
    DiskShrink(String),
    /// An allowlisted image has not yet been fetched (SPEC 5.1).
    #[error("lifecycle: image has no fetched version; run fetch-images: {0}")]
    NoImageVersion(String),
    /// A create must use one image version across the deployment
    /// (MULTI-NODE 13.2).
    #[error("the fleet is still fetching {image_name}: {missing}")]
    FleetImageNotReady { image_name: String, missing: String },
    /// A machine owns no active runner slot, so it has no address range
    /// to allocate from (MULTI-NODE 7.2). A machine that has joined the
    /// fleet but has not been given a slot is in this state.
    #[error("host {host_id} owns no active runner slot; give it one before placing an instance")]
    NoSlotForHost { host_id: i64 },
    /// Copying a live overlay could produce a torn disk image (SPEC 15).
    #[error("lifecycle: the cp source must be stopped: {0}")]
    CopySourceRunning(String),
    /// A domain name can only be changed while it is stopped (SPEC 7.3).
    #[error("lifecycle: stop the instance before renaming it: {0}")]
    RenameNeedsStop(String),
    /// An operation failed, with its orchestration context preserved.
    #[error("{0}")]
    Operation(String),
    /// An operation failed in a dependency, with the cause kept as the
    /// error source.
    ///
    /// The text alone is not enough. A caller has to be able to tell a
    /// capacity refusal from a name cooldown from a missing row, because
    /// each has its own answer (SPEC 6.1, 7.2, 12): the HTTP layer
    /// matches on the store's error type and returns 409 with the figures,
    /// 409 with the remaining cooldown, or 404. Flattening the cause to a
    /// string erased that type and turned every one of them into a 500.
    #[error("{message}")]
    Caused {
        message: String,
        #[source]
        source: DynError,
    },
}

impl Error {
    pub(crate) fn operation(message: impl Into<String>) -> Self {
        Self::Operation(message.into())
    }

    /// Wraps a dependency's error, keeping both its text and its type.
    pub(crate) fn caused(source: impl Into<DynError>) -> Self {
        let source = source.into();
        Self::Caused {
            message: source.to_string(),
            source,
        }
    }
}

#[cfg(test)]
mod tests;
