//! The front page (host figures, the viewer's share of them, the instance table) and the
//! new-instance form.

use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Form};
use bento_types::{User, Visibility};
use serde::{Deserialize, Serialize};

use super::{
    Page, Params, Viewer, VmView, checked, checked_name, done, failed, json, render, shell,
    visible_instances, vm_view,
};
use crate::{AppState, CreateSpec, error_parts};

#[derive(Debug, Serialize)]
pub(crate) struct UsageTile {
    label: &'static str,
    used: String,
    max: Option<String>,
    ratio: f64,
    full: bool,
}

/// A split bar: what is taken on the left, what is left on the right.
/// When more is promised than exists, the left is the whole host and the
/// right is the excess, and `over` colors it as a warning.
#[derive(Debug, Serialize)]
pub(crate) struct Bar {
    left: String,
    right: String,
    left_pct: i64,
    over: bool,
    /// What the left and right colors stand for, for the key under the bar.
    left_means: &'static str,
    right_means: &'static str,
}

impl Bar {
    /// `taken_means` names the left segment when nothing is over: "used"
    /// or "provisioned".
    pub(crate) fn split(
        taken: f64,
        total: f64,
        taken_means: &'static str,
        fmt: fn(f64) -> String,
    ) -> Self {
        if taken > total && total > 0.0 {
            Self {
                left: fmt(total),
                right: fmt(taken - total),
                left_pct: (total / taken * 100.0).round() as i64,
                over: true,
                left_means: "available on the host",
                right_means: "overprovisioned",
            }
        } else {
            Self {
                left: fmt(taken),
                right: fmt((total - taken).max(0.0)),
                left_pct: if total > 0.0 {
                    (taken / total * 100.0).round().clamp(0.0, 100.0) as i64
                } else {
                    0
                },
                over: false,
                left_means: taken_means,
                right_means: "remaining",
            }
        }
    }
}

fn gib(value: f64) -> String {
    super::gib(value)
}

fn mib(value: f64) -> String {
    super::mib(value.round() as i64)
}

#[derive(Debug, Serialize)]
pub(crate) struct HostFigures {
    memory_total_mib: i64,
    memory_provisioned_mib: i64,
    memory: Bar,
    storage_used_gib: f64,
    storage_total_gib: i64,
    storage_usage: Bar,
    storage_provisioned_gib: i64,
    storage: Bar,
    placeholder: bool,
}

