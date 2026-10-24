use axum::body::Body;
use axum::extract::{Extension, Path as AxumPath, State};
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bento_types::{SshKey, User};
use russh::keys::{HashAlg, PublicKey};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{
    AppState, HandlerError, StatusError, UsageJson, decode_json, error_response, is_not_found,
    json_response, owned_instance, rfc3339,
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
) -> Result<Response, HandlerError> {
    let instance = owned_instance(&state, &uuid, &user).await?;
    let shares = state.0.store.shares_for(&instance.uuid).await?;
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
    Ok(json_response(StatusCode::OK, &response))
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
) -> Result<Response, HandlerError> {
    let instance = owned_instance(&state, &uuid, &user).await?;
    let request: ShareRequest = decode_json(request).await?;
    let target = match state.0.store.user_by_name(&request.user).await {
        Ok(target) => target,
        Err(error) if is_not_found(&error) => {
            return Err(StatusError::new(
                StatusCode::NOT_FOUND,
                format!("no user named {}", request.user),
            )
            .into());
        }
        Err(error) => return Err(error.into()),
    };
    if target.id == user.id {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "you already own this instance",
        ));
    }
    state.0.store.add_share(&instance.uuid, target.id).await?;
    Ok(json_response(
        StatusCode::CREATED,
        &ShareJson {
            user: target.name,
            created_at: String::new(),
        },
    ))
}

pub(crate) async fn remove_share(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath((uuid, target_name)): AxumPath<(String, String)>,
) -> Result<Response, HandlerError> {
    let instance = owned_instance(&state, &uuid, &user).await?;
    let target = match state.0.store.user_by_name(&target_name).await {
        Ok(target) => target,
        Err(error) if is_not_found(&error) => {
            return Err(StatusError::new(
                StatusCode::NOT_FOUND,
                format!("no user named {target_name}"),
            )
            .into());
        }
        Err(error) => return Err(error.into()),
    };
    state
        .0
        .store
        .remove_share(&instance.uuid, target.id)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
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
) -> Result<Response, HandlerError> {
    let usage = state.0.store.usage_for(user.id).await?;
    let operator = is_operator(&state, &user);
    Ok(json_response(
        StatusCode::OK,
        &WhoamiResponse {
            user: UserJson {
                id: user.id,
                name: user.name,
                email: user.email,
                created_at: rfc3339(user.created_at),
            },
            usage: usage.into(),
            operator,
            db_path: if operator {
                state.0.db_path.clone()
            } else {
                String::new()
            },
        },
    ))
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ImageJson {
    pub(crate) name: String,
    /// Canonical source field for both download URLs and OCI references.
    pub(crate) source: String,
    /// Deprecated response alias retained for existing clients.
    pub(crate) url: String,
    pub(crate) kind: String,
    pub(crate) pinned_checksum: String,
    pub(crate) current_checksum: String,
    /// Counts instances built from a version that is no longer current
    /// (SPEC 5.1, the `images` command).
    pub(crate) instances_on_older_versions: i64,
}

/// Every allowlisted image with how many instances still hold an older
/// version (SPEC 5.1, the `images` command). Shared by the JSON route and
/// the configuration page.
pub(crate) async fn images_with_counts(
    state: &AppState,
) -> Result<Vec<ImageJson>, crate::BoxError> {
    let images = state.0.store.images().await?;
    let instances = state.0.store.instances().await?;
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
    Ok(images
        .into_iter()
        .map(|image| ImageJson {
            instances_on_older_versions: older.get(&image.name).copied().unwrap_or_default(),
            name: image.name,
            source: image.url.clone(),
            url: image.url,
            kind: image.kind.as_str().to_owned(),
            pinned_checksum: image.pinned_checksum.unwrap_or_default(),
            current_checksum: image.current_checksum.unwrap_or_default(),
        })
        .collect())
}

pub(crate) async fn list_images(
    State(state): State<AppState>,
    Extension(_user): Extension<User>,
) -> Result<Response, HandlerError> {
    let response = images_with_counts(&state).await?;
    Ok(json_response(StatusCode::OK, &response))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AddImageRequest {
    name: String,
    reference: String,
}

pub(crate) async fn add_image(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    request: Request<Body>,
) -> Result<Response, HandlerError> {
    if !is_operator(&state, &user) {
        return Ok(error_response(StatusCode::FORBIDDEN, "operator only"));
    }
    let request: AddImageRequest = decode_json(request).await?;
    let Some(admin) = &state.0.image_admin else {
        return Ok(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "runtime image management is unavailable",
        ));
    };
    admin
        .add_oci_image(&request.name, &request.reference)
        .await?;
    Ok(json_response(
        StatusCode::CREATED,
        &serde_json::json!({ "name": request.name }),
    ))
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
) -> Result<Response, HandlerError> {
    let keys = state.0.store.ssh_keys_for_user(user.id).await?;
    Ok(json_response(
        StatusCode::OK,
        &keys.into_iter().map(SshKeyJson::from).collect::<Vec<_>>(),
    ))
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
) -> Result<Response, HandlerError> {
    let request: AddSshKeyRequest = decode_json(request).await?;
    let Ok(public_key) = PublicKey::from_openssh(&request.public_key) else {
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "not a valid SSH public key",
        ));
    };
    let comment = if request.comment.is_empty() {
        public_key.comment().to_string()
    } else {
        request.comment
    };
    let fingerprint = public_key.fingerprint(HashAlg::Sha256).to_string();
    let id = state
        .0
        .store
        .add_ssh_key(user.id, &request.public_key, &fingerprint, &comment)
        .await?;
    Ok(json_response(
        StatusCode::CREATED,
        &SshKeyJson {
            id,
            fingerprint,
            comment,
            public_key: request.public_key,
            created_at: String::new(),
        },
    ))
}

pub(crate) async fn delete_ssh_key(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, HandlerError> {
    let Ok(id) = id.parse::<i64>() else {
        return Ok(error_response(StatusCode::BAD_REQUEST, "bad key id"));
    };
    state.0.store.delete_ssh_key(user.id, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Serves a consistent database snapshot (SPEC 12.1). The store writes it
/// through SQLite's backup mechanism — never a file copy, which is unsafe
/// under WAL — into a temporary directory removed after the response is built.
pub(crate) async fn dump_db(
    State(state): State<AppState>,
    Extension(user): Extension<User>,
) -> Result<Response, HandlerError> {
    if !is_operator(&state, &user) {
        return Ok(error_response(StatusCode::FORBIDDEN, "operator only"));
    }
    let directory = tempfile::Builder::new()
        .prefix("bento-dump-")
        .tempdir()
        .map_err(|error| HandlerError::from(crate::BoxError::from(error)))?;
    let destination = directory.path().join("bento.db");
    state.0.store.dump_db(&destination).await?;
    let bytes = tokio::fs::read(&destination)
        .await
        .map_err(|error| HandlerError::from(crate::BoxError::from(error)))?;
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
    Ok(response)
}
