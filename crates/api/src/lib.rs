//! The control-plane HTTP API consumed by the dashboard (SPEC section 14).
//!
//! The dashboard exposes every operation represented by its section 15
//! controls, so the API carries instance CRUD and lifecycle actions, rename,
//! resize, port, visibility, shares, images, SSH keys, whoami, and the database
//! download of SPEC 12.1.

mod instances;
mod interfaces;
mod misc;

pub(crate) use instances::*;
pub use interfaces::*;
pub(crate) use misc::*;

#[cfg(test)]
mod tests;

use std::error::Error as StdError;
use std::sync::Arc;

#[cfg(test)]
use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post};
#[cfg(test)]
use bento_store::Usage;
#[cfg(test)]
use bento_types::{Instance, Quota, Share, SshKey, User, Visibility};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Dependencies and operator policy for the API router.
pub struct Config {
    pub store: Arc<dyn Store>,
    pub lifecycle: Arc<dyn Lifecycle>,
    pub auth: Arc<dyn Authenticator>,

    /// Gates the operator-only database download (SPEC 12.1). `None` denies
    /// everyone.
    pub is_operator: Option<OperatorPredicate>,
    /// Builds newly appended OCI images. `None` disables runtime additions.
    pub image_admin: Option<Arc<dyn ImageAdmin>>,

    /// The documented database path shown to operators (SPEC 12.1).
    pub db_path: String,
}

#[derive(Clone)]
pub(crate) struct AppState(pub(crate) Arc<Config>);

/// Builds the `/api/` router. It carries every route under its full `/api`
/// path and can therefore be merged directly with `bento_dashboard::router()`.
pub fn router(config: Config) -> Router {
    let state = AppState(Arc::new(config));
    let routes = Router::new()
        .route("/api/whoami", get(handle_whoami))
        .route("/api/instances", get(list_instances).post(create_instance))
        .route(
            "/api/instances/{uuid}",
            get(get_instance).delete(delete_instance),
        )
        .route("/api/instances/{uuid}/start", post(start_instance))
        .route("/api/instances/{uuid}/stop", post(stop_instance))
        .route("/api/instances/{uuid}/restart", post(restart_instance))
        .route("/api/instances/{uuid}/rename", post(rename_instance))
        .route("/api/instances/{uuid}/resize", post(resize_instance))
        .route("/api/instances/{uuid}/port", post(set_port))
        .route("/api/instances/{uuid}/visibility", post(set_visibility))
        .route(
            "/api/instances/{uuid}/shares",
            get(list_shares).post(add_share),
        )
        .route("/api/instances/{uuid}/shares/{user}", delete(remove_share))
        .route("/api/images", get(list_images).post(add_image))
        .route("/api/ssh-keys", get(list_ssh_keys).post(add_ssh_key))
        .route("/api/ssh-keys/{id}", delete(delete_ssh_key))
        .route("/api/db.sqlite", get(dump_db))
        .route("/api/", any(not_found))
        .route("/api/{*path}", any(not_found))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate));
    routes.with_state(state)
}

async fn authenticate(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    match state.0.auth.user_from_headers(request.headers()).await {
        Ok(user) => {
            request.extensions_mut().insert(user);
            next.run(request).await
        }
        Err(_) => error_response(StatusCode::UNAUTHORIZED, "unauthorized"),
    }
}

async fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not found")
}

#[derive(Debug, Serialize, Deserialize)]
struct ErrorBody {
    error: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    cooldown_seconds: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quota: Option<QuotaDetail>,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

#[derive(Debug, Serialize, Deserialize)]
struct QuotaDetail {
    limit: String,
    used: i64,
    requested: i64,
    max: i64,
}

pub(crate) fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response {
    let mut body = serde_json::to_vec(value).expect("API response values serialize");
    body.push(b'\n');
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

pub(crate) fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    json_response(
        status,
        &ErrorBody {
            error: message.into(),
            cooldown_seconds: 0,
            quota: None,
        },
    )
}

fn find_error<'a, E: StdError + 'static>(error: &'a (dyn StdError + 'static)) -> Option<&'a E> {
    let mut current = Some(error);
    while let Some(candidate) = current {
        if let Some(found) = candidate.downcast_ref::<E>() {
            return Some(found);
        }
        current = candidate.source();
    }
    None
}

/// Maps persistence and lifecycle failures to the established HTTP contract.
pub(crate) fn mapped_error(error: BoxError) -> Response {
    if let Some(store_error) = find_error::<StoreError>(error.as_ref()) {
        match store_error {
            StoreError::NotFound => {
                return error_response(StatusCode::NOT_FOUND, "not found");
            }
            StoreError::NameTaken => {
                return error_response(StatusCode::CONFLICT, "that name is taken");
            }
            StoreError::Quota {
                limit,
                used,
                requested,
                max,
            } => {
                return json_response(
                    StatusCode::CONFLICT,
                    &ErrorBody {
                        error: store_error.to_string(),
                        cooldown_seconds: 0,
                        quota: Some(QuotaDetail {
                            limit: limit.clone(),
                            used: *used,
                            requested: *requested,
                            max: *max,
                        }),
                    },
                );
            }
            StoreError::NameCooldown { remaining, .. } => {
                return json_response(
                    StatusCode::CONFLICT,
                    &ErrorBody {
                        error: store_error.to_string(),
                        cooldown_seconds: remaining.as_secs() as i64,
                        quota: None,
                    },
                );
            }
        }
    }
    if let Some(status_error) = find_error::<StatusError>(error.as_ref()) {
        return error_response(status_error.http_status(), status_error.to_string());
    }
    error_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

pub(crate) fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let label_char = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    !bytes.is_empty()
        && bytes.len() <= 63
        && label_char(bytes[0])
        && label_char(bytes[bytes.len() - 1])
        && bytes.iter().all(|byte| label_char(*byte) || *byte == b'-')
}

pub(crate) const BAD_NAME: &str = "instance name must be a DNS label: lower-case letters, digits, and hyphens, up to 63 characters";

pub(crate) fn rfc3339(value: OffsetDateTime) -> String {
    if value == OffsetDateTime::UNIX_EPOCH {
        return String::new();
    }
    value
        .to_offset(time::UtcOffset::UTC)
        .format(time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second]Z"
        ))
        .unwrap_or_default()
}

pub(crate) fn optional_rfc3339(value: Option<OffsetDateTime>) -> String {
    value.map(rfc3339).unwrap_or_default()
}

pub(crate) async fn decode_json<T: for<'de> Deserialize<'de>>(
    request: Request<Body>,
) -> Result<T, Box<Response>> {
    let bytes = to_bytes(request.into_body(), 1 << 20)
        .await
        .map_err(|error| {
            Box::new(error_response(
                StatusCode::BAD_REQUEST,
                format!("bad request body: {error}"),
            ))
        })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        Box::new(error_response(
            StatusCode::BAD_REQUEST,
            format!("bad request body: {error}"),
        ))
    })
}

pub(crate) fn is_not_found(error: &BoxError) -> bool {
    matches!(
        find_error::<StoreError>(error.as_ref()),
        Some(StoreError::NotFound)
    )
}
