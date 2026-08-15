//! NoCloud cloud-init seed ISO construction for instance first boot
//! (SPEC section 5.2).

mod builder;
mod seed;

pub use builder::{Builder, CommandError, Error, Runner, delete};
pub use seed::Seed;
