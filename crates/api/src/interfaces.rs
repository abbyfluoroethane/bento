use std::error::Error as StdError;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use bento_store::Usage;
use bento_types::{Instance, Quota, Share, SshKey, User, Visibility};

/// An error returned through a consumer-side interface.
pub type BoxError = Box<dyn StdError + Send + Sync + 'static>;

/// An error whose producer chooses the HTTP response status.
///
/// Lifecycle and authentication implementations use this when a failure has
/// a meaningful client-facing status. Errors without this wrapper receive the
/// safe default of HTTP 500.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct StatusError {
    status: StatusCode,
    message: String,
}

/// Persistence failures whose response carries structured or stable API
/// semantics. The binary maps corresponding data-layer errors into these
/// variants when wiring [`Store`] and [`Lifecycle`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store: not found")]
    NotFound,
    #[error("store: name is taken by an existing instance")]
    NameTaken,
    #[error("store: quota exceeded: {limit} limit is {max}, {used} in use, {requested} requested")]
    Quota {
        limit: String,
        used: i64,
        requested: i64,
        max: i64,
    },
    #[error(
        "store: name {name:?} was released by another user and is in cooldown for another {remaining:?}"
    )]
    NameCooldown {
        name: String,
        remaining: std::time::Duration,
    },
}

impl StatusError {
    /// Builds an error with an explicit HTTP status.
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// Returns the status selected by the producer.
    pub fn http_status(&self) -> StatusCode {
        self.status
    }
}

/// The subset of the data layer that the API reads and writes directly.
/// The binary wires the persistence implementation; tests use a fake.
#[async_trait]
pub trait Store: Send + Sync + 'static {
    async fn user_by_id(&self, id: i64) -> Result<User, BoxError>;
    async fn user_by_name(&self, name: &str) -> Result<User, BoxError>;
    async fn quota_for(&self, user_id: i64) -> Result<Quota, BoxError>;
    async fn usage_for(&self, user_id: i64) -> Result<Usage, BoxError>;

    async fn instance(&self, uuid: &str) -> Result<Instance, BoxError>;
    async fn instances_by_owner(&self, owner_id: i64) -> Result<Vec<Instance>, BoxError>;
    async fn instances_shared_with(&self, user_id: i64) -> Result<Vec<Instance>, BoxError>;
    async fn instances(&self) -> Result<Vec<Instance>, BoxError>;

    async fn add_share(&self, instance_uuid: &str, user_id: i64) -> Result<(), BoxError>;
    async fn remove_share(&self, instance_uuid: &str, user_id: i64) -> Result<(), BoxError>;
    async fn shares_for(&self, instance_uuid: &str) -> Result<Vec<Share>, BoxError>;

    async fn images(&self) -> Result<Vec<bento_types::Image>, BoxError>;

    async fn add_ssh_key(
        &self,
        user_id: i64,
        public_key: &str,
        fingerprint: &str,
        comment: &str,
    ) -> Result<i64, BoxError>;
    async fn ssh_keys_for_user(&self, user_id: i64) -> Result<Vec<SshKey>, BoxError>;
    async fn delete_ssh_key(&self, user_id: i64, key_id: i64) -> Result<(), BoxError>;

    /// Writes a consistent snapshot with the SQLite backup API (SPEC 12.1).
    /// `destination` must not exist.
    async fn dump_db(&self, destination: &Path) -> Result<(), BoxError>;
}

/// What the dashboard sends to create an instance. Zero resource values mean
/// "use the operator default"; the lifecycle layer applies those defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateSpec {
    pub name: String,
    pub image: String,
    pub vcpu: u32,
    pub memory_mib: i64,
    pub disk_gib: i64,
    pub nested: bool,
    pub ksm: bool,
}

/// The full target shape for a resize. The handler fills omitted fields from
/// the current row, so the lifecycle always receives a complete spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeSpec {
    pub vcpu: u32,
    pub memory_mib: i64,
    pub disk_gib: i64,
    pub nested: bool,
}

/// The consumer-side view of instance lifecycle orchestration (SPEC 11.1).
/// Every operation touching libvirt, an overlay, the firewall, or cloud-init
/// goes through this interface.
#[async_trait]
pub trait Lifecycle: Send + Sync + 'static {
    async fn create(&self, owner: User, spec: CreateSpec) -> Result<Instance, BoxError>;
    async fn delete(&self, uuid: &str) -> Result<(), BoxError>;
    async fn start(&self, uuid: &str) -> Result<(), BoxError>;
    async fn stop(&self, uuid: &str) -> Result<(), BoxError>;
    async fn restart(&self, uuid: &str) -> Result<(), BoxError>;
    async fn rename(&self, uuid: &str, new_name: &str) -> Result<(), BoxError>;
    async fn resize(&self, uuid: &str, spec: ResizeSpec) -> Result<(), BoxError>;

    /// This lives on the lifecycle because a port change must also reload the
    /// nftables table (SPEC 6.3 rule 1).
    async fn set_http_port(&self, uuid: &str, port: u16) -> Result<(), BoxError>;

    /// This lives here for the same reason: published ports follow visibility,
    /// and SPEC 6.3 reloads the whole table on every change.
    async fn set_visibility(&self, uuid: &str, visibility: Visibility) -> Result<(), BoxError>;
}

/// Resolves the user behind a request. An error means the request is
/// unauthenticated and the API answers HTTP 401 (SPEC 13).
#[async_trait]
pub trait Authenticator: Send + Sync + 'static {
    async fn user_from_request(&self, request: &Request<Body>) -> Result<User, BoxError>;
}

/// The operator-only route predicate supplied by the binary.
pub type OperatorPredicate = Arc<dyn Fn(&User) -> bool + Send + Sync>;
