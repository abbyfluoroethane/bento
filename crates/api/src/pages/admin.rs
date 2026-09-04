//! The operator's settings: users and their consumption, and the
//! deployment's configuration (images, database).

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::{Extension, Form};
use bento_types::User;
use serde::{Deserialize, Serialize};

use super::{Page, Params, Viewer, done, error_page, failed, render, shell, urlencode};
use crate::{AppState, error_parts, is_operator};

#[derive(Debug, Serialize)]
struct UserRow {
    name: String,
    email: String,
    instances: i64,
    vcpu: i64,
    memory_mib: i64,
    disk_gib: i64,
    cpu_pct: f64,
    memory_used_mib: i64,
    storage_used_gib: f64,
    placeholder: bool,
}

#[derive(Debug, Serialize)]
struct UsersData {
    title: &'static str,
    tab: &'static str,
    users: Vec<UserRow>,
    host_cpu_count: i64,
    host_memory_mib: i64,
    host_storage_gib: i64,
}

async fn operator_only(
    state: &AppState,
    user: &User,
    path: &str,
) -> Result<super::Shell, Box<Response>> {
    let shell = match shell(state, user, path, &Default::default()).await {
        Ok(shell) => shell,
        Err(error) => return Err(Box::new(failed(state, user, path, error).await)),
    };
    if is_operator(state, user) {
        Ok(shell)
    } else {
        Err(Box::new(error_page(
            shell,
            StatusCode::FORBIDDEN,
            "Operators only.",
        )))
    }
}

/// Every account, with what it holds.
pub(crate) async fn users(
    State(state): State<AppState>,
    Extension(user): Viewer,
    axum::extract::Query(params): Params,
) -> Response {
    let path = "/settings";
    if !is_operator(&state, &user) {
        // Settings for a non-operator is the account tab alone.
        use axum::response::IntoResponse;
        return axum::response::Redirect::to("/settings/account").into_response();
    }
    let mut shell = match operator_only(&state, &user, path).await {
        Ok(shell) => shell,
        Err(response) => return *response,
    };
    shell.toast = super::toast_from(&params);
    let result = async {
        let accounts = state.0.store.users().await?;
        let mut rows = Vec::with_capacity(accounts.len());
        for account in accounts {
            let id = account.id;
            let usage = state.0.store.usage_for(id).await?;
            let metrics = state.0.metrics.user(id).await.unwrap_or_default();
            rows.push(UserRow {
                name: account.name,
                email: account.email,
                instances: usage.instances,
                vcpu: usage.vcpu,
                memory_mib: usage.memory_mib,
                disk_gib: usage.disk_gib,
                cpu_pct: metrics.cpu_pct,
                memory_used_mib: metrics.memory_used_mib,
                storage_used_gib: metrics.storage_used_gib,
                placeholder: metrics.placeholder,
            });
        }
        rows.sort_by(|left, right| left.name.cmp(&right.name));
        let host = state
            .0
            .metrics
            .host(std::time::Duration::from_secs(60))
            .await?;
        Ok::<_, crate::BoxError>((rows, host))
    }
    .await;
    match result {
        Ok((users, host)) => render(
            StatusCode::OK,
            "settings_users.html",
            &Page {
                shell,
                data: UsersData {
                    title: "Settings",
                    tab: "users",
                    users,
                    host_cpu_count: host.cpu_count,
                    host_memory_mib: host.memory_total_mib,
                    host_storage_gib: host.storage_total_gib,
                },
            },
        ),
        Err(error) => failed(&state, &user, path, error).await,
    }
}

#[derive(Debug, Serialize)]
struct ImageRow {
    name: String,
    kind: String,
    source: String,
    current_checksum: String,
    pinned_checksum: String,
    instances_on_older_versions: i64,
}

#[derive(Debug, Serialize)]
struct ConfigurationData {
    title: &'static str,
    tab: &'static str,
    images: Vec<ImageRow>,
    db_path: String,
    image_admin: bool,
    defaults: DefaultsView,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct DefaultsView {
    vcpu: u32,
    memory_mib: i64,
    disk_gib: i64,
}

async fn render_configuration(
    state: &AppState,
    user: &User,
    params: &std::collections::HashMap<String, String>,
    status: StatusCode,
    error: Option<String>,
) -> Response {
    let path = "/settings/configuration";
    let mut shell = match operator_only(state, user, path).await {
        Ok(shell) => shell,
        Err(response) => return *response,
    };
    shell.toast = super::toast_from(params);
    let images = match crate::images_with_counts(state).await {
        Ok(images) => images
            .into_iter()
            .map(|image| ImageRow {
                name: image.name,
                kind: image.kind,
                source: image.source,
                current_checksum: image.current_checksum,
                pinned_checksum: image.pinned_checksum,
                instances_on_older_versions: image.instances_on_older_versions,
            })
            .collect(),
        Err(error) => return failed(state, user, path, error).await,
    };
    let defaults = state.0.defaults;
    render(
        status,
        "settings_configuration.html",
        &Page {
            shell,
            data: ConfigurationData {
                title: "Settings",
                tab: "configuration",
                images,
                db_path: state.0.db_path.clone(),
                image_admin: state.0.image_admin.is_some(),
                defaults: DefaultsView {
                    vcpu: defaults.vcpu,
                    memory_mib: defaults.memory_mib,
                    disk_gib: defaults.disk_gib,
                },
                error,
            },
        },
    )
}

pub(crate) async fn configuration(
    State(state): State<AppState>,
    Extension(user): Viewer,
    axum::extract::Query(params): Params,
) -> Response {
    render_configuration(&state, &user, &params, StatusCode::OK, None).await
}

#[derive(Debug, Deserialize)]
pub(crate) struct ImageForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    reference: String,
}

pub(crate) async fn add_image(
    State(state): State<AppState>,
    Extension(user): Viewer,
    Form(form): Form<ImageForm>,
) -> Response {
    if !is_operator(&state, &user) {
        return render_configuration(
            &state,
            &user,
            &Default::default(),
            StatusCode::FORBIDDEN,
            None,
        )
        .await;
    }
    let Some(admin) = &state.0.image_admin else {
        return render_configuration(
            &state,
            &user,
            &Default::default(),
            StatusCode::SERVICE_UNAVAILABLE,
            Some("runtime image management is unavailable".to_string()),
        )
        .await;
    };
    let name = form.name.trim();
    let reference = form.reference.trim();
    if name.is_empty() || reference.is_empty() {
        return render_configuration(
            &state,
            &user,
            &Default::default(),
            StatusCode::BAD_REQUEST,
            Some("both the name and the OCI reference are required".to_string()),
        )
        .await;
    }
    match admin.add_oci_image(name, reference).await {
        Ok(()) => done("/settings/configuration", &format!("Added {name}")),
        Err(error) => {
            let (status, message) = error_parts(&error);
            let _ = urlencode;
            render_configuration(&state, &user, &Default::default(), status, Some(message)).await
        }
    }
}
