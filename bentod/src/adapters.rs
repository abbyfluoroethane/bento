//! Thin adapters between consumer-side interfaces and concrete
//! implementations. Every deliberate mapping decision lives here.

use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bento_api::{BoxError as ApiError, StoreError as ApiStoreError};
use bento_auth::{BoxError as AuthError, TokenLookup};
use bento_cli::{BoxError as CliError, ReadWrite};
use bento_hypervisor::{Hypervisor, NetworkManager, StopResult};
use bento_images::{DB, DynError as ImagesError, ReportSource};
use bento_lifecycle::{DynError as LifecycleError, Manager, NewRequest};
use bento_network::{Plan, UserNetwork};
use bento_proxy::{Access, BoxError as ProxyError, ProxyBody};
use bento_sshfront::{BoxError as SshError, Registration};
use bento_store::{Error as StoreError, Store};
use bento_types::{
    DesiredState, Image, ImageVersion, Instance, Quota, Share, SshKey, State, Token, User,
    Visibility,
};
use http::{Request, StatusCode};
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::firewall::Firewall;

// ---- images ----

pub(crate) struct ImageDb(pub(crate) Store);

#[async_trait]
impl DB for ImageDb {
    async fn images(&self) -> Result<Vec<Image>, ImagesError> {
        Ok(self.0.images().await?)
    }

    async fn has_image_version(&self, checksum: &str) -> Result<bool, ImagesError> {
        Ok(self
            .image_versions()
            .await?
            .iter()
            .any(|version| version.checksum == checksum))
    }

    async fn insert_image_version(&self, version: ImageVersion) -> Result<(), ImagesError> {
        Ok(self.0.add_image_version(version).await?)
    }

    async fn set_current_checksum(
        &self,
        image_name: &str,
        checksum: &str,
    ) -> Result<(), ImagesError> {
        Ok(self.0.set_current_checksum(image_name, checksum).await?)
    }

    async fn image_versions(&self) -> Result<Vec<ImageVersion>, ImagesError> {
        let mut versions = Vec::new();
        for image in self.0.images().await? {
            versions.extend(self.0.image_versions(&image.name).await?);
        }
        Ok(versions)
    }

    async fn delete_image_version(&self, checksum: &str) -> Result<(), ImagesError> {
        Ok(self.0.delete_image_version(checksum).await?)
    }

    async fn checksum_in_use(&self, checksum: &str) -> Result<bool, ImagesError> {
        Ok(self
            .0
            .instances()
            .await?
            .iter()
            .any(|instance| instance.base_checksum == checksum))
    }
}

pub(crate) struct ImageReport(pub(crate) Store);

#[async_trait]
impl ReportSource for ImageReport {
    async fn images(&self) -> Result<Vec<Image>, ImagesError> {
        Ok(self.0.images().await?)
    }

    async fn count_instances_on_other_versions(
        &self,
        image_name: &str,
        checksum: &str,
    ) -> Result<i64, ImagesError> {
        Ok(self
            .0
            .instances()
            .await?
            .iter()
            .filter(|instance| {
                instance.image_name == image_name && instance.base_checksum != checksum
            })
            .count() as i64)
    }
}

// ---- lifecycle dependencies and backends ----

pub(crate) struct LifecycleStore(pub(crate) Store);

