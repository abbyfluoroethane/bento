//! The content-addressed image store, fetch-images pipeline, and qcow2
//! overlay creation (SPEC sections 5.1 and 5.2).

mod fetch;
mod overlay;
mod report;
mod store;

pub use report::{ReportSource, Status, report};
pub use store::{
    CommandRunner, DB, DEFAULT_DIR, Doer, DynError, Error, ReqwestClient, Result, RunError, Runner,
    Store,
};

#[cfg(test)]
mod test_support;
