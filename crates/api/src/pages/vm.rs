//! One instance: its dashboard, terminal, settings, and danger zone.

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::{Extension, Form};
use bento_types::{Instance, User, Visibility};
use serde::{Deserialize, Serialize};

use super::home::window_from;
use super::{
    Page, Params, Shell, Viewer, VmView, checked, checked_name, done, error_page, failed, json,
    readable_instance, render, shell, urlencode, vm_view,
};
use crate::{Action, AppState, HandlerError, ResizeSpec, error_parts};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Tab {
    Dashboard,
    Terminal,
    Settings,
    Sharing,
    Danger,
}

#[derive(Debug, Serialize)]
struct ShareView {
    user: String,
}

#[derive(Debug, Serialize)]
struct VmData {
    title: String,
    tab: Tab,
    vm: VmView,
    shares: Vec<ShareView>,
    /// Accounts the VM could be shared with, for the form's suggestions.
    candidates: Vec<String>,
    error: Option<String>,
    storage_used_gib: f64,
    storage: super::home::Bar,
    metrics_placeholder: bool,
}

/// Loads the instance and the shell, or answers 404 inside the shell.
async fn load(
    state: &AppState,
    user: &User,
    uuid: &str,
    path: &str,
    params: &std::collections::HashMap<String, String>,
) -> Result<(Shell, Instance), HandlerError> {
    let shell = match shell(state, user, path, params).await {
        Ok(shell) => shell,
        Err(error) => return Err(failed(state, user, path, error).await.into()),
    };
    match readable_instance(state, uuid, user).await {
        Ok(Some(instance)) => Ok((shell, instance)),
        Ok(None) => Err(error_page(shell, StatusCode::NOT_FOUND, "No such VM.").into()),
        Err(error) => Err(failed(state, user, path, error).await.into()),
    }
}

async fn page(
    state: &AppState,
    user: &User,
    uuid: &str,
    tab: Tab,
    params: &std::collections::HashMap<String, String>,
    status: StatusCode,
    error: Option<String>,
) -> Result<Response, HandlerError> {
    let path = match tab {
        Tab::Dashboard => format!("/vm/{uuid}"),
        Tab::Terminal => format!("/vm/{uuid}/terminal"),
        Tab::Settings => format!("/vm/{uuid}/settings"),
        Tab::Sharing => format!("/vm/{uuid}/sharing"),
        Tab::Danger => format!("/vm/{uuid}/danger"),
    };
    let (shell, instance) = load(state, user, uuid, &path, params).await?;
    let shares = if tab == Tab::Sharing && instance.owner_id == user.id {
        match state.0.store.shares_for(&instance.uuid).await {
            Ok(shares) => {
                let mut views = Vec::with_capacity(shares.len());
                for share in shares {
                    let name = state
                        .0
                        .store
                        .user_by_id(share.user_id)
                        .await
                        .map(|user| user.name)
                        .unwrap_or_default();
                    views.push(ShareView { user: name });
                }
                views
            }
            Err(error) => return Err(failed(state, user, &path, error).await.into()),
        }
    } else {
        Vec::new()
    };
    let candidates = if tab == Tab::Sharing && instance.owner_id == user.id {
        match state.0.store.users().await {
            Ok(users) => users
                .into_iter()
                .filter(|account| account.id != user.id)
                .filter(|account| !shares.iter().any(|share| share.user == account.name))
                .map(|account| account.name)
                .collect(),
            Err(error) => return Err(failed(state, user, &path, error).await.into()),
        }
    } else {
        Vec::new()
    };
    let (storage_used_gib, metrics_placeholder) = if tab == Tab::Dashboard {
        match state
            .0
            .metrics
            .instance(&instance.uuid, std::time::Duration::from_secs(60))
            .await
        {
            Ok(metrics) => (metrics.storage_used_gib, metrics.placeholder),
            Err(_) => (0.0, true),
        }
    } else {
        (0.0, false)
    };
    let template = match tab {
        Tab::Dashboard => "vm_dashboard.html",
        Tab::Terminal => "vm_terminal.html",
        Tab::Settings => "vm_settings.html",
        Tab::Sharing => "vm_sharing.html",
        Tab::Danger => "vm_danger.html",
    };
    let vm = vm_view(state, instance, user).await;
    let data = VmData {
        title: vm.name.clone(),
        tab,
        storage: super::home::Bar::split(storage_used_gib, vm.disk_gib as f64, "used", super::gib),
        storage_used_gib,
        metrics_placeholder,
        vm,
        shares,
        candidates,
        error,
    };
    Ok(render(status, template, &Page { shell, data }))
}