#[async_trait]
impl bento_lifecycle::Store for LifecycleStore {
    async fn create_instance(
        &self,
        instance: Instance,
        cooldown: Duration,
    ) -> Result<(), LifecycleError> {
        Ok(self.0.create_instance(instance, cooldown).await?)
    }
    async fn delete_instance(&self, uuid: &str) -> Result<Instance, LifecycleError> {
        Ok(self.0.delete_instance(uuid).await?)
    }
    async fn instance(&self, uuid: &str) -> Result<Instance, LifecycleError> {
        Ok(self.0.instance(uuid).await?)
    }
    async fn instances(&self) -> Result<Vec<Instance>, LifecycleError> {
        Ok(self.0.instances().await?)
    }
    async fn instances_to_restore(&self) -> Result<Vec<Instance>, LifecycleError> {
        Ok(self.0.instances_to_restore().await?)
    }
    async fn image(&self, name: &str) -> Result<Image, LifecycleError> {
        Ok(self.0.image(name).await?)
    }
    async fn user_by_id(&self, id: i64) -> Result<User, LifecycleError> {
        Ok(self.0.user_by_id(id).await?)
    }
    async fn rename_instance(
        &self,
        uuid: &str,
        new_name: &str,
        cooldown: Duration,
    ) -> Result<(), LifecycleError> {
        Ok(self.0.rename_instance(uuid, new_name, cooldown).await?)
    }
    async fn resize(
        &self,
        uuid: &str,
        vcpu: u32,
        memory_mib: i64,
        disk_gib: i64,
        nested: bool,
    ) -> Result<(), LifecycleError> {
        Ok(self
            .0
            .resize(uuid, vcpu, memory_mib, disk_gib, nested)
            .await?)
    }
    async fn set_desired_state(
        &self,
        uuid: &str,
        state: DesiredState,
    ) -> Result<(), LifecycleError> {
        Ok(self.0.set_desired_state(uuid, state).await?)
    }
    async fn set_observed_state(&self, uuid: &str, state: State) -> Result<(), LifecycleError> {
        Ok(self.0.set_observed_state(uuid, state).await?)
    }
    async fn update_observed_states(
        &self,
        states: std::collections::HashMap<String, State>,
    ) -> Result<(), LifecycleError> {
        Ok(self.0.update_observed_states(states).await?)
    }
}

pub(crate) struct LifecycleImages(pub(crate) Arc<bento_images::Store>);

#[async_trait]
impl bento_lifecycle::ImageStore for LifecycleImages {
    async fn create_overlay(
        &self,
        checksum: &str,
        overlay_path: &Path,
        disk_gib: i64,
    ) -> Result<(), LifecycleError> {
        Ok(self
            .0
            .create_overlay(checksum, overlay_path, disk_gib)
            .await?)
    }
}

pub(crate) struct Backend {
    pub(crate) manager: Arc<Manager>,
    pub(crate) store: Store,
    pub(crate) host_id: i64,
    pub(crate) frontend_key: String,
    pub(crate) firewall: Option<Arc<Firewall>>,
}

