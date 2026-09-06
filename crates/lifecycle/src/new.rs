use bento_cloudinit::Seed;
use bento_network::GuestNetwork;
use bento_types::{DesiredState, ImageKind, Instance, Placing, State, User, Visibility};

use crate::fs::remove_file;
use crate::manager::parse_prefix;
use crate::{Error, Manager, Result};

/// The one account cloud-init creates in every instance (SPEC 5.2). A fixed
/// name lets the SSH frontend authenticate to every guest (SPEC 10 step 9).
pub const GUEST_USER: &str = "bento";

/// Everything the frontend resolved for a new instance.
#[derive(Debug, Clone)]
pub struct NewRequest {
    pub name: String,
    pub owner: User,
    /// The machine that received the request.
    ///
    /// It is used only when the deployment has one machine that can take
    /// an instance. With more than one, placement chooses, and this is
    /// ignored (MULTI-NODE 12).
    pub host_id: i64,
    /// Owner public keys installed by cloud-init (SPEC 5.2).
    pub ssh_keys: Vec<String>,
    pub image_name: String,
    pub vcpu: u32,
    pub memory_mib: i64,
    pub disk_gib: i64,
    pub nested: bool,
    /// The zero value keeps same-page merging enabled (SPEC 5.4).
    pub disable_ksm: bool,
    pub http_port: u16,
}