pub(crate) async fn dashboard(
    State(state): State<AppState>,
    Extension(user): Viewer,
    AxumPath(uuid): AxumPath<String>,
    axum::extract::Query(params): Params,
) -> Result<Response, HandlerError> {
    page(
        &state,
        &user,
        &uuid,
        Tab::Dashboard,
        &params,
        StatusCode::OK,
        None,
    )
    .await
}

pub(crate) async fn terminal(
    State(state): State<AppState>,
    Extension(user): Viewer,
    AxumPath(uuid): AxumPath<String>,
    axum::extract::Query(params): Params,
) -> Result<Response, HandlerError> {
    page(
        &state,
        &user,
        &uuid,
        Tab::Terminal,
        &params,
        StatusCode::OK,
        None,
    )
    .await
}

pub(crate) async fn settings(
    State(state): State<AppState>,
    Extension(user): Viewer,
    AxumPath(uuid): AxumPath<String>,
    axum::extract::Query(params): Params,
) -> Result<Response, HandlerError> {
    page(
        &state,
        &user,
        &uuid,
        Tab::Settings,
        &params,
        StatusCode::OK,
        None,
    )
    .await
}

pub(crate) async fn sharing(
    State(state): State<AppState>,
    Extension(user): Viewer,
    AxumPath(uuid): AxumPath<String>,
    axum::extract::Query(params): Params,
) -> Result<Response, HandlerError> {
    page(
        &state,
        &user,
        &uuid,
        Tab::Sharing,
        &params,
        StatusCode::OK,
        None,
    )
    .await
}

pub(crate) async fn danger(
    State(state): State<AppState>,
    Extension(user): Viewer,
    AxumPath(uuid): AxumPath<String>,
    axum::extract::Query(params): Params,
) -> Result<Response, HandlerError> {
    page(
        &state,
        &user,
        &uuid,
        Tab::Danger,
        &params,
        StatusCode::OK,
        None,
    )
    .await
}

#[derive(Debug, Serialize)]
struct SeriesJson {
    at: Vec<i64>,
    value: Vec<f64>,
}

#[derive(Debug, Serialize)]
struct InstanceMetricsJson {
    placeholder: bool,
    cpu_pct: SeriesJson,
    memory_used_mib: SeriesJson,
    memory_total_mib: u64,
}

fn series(points: &[crate::Point]) -> SeriesJson {
    SeriesJson {
        at: points.iter().map(|p| p.at).collect(),
        value: points.iter().map(|p| p.value).collect(),
    }
}

pub(crate) async fn metrics(
    State(state): State<AppState>,
    Extension(user): Viewer,
    AxumPath(uuid): AxumPath<String>,
    axum::extract::Query(params): Params,
) -> Response {
    let instance = match readable_instance(&state, &uuid, &user).await {
        Ok(Some(instance)) => instance,
        Ok(None) => return crate::error_response(StatusCode::NOT_FOUND, "not found"),
        Err(error) => return crate::mapped_error(error),
    };
    match state
        .0
        .metrics
        .instance(&instance.uuid, window_from(&params))
        .await
    {
        Ok(metrics) => json(&InstanceMetricsJson {
            placeholder: metrics.placeholder,
            cpu_pct: series(&metrics.cpu_pct),
            memory_used_mib: series(&metrics.memory_used_mib),
            memory_total_mib: instance.memory_mib,
        }),
        Err(error) => crate::mapped_error(error),
    }
}

/// The state badge and address, polled every 10 s from the header.
pub(crate) async fn state_fragment(
    State(state): State<AppState>,
    Extension(user): Viewer,
    AxumPath(uuid): AxumPath<String>,
) -> Response {
    match readable_instance(&state, &uuid, &user).await {
        Ok(Some(instance)) => {
            let vm = vm_view(&state, instance, &user).await;
            render(
                StatusCode::OK,
                "partials/state_badge.html",
                &StateData { vm },
            )
        }
        Ok(None) => super::html(StatusCode::NOT_FOUND, String::new()),
        Err(error) => crate::mapped_error(error),
    }
}

#[derive(Debug, Serialize)]
struct StateData {
    vm: VmView,
}

