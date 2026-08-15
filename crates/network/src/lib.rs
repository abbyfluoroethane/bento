//! Per-user libvirt networks, address allocation, MAC assignment, and
//! the Bento nftables table (SPEC sections 6.2 and 6.3).

mod apply;
mod libvirt;
mod mac;
mod nftables;
mod subnet;

pub use apply::{Applier, NftApplier, reload};
pub use bento_config::Ipv4Prefix;
pub use libvirt::UserNetwork;
pub use mac::mac;
pub use nftables::{FirewallUser, PortRange, PublishedInstance, Ruleset};
pub use subnet::{
    AddressStore, DEFAULT_DNS, GuestNetwork, Plan, SubnetStore, allocate_address, gateway,
};

/// An error from network planning, rendering, or policy application.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network: {0}")]
    Invalid(String),
    /// Every `/24` in the private range is assigned to a user.
    #[error("network: no free /24 subnet in the private range")]
    SubnetsExhausted,
    /// Every usable address in a user subnet is assigned to an instance.
    #[error("network: no free address in the subnet")]
    AddressesExhausted,
    #[error("network: list used subnets: {0}")]
    ListUsedSubnets(#[source] DynError),
    #[error("network: list used addresses: {0}")]
    ListUsedAddresses(#[source] DynError),
    #[error("{0}")]
    Apply(#[source] DynError),
}

/// The result type used throughout this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A thread-safe dynamic error returned through an injection seam.
pub type DynError = Box<dyn std::error::Error + Send + Sync + 'static>;

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}