#[derive(Debug, Serialize)]
struct HomeData {
    title: &'static str,
    instances: Vec<VmView>,
    counts: StateCounts,
    tiles: Vec<UsageTile>,
    host: HostFigures,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct StateCounts {
    running: usize,
    starting: usize,
    stopped: usize,
    total: usize,
}

fn ratio(used: f64, max: f64) -> f64 {
    if max <= 0.0 {
        0.0
    } else {
        (used / max).clamp(0.0, 1.0)
    }
}

/// The viewer's provisioned resources against the host's totals. There
/// are no per-user quotas: the host is the only limit.
async fn usage_tiles(
    state: &AppState,
    user: &User,
    host: &crate::HostMetrics,
) -> Result<Vec<UsageTile>, crate::BoxError> {
    let usage = state.0.store.usage_for(user.id).await?;
    let tile = |label, used: i64, max: Option<i64>, fmt: fn(i64) -> String| UsageTile {
        label,
        used: fmt(used),
        max: max.map(fmt),
        ratio: max.map_or(0.0, |max| ratio(used as f64, max as f64)),
        full: max.is_some_and(|max| used >= max),
    };
    Ok(vec![
        tile("VMs", usage.instances, None, |n| n.to_string()),
        tile("vCPU", usage.vcpu, Some(host.cpu_count), |n| n.to_string()),
        tile(
            "Memory",
            usage.memory_mib,
            Some(host.memory_total_mib),
            super::mib,
        ),
        tile("Disk", usage.disk_gib, Some(host.storage_total_gib), |n| {
            format!("{n} GiB")
        }),
    ])
}

/// Provisioned figures are real (summed from the instance rows); usage
/// figures come from [`crate::Metrics`], which may be the placeholder.
async fn host_figures(
    state: &AppState,
    host: &crate::HostMetrics,
) -> Result<HostFigures, crate::BoxError> {
    let all = state.0.store.instances().await?;
    let memory_provisioned_mib: i64 = all.iter().map(|i| i.memory_mib).sum();
    let storage_provisioned_gib: i64 = all.iter().map(|i| i.disk_gib).sum();
    Ok(HostFigures {
        memory_total_mib: host.memory_total_mib,
        memory_provisioned_mib,
        memory: Bar::split(
            memory_provisioned_mib as f64,
            host.memory_total_mib as f64,
            "provisioned",
            mib,
        ),
        storage_used_gib: host.storage_used_gib,
        storage_total_gib: host.storage_total_gib,
        storage_usage: Bar::split(
            host.storage_used_gib,
            host.storage_total_gib as f64,
            "used",
            gib,
        ),
        storage_provisioned_gib,
        storage: Bar::split(
            storage_provisioned_gib as f64,
            host.storage_total_gib as f64,
            "provisioned",
            gib,
        ),
        placeholder: host.placeholder,
    })
}

async fn views(
    state: &AppState,
    user: &User,
) -> Result<(Vec<VmView>, StateCounts), crate::BoxError> {
    let mut views = Vec::new();
    let mut counts = StateCounts::default();
    for instance in visible_instances(state, user).await? {
        match instance.state {
            bento_types::State::Running => counts.running += 1,
            bento_types::State::Starting => counts.starting += 1,
            bento_types::State::Stopped => counts.stopped += 1,
        }
        counts.total += 1;
        views.push(vm_view(state, instance, user).await);
    }
    Ok((views, counts))
}

pub(crate) async fn home(
    State(state): State<AppState>,
    Extension(user): Viewer,
    axum::extract::Query(params): Params,
) -> Response {
    let result = async {
        let shell = shell(&state, &user, "/", &params).await?;
        let (instances, counts) = views(&state, &user).await?;
        let host = state.0.metrics.host(Duration::from_secs(60)).await?;
        let data = HomeData {
            title: "Virtual Machines",
            instances,
            counts,
            tiles: usage_tiles(&state, &user, &host).await?,
            host: host_figures(&state, &host).await?,
        };
        Ok::<_, crate::BoxError>(render(StatusCode::OK, "home.html", &Page { shell, data }))
    }
    .await;
    match result {
        Ok(response) => response,
        Err(error) => failed(&state, &user, "/", error).await,
    }
}

/// The instance table alone, for the 10 s poll (SPEC 14.4).
pub(crate) async fn instances_fragment(
    State(state): State<AppState>,
    Extension(user): Viewer,
) -> Response {
    match views(&state, &user).await {
        Ok((instances, counts)) => render(
            StatusCode::OK,
            "partials/instances.html",
            &InstancesData {
                instances,
                counts,
                base_domain: state.0.base_domain.clone(),
            },
        ),
        Err(error) => {
            let (status, message) = error_parts(&error);
            super::html(
                status,
                format!("<p class=\"text-destructive\">{}</p>", message),
            )
        }
    }
}

#[derive(Debug, Serialize)]
struct InstancesData {
    instances: Vec<VmView>,
    counts: StateCounts,
    base_domain: String,
}

pub(crate) async fn sidebar_fragment(
    State(state): State<AppState>,
    Extension(user): Viewer,
) -> Response {
    match shell(&state, &user, "", &Default::default()).await {
        Ok(shell) => render(StatusCode::OK, "partials/sidebar.html", &shell),
        Err(error) => {
            let (status, message) = error_parts(&error);
            super::html(status, message)
        }
    }
}

#[derive(Debug, Serialize)]
struct SeriesJson {
    at: Vec<i64>,
    value: Vec<f64>,
}

fn series(points: &[crate::Point]) -> SeriesJson {
    SeriesJson {
        at: points.iter().map(|p| p.at).collect(),
        value: points.iter().map(|p| p.value).collect(),
    }
}

#[derive(Debug, Serialize)]
struct HostMetricsJson {
    placeholder: bool,
    cpu_pct: SeriesJson,
    memory_used_mib: SeriesJson,
    memory_total_mib: i64,
}

/// Chart data for the front page. `window` is seconds, at most a day.
pub(crate) async fn host_metrics(
    State(state): State<AppState>,
    Extension(_user): Viewer,
    axum::extract::Query(params): Params,
) -> Response {
    let window = window_from(&params);
    match state.0.metrics.host(window).await {
        Ok(host) => json(&HostMetricsJson {
            placeholder: host.placeholder,
            cpu_pct: series(&host.cpu_pct),
            memory_used_mib: series(&host.memory_used_mib),
            memory_total_mib: host.memory_total_mib,
        }),
        Err(error) => crate::mapped_error(error),
    }
}

pub(crate) fn window_from(params: &std::collections::HashMap<String, String>) -> Duration {
    let seconds = params
        .get("window")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(3600)
        .clamp(60, 86_400);
    Duration::from_secs(seconds)
}

#[derive(Debug, Serialize)]
struct NewData<'a> {
    title: &'static str,
    images: Vec<String>,
    form: NewForm,
    error: Option<&'a str>,
}

