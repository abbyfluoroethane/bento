use bento_hypervisor::Error as HypervisorError;
use bento_network::GuestNetwork;
use bento_types::{DesiredState, ImageKind, Instance, State, Visibility};

use crate::fs::copy_file;
use crate::manager::parse_prefix;
use crate::new::seed;
use crate::{Error, Manager, NewRequest, Result};

impl Manager {
    /// Clones a stopped instance (SPEC 15). The clone retains the exact
    /// backing image version and receives its own UUID, name, network identity,
    /// and seed. Its disk may grow but never shrink.
    pub async fn copy(&self, source_uuid: &str, request: NewRequest) -> Result<Instance> {
        let source = self
            .store
            .instance(source_uuid)
            .await
            .map_err(crate::actions::external)?;
        let source_version = self
            .store
            .image_version(&source.base_checksum)
            .await
            .map_err(crate::actions::external)?;
        if request.name.is_empty() {
            return Err(Error::operation("lifecycle: cp needs a target name"));
        }
        if request.vcpu == 0 || request.memory_mib <= 0 || request.disk_gib <= 0 {
            return Err(Error::operation(
                "lifecycle: cp needs positive vcpu, memory, and disk",
            ));
        }
        if request.disk_gib < source.disk_gib {
            return Err(Error::DiskShrink(format!(
                "{} has {} GiB, requested {} GiB",
                source.name, source.disk_gib, request.disk_gib
            )));
        }
        if request.nested {
            self.check_nested()?;
        }
        match self.hyp.state(&source.name).await {
            Ok(State::Stopped) | Err(HypervisorError::DomainNotFound(_)) => {}
            Ok(state) => {
                return Err(Error::CopySourceRunning(format!(
                    "{} is {state}",
                    source.name
                )));
            }
            Err(error) => {
                return Err(Error::operation(format!(
                    "lifecycle: cp {}: {error}",
                    source.name
                )));
            }
        }
        // A copy stays on its source's machine, because copying an
        // overlay is a local file copy (MULTI-NODE 13.3). Copying one
        // between machines is a transfer with its own verification, which
        // is the slot-move workflow of section 17 and not this.
        if source.host_id != self.host_id {
            return Err(Error::operation(format!(
                "lifecycle: cp {}: it runs on another machine, and copying \
                 between machines is not implemented",
                source.name
            )));
        }
        let subnet = parse_prefix(&request.owner.subnet).map_err(|error| {
            Error::operation(format!(
                "lifecycle: user {} has a bad subnet {:?}: {error}",
                request.owner.name, request.owner.subnet
            ))
        })?;
        let (address, slot) =
            crate::manager::allocate_in_owned_slot(&self.store, request.host_id, subnet).await?;
        // A copy stays on its source's machine, so it is weighed against
        // that machine's ceiling (MULTI-NODE 12, 13.3).
        let capacity = self
            .store
            .host_capacity(request.host_id)
            .await
            .map_err(Error::caused)?;
        let uuid = (self.new_uuid)();
        let mut instance = Instance {
            uuid: uuid.clone(),
            name: request.name.clone(),
            owner_id: request.owner.id,
            host_id: request.host_id,
            image_name: source.image_name.clone(),
            base_checksum: source.base_checksum.clone(),
            state: State::Stopped,
            desired_state: DesiredState::Running,
            address: address.to_string(),
            mac: bento_network::mac(&uuid),
            vcpu: request.vcpu,
            memory_mib: request.memory_mib,
            disk_gib: request.disk_gib,
            nested: request.nested,
            ksm: !request.disable_ksm,
            http_port: request.http_port,
            visibility: Visibility::Off,
            created_at: (self.now)().to_offset(time::UtcOffset::UTC),
            last_seen_at: None,
            // A copy stays on the source's runner (MULTI-NODE 13.3), but
            // it gets its own address, so it records the slot that address
            // came from rather than the source's. The two differ when the
            // source's slot is full and the machine owns another.
            slot: Some(slot),
        };
        self.store
            .create_instance(instance.clone(), self.cooldown, capacity)
            .await
            .map_err(crate::actions::external)?;
        if let Err(error) =
            copy_file(&self.overlay_path(source_uuid), &self.overlay_path(&uuid)).await
        {
            return Err(self.unwind_copy(&instance, Box::new(error)).await);
        }
        if request.disk_gib > source.disk_gib
            && let Err(error) = self
                .resizer
                .resize_overlay(&self.overlay_path(&uuid), request.disk_gib)
                .await
        {
            return Err(self.unwind_copy(&instance, error).await);
        }
        let guest = match GuestNetwork::new(subnet, address, Some(&self.dns)) {
            Ok(guest) => guest,
            Err(error) => {
                return Err(self.unwind_copy(&instance, Box::new(error)).await);
            }
        };
        let cloud_seed = seed(
            &instance,
            &request,
            &guest,
            source_version.kind != ImageKind::Oci,
        );
        if let Err(error) = self
            .iso
            .build(&cloud_seed, &self.seed_iso_path(&uuid))
            .await
        {
            return Err(self.unwind_copy(&instance, error).await);
        }
        let xml = match self.domain_xml(&instance, &request.owner, true) {
            Ok(xml) => xml,
            Err(error) => {
                return Err(self.unwind_copy(&instance, Box::new(error)).await);
            }
        };
        if let Err(error) = self.hyp.create(&xml).await {
            return Err(self.unwind_copy(&instance, Box::new(error)).await);
        }
        instance.state = State::Running;
        if let Err(error) = self.store.set_observed_state(&uuid, State::Running).await {
            self.log.warn(&format!(
                "cp: observed state not recorded; poller will catch up: {error}"
            ));
        }
        Ok(instance)
    }
}
