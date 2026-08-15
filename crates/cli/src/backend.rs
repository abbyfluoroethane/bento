use std::error::Error as StdError;
use std::io::{Read, Write};

use async_trait::async_trait;
use bento_hypervisor::StopResult;
use bento_store::Usage;
use bento_types::{Image, Instance, Quota, Share, SshKey, User, Visibility};

/// An error that can cross a CLI integration seam.
pub type BoxError = Box<dyn StdError + Send + Sync + 'static>;

/// A combined session stream used by interactive commands such as console.
pub trait ReadWrite: Read + Write + Send {}

impl<T: Read + Write + Send> ReadWrite for T {}

/// The slice of the data layer used by the CLI. The data store satisfies it;
/// tests use a fake.
#[async_trait]
pub trait Store: Send + Sync {
    async fn user_by_id(&self, id: i64) -> Result<User, BoxError>;
    async fn user_by_name(&self, name: &str) -> Result<User, BoxError>;
    async fn quota_for(&self, user_id: i64) -> Result<Quota, BoxError>;
    async fn usage_for(&self, user_id: i64) -> Result<Usage, BoxError>;
    async fn instance_by_name(&self, name: &str) -> Result<Instance, BoxError>;
    async fn instances_by_owner(&self, owner_id: i64) -> Result<Vec<Instance>, BoxError>;
    async fn instances_shared_with(&self, user_id: i64) -> Result<Vec<Instance>, BoxError>;
    async fn instances(&self) -> Result<Vec<Instance>, BoxError>;
    async fn has_access(&self, instance_uuid: &str, user_id: i64) -> Result<bool, BoxError>;
    async fn add_share(&self, instance_uuid: &str, user_id: i64) -> Result<(), BoxError>;
    async fn remove_share(&self, instance_uuid: &str, user_id: i64) -> Result<(), BoxError>;
    async fn shares_for(&self, instance_uuid: &str) -> Result<Vec<Share>, BoxError>;
    async fn add_ssh_key(
        &self,
        user_id: i64,
        public_key: &str,
        fingerprint: &str,
        comment: &str,
    ) -> Result<i64, BoxError>;
    async fn ssh_keys_for_user(&self, user_id: i64) -> Result<Vec<SshKey>, BoxError>;
    async fn delete_ssh_key(&self, user_id: i64, key_id: i64) -> Result<(), BoxError>;
    async fn images(&self) -> Result<Vec<Image>, BoxError>;
}

#[async_trait]
impl Store for bento_store::Store {
    async fn user_by_id(&self, id: i64) -> Result<User, BoxError> {
        Ok(self.user_by_id(id).await?)
    }
    async fn user_by_name(&self, name: &str) -> Result<User, BoxError> {
        Ok(self.user_by_name(name).await?)
    }
    async fn quota_for(&self, user_id: i64) -> Result<Quota, BoxError> {
        Ok(self.quota_for(user_id).await?)
    }
    async fn usage_for(&self, user_id: i64) -> Result<Usage, BoxError> {
        Ok(self.usage_for(user_id).await?)
    }
    async fn instance_by_name(&self, name: &str) -> Result<Instance, BoxError> {
        Ok(self.instance_by_name(name).await?)
    }
    async fn instances_by_owner(&self, owner_id: i64) -> Result<Vec<Instance>, BoxError> {
        Ok(self.instances_by_owner(owner_id).await?)
    }
    async fn instances_shared_with(&self, user_id: i64) -> Result<Vec<Instance>, BoxError> {
        Ok(self.instances_shared_with(user_id).await?)
    }
    async fn instances(&self) -> Result<Vec<Instance>, BoxError> {
        Ok(self.instances().await?)
    }
    async fn has_access(&self, instance_uuid: &str, user_id: i64) -> Result<bool, BoxError> {
        Ok(self.has_access(instance_uuid, user_id).await?)
    }
    async fn add_share(&self, instance_uuid: &str, user_id: i64) -> Result<(), BoxError> {
        Ok(self.add_share(instance_uuid, user_id).await?)
    }
    async fn remove_share(&self, instance_uuid: &str, user_id: i64) -> Result<(), BoxError> {
        Ok(self.remove_share(instance_uuid, user_id).await?)
    }
    async fn shares_for(&self, instance_uuid: &str) -> Result<Vec<Share>, BoxError> {
        Ok(self.shares_for(instance_uuid).await?)
    }
    async fn add_ssh_key(
        &self,
        user_id: i64,
        public_key: &str,
        fingerprint: &str,
        comment: &str,
    ) -> Result<i64, BoxError> {
        Ok(self
            .add_ssh_key(user_id, public_key, fingerprint, comment)
            .await?)
    }
    async fn ssh_keys_for_user(&self, user_id: i64) -> Result<Vec<SshKey>, BoxError> {
        Ok(self.ssh_keys_for_user(user_id).await?)
    }
    async fn delete_ssh_key(&self, user_id: i64, key_id: i64) -> Result<(), BoxError> {
        Ok(self.delete_ssh_key(user_id, key_id).await?)
    }
    async fn images(&self) -> Result<Vec<Image>, BoxError> {
        Ok(self.images().await?)
    }
}

