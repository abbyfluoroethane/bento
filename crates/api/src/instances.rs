use axum::body::Body;
use axum::extract::{Extension, Path as AxumPath, State};
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use bento_store::Usage;
use bento_types::{Instance, Quota, User, Visibility};
use serde::{Deserialize, Serialize};

use crate::{
    AppState, BAD_NAME, CreateSpec, ResizeSpec, decode_json, error_response, is_not_found,
    json_response, mapped_error, optional_rfc3339, rfc3339, valid_name,
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
    pub(crate) memory_mib: i64,
    pub(crate) disk_gib: i64,
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
pub(crate) struct QuotaJson {
    pub(crate) max_instances: i64,
    pub(crate) max_vcpu: i64,
    pub(crate) max_memory_mib: i64,
    pub(crate) max_disk_gib: i64,
}

impl From<Quota> for QuotaJson {
    fn from(quota: Quota) -> Self {
        Self {
            max_instances: quota.max_instances,
            max_vcpu: quota.max_vcpu,
            max_memory_mib: quota.max_memory_mib,
            max_disk_gib: quota.max_disk_gib,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UsageJson {
    pub(crate) instances: i64,
    pub(crate) vcpu: i64,
    pub(crate) memory_mib: i64,
    pub(crate) disk_gib: i64,
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
    pub(crate) quota: Option<QuotaJson>,
    pub(crate) usage: UsageJson,
}

/// Answers the primary dashboard view (SPEC 14.4): owned and shared
/// instances sorted by name, plus all four quota limits and current use.
pub(crate) async fn list_instances(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
) -> Response {
    let owned = match state.0.store.instances_by_owner(user.id).await {
        Ok(instances) => instances,
        Err(error) => return mapped_error(error),
    };
    let shared = match state.0.store.instances_shared_with(user.id).await {
        Ok(instances) => instances,
        Err(error) => return mapped_error(error),
    };
    let usage = match state.0.store.usage_for(user.id).await {
        Ok(usage) => usage,
        Err(error) => return mapped_error(error),
    };
    let quota = match state.0.store.quota_for(user.id).await {
        Ok(quota) => Some(quota.into()),
        Err(error) if is_not_found(&error) => None,
        Err(error) => return mapped_error(error),
    };

    let mut owners = std::collections::HashMap::from([(user.id, user.name.clone())]);
    let mut instances = Vec::with_capacity(owned.len() + shared.len());
    for instance in owned.into_iter().chain(shared) {
        instances.push(instance_json(&state, instance, &user, &mut owners).await);
    }
    instances.sort_by(|left, right| left.name.cmp(&right.name));
    json_response(
        StatusCode::OK,
        &InstanceListResponse {
            instances,
            quota,
            usage: usage.into(),
        },
    )
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
) -> Response {
    let instance = match state.0.store.instance(&uuid).await {
        Ok(instance) => instance,
        Err(error) => return mapped_error(error),
    };
    if !can_read_instance(&state, &instance, &user).await {
        return error_response(StatusCode::NOT_FOUND, "not found");
    }
    let mut owners = std::collections::HashMap::from([(user.id, user.name.clone())]);
    json_response(
        StatusCode::OK,
        &instance_json(&state, instance, &user, &mut owners).await,
    )
}

pub(crate) async fn owned_instance(
    state: &AppState,
    uuid: &str,
    user: &User,
) -> Result<Instance, Box<Response>> {
    let instance = state
        .0
        .store
        .instance(uuid)
        .await
        .map_err(|error| Box::new(mapped_error(error)))?;
    if instance.owner_id == user.id {
        return Ok(instance);
    }
    match state.0.store.shares_for(&instance.uuid).await {
        Ok(shares) if shares.iter().any(|share| share.user_id == user.id) => Err(Box::new(
            error_response(StatusCode::FORBIDDEN, "only the owner may do this"),
        )),
        _ => Err(Box::new(error_response(StatusCode::NOT_FOUND, "not found"))),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateRequest {
    name: String,
    #[serde(default)]
    image: String,
    #[serde(default)]
    vcpu: i64,
    #[serde(default)]
    memory_mib: i64,
    #[serde(default)]
    disk_gib: i64,
    #[serde(default)]
    nested: bool,
    ksm: Option<bool>,
}

pub(crate) async fn create_instance(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    request: Request<Body>,
) -> Response {
    let request: CreateRequest = match decode_json(request).await {
        Ok(request) => request,
        Err(response) => return *response,
    };
    if !valid_name(&request.name) {
        return error_response(StatusCode::BAD_REQUEST, BAD_NAME);
    }
    if request.image.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "image is required");
    }
    if request.vcpu < 0 || request.memory_mib < 0 || request.disk_gib < 0 {
        return error_response(
            StatusCode::BAD_REQUEST,
            "vcpu, memory_mib, and disk_gib must not be negative",
        );
    }
    let Ok(vcpu) = u32::try_from(request.vcpu) else {
        return error_response(StatusCode::BAD_REQUEST, "vcpu is too large");
    };
    let spec = CreateSpec {
        name: request.name,
        image: request.image,
        vcpu,
        memory_mib: request.memory_mib,
        disk_gib: request.disk_gib,
        nested: request.nested,
        ksm: request.ksm.unwrap_or(true),
    };
    let instance = match state.0.lifecycle.create(user.clone(), spec).await {
        Ok(instance) => instance,
        Err(error) => return mapped_error(error),
    };
    let mut owners = std::collections::HashMap::from([(user.id, user.name.clone())]);
    json_response(
        StatusCode::CREATED,
        &instance_json(&state, instance, &user, &mut owners).await,
    )
}

pub(crate) async fn delete_instance(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
) -> Response {
    let instance = match owned_instance(&state, &uuid, &user).await {
        Ok(instance) => instance,
        Err(response) => return *response,
    };
    match state.0.lifecycle.delete(&instance.uuid).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => mapped_error(error),
    }
}

#[derive(Serialize)]
pub(crate) struct ActionResponse<'a> {
    action: &'a str,
    uuid: &'a str,
}

pub(crate) async fn lifecycle_action(
    state: AppState,
    user: User,
    uuid: String,
    action: &'static str,
) -> Response {
    let instance = match owned_instance(&state, &uuid, &user).await {
        Ok(instance) => instance,
        Err(response) => return *response,
    };
    let result = match action {
        "start" => state.0.lifecycle.start(&instance.uuid).await,
        "stop" => state.0.lifecycle.stop(&instance.uuid).await,
        "restart" => state.0.lifecycle.restart(&instance.uuid).await,
        _ => unreachable!(),
    };
    match result {
        Ok(()) => json_response(
            StatusCode::ACCEPTED,
            &ActionResponse {
                action,
                uuid: &instance.uuid,
            },
        ),
        Err(error) => mapped_error(error),
    }
}

pub(crate) async fn start_instance(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
) -> Response {
    lifecycle_action(state, user, uuid, "start").await
}

pub(crate) async fn stop_instance(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
) -> Response {
    lifecycle_action(state, user, uuid, "stop").await
}

pub(crate) async fn restart_instance(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
) -> Response {
    lifecycle_action(state, user, uuid, "restart").await
}

pub(crate) async fn refreshed(state: &AppState, uuid: &str, user: &User) -> Response {
    let instance = match state.0.store.instance(uuid).await {
        Ok(instance) => instance,
        Err(error) => return mapped_error(error),
    };
    let mut owners = std::collections::HashMap::from([(user.id, user.name.clone())]);
    json_response(
        StatusCode::OK,
        &instance_json(state, instance, user, &mut owners).await,
    )
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
) -> Response {
    let instance = match owned_instance(&state, &uuid, &user).await {
        Ok(instance) => instance,
        Err(response) => return *response,
    };
    let request: RenameRequest = match decode_json(request).await {
        Ok(request) => request,
        Err(response) => return *response,
    };
    if !valid_name(&request.new_name) {
        return error_response(StatusCode::BAD_REQUEST, BAD_NAME);
    }
    match state
        .0
        .lifecycle
        .rename(&instance.uuid, &request.new_name)
        .await
    {
        Ok(()) => refreshed(&state, &instance.uuid, &user).await,
        Err(error) => mapped_error(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResizeRequest {
    #[serde(default)]
    vcpu: i64,
    #[serde(default)]
    memory_mib: i64,
    #[serde(default)]
    disk_gib: i64,
    nested: Option<bool>,
}

pub(crate) async fn resize_instance(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
    request: Request<Body>,
) -> Response {
    let instance = match owned_instance(&state, &uuid, &user).await {
        Ok(instance) => instance,
        Err(response) => return *response,
    };
    let request: ResizeRequest = match decode_json(request).await {
        Ok(request) => request,
        Err(response) => return *response,
    };
    let vcpu = if request.vcpu == 0 {
        instance.vcpu
    } else if let Ok(vcpu) = u32::try_from(request.vcpu) {
        vcpu
    } else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "vcpu and memory_mib must be positive",
        );
    };
    let spec = ResizeSpec {
        vcpu,
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
        return error_response(
            StatusCode::BAD_REQUEST,
            "vcpu and memory_mib must be positive",
        );
    }
    // A qcow2 overlay only grows (SPEC 11.1); catching a shrink here gives a
    // clear message instead of a qemu-img failure.
    if spec.disk_gib < instance.disk_gib {
        return error_response(
            StatusCode::BAD_REQUEST,
            "the disk can grow but never shrink",
        );
    }
    match state.0.lifecycle.resize(&instance.uuid, spec).await {
        Ok(()) => refreshed(&state, &instance.uuid, &user).await,
        Err(error) => mapped_error(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PortRequest {
    port: i64,
}

pub(crate) async fn set_port(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
    request: Request<Body>,
) -> Response {
    let instance = match owned_instance(&state, &uuid, &user).await {
        Ok(instance) => instance,
        Err(response) => return *response,
    };
    let request: PortRequest = match decode_json(request).await {
        Ok(request) => request,
        Err(response) => return *response,
    };
    let Ok(port) = u16::try_from(request.port) else {
        return error_response(StatusCode::BAD_REQUEST, "port must be between 1 and 65535");
    };
    if port == 0 {
        return error_response(StatusCode::BAD_REQUEST, "port must be between 1 and 65535");
    }
    match state.0.lifecycle.set_http_port(&instance.uuid, port).await {
        Ok(()) => refreshed(&state, &instance.uuid, &user).await,
        Err(error) => mapped_error(error),
    }
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
) -> Response {
    let instance = match owned_instance(&state, &uuid, &user).await {
        Ok(instance) => instance,
        Err(response) => return *response,
    };
    let request: VisibilityRequest = match decode_json(request).await {
        Ok(request) => request,
        Err(response) => return *response,
    };
    let visibility = match request.visibility.as_str() {
        "off" => Visibility::Off,
        "private" => Visibility::Private,
        "public" => Visibility::Public,
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "visibility must be \"off\", \"private\", or \"public\"",
            );
        }
    };
    // Through the lifecycle, not the store: visibility alters published
    // ports, and SPEC 6.3 reloads the nftables table on every change.
    match state
        .0
        .lifecycle
        .set_visibility(&instance.uuid, visibility)
        .await
    {
        Ok(()) => refreshed(&state, &instance.uuid, &user).await,
        Err(error) => mapped_error(error),
    }
}