/// Looks up an instance the viewer owns, for the mutating routes. A
/// shared instance answers 403 like the API; a missing one 404.
async fn owned(
    state: &AppState,
    user: &User,
    uuid: &str,
    path: &str,
) -> Result<Instance, HandlerError> {
    let (status, message) = match readable_instance(state, uuid, user).await {
        Ok(Some(instance)) if instance.owner_id == user.id => return Ok(instance),
        Ok(Some(_)) => (StatusCode::FORBIDDEN, "You cannot change this VM."),
        Ok(None) => (StatusCode::NOT_FOUND, "No such VM."),
        Err(error) => return Err(failed(state, user, path, error).await.into()),
    };
    Err(match shell(state, user, path, &Default::default()).await {
        Ok(shell) => error_page(shell, status, message),
        Err(error) => failed(state, user, path, error).await,
    }
    .into())
}

#[derive(Debug, Deserialize)]
pub(crate) struct SettingsForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    vcpu: u32,
    #[serde(default)]
    memory_gib: f64,
    #[serde(default)]
    disk_gib: u64,
    #[serde(default)]
    nested: Option<String>,
    #[serde(default)]
    public: Option<String>,
    #[serde(default)]
    http_port: u16,
}

/// Applies every changed field of the settings form, in an order that
/// leaves the instance consistent if one step fails: resources, port,
/// visibility, and the rename last because it changes the URL.
pub(crate) async fn save_settings(
    State(state): State<AppState>,
    Extension(user): Viewer,
    AxumPath(uuid): AxumPath<String>,
    Form(form): Form<SettingsForm>,
) -> Result<Response, HandlerError> {
    let path = format!("/vm/{uuid}/settings");
    let instance = owned(&state, &user, &uuid, &path).await?;
    let fail = |message: String, status: StatusCode| {
        let state = state.clone();
        let user = user.clone();
        let uuid = uuid.clone();
        async move {
            page(
                &state,
                &user,
                &uuid,
                Tab::Settings,
                &Default::default(),
                status,
                Some(message),
            )
            .await
        }
    };

    let new_name = match checked_name(&form.name, &state.0.reserved_names) {
        Ok(name) => name,
        Err(message) => return fail(message, StatusCode::BAD_REQUEST).await,
    };
    let memory_mib = bento_types::mib_from_gib(form.memory_gib);
    if form.vcpu < 1 || memory_mib < 128 {
        return fail(
            "vCPU and memory must be positive".to_string(),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    if form.disk_gib < instance.disk_gib {
        return fail(
            "the disk can grow but never shrink".to_string(),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    // One switch: on means public. Off means "not public": a public VM
    // becomes private; a VM that is already private or off is left as it
    // is. (The `off` setting is being retired; see the repository issues.)
    let visibility = if checked(&form.public) {
        Visibility::Public
    } else if instance.visibility == Visibility::Public {
        Visibility::Private
    } else {
        instance.visibility
    };
    let port = form.http_port;
    if port == 0 {
        return fail(
            "port must be between 1 and 65535".to_string(),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }

    let spec = ResizeSpec {
        vcpu: form.vcpu,
        memory_mib,
        disk_gib: form.disk_gib,
        nested: checked(&form.nested),
    };
    let current = ResizeSpec {
        vcpu: instance.vcpu,
        memory_mib: instance.memory_mib,
        disk_gib: instance.disk_gib,
        nested: instance.nested,
    };
    let mut changed = Vec::new();
    if spec != current {
        if let Err(error) = state.0.lifecycle.resize(&instance.uuid, spec).await {
            let (status, message) = error_parts(&error);
            return fail(message, status).await;
        }
        changed.push("resources");
    }
    if port != instance.http_port {
        if let Err(error) = state.0.lifecycle.set_http_port(&instance.uuid, port).await {
            let (status, message) = error_parts(&error);
            return fail(message, status).await;
        }
        changed.push("port");
    }
    if visibility != instance.visibility {
        if let Err(error) = state
            .0
            .lifecycle
            .set_visibility(&instance.uuid, visibility)
            .await
        {
            let (status, message) = error_parts(&error);
            return fail(message, status).await;
        }
        changed.push("visibility");
    }
    if new_name != instance.name {
        if let Err(error) = state.0.lifecycle.rename(&instance.uuid, &new_name).await {
            let (status, message) = error_parts(&error);
            return fail(message, status).await;
        }
        changed.push("name");
    }
    let toast = if changed.is_empty() {
        "Nothing changed".to_string()
    } else {
        format!("Saved {}", changed.join(", "))
    };
    Ok(done(&path, &toast))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ShareForm {
    /// The combobox's chosen option.
    #[serde(default)]
    user: String,
    /// What was typed, for a submit without JavaScript or before a pick.
    #[serde(default)]
    user_text: String,
}

pub(crate) async fn add_share(
    State(state): State<AppState>,
    Extension(user): Viewer,
    AxumPath(uuid): AxumPath<String>,
    Form(form): Form<ShareForm>,
) -> Result<Response, HandlerError> {
    let path = format!("/vm/{uuid}/sharing");
    let instance = owned(&state, &user, &uuid, &path).await?;
    let name = if form.user.trim().is_empty() {
        form.user_text.trim()
    } else {
        form.user.trim()
    };
    if name.is_empty() {
        return Ok(warn(&path, "Type a user name"));
    }
    let target = match state.0.store.user_by_name(name).await {
        Ok(target) => target,
        Err(error) if crate::is_not_found(&error) => {
            return Ok(warn(&path, &format!("no user named {name}")));
        }
        Err(error) => return Err(failed(&state, &user, &path, error).await.into()),
    };
    if target.id == user.id {
        return Ok(warn(&path, "You own this VM"));
    }
    Ok(
        match state.0.store.add_share(&instance.uuid, target.id).await {
            Ok(()) => done(&path, &format!("Shared with {}", target.name)),
            Err(error) => {
                let (_, message) = error_parts(&error);
                warn(&path, &message)
            }
        },
    )
}

pub(crate) async fn remove_share(
    State(state): State<AppState>,
    Extension(user): Viewer,
    AxumPath((uuid, target_name)): AxumPath<(String, String)>,
) -> Result<Response, HandlerError> {
    let path = format!("/vm/{uuid}/sharing");
    let instance = owned(&state, &user, &uuid, &path).await?;
    let target = match state.0.store.user_by_name(&target_name).await {
        Ok(target) => target,
        Err(error) if crate::is_not_found(&error) => {
            return Ok(warn(&path, &format!("no user named {target_name}")));
        }
        Err(error) => return Err(failed(&state, &user, &path, error).await.into()),
    };
    Ok(
        match state.0.store.remove_share(&instance.uuid, target.id).await {
            Ok(()) => done(&path, &format!("Revoked {}", target.name)),
            Err(error) => {
                let (_, message) = error_parts(&error);
                warn(&path, &message)
            }
        },
    )
}

fn warn(path: &str, message: &str) -> Response {
    use axum::response::IntoResponse;
    axum::response::Redirect::to(&format!("{path}?warn={}", urlencode(message))).into_response()
}

async fn action(
    state: AppState,
    user: User,
    uuid: String,
    action: Action,
) -> Result<Response, HandlerError> {
    let path = format!("/vm/{uuid}/danger");
    let instance = owned(&state, &user, &uuid, &path).await?;
    Ok(
        match action.run(&*state.0.lifecycle, &instance.uuid).await {
            Ok(()) => done(
                &path,
                &format!(
                    "{} {}",
                    match action {
                        Action::Start => "Starting",
                        Action::Stop => "Stopping",
                        Action::Restart => "Restarting",
                    },
                    instance.name
                ),
            ),
            Err(error) => {
                let (_, message) = error_parts(&error);
                warn(&path, &message)
            }
        },
    )
}

pub(crate) async fn start(
    State(state): State<AppState>,
    Extension(user): Viewer,
    AxumPath(uuid): AxumPath<String>,
) -> Result<Response, HandlerError> {
    action(state, user, uuid, Action::Start).await
}

pub(crate) async fn stop(
    State(state): State<AppState>,
    Extension(user): Viewer,
    AxumPath(uuid): AxumPath<String>,
) -> Result<Response, HandlerError> {
    action(state, user, uuid, Action::Stop).await
}

pub(crate) async fn restart(
    State(state): State<AppState>,
    Extension(user): Viewer,
    AxumPath(uuid): AxumPath<String>,
) -> Result<Response, HandlerError> {
    action(state, user, uuid, Action::Restart).await
}

#[derive(Debug, Deserialize)]
pub(crate) struct DeleteForm {
    #[serde(default)]
    confirm: String,
}

/// The `rm` confirmation (SPEC 14.4, 15): the typed name must match.
pub(crate) async fn delete(
    State(state): State<AppState>,
    Extension(user): Viewer,
    AxumPath(uuid): AxumPath<String>,
    Form(form): Form<DeleteForm>,
) -> Result<Response, HandlerError> {
    let path = format!("/vm/{uuid}/danger");
    let instance = owned(&state, &user, &uuid, &path).await?;
    if form.confirm.trim() != instance.name {
        return Ok(warn(&path, "Name does not match"));
    }
    Ok(match state.0.lifecycle.delete(&instance.uuid).await {
        Ok(()) => done("/", &format!("Deleted {}", instance.name)),
        Err(error) => {
            let (_, message) = error_parts(&error);
            warn(&path, &message)
        }
    })
}