/// The form's fields, echoed back on a validation error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct NewForm {
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) image: String,
    #[serde(default)]
    pub(crate) vcpu: u32,
    /// Memory in GiB, as the form shows it; halves are allowed.
    #[serde(default)]
    pub(crate) memory_gib: f64,
    #[serde(default)]
    pub(crate) disk_gib: i64,
    #[serde(default)]
    pub(crate) public: Option<String>,
    #[serde(default)]
    pub(crate) ksm: Option<String>,
    #[serde(default)]
    pub(crate) nested: Option<String>,
}

impl NewForm {
    fn defaults(state: &AppState) -> Self {
        let defaults = state.0.defaults;
        Self {
            name: String::new(),
            image: String::new(),
            vcpu: defaults.vcpu,
            memory_gib: defaults.memory_mib as f64 / 1024.0,
            disk_gib: defaults.disk_gib,
            public: None,
            ksm: Some("on".to_string()),
            nested: None,
        }
    }
}

async fn image_names(state: &AppState) -> Result<Vec<String>, crate::BoxError> {
    Ok(state
        .0
        .store
        .images()
        .await?
        .into_iter()
        .map(|image| image.name)
        .collect())
}

async fn render_new(
    state: &AppState,
    user: &User,
    form: NewForm,
    status: StatusCode,
    error: Option<&str>,
) -> Response {
    let result = async {
        let shell = shell(state, user, "/new", &Default::default()).await?;
        let data = NewData {
            title: "New VM",
            images: image_names(state).await?,
            form,
            error,
        };
        Ok::<_, crate::BoxError>(render(status, "new.html", &Page { shell, data }))
    }
    .await;
    match result {
        Ok(response) => response,
        Err(error) => failed(state, user, "/new", error).await,
    }
}

pub(crate) async fn new_form(State(state): State<AppState>, Extension(user): Viewer) -> Response {
    let form = NewForm::defaults(&state);
    render_new(&state, &user, form, StatusCode::OK, None).await
}

/// Creates the instance, then applies the public switch as a second step:
/// creation always starts `off`, the same as the CLI (SPEC 9.2).
pub(crate) async fn create(
    State(state): State<AppState>,
    Extension(user): Viewer,
    Form(form): Form<NewForm>,
) -> Response {
    let name = match checked_name(&form.name) {
        Ok(name) => name,
        Err(message) => {
            return render_new(&state, &user, form, StatusCode::BAD_REQUEST, Some(&message)).await;
        }
    };
    if form.image.is_empty() {
        return render_new(
            &state,
            &user,
            form,
            StatusCode::BAD_REQUEST,
            Some("choose an image"),
        )
        .await;
    }
    if form.vcpu < 1 || form.memory_gib < 0.125 || form.disk_gib < 1 {
        return render_new(
            &state,
            &user,
            form,
            StatusCode::BAD_REQUEST,
            Some("vCPU, memory, and disk must all be set"),
        )
        .await;
    }
    let spec = CreateSpec {
        name,
        image: form.image.clone(),
        vcpu: form.vcpu,
        memory_mib: (form.memory_gib * 1024.0).round() as i64,
        disk_gib: form.disk_gib,
        nested: checked(&form.nested),
        ksm: checked(&form.ksm),
    };
    let instance = match state.0.lifecycle.create(user.clone(), spec).await {
        Ok(instance) => instance,
        Err(error) => {
            let (status, message) = error_parts(&error);
            return render_new(&state, &user, form, status, Some(&message)).await;
        }
    };
    if checked(&form.public)
        && let Err(error) = state
            .0
            .lifecycle
            .set_visibility(&instance.uuid, Visibility::Public)
            .await
    {
        let (_, message) = error_parts(&error);
        return axum::response::Redirect::to(&format!(
            "/vm/{}?warn={}",
            instance.uuid,
            super::urlencode(&format!("created, but not made public: {message}"))
        ))
        .into_response();
    }
    done(
        &format!("/vm/{}", instance.uuid),
        &format!("Created {}", instance.name),
    )
}
