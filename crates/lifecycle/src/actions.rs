use bento_hypervisor::{Error as HypervisorError, StopResult};
use bento_types::{DesiredState, Instance, State};

use crate::{Error, Manager, Result};

/// The complete target shape of an instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizeRequest {
    pub uuid: String,
    pub vcpu: u32,
    pub memory_mib: i64,
    pub disk_gib: i64,
    pub nested: bool,
}

/// What a resize changed and what the caller must explain to the user.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResizeResult {
    pub restart_required: bool,
    pub disk_grown: bool,
}

impl Manager {
    /// Starts a stopped instance and records desired running (SPEC 11.1).
    pub async fn start(&self, uuid: &str) -> Result<()> {
        let instance = self.store.instance(uuid).await.map_err(external)?;
        self.hyp_for(&instance)
            .await?
            .start(&instance.name)
            .await
            .map_err(|error| {
                Error::operation(format!("lifecycle: start {}: {error}", instance.name))
            })?;
        self.store
            .set_desired_state(uuid, DesiredState::Running)
            .await
            .map_err(external)?;
        self.store
            .set_observed_state(uuid, State::Running)
            .await
            .map_err(external)
    }

    /// Requests ACPI shutdown, waits, and only then destroys. Desired stopped
    /// is persisted before the wait so a crash cannot restore it running
    /// (SPEC 11.1).
    pub async fn stop(&self, uuid: &str) -> Result<StopResult> {
        let instance = self.store.instance(uuid).await.map_err(external)?;
        self.store
            .set_desired_state(uuid, DesiredState::Stopped)
            .await
            .map_err(external)?;
        let result = self
            .hyp_for(&instance)
            .await?
            .stop(&instance.name)
            .await
            .map_err(|error| {
                Error::operation(format!("lifecycle: stop {}: {error}", instance.name))
            })?;
        self.store
            .set_observed_state(uuid, State::Stopped)
            .await
            .map_err(external)?;
        self.log.info(&format!(
            "stop: instance {} stopped via {result:?}",
            instance.name
        ));
        Ok(result)
    }

    /// Reboots and records desired running (SPEC 11.1).
    pub async fn restart(&self, uuid: &str) -> Result<()> {
        let instance = self.store.instance(uuid).await.map_err(external)?;
        self.hyp_for(&instance)
            .await?
            .reboot(&instance.name)
            .await
            .map_err(|error| {
                Error::operation(format!("lifecycle: restart {}: {error}", instance.name))
            })?;
        self.store
            .set_desired_state(uuid, DesiredState::Running)
            .await
            .map_err(external)
    }

    /// Performs the four ordered removal steps of SPEC 11.1: domain,
    /// overlay, shares, then released name. The store combines the final two
    /// in one transaction. Nothing invokes this on a timer: Bento never
    /// deletes an instance on its own (SPEC section 3).
    pub async fn remove(&self, uuid: &str) -> Result<()> {
        let instance = self.store.instance(uuid).await.map_err(external)?;
        match self.hyp_for(&instance).await?.remove(&instance.name).await {
            Ok(()) | Err(HypervisorError::DomainNotFound(_)) => {}
            Err(error) => {
                return Err(Error::operation(format!(
                    "lifecycle: rm {}: {error}",
                    instance.name
                )));
            }
        }
        // The disk and the seed image are on the machine that ran the
        // instance, and only that machine can delete them. It does so
        // when it undefines the domain, so this call is a no-op for an
        // instance that ran elsewhere (MULTI-NODE 13.1).
        if let Err(error) = self.fleet.deprovision(instance.host_id, &instance).await {
            self.log.warn(&format!(
                "rm: files not deleted for {}: {error}",
                instance.name
            ));
        }
        self.store.delete_instance(uuid).await.map_err(|error| {
            Error::operation(format!("lifecycle: rm {}: {error}", instance.name))
        })?;
        self.log
            .info(&format!("rm: instance {} removed", instance.name));
        Ok(())
    }

