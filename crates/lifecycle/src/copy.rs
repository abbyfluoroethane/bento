use bento_hypervisor::Error as HypervisorError;
use bento_network::GuestNetwork;
use bento_types::{DesiredState, ImageKind, Instance, State, Visibility};

use crate::fs::copy_file;
use crate::manager::{AddressView, parse_prefix};
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
        let source_image = self
            .store
            .image(&source.image_name)
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
        let subnet = parse_prefix(&request.owner.subnet).map_err(|error| {
            Error::operation(format!(
                "lifecycle: user {} has a bad subnet {:?}: {error}",
                request.owner.name, request.owner.subnet
            ))
        })?;
        let address = bento_network::allocate_address(&AddressView(self.store.clone()), subnet)
            .await
            .map_err(|error| Error::operation(error.to_string()))?;
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
        };
        self.store
            .create_instance(instance.clone(), self.cooldown)
            .await
            .map_err(crate::actions::external)?;
        if let Err(error) =
            copy_file(&self.overlay_path(source_uuid), &self.overlay_path(&uuid)).await
        {
            return Err(self
                .unwind_new(&instance, Box::new(error), false, false)
                .await);
        }
        if request.disk_gib > source.disk_gib
            && let Err(error) = self
                .resizer
                .resize_overlay(&self.overlay_path(&uuid), request.disk_gib)
                .await
        {
            return Err(self.unwind_new(&instance, error, true, false).await);
        }
        let guest = match GuestNetwork::new(subnet, address, Some(&self.dns)) {
            Ok(guest) => guest,
            Err(error) => {
                return Err(self
                    .unwind_new(&instance, Box::new(error), true, false)
                    .await);
            }
        };
        let cloud_seed = seed(
            &instance,
            &request,
            &guest,
            source_image.kind != ImageKind::Oci,
        );
        if let Err(error) = self
            .iso
            .build(&cloud_seed, &self.seed_iso_path(&uuid))
            .await
        {
            return Err(self.unwind_new(&instance, error, true, false).await);
        }
        let xml = match self.domain_xml(&instance, &request.owner, true) {
            Ok(xml) => xml,
            Err(error) => {
                return Err(self
                    .unwind_new(&instance, Box::new(error), true, true)
                    .await);
            }
        };
        if let Err(error) = self.hyp.create(&xml).await {
            return Err(self
                .unwind_new(&instance, Box::new(error), true, true)
                .await);
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