impl Backend {
    async fn reload_firewall(&self) -> anyhow::Result<()> {
        if let Some(firewall) = &self.firewall {
            firewall.reload().await?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn new_request(
        &self,
        owner_id: i64,
        name: String,
        image: String,
        vcpu: u32,
        memory_mib: i64,
        disk_gib: i64,
        nested: bool,
        ksm: bool,
    ) -> anyhow::Result<NewRequest> {
        let owner = self.store.user_by_id(owner_id).await?;
        let keys = self.store.ssh_keys_for_user(owner_id).await?;
        let mut ssh_keys = keys
            .into_iter()
            .map(|key| key.public_key)
            .collect::<Vec<_>>();
        if !self.frontend_key.is_empty() {
            ssh_keys.push(self.frontend_key.clone());
        }
        Ok(NewRequest {
            name,
            owner,
            host_id: self.host_id,
            ssh_keys,
            image_name: image,
            vcpu,
            memory_mib,
            disk_gib,
            nested,
            disable_ksm: !ksm,
            http_port: 0,
        })
    }
}

pub(crate) struct CliBackend(pub(crate) Backend);

#[async_trait]
impl bento_cli::Lifecycle for CliBackend {
    async fn create(&self, request: bento_cli::CreateRequest) -> Result<Instance, CliError> {
        let request = self
            .0
            .new_request(
                request.owner_id,
                request.name,
                request.image,
                request.vcpu,
                request.memory_mib,
                request.disk_gib,
                request.nested,
                request.ksm,
            )
            .await?;
        let instance = self.0.manager.create(request).await?;
        self.0.reload_firewall().await.map_err(|error| {
            io::Error::other(format!("instance created, firewall reload failed: {error}"))
        })?;
        Ok(instance)
    }

    async fn start(&self, instance: Instance) -> Result<(), CliError> {
        Ok(self.0.manager.start(&instance.uuid).await?)
    }

    async fn stop(&self, instance: Instance) -> Result<StopResult, CliError> {
        Ok(self.0.manager.stop(&instance.uuid).await?)
    }

    async fn restart(&self, instance: Instance) -> Result<(), CliError> {
        Ok(self.0.manager.restart(&instance.uuid).await?)
    }

    async fn remove(&self, instance: Instance) -> Result<(), CliError> {
        self.0.manager.remove(&instance.uuid).await?;
        self.0.reload_firewall().await.map_err(|error| {
            io::Error::other(format!("instance removed, firewall reload failed: {error}"))
        })?;
        Ok(())
    }

    async fn rename(&self, instance: Instance, new_name: &str) -> Result<(), CliError> {
        Ok(self.0.manager.rename(&instance.uuid, new_name).await?)
    }

    async fn copy(
        &self,
        source: Instance,
        request: bento_cli::CreateRequest,
    ) -> Result<Instance, CliError> {
        let request = self
            .0
            .new_request(
                request.owner_id,
                request.name,
                request.image,
                request.vcpu,
                request.memory_mib,
                request.disk_gib,
                request.nested,
                request.ksm,
            )
            .await?;
        let instance = self.0.manager.copy(&source.uuid, request).await?;
        self.0.reload_firewall().await.map_err(|error| {
            io::Error::other(format!("instance copied, firewall reload failed: {error}"))
        })?;
        Ok(instance)
    }

    async fn resize(
        &self,
        instance: Instance,
        request: bento_cli::ResizeRequest,
    ) -> Result<(), CliError> {
        self.0
            .manager
            .resize(bento_lifecycle::ResizeRequest {
                uuid: instance.uuid,
                vcpu: request.vcpu.unwrap_or(instance.vcpu),
                memory_mib: request.memory_mib.unwrap_or(instance.memory_mib),
                disk_gib: request.disk_gib.unwrap_or(instance.disk_gib),
                nested: request.nested.unwrap_or(instance.nested),
            })
            .await?;
        Ok(())
    }

    async fn console(&self, _instance: Instance, _rw: &mut dyn ReadWrite) -> Result<(), CliError> {
        Err(io::Error::other(
            "console: the serial console is not wired in this build; connect with ssh instead",
        )
        .into())
    }

    async fn set_http_port(&self, instance: Instance, port: u16) -> Result<(), CliError> {
        self.0.store.set_http_port(&instance.uuid, port).await?;
        self.0.reload_firewall().await.map_err(|error| {
            io::Error::other(format!("port stored, firewall reload failed: {error}"))
        })?;
        Ok(())
    }

    async fn set_visibility(
        &self,
        instance: Instance,
        visibility: Visibility,
    ) -> Result<(), CliError> {
        self.0
            .store
            .set_visibility(&instance.uuid, visibility)
            .await?;
        self.0.reload_firewall().await.map_err(|error| {
            io::Error::other(format!(
                "visibility stored, firewall reload failed: {error}"
            ))
        })?;
        Ok(())
    }
}

pub(crate) struct ApiBackend(pub(crate) Backend);

#[async_trait]
impl bento_api::Lifecycle for ApiBackend {
    async fn create(&self, owner: User, spec: bento_api::CreateSpec) -> Result<Instance, ApiError> {
        let request = self
            .0
            .new_request(
                owner.id,
                spec.name,
                spec.image,
                spec.vcpu,
                spec.memory_mib,
                spec.disk_gib,
                spec.nested,
                spec.ksm,
            )
            .await?;
        let instance = self.0.manager.create(request).await?;
        self.0.reload_firewall().await.map_err(|error| {
            io::Error::other(format!("instance created, firewall reload failed: {error}"))
        })?;
        Ok(instance)
    }
    async fn delete(&self, uuid: &str) -> Result<(), ApiError> {
        self.0.manager.remove(uuid).await?;
        self.0.reload_firewall().await?;
        Ok(())
    }
    async fn start(&self, uuid: &str) -> Result<(), ApiError> {
        Ok(self.0.manager.start(uuid).await?)
    }
    async fn stop(&self, uuid: &str) -> Result<(), ApiError> {
        self.0.manager.stop(uuid).await?;
        Ok(())
    }
    async fn restart(&self, uuid: &str) -> Result<(), ApiError> {
        Ok(self.0.manager.restart(uuid).await?)
    }
    async fn rename(&self, uuid: &str, new_name: &str) -> Result<(), ApiError> {
        Ok(self.0.manager.rename(uuid, new_name).await?)
    }
    async fn resize(&self, uuid: &str, spec: bento_api::ResizeSpec) -> Result<(), ApiError> {
        self.0
            .manager
            .resize(bento_lifecycle::ResizeRequest {
                uuid: uuid.to_owned(),
                vcpu: spec.vcpu,
                memory_mib: spec.memory_mib,
                disk_gib: spec.disk_gib,
                nested: spec.nested,
            })
            .await?;
        Ok(())
    }
    async fn set_http_port(&self, uuid: &str, port: u16) -> Result<(), ApiError> {
        self.0.store.set_http_port(uuid, port).await?;
        self.0.reload_firewall().await?;
        Ok(())
    }
    async fn set_visibility(&self, uuid: &str, visibility: Visibility) -> Result<(), ApiError> {
        self.0.store.set_visibility(uuid, visibility).await?;
        self.0.reload_firewall().await?;
        Ok(())
    }
}

// ---- API store ----

pub(crate) struct ApiStore(pub(crate) Store);

fn api_store_error(error: StoreError) -> ApiError {
    match error {
        StoreError::NotFound => Box::new(ApiStoreError::NotFound),
        StoreError::NameTaken => Box::new(ApiStoreError::NameTaken),
        StoreError::Quota {
            limit,
            used,
            requested,
            max,
        } => Box::new(ApiStoreError::Quota {
            limit: limit.to_owned(),
            used,
            requested,
            max,
        }),
        StoreError::NameCooldown { name, remaining } => {
            Box::new(ApiStoreError::NameCooldown { name, remaining })
        }
        other => Box::new(other),
    }
}

#[async_trait]
impl bento_api::Store for ApiStore {
    async fn user_by_id(&self, id: i64) -> Result<User, ApiError> {
        self.0.user_by_id(id).await.map_err(api_store_error)
    }
    async fn user_by_name(&self, name: &str) -> Result<User, ApiError> {
        self.0.user_by_name(name).await.map_err(api_store_error)
    }
    async fn quota_for(&self, user_id: i64) -> Result<Quota, ApiError> {
        self.0.quota_for(user_id).await.map_err(api_store_error)
    }
    async fn usage_for(&self, user_id: i64) -> Result<bento_store::Usage, ApiError> {
        self.0.usage_for(user_id).await.map_err(api_store_error)
    }
    async fn instance(&self, uuid: &str) -> Result<Instance, ApiError> {
        self.0.instance(uuid).await.map_err(api_store_error)
    }
    async fn instances_by_owner(&self, owner_id: i64) -> Result<Vec<Instance>, ApiError> {
        self.0
            .instances_by_owner(owner_id)
            .await
            .map_err(api_store_error)
    }
    async fn instances_shared_with(&self, user_id: i64) -> Result<Vec<Instance>, ApiError> {
        self.0
            .instances_shared_with(user_id)
            .await
            .map_err(api_store_error)
    }
    async fn instances(&self) -> Result<Vec<Instance>, ApiError> {
        self.0.instances().await.map_err(api_store_error)
    }
    async fn add_share(&self, instance_uuid: &str, user_id: i64) -> Result<(), ApiError> {
        self.0
            .add_share(instance_uuid, user_id)
            .await
            .map_err(api_store_error)
    }
    async fn remove_share(&self, instance_uuid: &str, user_id: i64) -> Result<(), ApiError> {
        self.0
            .remove_share(instance_uuid, user_id)
            .await
            .map_err(api_store_error)
    }
    async fn shares_for(&self, instance_uuid: &str) -> Result<Vec<Share>, ApiError> {
        self.0
            .shares_for(instance_uuid)
            .await
            .map_err(api_store_error)
    }
    async fn images(&self) -> Result<Vec<Image>, ApiError> {
        self.0.images().await.map_err(api_store_error)
    }
    async fn add_ssh_key(
        &self,
        user_id: i64,
        public_key: &str,
        fingerprint: &str,
        comment: &str,
    ) -> Result<i64, ApiError> {
        self.0
            .add_ssh_key(user_id, public_key, fingerprint, comment)
            .await
            .map_err(api_store_error)
    }
    async fn ssh_keys_for_user(&self, user_id: i64) -> Result<Vec<SshKey>, ApiError> {
        self.0
            .ssh_keys_for_user(user_id)
            .await
            .map_err(api_store_error)
    }
    async fn delete_ssh_key(&self, user_id: i64, key_id: i64) -> Result<(), ApiError> {
        self.0
            .delete_ssh_key(user_id, key_id)
            .await
            .map_err(api_store_error)
    }
    async fn dump_db(&self, destination: &Path) -> Result<(), ApiError> {
        self.0.dump_db(destination).await.map_err(api_store_error)
    }
}

// ---- auth ----

pub(crate) struct AuthUsers(pub(crate) Store);

#[async_trait]
impl bento_auth::UserStore for AuthUsers {
    async fn user_by_oidc_subject(&self, subject: &str) -> Result<Option<User>, AuthError> {
        if subject.is_empty() {
            return Ok(None);
        }
        match self.0.user_by_oidc_subject(subject).await {
            Ok(user) => Ok(Some(user)),
            Err(StoreError::NotFound) => Ok(None),
            Err(error) => Err(Box::new(error)),
        }
    }
}

pub(crate) struct AuthAccess(pub(crate) Store);

#[async_trait]
impl bento_auth::AccessStore for AuthAccess {
    async fn has_access(&self, uuid: &str, user_id: i64) -> Result<bool, AuthError> {
        Ok(self.0.has_access(uuid, user_id).await?)
    }
}

pub(crate) struct AuthTokens(pub(crate) Store);

#[async_trait]
impl bento_auth::TokenStore for AuthTokens {
    async fn create_token(
        &self,
        user_id: i64,
        hash: &str,
        expires_at: OffsetDateTime,
    ) -> Result<Token, AuthError> {
        let id = self.0.create_token(user_id, hash, expires_at).await?;
        Ok(Token {
            id,
            user_id,
            hash: hash.to_owned(),
            expires_at,
        })
    }
    async fn token_by_hash(&self, hash: &str) -> Result<TokenLookup, AuthError> {
        match self.0.token_by_hash(hash).await {
            Ok(token) => Ok(TokenLookup::Found(token)),
            Err(StoreError::NotFound) => Ok(TokenLookup::NotFound),
            Err(StoreError::TokenExpired(token)) => Ok(TokenLookup::Expired(*token)),
            Err(error) => Err(Box::new(error)),
        }
    }
    async fn delete_token(&self, id: i64) -> Result<(), AuthError> {
        match self.0.delete_token_by_id(id).await {
            Ok(()) | Err(StoreError::NotFound) => Ok(()),
            Err(error) => Err(Box::new(error)),
        }
    }
}

pub(crate) struct Authenticator {
    pub(crate) service: Arc<bento_auth::Service>,
    pub(crate) store: Store,
}

#[async_trait]
impl bento_api::Authenticator for Authenticator {
    async fn user_from_headers(&self, headers: &http::HeaderMap) -> Result<User, ApiError> {
        if let Some(session) = self.service.session_from_headers(headers).await {
            return Ok(self.store.user_by_id(session.user_id).await?);
        }
        if let Some(plaintext) = bento_auth::bearer_token(headers) {
            let token = self.service.authenticate_token(plaintext).await?;
            return Ok(self.store.user_by_id(token.user_id).await?);
        }
        Err(Box::new(bento_auth::Error::Unauthenticated))
    }
}

pub(crate) async fn access_status(
    service: &bento_auth::Service,
    headers: &http::HeaderMap,
    uuid: &str,
) -> StatusCode {
    let mut result = service.authorize_request(headers, uuid).await.map(|_| ());
    if matches!(result, Err(bento_auth::Error::Unauthenticated))
        && let Some(plaintext) = bento_auth::bearer_token(headers)
        && let Ok(token) = service.authenticate_token(plaintext).await
    {
        result = service.authorize_user(token.user_id, uuid).await;
    }
    match result {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(bento_auth::Error::Forbidden) => StatusCode::FORBIDDEN,
        _ => StatusCode::UNAUTHORIZED,
    }
}

// ---- proxy ----

#[derive(Clone)]
pub(crate) struct ProxySource(pub(crate) Store);

#[async_trait]
impl bento_proxy::InstanceSource for ProxySource {
    async fn instance_by_name(&self, name: &str) -> Result<Option<Instance>, ProxyError> {
        match self.0.instance_by_name(name).await {
            Ok(instance) => Ok(Some(instance)),
            Err(StoreError::NotFound) => Ok(None),
            Err(error) => Err(Box::new(error)),
        }
    }
}

#[async_trait]
impl bento_proxy::LastSeenRecorder for ProxySource {
    async fn touch_last_seen(&self, uuid: &str) -> Result<(), ProxyError> {
        Ok(self.0.touch_last_seen(uuid).await?)
    }
}

pub(crate) struct RemoteSession {
    pub(crate) base: String,
    pub(crate) client: reqwest::Client,
}

#[async_trait]
impl bento_proxy::SessionChecker for RemoteSession {
    async fn access(&self, request: &Request<ProxyBody>, uuid: &str) -> Access {
        let Ok(url) = reqwest::Url::parse(&format!("{}/access/{uuid}", self.base)) else {
            return Access::Forbidden;
        };
        let mut outbound = self.client.get(url);
        for name in [http::header::COOKIE, http::header::AUTHORIZATION] {
            if let Some(value) = request.headers().get(&name) {
                outbound = outbound.header(name, value);
            }
        }
        match outbound.send().await {
            Ok(response) if response.status().is_success() => Access::Granted,
            Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
                Access::Unauthenticated
            }
            _ => Access::Forbidden,
        }
    }
}

// ---- SSH frontend ----

pub(crate) struct Starter(pub(crate) Arc<dyn Hypervisor>);

#[async_trait]
impl bento_sshfront::Starter for Starter {
    async fn start_instance(&self, instance: Instance) -> Result<(), SshError> {
        Ok(self.0.start(&instance.name).await?)
    }
}

#[async_trait]
pub(crate) trait NetworkEnsurer: Send + Sync {
    async fn ensure_network(&self, name: &str, xml: &str) -> anyhow::Result<()>;
}

#[async_trait]
impl NetworkEnsurer for bento_hypervisor::Client {
    async fn ensure_network(&self, name: &str, xml: &str) -> anyhow::Result<()> {
        Ok(NetworkManager::ensure_network(self, name, xml).await?)
    }
}

pub(crate) struct Registrar {
    pub(crate) store: Store,
    pub(crate) plan: Plan,
    pub(crate) networks: Option<Arc<dyn NetworkEnsurer>>,
    pub(crate) firewall: Option<Arc<Firewall>>,
}

#[async_trait]
impl bento_sshfront::Registrar for Registrar {
    async fn register(&self, registration: Registration) -> Result<User, SshError> {
        let user = self
            .store
            .register_user(
                &registration.name,
                &registration.email,
                None,
                self.plan.range(),
            )
            .await?;
        self.store
            .add_ssh_key(
                user.id,
                &registration.public_key,
                &registration.fingerprint,
                &registration.comment,
            )
            .await?;
        let network = user_network(self.plan, &user.subnet)?;
        if let Some(networks) = &self.networks {
            if let Err(error) = networks
                .ensure_network(&network.name, &network.xml()?)
                .await
            {
                tracing::warn!(
                    user = %user.name,
                    network = %network.name,
                    %error,
                    "registration: user network not defined yet; the control plane will retry"
                );
            }
        } else {
            tracing::warn!(
                user = %user.name,
                "registration: no libvirt connection; the control plane will define the user network"
            );
        }
        if let Some(firewall) = &self.firewall
            && let Err(error) = firewall.reload().await
        {
            tracing::warn!(
                user = %user.name,
                %error,
                "registration: firewall reload failed; the control plane will retry"
            );
        }
        Ok(user)
    }
}

pub(crate) fn user_network(plan: Plan, subnet: &str) -> anyhow::Result<UserNetwork> {
    let prefix = bento_config::parse_prefix(subnet)
        .map_err(|error| anyhow::anyhow!("user subnet {subnet:?}: {error}"))?;
    let index = plan.index(prefix)?;
    Ok(UserNetwork::new(plan, index as isize)?)
}

/// Bridges the async SSH channel streams to the blocking stream interface of
/// the command interpreter without buffering an interactive session.
///
/// The streams are moved across channels rather than wrapped in a
/// [`tokio_util::io::SyncIoBridge`], which cannot be used here. The
/// interpreter is an async function that takes *blocking* handles, so it has
/// to be driven with `block_on` on a blocking thread, and that establishes a
/// runtime context for the whole call. `SyncIoBridge` calls `block_on`
/// internally on every read and write, so each one nests inside that context
/// and panics with "Cannot start a runtime from within a runtime" — the
/// session dies before a byte reaches the client.
///
/// So nothing on the interpreter's side of the channel may touch the runtime:
/// the writer is an unbounded [`tokio::sync::mpsc`] sender, whose `send` is
/// an ordinary non-blocking call, and the reader is a [`std::sync::mpsc`]
/// receiver, whose `recv` blocks the thread without consulting any runtime.
/// The pump tasks on the other end do the awaiting.
pub(crate) struct CliRunner(pub(crate) Arc<bento_cli::Cli>);

/// The blocking write half: hands each write to the pump task that owns the
/// async stream. `send` neither blocks nor enters the runtime.
struct ChannelWriter(tokio::sync::mpsc::UnboundedSender<Vec<u8>>);

impl io::Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "ssh stream closed"))?;
        Ok(buf.len())
    }

    /// The pump writes each chunk as it arrives, so there is nothing held
    /// back here to flush.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The blocking read half. `recv` parks this thread until the pump task