impl Manager {
    /// Creates a capacity-checked row, assigns network identity, creates
    /// the overlay and seed, then defines and starts the domain, in order
    /// (SPEC sections 5.2, 6.1, and 11.1). Failure after insertion unwinds
    /// all partial work so retry starts clean.
    pub async fn create(&self, request: NewRequest) -> Result<Instance> {
        if request.name.is_empty() {
            return Err(Error::operation("lifecycle: new needs a name"));
        }
        if request.image_name.is_empty() {
            return Err(Error::operation("lifecycle: new needs an image"));
        }
        if request.vcpu == 0 || request.memory_mib <= 0 || request.disk_gib <= 0 {
            return Err(Error::operation(
                "lifecycle: new needs positive vcpu, memory, and disk",
            ));
        }
        if request.nested {
            self.check_nested()?;
        }
        let image = self
            .store
            .image(&request.image_name)
            .await
            .map_err(|error| {
                Error::operation(format!(
                    "lifecycle: image {:?}: {error}",
                    request.image_name
                ))
            })?;
        let checksum = image
            .current_checksum
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::NoImageVersion(request.image_name.clone()))?;
        let missing = self
            .store
            .hosts_missing_image(&request.image_name, &checksum)
            .await
            .map_err(Error::caused)?;
        if !missing.is_empty() {
            let verb = if missing.len() == 1 { "is" } else { "are" };
            return Err(Error::FleetImageNotReady {
                image_name: request.image_name.clone(),
                missing: format!("{} {verb} not ready", missing.join(", ")),
            });
        }
        let image_version = self.store.image_version(&checksum).await.map_err(|error| {
            Error::operation(format!("lifecycle: image version {checksum:?}: {error}"))
        })?;
        let subnet = parse_prefix(&request.owner.subnet).map_err(|error| {
            Error::operation(format!(
                "lifecycle: user {} has a bad subnet {:?}: {error}",
                request.owner.name, request.owner.subnet
            ))
        })?;
        let host_id = self.place(&request).await?;
        let (address, slot) =
            crate::manager::allocate_in_owned_slot(&self.store, host_id, subnet).await?;
        let uuid = (self.new_uuid)();
        let mut instance = Instance {
            uuid: uuid.clone(),
            name: request.name.clone(),
            owner_id: request.owner.id,
            host_id,
            image_name: request.image_name.clone(),
            base_checksum: checksum.clone(),
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
            // The address came from a slot this machine owns, so the
            // address alone says where the instance runs (MULTI-NODE 7.2).
            slot: Some(slot),
        };
        // The ceiling of the machine the instance will run on, not this
        // one. They are the same on a deployment with one machine
        // (MULTI-NODE 12).
        let capacity = self
            .store
            .host_capacity(host_id)
            .await
            .map_err(Error::caused)?;
        self.store
            .create_instance(instance.clone(), self.cooldown, capacity)
            .await
            .map_err(Error::caused)?;
        let guest = match GuestNetwork::new(subnet, address, Some(&self.dns)) {
            Ok(guest) => guest,
            Err(error) => {
                return Err(self.unwind_new(&instance, Box::new(error)).await);
            }
        };
        let network = match self.user_network_name(&request.owner) {
            Ok(name) => name,
            Err(error) => return Err(self.unwind_new(&instance, Box::new(error)).await),
        };
        let spec = crate::manager::ProvisionSpec {
            host_id,
            instance: instance.clone(),
            network,
            seed: seed(
                &instance,
                &request,
                &guest,
                image_version.kind != ImageKind::Oci,
            ),
            // A bootc image bakes its packages in, so it needs no seed
            // drive for the guest agent (SPEC 5.2).
            with_seed_iso: true,
            start: true,
        };
        // The machine that will run the instance builds it. It cleans up
        // its own partial work; only the row is left for this side to
        // remove, because a controller cannot delete a file on another
        // machine (MULTI-NODE 13.1).
        let observed = match self.fleet.provision(&spec).await {
            Ok(state) => state,
            Err(error) => return Err(self.unwind_new(&instance, error).await),
        };
        instance.state = observed;
        if let Err(error) = self.store.set_observed_state(&uuid, observed).await {
            self.log.warn(&format!(
                "new: observed state not recorded; poller will catch up: {error}"
            ));
        }
        self.log
            .info(&format!("new: instance {} created", request.name));
        Ok(instance)
    }

    /// Chooses the machine for a new instance (MULTI-NODE 12).
    ///
    /// A deployment with one machine that can take an instance keeps
    /// using it, which is what version 1 did and what a single-machine
    /// deployment still is. Placement would otherwise make every create
    /// depend on a health poll such a deployment need not run.
    async fn place(&self, request: &NewRequest) -> Result<i64> {
        let placeable = self.store.placeable_hosts().await.map_err(Error::caused)?;
        if placeable <= 1 {
            return Ok(request.host_id);
        }
        self.store
            .choose_host(Placing {
                vcpu: i64::from(request.vcpu),
                memory_mib: request.memory_mib,
                disk_gib: request.disk_gib,
                // An image version carries no architecture of its own:
                // the allowlist URL names one build, and the controller
                // fetched it for the architecture it runs. Bento runs
                // only KVM guests of the host's architecture, so a
                // machine that does not match cannot boot this image
                // whatever room it has (MULTI-NODE 12).
                arch: Some(std::env::consts::ARCH.to_owned()),
            })
            .await
            .map_err(Error::caused)
    }

    /// The libvirt network of an owner, by the deterministic name every
    /// machine derives from the subnet index (SPEC 6.2).
    pub(crate) fn user_network_name(&self, owner: &User) -> Result<String> {
        let subnet = parse_prefix(&owner.subnet).map_err(|error| {
            Error::operation(format!(
                "lifecycle: user {} has a bad subnet {:?}: {error}",
                owner.name, owner.subnet
            ))
        })?;
        let index = self.plan.index(subnet).map_err(|error| {
            Error::operation(format!(
                "lifecycle: subnet {}/{} outside the private range: {error}",
                subnet.addr, subnet.bits
            ))
        })?;
        Ok(bento_network::UserNetwork::new(self.plan, index as isize)
            .map_err(Error::caused)?
            .name)
    }

    /// Removes the row and the local files of a copy that failed.
    ///
    /// A copy is built on this machine, so this side made the files and
    /// this side removes them. That is the difference from
    /// [`Manager::unwind_new`], where the files are on the machine that
    /// tried to build the instance.
    pub(crate) async fn unwind_copy(&self, instance: &Instance, cause: crate::DynError) -> Error {
        let mut errors = vec![cause.to_string()];
        if let Err(error) = (self.delete_iso)(self.seed_iso_path(&instance.uuid)).await {
            errors.push(format!("unwind seed iso: {error}"));
        }
        if let Err(error) = remove_file(&self.overlay_path(&instance.uuid)).await {
            errors.push(format!("unwind overlay: {error}"));
        }
        if let Err(error) = self.store.delete_instance(&instance.uuid).await {
            errors.push(format!("unwind instance row: {error}"));
        }
        self.log
            .warn(&format!("cp: failed, partial work unwound: {cause}"));
        Error::operation(format!(
            "lifecycle: cp {}: {}",
            instance.name,
            errors.join(": ")
        ))
    }

    /// Removes the row of an instance that could not be built.
    ///
    /// The files are not removed here. The machine that tried to build
    /// the instance removed its own partial work before it reported the
    /// failure, and it is the only side that can: a controller cannot
    /// delete a file on another machine (MULTI-NODE 13.1).
    pub(crate) async fn unwind_new(&self, instance: &Instance, cause: crate::DynError) -> Error {
        let mut errors = vec![cause.to_string()];
        if let Err(error) = self.store.delete_instance(&instance.uuid).await {
            errors.push(format!("unwind instance row: {error}"));
        }
        self.log
            .warn(&format!("new: failed, partial work unwound: {cause}"));
        Error::operation(format!(
            "lifecycle: new {}: {}",
            instance.name,
            errors.join(": ")
        ))
    }
}

pub(crate) fn seed(
    instance: &Instance,
    request: &NewRequest,
    guest: &GuestNetwork,
    install_guest_agent: bool,
) -> Seed {
    Seed {
        instance_id: instance.uuid.clone(),
        hostname: request.name.clone(),
        user_name: GUEST_USER.to_string(),
        authorized_keys: request.ssh_keys.clone(),
        mac: instance.mac.clone(),
        address_cidr: format!("{}/{}", guest.address.addr, guest.address.bits),
        gateway: guest.gateway.to_string(),
        dns: guest.dns[0].to_string(),
        install_guest_agent,
    }
}
