//! Libvirt domain management, fixed domain XML generation, and host
//! requirement checks (SPEC sections 4.2, 5, and 11).

mod check;
mod client;
mod error;
mod fake;
mod rpc;
mod xdr;
mod xml;

pub use check::{
    CheckConfig, CheckDeps, CheckReport, CheckResult, FileKind, check, default_check_deps,
    nested_enabled,
};
pub use client::{
    AutostartClearer, Client, Definer, DomainInfo, Hypervisor, NetworkManager, StopResult,
};
pub use error::{ERR_NO_DOMAIN, ERR_NO_NETWORK, Error, LibvirtError};
pub use fake::{Fake, FakeDomain};
pub use xml::{ARCH_AMD64, ARCH_ARM64, DomainSpec, domain_xml, host_arch};

/// The traditional local socket for the `qemu:///system` connection.
pub const DEFAULT_SOCKET_PATH: &str = "/var/run/libvirt/libvirt-sock";
