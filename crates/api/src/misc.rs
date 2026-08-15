use axum::body::Body;
use axum::extract::{Extension, Path as AxumPath, State};
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bento_types::{SshKey, User};
use russh::keys::{HashAlg, PublicKey};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{
    AppState, QuotaJson, UsageJson, decode_json, error_response, is_not_found, json_response,
    mapped_error, owned_instance, rfc3339,
};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ShareJson {
    pub(crate) user: String,
    pub(crate) created_at: String,
}

pub(crate) async fn list_shares(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
) -> Response {
    let instance = match owned_instance(&state, &uuid, &user).await {
        Ok(instance) => instance,
        Err(response) => return *response,
    };
    let shares = match state.0.store.shares_for(&instance.uuid).await {
        Ok(shares) => shares,
        Err(error) => return mapped_error(error),
    };
    let mut response = Vec::with_capacity(shares.len());
    for share in shares {
        let name = state
            .0
            .store
            .user_by_id(share.user_id)
            .await
            .map(|user| user.name)
            .unwrap_or_default();
        response.push(ShareJson {
            user: name,
            created_at: rfc3339(share.created_at),
        });
    }
    json_response(StatusCode::OK, &response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShareRequest {
    user: String,
}

pub(crate) async fn add_share(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(uuid): AxumPath<String>,
    request: Request<Body>,
) -> Response {
    let instance = match owned_instance(&state, &uuid, &user).await {
        Ok(instance) => instance,
        Err(response) => return *response,
    };
    let request: ShareRequest = match decode_json(request).await {
        Ok(request) => request,
        Err(response) => return *response,
    };
    let target = match state.0.store.user_by_name(&request.user).await {
        Ok(target) => target,
        Err(error) if is_not_found(&error) => {
            return error_response(
                StatusCode::NOT_FOUND,
                format!("no user named {}", request.user),
            );
        }
        Err(error) => return mapped_error(error),
    };
    if target.id == user.id {
        return error_response(StatusCode::BAD_REQUEST, "you already own this instance");
    }
    match state.0.store.add_share(&instance.uuid, target.id).await {
        Ok(()) => json_response(
            StatusCode::CREATED,
            &ShareJson {
                user: target.name,
                created_at: String::new(),
            },
        ),
        Err(error) => mapped_error(error),
    }
}

pub(crate) async fn remove_share(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath((uuid, target_name)): AxumPath<(String, String)>,
) -> Response {
    let instance = match owned_instance(&state, &uuid, &user).await {
        Ok(instance) => instance,
        Err(response) => return *response,
    };
    let target = match state.0.store.user_by_name(&target_name).await {
        Ok(target) => target,
        Err(error) if is_not_found(&error) => {
            return error_response(
                StatusCode::NOT_FOUND,
                format!("no user named {target_name}"),
            );
        }
        Err(error) => return mapped_error(error),
    };
    match state.0.store.remove_share(&instance.uuid, target.id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => mapped_error(error),
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UserJson {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) email: String,
    pub(crate) created_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WhoamiResponse {
    pub(crate) user: UserJson,
    pub(crate) quota: Option<QuotaJson>,
    pub(crate) usage: UsageJson,
    pub(crate) operator: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) db_path: String,
}

pub(crate) fn is_operator(state: &AppState, user: &User) -> bool {
    state
        .0
        .is_operator
        .as_ref()
        .is_some_and(|predicate| predicate(user))
}

pub(crate) async fn handle_whoami(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
) -> Response {
    let usage = match state.0.store.usage_for(user.id).await {
        Ok(usage) => usage,
        Err(error) => return mapped_error(error),
    };
    let quota = match state.0.store.quota_for(user.id).await {
        Ok(quota) => Some(quota.into()),
        Err(error) if is_not_found(&error) => None,
        Err(error) => return mapped_error(error),
    };
    let operator = is_operator(&state, &user);
    json_response(
        StatusCode::OK,
        &WhoamiResponse {
            user: UserJson {
                id: user.id,
                name: user.name,
                email: user.email,
                created_at: rfc3339(user.created_at),
            },
            quota,
            usage: usage.into(),
            operator,
            db_path: if operator {
                state.0.db_path.clone()
            } else {
                String::new()
            },
        },
    )
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ImageJson {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) pinned_checksum: String,
    pub(crate) current_checksum: String,
    /// Counts instances built from a version that is no longer current
    /// (SPEC 5.1, the `images` command).
    pub(crate) instances_on_older_versions: i64,
}

pub(crate) async fn list_images(
    State(state): State<AppState>,
    Extension(_user): Extension<User>,
) -> Response {
    let images = match state.0.store.images().await {
        Ok(images) => images,
        Err(error) => return mapped_error(error),
    };
    let instances = match state.0.store.instances().await {
        Ok(instances) => instances,
        Err(error) => return mapped_error(error),
    };
    let current: std::collections::HashMap<&str, &str> = images
        .iter()
        .map(|image| {
            (
                image.name.as_str(),
                image.current_checksum.as_deref().unwrap_or_default(),
            )
        })
        .collect();
    let mut older = std::collections::HashMap::<String, i64>::new();
    for instance in instances {
        if let Some(current_checksum) = current.get(instance.image_name.as_str())
            && !instance.base_checksum.is_empty()
            && instance.base_checksum != *current_checksum
        {
            *older.entry(instance.image_name).or_default() += 1;
        }
    }
    let response: Vec<ImageJson> = images
        .into_iter()
        .map(|image| ImageJson {
            instances_on_older_versions: older.get(&image.name).copied().unwrap_or_default(),
            name: image.name,
            url: image.url,
            pinned_checksum: image.pinned_checksum.unwrap_or_default(),
            current_checksum: image.current_checksum.unwrap_or_default(),
        })
        .collect();
    json_response(StatusCode::OK, &response)
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SshKeyJson {
    pub(crate) id: i64,
    pub(crate) fingerprint: String,
    pub(crate) comment: String,
    pub(crate) public_key: String,
    pub(crate) created_at: String,
}

impl From<SshKey> for SshKeyJson {
    fn from(key: SshKey) -> Self {
        Self {
            id: key.id,
            fingerprint: key.fingerprint,
            comment: key.comment,
            public_key: key.public_key,
            created_at: rfc3339(key.created_at),
        }
    }
}

pub(crate) async fn list_ssh_keys(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
) -> Response {
    match state.0.store.ssh_keys_for_user(user.id).await {
        Ok(keys) => json_response(
            StatusCode::OK,
            &keys.into_iter().map(SshKeyJson::from).collect::<Vec<_>>(),
        ),
        Err(error) => mapped_error(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AddSshKeyRequest {
    public_key: String,
    #[serde(default)]
    comment: String,
}

pub(crate) async fn add_ssh_key(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    request: Request<Body>,
) -> Response {
    let request: AddSshKeyRequest = match decode_json(request).await {
        Ok(request) => request,
        Err(response) => return *response,
    };
    let public_key = match PublicKey::from_openssh(&request.public_key) {
        Ok(public_key) => public_key,
        Err(_) => {
            return error_response(StatusCode::BAD_REQUEST, "not a valid SSH public key");
        }
    };
    let comment = if request.comment.is_empty() {
        public_key.comment().to_string()
    } else {
        request.comment
    };
    let fingerprint = public_key.fingerprint(HashAlg::Sha256).to_string();
    let id = match state
        .0
        .store
        .add_ssh_key(user.id, &request.public_key, &fingerprint, &comment)
        .await
    {
        Ok(id) => id,
        Err(error) => return mapped_error(error),
    };
    json_response(
        StatusCode::CREATED,
        &SshKeyJson {
            id,
            fingerprint,
            comment,
            public_key: request.public_key,
            created_at: String::new(),
        },
    )
}

pub(crate) async fn delete_ssh_key(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let id = match id.parse::<i64>() {
        Ok(id) => id,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "bad key id"),
    };
    match state.0.store.delete_ssh_key(user.id, id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => mapped_error(error),
    }
}

/// Serves a consistent database snapshot (SPEC 12.1). The store writes it
/// through SQLite's backup mechanism — never a file copy, which is unsafe
/// under WAL — into a temporary directory removed after the response is built.
pub(crate) async fn dump_db(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
) -> Response {
    if !is_operator(&state, &user) {
        return error_response(StatusCode::FORBIDDEN, "operator only");
    }
    let directory = match tempfile::Builder::new().prefix("bento-dump-").tempdir() {
        Ok(directory) => directory,
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let destination = directory.path().join("bento.db");
    if let Err(error) = state.0.store.dump_db(&destination).await {
        return mapped_error(error);
    }
    let bytes = match tokio::fs::read(&destination).await {
        Ok(bytes) => bytes,
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    let content_length = bytes.len();
    let timestamp = OffsetDateTime::now_utc()
        .format(time::macros::format_description!(
            "[year][month][day]T[hour][minute][second]Z"
        ))
        .expect("current UTC time formats");
    let disposition = format!("attachment; filename=\"bento-{timestamp}.db\"");
    let mut response = bytes.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.sqlite3"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&content_length.to_string()).expect("body length is a valid header"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).expect("generated disposition is valid"),
    );
    response
}
