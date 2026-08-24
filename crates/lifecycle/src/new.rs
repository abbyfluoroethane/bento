use bento_cloudinit::Seed;
use bento_network::GuestNetwork;
use bento_types::{DesiredState, ImageKind, Instance, State, User, Visibility};

use crate::fs::remove_file;
use crate::manager::{AddressView, parse_prefix};
use crate::{Error, Manager, Result};

/// The one account cloud-init creates in every instance (SPEC 5.2). A fixed
/// name lets the SSH frontend authenticate to every guest (SPEC 10 step 9).
pub const GUEST_USER: &str = "bento";

/// Everything the frontend resolved for a new instance.
#[derive(Debug, Clone)]
pub struct NewRequest {
    pub name: String,
    pub owner: User,
    /// The one host in version 1 (SPEC sections 12 and 17).
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
    /// Creates a quota-checked row, assigns network identity, creates the
    /// overlay and seed, then defines and starts the domain, in that order
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
        let image_version = self.store.image_version(&checksum).await.map_err(|error| {
            Error::operation(format!("lifecycle: image version {checksum:?}: {error}"))
        })?;
        let subnet = parse_prefix(&request.owner.subnet).map_err(|error| {
            Error::operation(format!(
                "lifecycle: user {} has a bad subnet {:?}: {error}",
                request.owner.name, request.owner.subnet
            ))
        })?;
        let address = bento_network::allocate_address(&AddressView(self.store.clone()), subnet)
            .await
            .map_err(Error::caused)?;
        let uuid = (self.new_uuid)();
        let mut instance = Instance {
            uuid: uuid.clone(),
            name: request.name.clone(),
            owner_id: request.owner.id,
            host_id: request.host_id,
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
        };
        self.store
            .create_instance(instance.clone(), self.cooldown)
            .await
            .map_err(Error::caused)?;
        let overlay = self.overlay_path(&uuid);
        if let Err(error) = self
            .images
            .create_overlay(&checksum, &overlay, request.disk_gib)
            .await
        {
            return Err(self.unwind_new(&instance, error, false, false).await);
        }
        let guest = match GuestNetwork::new(subnet, address, Some(&self.dns)) {
            Ok(guest) => guest,
            Err(error) => {
                return Err(self
                    .unwind_new(&instance, Box::new(error), true, false)
                    .await);
            }
        };
        let seed = seed(
            &instance,
            &request,
            &guest,
            image_version.kind != ImageKind::Oci,
        );
        if let Err(error) = self.iso.build(&seed, &self.seed_iso_path(&uuid)).await {
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
                "new: observed state not recorded; poller will catch up: {error}"
            ));
        }
        self.log
            .info(&format!("new: instance {} created", request.name));
        Ok(instance)
    }

    pub(crate) async fn unwind_new(
        &self,
        instance: &Instance,
        cause: crate::DynError,
        overlay: bool,
        iso: bool,
    ) -> Error {
        let mut errors = vec![cause.to_string()];
        if iso && let Err(error) = (self.delete_iso)(self.seed_iso_path(&instance.uuid)).await {
            errors.push(format!("unwind seed iso: {error}"));
        }
        if overlay && let Err(error) = remove_file(&self.overlay_path(&instance.uuid)).await {
            errors.push(format!("unwind overlay: {error}"));
        }
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
