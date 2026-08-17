//! Public-key authentication, instance forwarding, key linking, and the
//! command line session for the SSH frontend (SPEC sections 10, 13, and 15).
//!
//! One server on one address answers every connection with one host key. The
//! SSH user name field carries the instance name. A stock SSH client always
//! sends a user name -- `ssh bento.foid.space` sends the local login name -- so
//! the frontend cannot demand an empty one: a known key whose user name is not
//! an instance the user can reach runs the command line interface, and an
//! unknown key is offered a link to sign in with, whatever the user name says.

mod server;
mod session;

pub use server::{
    AsyncReadWrite, BoxError, BoxedIo, CLIRunner, Dialer, InstanceStore, KeyLinker, KeyStore,
    PairingRequest, PendingLink, Server, Starter,
};

/// How long the frontend waits for sshd in a freshly started instance
/// (SPEC 10 step 8).
pub const DEFAULT_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The pause between connection attempts during the sshd wait.
pub const DEFAULT_DIAL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// The account cloud-init creates in every instance (SPEC 5.2).
pub const DEFAULT_GUEST_USER: &str = "bento";
