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
    Clock, Config, DeleteIso, DeleteIsoFuture, DynError, ISOBuilder, ImageStore, IsoExists,
    LifecycleLogger, Manager, NestedProbe, OverlayResizer, Result, Sleep, Store, UuidMint,
};
pub use new::{GUEST_USER, NewRequest};
pub use reconcile::ReconcileReport;
pub use runner::{QemuImgResizer, RunError, Runner};

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
    /// Copying a live overlay could produce a torn disk image (SPEC 15).
    #[error("lifecycle: the cp source must be stopped: {0}")]
    CopySourceRunning(String),
    /// A domain name can only be changed while it is stopped (SPEC 7.3).
    #[error("lifecycle: stop the instance before renaming it: {0}")]
    RenameNeedsStop(String),
    /// An operation failed, with its orchestration context preserved.
    #[error("{0}")]
    Operation(String),
}

impl Error {
    pub(crate) fn operation(message: impl Into<String>) -> Self {
        Self::Operation(message.into())
    }
}

#[cfg(test)]
mod tests;
