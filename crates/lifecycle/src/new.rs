use bento_cloudinit::Seed;
use bento_network::GuestNetwork;
use bento_types::{DesiredState, ImageKind, Instance, Placing, State, User, Visibility};

use crate::fs::remove_file;
use crate::{Error, Manager, Result};
use bento_network::Ipv4Prefix;

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
    pub memory_mib: u64,
    pub disk_gib: u64,
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
        if request.vcpu == 0 || request.memory_mib == 0 || request.disk_gib == 0 {
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
                    request.image_name,
                    error = bento_types::error_chain(&*error)
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
            Error::operation(format!(
                "lifecycle: image version {checksum:?}: {error}",
                error = bento_types::error_chain(&*error)
            ))
        })?;
        let subnet = request
            .owner
            .subnet
            .parse::<Ipv4Prefix>()
            .map_err(|error| {
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
            // The row exists before the domain does. `provisioning` keeps
            // the poll and restore away from it until this create ends
            // (SPEC 11.2).
            state: State::Provisioning,
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
        // Held until the create ends, so a remove, a start, or the poller's
        // first-boot cleanup cannot act on a half-built instance
        // (SPEC 11.1).
        let _guard = self.lock_instance(&uuid).await;
        self.store
            .create_instance(instance.clone(), self.cooldown, capacity)
            .await
            .map_err(Error::caused)?;
        let guest = match GuestNetwork::new(subnet, address, Some(&self.dns)) {
            Ok(guest) => guest,
            Err(error) => {
                return Err(self.unwind_new(&instance, Box::new(error), false).await);
            }
        };
        let network = match self.user_network_name(&request.owner) {
            Ok(name) => name,
            Err(error) => return Err(self.unwind_new(&instance, Box::new(error), false).await),
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
            Err(error) => return Err(self.unwind_new(&instance, error, true).await),
        };
        instance.state = observed;
        self.finish_provisioning(&instance.name, &uuid, observed)
            .await;
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
                vcpu: request.vcpu,
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
        let subnet = owner.subnet.parse::<Ipv4Prefix>().map_err(|error| {
            Error::operation(format!(
                "lifecycle: user {} has a bad subnet {:?}: {error}",
                owner.name, owner.subnet
            ))
        })?;
        let index = self.plan.index(subnet).map_err(|error| {
            Error::operation(format!(
                "lifecycle: subnet {}/{} outside the private range: {error}",
                subnet.addr,
                subnet.bits,
                error = bento_types::error_chain(&error)
            ))
        })?;
        Ok(bento_network::UserNetwork::new(self.plan, index as isize)
            .map_err(Error::caused)?
            .name)
    }

    /// Ends the `provisioning` state with what libvirt showed. When this
    /// write fails the row stays `provisioning`, and the poll does not
    /// touch it: the next start of bentod settles it from libvirt, and a
    /// remove always works (SPEC 11.2).
    pub(crate) async fn finish_provisioning(&self, name: &str, uuid: &str, observed: State) {
        match self.store.finish_provisioning(uuid, observed).await {
            Ok(true) => {}
            Ok(false) => self.log.warn(&format!(
                "new: {name} is no longer provisioning; its state was not recorded"
            )),
            Err(error) => self.log.warn(&format!(
                "new: observed state of {name} not recorded; it stays provisioning \
                 until bentod restarts: {error}",
                error = bento_types::error_chain(&*error)
            )),
        }
    }

    /// Turns a kept `provisioning` row into a stopped instance whose
    /// desired state is stopped (SPEC 11.2).
    async fn keep_row_stopped(&self, instance: &Instance) {
        if let Err(error) = self
            .store
            .finish_provisioning(&instance.uuid, State::Stopped)
            .await
        {
            self.log.warn(&format!(
                "new: row of {} not set to stopped: {error}",
                instance.name,
                error = bento_types::error_chain(&*error)
            ));
        }
        if let Err(error) = self
            .store
            .set_desired_state(&instance.uuid, DesiredState::Stopped)
            .await
        {
            self.log.warn(&format!(
                "new: desired state of {} not set to stopped: {error}",
                instance.name,
                error = bento_types::error_chain(&*error)
            ));
        }
    }

    /// Removes the row and the local files of a copy that failed.
    ///
    /// A copy is built on this machine, so this side made the files and
    /// this side removes them. That is the difference from
    /// [`Manager::unwind_new`], where the files are on the machine that
    /// tried to build the instance.
    pub(crate) async fn unwind_copy(&self, instance: &Instance, cause: crate::DynError) -> Error {
        let mut errors = vec![bento_types::error_chain(&*cause)];
        if let Err(error) = (self.delete_iso)(self.seed_iso_path(&instance.uuid)).await {
            errors.push(format!(
                "unwind seed iso: {error}",
                error = bento_types::error_chain(&*error)
            ));
        }
        if let Err(error) = remove_file(&self.overlay_path(&instance.uuid)).await {
            errors.push(format!(
                "unwind overlay: {error}",
                error = bento_types::error_chain(&error)
            ));
        }
        match self.store.delete_instance(&instance.uuid).await {
            Ok(_) => self.forget_lock(&instance.uuid),
            Err(error) => errors.push(format!(
                "unwind instance row: {error}",
                error = bento_types::error_chain(&*error)
            )),
        }
        self.log.warn(&format!(
            "cp: failed, partial work unwound: {}",
            bento_types::error_chain(&*cause)
        ));
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
    ///
    /// `sent` says whether the machine was asked to build the instance.
    /// Before that, it cannot hold a domain, so it is not asked about one.
    pub(crate) async fn unwind_new(
        &self,
        instance: &Instance,
        cause: crate::DynError,
        sent: bool,
    ) -> Error {
        let mut errors = vec![bento_types::error_chain(&*cause)];
        // An error from another machine does not prove that it did
        // nothing: a timeout or a lost reply can follow a finished create.
        // The row is the only record of that domain and of its address, so
        // it goes only after the machine confirms the domain is gone. An
        // unreachable machine leaves the row, and the user can remove it
        // later (MULTI-NODE 14.1).
        if sent
            && instance.host_id != self.host_id
            && let Err(error) = self.remove_remote_domain(instance).await
        {
            errors.push(format!("row kept, domain not confirmed gone: {error}"));
            self.log.warn(&format!(
                "new: failed, row of {} kept: {}",
                instance.name,
                errors.join(": ")
            ));
            // The kept row is a plain stopped instance that the user can
            // remove. Desired stopped keeps restore from starting a
            // domain that may be half built (SPEC 11.2, MULTI-NODE 14.1).
            self.keep_row_stopped(instance).await;
            return Error::operation(format!(
                "lifecycle: new {}: {}",
                instance.name,
                errors.join(": ")
            ));
        }
        match self.store.delete_instance(&instance.uuid).await {
            Ok(_) => self.forget_lock(&instance.uuid),
            Err(error) => errors.push(format!(
                "unwind instance row: {error}",
                error = bento_types::error_chain(&*error)
            )),
        }
        self.log.warn(&format!(
            "new: failed, partial work unwound: {}",
            bento_types::error_chain(&*cause)
        ));
        Error::operation(format!(
            "lifecycle: new {}: {}",
            instance.name,
            errors.join(": ")
        ))
    }

    /// Removes the domain of an instance that failed to build on another
    /// machine, when that machine has one. A domain is matched by UUID,
    /// never by name alone, so a domain that belongs to something else is
    /// not touched.
    async fn remove_remote_domain(&self, instance: &Instance) -> std::result::Result<(), String> {
        let hyp = self
            .hyp_for(instance)
            .await
            .map_err(|error| bento_types::error_chain(&error))?;
        let domains = hyp
            .list()
            .await
            .map_err(|error| bento_types::error_chain(&error))?;
        if !domains.iter().any(|domain| domain.uuid == instance.uuid) {
            return Ok(());
        }
        match hyp.remove(&instance.name).await {
            Ok(()) | Err(bento_hypervisor::Error::DomainNotFound(_)) => Ok(()),
            Err(error) => Err(bento_types::error_chain(&error)),
        }
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