/// Describes a `new` or the target of a `cp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRequest {
    pub owner_id: i64,
    pub name: String,
    pub image: String,
    pub vcpu: u32,
    pub memory_mib: i64,
    pub disk_gib: i64,
    pub nested: bool,
    pub ksm: bool,
}

/// Carries the changed values of a `resize`; `None` means unchanged (SPEC
/// 11.1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResizeRequest {
    pub vcpu: Option<u32>,
    pub memory_mib: Option<i64>,
    pub disk_gib: Option<i64>,
    pub nested: Option<bool>,
}

/// The consumer-side view of the instance lifecycle actions (SPEC 11.1).
/// Implementations own quota checks, the name cooldown, desired-state
/// bookkeeping, and hypervisor calls.
#[async_trait]
pub trait Lifecycle: Send + Sync {
    /// Builds and starts a new instance (`new`).
    async fn create(&self, request: CreateRequest) -> Result<Instance, BoxError>;
    /// Starts a stopped instance and sets desired state running.
    async fn start(&self, instance: Instance) -> Result<(), BoxError>;
    /// Stops the instance (ACPI request, 60 s wait, then destroy) and reports
    /// which path the stop took.
    async fn stop(&self, instance: Instance) -> Result<StopResult, BoxError>;
    /// Reboots the instance.
    async fn restart(&self, instance: Instance) -> Result<(), BoxError>;
    /// Runs the four `rm` steps of SPEC 11.1 in order.
    async fn remove(&self, instance: Instance) -> Result<(), BoxError>;
    /// Renames the instance; the old name enters cooldown (SPEC 7.3). No
    /// alias or redirect is created.
    async fn rename(&self, instance: Instance, new_name: &str) -> Result<(), BoxError>;
    /// Clones a stopped source into a new instance (`cp`).
    async fn copy(&self, source: Instance, request: CreateRequest) -> Result<Instance, BoxError>;
    /// Applies a resize; the change needs a restart.
    async fn resize(&self, instance: Instance, request: ResizeRequest) -> Result<(), BoxError>;
    /// Attaches the serial console until the stream or console closes.
    async fn console(&self, instance: Instance, rw: &mut dyn ReadWrite) -> Result<(), BoxError>;
    /// Sets the default HTTP port. Published ports feed the nftables table,
    /// and SPEC 6.3 reloads the whole table on every change.
    async fn set_http_port(&self, instance: Instance, port: u16) -> Result<(), BoxError>;
    /// Sets visibility, which also changes the published ports; the same
    /// SPEC 6.3 reload applies.
    async fn set_visibility(
        &self,
        instance: Instance,
        visibility: Visibility,
    ) -> Result<(), BoxError>;
}