/// delivers the next chunk, which is what makes `confirm` and the key
/// prompts interactive rather than reading end-of-file immediately.
pub(crate) struct ChannelReader {
    pub(crate) rx: std::sync::mpsc::Receiver<Vec<u8>>,
    pub(crate) chunk: Vec<u8>,
    pub(crate) offset: usize,
}

impl io::Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.offset >= self.chunk.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.chunk = chunk;
                    self.offset = 0;
                }
                // The sender is gone: the client closed stdin. Report the
                // end of the stream rather than an error.
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.chunk.len() - self.offset);
        buf[..n].copy_from_slice(&self.chunk[self.offset..self.offset + n]);
        self.offset += n;
        Ok(n)
    }
}

#[async_trait]
impl bento_sshfront::CLIRunner for CliRunner {
    async fn run(
        &self,
        user: User,
        args: Vec<String>,
        stdin: Pin<Box<dyn AsyncRead + Send>>,
        stdout: Pin<Box<dyn AsyncWrite + Send>>,
        stderr: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> i32 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (err_tx, mut err_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();

        // Forward stdin until the client closes it or the interpreter exits
        // and drops the receiver. The channel is unbounded so this never
        // blocks a runtime worker; SSH's own window limits the read-ahead.
        let stdin_pump = tokio::spawn(async move {
            let mut stdin = stdin;
            let mut buf = [0u8; 8192];
            while let Ok(n) = stdin.read(&mut buf).await {
                if n == 0 || in_tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });

        let out_pump = tokio::spawn(async move {
            let mut stdout = stdout;
            while let Some(chunk) = out_rx.recv().await {
                if stdout.write_all(&chunk).await.is_err() {
                    break;
                }
            }
            let _ = stdout.flush().await;
        });
        let err_pump = tokio::spawn(async move {
            let mut stderr = stderr;
            while let Some(chunk) = err_rx.recv().await {
                if stderr.write_all(&chunk).await.is_err() {
                    break;
                }
            }
            let _ = stderr.flush().await;
        });

        let cli = self.0.clone();
        let handle = tokio::runtime::Handle::current();
        let code = tokio::task::spawn_blocking(move || {
            let mut stdin = ChannelReader {
                rx: in_rx,
                chunk: Vec::new(),
                offset: 0,
            };
            let mut stdout = ChannelWriter(out_tx);
            let mut stderr = ChannelWriter(err_tx);
            handle.block_on(cli.run(user, &args, &mut stdin, &mut stdout, &mut stderr))
            // The senders drop here, which ends both output pumps.
        })
        .await
        .unwrap_or(1);

        // Drain what the interpreter wrote before returning: the caller
        // closes the SSH channel on this return, and anything still sitting
        // in a pump would be lost with it.
        let _ = out_pump.await;
        let _ = err_pump.await;
        stdin_pump.abort();
        code
    }
}

pub(crate) fn operator_predicate(names: &[String]) -> HashSet<String> {
    names.iter().cloned().collect()
}
