use bento_hypervisor::{Error as HypervisorError, StopResult};
use bento_types::{DesiredState, Instance, State};

use crate::fs::remove_file;
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
        self.hyp.start(&instance.name).await.map_err(|error| {
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
        let result = self.hyp.stop(&instance.name).await.map_err(|error| {
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
        self.hyp.reboot(&instance.name).await.map_err(|error| {
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
        match self.hyp.remove(&instance.name).await {
            Ok(()) | Err(HypervisorError::DomainNotFound(_)) => {}
            Err(error) => {
                return Err(Error::operation(format!(
                    "lifecycle: rm {}: {error}",
                    instance.name
                )));
            }
        }
        remove_file(&self.overlay_path(uuid))
            .await
            .map_err(|error| {
                Error::operation(format!(
                    "lifecycle: rm {}: delete overlay: {error}",
                    instance.name
                ))
            })?;
        if let Err(error) = (self.delete_iso)(self.seed_iso_path(uuid)).await {
            self.log.warn(&format!(
                "rm: seed iso not deleted for {}: {error}",
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

    /// Changes vCPU, memory, disk, and nesting (SPEC 11.1). Only disk growth
    /// is supported; quota is checked before any host mutation.
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
        self.store
            .resize(
                &request.uuid,
                request.vcpu,
                request.memory_mib,
                request.disk_gib,
                request.nested,
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

pub(crate) fn external(error: crate::DynError) -> Error {
    Error::operation(error.to_string())
}
