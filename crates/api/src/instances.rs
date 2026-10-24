use axum::body::Body;
use axum::extract::{Extension, Path as AxumPath, State};
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use bento_store::Usage;
use bento_types::{Instance, User, Visibility};
use serde::{Deserialize, Serialize};

use crate::{
    AppState, BAD_NAME, BoxError, CreateSpec, HandlerError, Lifecycle, ResizeSpec, StatusError,
    decode_json, error_response, json_response, optional_rfc3339, rfc3339, valid_name,
};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct InstanceJson {
    pub(crate) uuid: String,
    pub(crate) name: String,
    pub(crate) owner: String,
    pub(crate) state: String,
    pub(crate) desired_state: String,
    pub(crate) address: String,
    pub(crate) mac: String,
    pub(crate) image: String,
    pub(crate) base_checksum: String,
    pub(crate) vcpu: u32,
    pub(crate) memory_mib: u64,
    pub(crate) disk_gib: u64,
    pub(crate) nested: bool,
    pub(crate) ksm: bool,
    pub(crate) http_port: u16,
    pub(crate) visibility: String,
    pub(crate) created_at: String,
    pub(crate) last_seen_at: String,
    pub(crate) shared_with_me: bool,
}

pub(crate) async fn instance_json(
    state: &AppState,
    instance: Instance,
    viewer: &User,
    owners: &mut std::collections::HashMap<i64, String>,
) -> InstanceJson {
    let owner = if let Some(owner) = owners.get(&instance.owner_id) {
        owner.clone()
    } else {
        let owner = state
            .0
            .store
            .user_by_id(instance.owner_id)
            .await
            .map(|user| user.name)
            .unwrap_or_default();
        owners.insert(instance.owner_id, owner.clone());
        owner
    };
    InstanceJson {
        uuid: instance.uuid,
        name: instance.name,
        owner,
        state: instance.state.to_string(),
        desired_state: instance.desired_state.to_string(),
        address: instance.address,
        mac: instance.mac,
        image: instance.image_name,
        base_checksum: instance.base_checksum,
        vcpu: instance.vcpu,
        memory_mib: instance.memory_mib,
        disk_gib: instance.disk_gib,
        nested: instance.nested,
        ksm: instance.ksm,
        http_port: instance.http_port,
        visibility: instance.visibility.to_string(),
        created_at: rfc3339(instance.created_at),
        last_seen_at: optional_rfc3339(instance.last_seen_at),
        shared_with_me: instance.owner_id != viewer.id,
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UsageJson {
    pub(crate) instances: u64,
    pub(crate) vcpu: u64,
    pub(crate) memory_mib: u64,
    pub(crate) disk_gib: u64,
}

impl From<Usage> for UsageJson {
    fn from(usage: Usage) -> Self {
        Self {
            instances: usage.instances,
            vcpu: usage.vcpu,
            memory_mib: usage.memory_mib,
            disk_gib: usage.disk_gib,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct InstanceListResponse {
    pub(crate) instances: Vec<InstanceJson>,
    pub(crate) usage: UsageJson,
}

/// Answers the primary dashboard view (SPEC 14.4): owned and shared
/// instances sorted by name, plus what the viewer has provisioned. There
/// is no per-user limit to report against it (SPEC 6.1).
pub(crate) async fn list_instances(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
) -> Result<Response, HandlerError> {
    let owned = state.0.store.instances_by_owner(user.id).await?;
    let shared = state.0.store.instances_shared_with(user.id).await?;
    let usage = state.0.store.usage_for(user.id).await?;
    let mut owners = std::collections::HashMap::from([(user.id, user.name.clone())]);
    let mut instances = Vec::with_capacity(owned.len() + shared.len());
    for instance in owned.into_iter().chain(shared) {
        instances.push(instance_json(&state, instance, &user, &mut owners).await);
    }
    instances.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(json_response(
        StatusCode::OK,
        &InstanceListResponse {
            instances,
            usage: usage.into(),
        },
    ))
}

pub(crate) async fn can_read_instance(state: &AppState, instance: &Instance, user: &User) -> bool {
    if instance.owner_id == user.id {
        return true;
    }
    state
        .0
        .store
        .shares_for(&instance.uuid)
        .await
        .is_ok_and(|shares| shares.iter().any(|share| share.user_id == user.id))
}

pub(crate) async fn get_instance(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
) -> Result<Response, HandlerError> {
    let instance = state.0.store.instance(&uuid).await?;
    if !can_read_instance(&state, &instance, &user).await {
        return Err(not_found().into());
    }
    let mut owners = std::collections::HashMap::from([(user.id, user.name.clone())]);
    Ok(json_response(
        StatusCode::OK,
        &instance_json(&state, instance, &user, &mut owners).await,
    ))
}

pub(crate) async fn owned_instance(
    state: &AppState,
    uuid: &str,
    user: &User,
) -> Result<Instance, HandlerError> {
    let instance = state.0.store.instance(uuid).await?;
    if instance.owner_id == user.id {
        return Ok(instance);
    }
    match state.0.store.shares_for(&instance.uuid).await {
        Ok(shares) if shares.iter().any(|share| share.user_id == user.id) => {
            Err(StatusError::new(StatusCode::FORBIDDEN, "only the owner may do this").into())
        }
        _ => Err(not_found().into()),
    }
}

fn not_found() -> StatusError {
    StatusError::new(StatusCode::NOT_FOUND, "not found")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateRequest {
    name: String,
    #[serde(default)]
    image: String,
    #[serde(default)]
    vcpu: u32,
    #[serde(default)]
    memory_mib: u64,
    #[serde(default)]
    disk_gib: u64,
    #[serde(default)]
    nested: bool,
    ksm: Option<bool>,
}

pub(crate) async fn create_instance(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    request: Request<Body>,
) -> Result<Response, HandlerError> {
    let request: CreateRequest = decode_json(request).await?;
    if !valid_name(&request.name) {
        return Ok(error_response(StatusCode::BAD_REQUEST, BAD_NAME));
    }
    if bento_types::is_reserved(&request.name, &state.0.reserved_names) {
        return Ok(crate::reserved_response(&request.name));
    }
    if request.image.is_empty() {
        return Ok(error_response(StatusCode::BAD_REQUEST, "image is required"));
    }
    // The request types are unsigned, so a negative or oversized number
    // already failed to decode.
    let spec = CreateSpec {
        name: request.name,
        image: request.image,
        vcpu: request.vcpu,
        memory_mib: request.memory_mib,
        disk_gib: request.disk_gib,
        nested: request.nested,
        ksm: request.ksm.unwrap_or(true),
    };
    let instance = state.0.lifecycle.create(user.clone(), spec).await?;
    let mut owners = std::collections::HashMap::from([(user.id, user.name.clone())]);
    Ok(json_response(
        StatusCode::CREATED,
        &instance_json(&state, instance, &user, &mut owners).await,
    ))
}

pub(crate) async fn delete_instance(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
) -> Result<Response, HandlerError> {
    let instance = owned_instance(&state, &uuid, &user).await?;
    state.0.lifecycle.delete(&instance.uuid).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// A power action on an instance. The API and the dashboard share it, so
/// neither needs a string name for the action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Action {
    Start,
    Stop,
    Restart,
}

impl Action {
    pub(crate) async fn run(self, lifecycle: &dyn Lifecycle, uuid: &str) -> Result<(), BoxError> {
        match self {
            Self::Start => lifecycle.start(uuid).await,
            Self::Stop => lifecycle.stop(uuid).await,
            Self::Restart => lifecycle.restart(uuid).await,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct ActionResponse<'a> {
    action: Action,
    uuid: &'a str,
}

pub(crate) async fn lifecycle_action(
    state: AppState,
    user: User,
    uuid: String,
    action: Action,
) -> Result<Response, HandlerError> {
    let instance = owned_instance(&state, &uuid, &user).await?;
    action.run(&*state.0.lifecycle, &instance.uuid).await?;
    Ok(json_response(
        StatusCode::ACCEPTED,
        &ActionResponse {
            action,
            uuid: &instance.uuid,
        },
    ))
}

pub(crate) async fn start_instance(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
) -> Result<Response, HandlerError> {
    lifecycle_action(state, user, uuid, Action::Start).await
}

pub(crate) async fn stop_instance(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
) -> Result<Response, HandlerError> {
    lifecycle_action(state, user, uuid, Action::Stop).await
}

pub(crate) async fn restart_instance(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
) -> Result<Response, HandlerError> {
    lifecycle_action(state, user, uuid, Action::Restart).await
}

pub(crate) async fn refreshed(
    state: &AppState,
    uuid: &str,
    user: &User,
) -> Result<Response, HandlerError> {
    let instance = state.0.store.instance(uuid).await?;
    let mut owners = std::collections::HashMap::from([(user.id, user.name.clone())]);
    Ok(json_response(
        StatusCode::OK,
        &instance_json(state, instance, user, &mut owners).await,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RenameRequest {
    new_name: String,
}

/// The confirmation dialog (old links break and the SSH user changes) is the
/// dashboard's job; this endpoint performs only the explicit rename requested.
pub(crate) async fn rename_instance(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
    request: Request<Body>,
) -> Result<Response, HandlerError> {
    let instance = owned_instance(&state, &uuid, &user).await?;
    let request: RenameRequest = decode_json(request).await?;
    if !valid_name(&request.new_name) {
        return Ok(error_response(StatusCode::BAD_REQUEST, BAD_NAME));
    }
    if bento_types::is_reserved(&request.new_name, &state.0.reserved_names) {
        return Ok(crate::reserved_response(&request.new_name));
    }
    state
        .0
        .lifecycle
        .rename(&instance.uuid, &request.new_name)
        .await?;
    refreshed(&state, &instance.uuid, &user).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResizeRequest {
    #[serde(default)]
    vcpu: u32,
    #[serde(default)]
    memory_mib: u64,
    #[serde(default)]
    disk_gib: u64,
    nested: Option<bool>,
}

pub(crate) async fn resize_instance(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
    request: Request<Body>,
) -> Result<Response, HandlerError> {
    let instance = owned_instance(&state, &uuid, &user).await?;
    let request: ResizeRequest = decode_json(request).await?;
    let spec = ResizeSpec {
        vcpu: if request.vcpu == 0 {
            instance.vcpu
        } else {
            request.vcpu
        },
        memory_mib: if request.memory_mib == 0 {
            instance.memory_mib
        } else {
            request.memory_mib
        },
        disk_gib: if request.disk_gib == 0 {
            instance.disk_gib
        } else {
            request.disk_gib
        },
        nested: request.nested.unwrap_or(instance.nested),
    };
    if spec.vcpu < 1 || spec.memory_mib < 1 {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "vcpu and memory_mib must be positive",
        ));
    }
    // A qcow2 overlay only grows (SPEC 11.1); catching a shrink here gives a
    // clear message instead of a qemu-img failure.
    if spec.disk_gib < instance.disk_gib {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "the disk can grow but never shrink",
        ));
    }
    state.0.lifecycle.resize(&instance.uuid, spec).await?;
    refreshed(&state, &instance.uuid, &user).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PortRequest {
    port: u16,
}

pub(crate) async fn set_port(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
    request: Request<Body>,
) -> Result<Response, HandlerError> {
    let instance = owned_instance(&state, &uuid, &user).await?;
    let request: PortRequest = decode_json(request).await?;
    let port = request.port;
    if port == 0 {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "port must be between 1 and 65535",
        ));
    }
    state
        .0
        .lifecycle
        .set_http_port(&instance.uuid, port)
        .await?;
    refreshed(&state, &instance.uuid, &user).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VisibilityRequest {
    visibility: String,
}

pub(crate) async fn set_visibility(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
    request: Request<Body>,
) -> Result<Response, HandlerError> {
    let instance = owned_instance(&state, &uuid, &user).await?;
    let request: VisibilityRequest = decode_json(request).await?;
    let visibility = match request.visibility.as_str() {
        "off" => Visibility::Off,
        "private" => Visibility::Private,
        "public" => Visibility::Public,
        _ => {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                "visibility must be \"off\", \"private\", or \"public\"",
            ));
        }
    };
    // Through the lifecycle, not the store: visibility alters published
    // ports, and SPEC 6.3 reloads the nftables table on every change.
    state
        .0
        .lifecycle
        .set_visibility(&instance.uuid, visibility)
        .await?;
    refreshed(&state, &instance.uuid, &user).await
}