    /// Changes vCPU, memory, disk, and nesting (SPEC 11.1). Only disk
    /// growth is supported, and host capacity is checked before any host
    /// mutation.
    pub async fn resize(&self, request: ResizeRequest) -> Result<ResizeResult> {
        let mut instance = self.store.instance(&request.uuid).await.map_err(external)?;
        if request.vcpu == 0 || request.memory_mib <= 0 || request.disk_gib <= 0 {
            return Err(Error::operation(
                "lifecycle: resize needs positive vcpu, memory, and disk",
            ));
        }
        if request.disk_gib < instance.disk_gib {
            return Err(Error::DiskShrink(format!(
                "{} has {} GiB, requested {} GiB",
                instance.name, instance.disk_gib, request.disk_gib
            )));
        }
        if request.nested && !instance.nested {
            self.check_nested()?;
        }
        let result = ResizeResult {
            restart_required: request.vcpu != instance.vcpu
                || request.memory_mib != instance.memory_mib
                || request.nested != instance.nested,
            disk_grown: request.disk_gib > instance.disk_gib,
        };
        // Weighed against the ceiling of the machine this instance runs
        // on (MULTI-NODE 12).
        let capacity = self
            .store
            .host_capacity(instance.host_id)
            .await
            .map_err(external)?;
        self.store
            .resize(
                &request.uuid,
                request.vcpu,
                request.memory_mib,
                request.disk_gib,
                request.nested,
                capacity,
            )
            .await
            .map_err(external)?;
        if result.disk_grown {
            self.resizer
                .resize_overlay(&self.overlay_path(&request.uuid), request.disk_gib)
                .await
                .map_err(external)?;
        }
        if result.restart_required {
            instance.vcpu = request.vcpu;
            instance.memory_mib = request.memory_mib;
            instance.disk_gib = request.disk_gib;
            instance.nested = request.nested;
            self.redefine(&instance).await?;
        }
        Ok(result)
    }

    pub(crate) async fn redefine(&self, instance: &Instance) -> Result<()> {
        if instance.host_id != self.host_id {
            // The local definer would put the domain on this machine, and
            // the instance runs on another one (MULTI-NODE 11.5).
            return self.redefine_elsewhere(instance, None).await.map(|_| ());
        }
        let Some(definer) = &self.definer else {
            self.log.warn("redefine: hypervisor cannot redefine XML; stored configuration applies at the next redefine");
            return Ok(());
        };
        let xml = self
            .domain_xml_by_uuid(
                instance,
                (self.iso_exists)(&self.seed_iso_path(&instance.uuid)),
            )
            .await?;
        definer.define(&xml).await.map_err(|error| {
            Error::operation(format!("lifecycle: redefine {}: {error}", instance.name))
        })
    }
}

impl Manager {
    /// Asks the machine that runs `instance` to define it again, with the
    /// facts that machine renders into its own XML (MULTI-NODE 11.5).
    /// `previous_name` is the domain name libvirt has now, for a rename.
    pub(crate) async fn redefine_elsewhere(
        &self,
        instance: &Instance,
        previous_name: Option<&str>,
    ) -> Result<State> {
        let owner = self
            .store
            .user_by_id(instance.owner_id)
            .await
            .map_err(|error| {
                Error::operation(format!("lifecycle: owner of {}: {error}", instance.name))
            })?;
        let spec = crate::RedefineSpec {
            host_id: instance.host_id,
            instance: instance.clone(),
            network: self.user_network_name(&owner)?,
            previous_name: previous_name.map(str::to_owned),
        };
        self.fleet.redefine(&spec).await.map_err(|error| {
            Error::operation(format!("lifecycle: redefine {}: {error}", instance.name))
        })
    }
}

/// The funnel for every error a dependency hands the lifecycle layer.
/// It keeps the cause as the error source so the HTTP layer can still
/// recognize a store error and answer it properly.
pub(crate) fn external(error: crate::DynError) -> Error {
    Error::caused(error)
}
