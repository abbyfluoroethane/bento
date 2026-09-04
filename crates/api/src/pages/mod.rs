//! The server-rendered dashboard (SPEC 14). Every page is HTML from a
//! template over the same [`Store`], [`Lifecycle`], and [`Metrics`]
//! adapters the JSON API uses; HTMX on the client polls a few fragments
//! and boosts navigation, and every form still works without it.
//!
//! Routes are mounted at `/` beside `/api/`; the binary wraps this router
//! in the sign-in gate, so a request that reaches a handler has a session.

mod account;
mod admin;
mod home;
mod vm;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use bento_types::{Instance, User};
use minijinja::{Environment, Value};
use serde::Serialize;
use time::OffsetDateTime;

use crate::{AppState, BoxError, Config, error_parts, is_operator, valid_name};

/// Builds the page router over a [`Config`] shared with the API router.
pub fn router(config: Arc<Config>) -> Router {
    let state = AppState(config);
    Router::new()
        .route("/", get(home::home))
        .route("/fragments/instances", get(home::instances_fragment))
        .route("/fragments/sidebar", get(home::sidebar_fragment))
        .route("/metrics/host.json", get(home::host_metrics))
        .route("/new", get(home::new_form).post(home::create))
        .route("/vm/{uuid}", get(vm::dashboard))
        .route("/vm/{uuid}/metrics.json", get(vm::metrics))
        .route("/vm/{uuid}/fragments/state", get(vm::state_fragment))
        .route("/vm/{uuid}/terminal", get(vm::terminal))
        .route(
            "/vm/{uuid}/settings",
            get(vm::settings).post(vm::save_settings),
        )
        .route("/vm/{uuid}/sharing", get(vm::sharing))
        .route("/vm/{uuid}/shares", post(vm::add_share))
        .route("/vm/{uuid}/shares/{user}/remove", post(vm::remove_share))
        .route("/vm/{uuid}/danger", get(vm::danger))
        .route("/vm/{uuid}/start", post(vm::start))
        .route("/vm/{uuid}/stop", post(vm::stop))
        .route("/vm/{uuid}/restart", post(vm::restart))
        .route("/vm/{uuid}/delete", post(vm::delete))
        .route("/settings", get(admin::users))
        .route("/settings/configuration", get(admin::configuration))
        .route("/settings/images", post(admin::add_image))
        .route("/settings/account", get(account::account))
        .route("/settings/account/keys", post(account::add_key))
        .route(
            "/settings/account/keys/{id}/remove",
            post(account::remove_key),
        )
        .route(
            "/account",
            get(|| async { Redirect::permanent("/settings/account") }),
        )
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}

/// Resolves the user like the API does. A request without a session should
/// never get here (the gate answers first), so the failure path only has
/// to be safe: HTMX gets a redirect header it acts on, a browser gets a
/// link. Neither redirects to `/`, which would loop when the gate is off.
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
        Err(_) => {
            let mut response = html(
                StatusCode::UNAUTHORIZED,
                "<!doctype html><title>Sign in</title><p>Your session ended. <a href=\"/\">Sign in again</a>.</p>".to_string(),
            );
            if request.headers().contains_key("hx-request") {
                response
                    .headers_mut()
                    .insert("hx-redirect", HeaderValue::from_static("/"));
            }
            response
        }
    }
}

// ---------------------------------------------------------------------------
// Templates

static ENV: LazyLock<Environment<'static>> = LazyLock::new(|| {
    let mut env = Environment::new();
    macro_rules! template {
        ($name:literal) => {
            env.add_template($name, include_str!(concat!("../../templates/", $name)))
                .expect(concat!("template ", $name, " parses"));
        };
    }
    template!("layout.html");
    template!("home.html");
    template!("new.html");
    template!("vm_dashboard.html");
    template!("vm_terminal.html");
    template!("vm_settings.html");
    template!("vm_sharing.html");
    template!("vm_danger.html");
    template!("settings_users.html");
    template!("settings_configuration.html");
    template!("account.html");
    template!("error.html");
    template!("partials/sidebar.html");
    template!("partials/settings_tabs.html");
    template!("partials/instances.html");
    template!("partials/vm_header.html");
    template!("partials/state_badge.html");
    template!("partials/toast.html");
    template!("partials/bar.html");
    env.add_filter("mib", mib);
    env.add_filter("gib", gib);
    env.add_filter("pct", pct);
    env.add_filter("num", |value: f64| -> String {
        if (value - value.round()).abs() < 1e-9 {
            format!("{}", value.round() as i64)
        } else {
            format!("{value}")
        }
    });
    env.add_filter("startswith", |text: &str, prefix: &str| {
        text.starts_with(prefix)
    });
    env.add_filter("truncate", |text: &str, length: usize| -> String {
        if text.chars().count() <= length {
            text.to_string()
        } else {
            let mut out: String = text.chars().take(length.saturating_sub(1)).collect();
            out.push('…');
            out
        }
    });
    env
});

/// Renders a template to an HTML response.
pub(crate) fn render<T: Serialize>(status: StatusCode, template: &str, context: &T) -> Response {
    let body = ENV
        .get_template(template)
        .and_then(|template| template.render(Value::from_serialize(context)));
    match body {
        Ok(body) => html(status, body),
        Err(error) => {
            tracing::error!(%error, template, "template render failed");
            html(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "<!doctype html><title>Error</title><p>Template error: {}</p>",
                    escape(&error.to_string())
                ),
            )
        }
    }
}

pub(crate) fn html(status: StatusCode, body: String) -> Response {
    (
        status,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
        .into_response()
}

pub(crate) fn json<T: Serialize>(value: &T) -> Response {
    crate::json_response(StatusCode::OK, value)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// Formatting helpers, exposed to templates as filters

/// `1536` → `1.5 GiB`, `512` → `512 MiB`.
pub(crate) fn mib(value: i64) -> String {
    if value >= 1024 {
        let gib = value as f64 / 1024.0;
        if (gib - gib.round()).abs() < 0.005 {
            return format!("{} GiB", gib.round() as i64);
        }
        return format!("{gib:.1} GiB");
    }
    format!("{value} MiB")
}

pub(crate) fn gib(value: f64) -> String {
    if (value - value.round()).abs() < 0.05 {
        format!("{} GiB", value.round() as i64)
    } else {
        format!("{value:.1} GiB")
    }
}

pub(crate) fn pct(value: f64) -> String {
    format!("{}%", value.round() as i64)
}

/// A human cooldown, `90` → `2 min`, `7200` → `2 h`.
pub(crate) fn cooldown_text(seconds: u64) -> String {
    if seconds >= 3600 {
        format!("{} h", seconds.div_ceil(3600))
    } else if seconds >= 60 {
        format!("{} min", seconds.div_ceil(60))
    } else {
        format!("{seconds} s")
    }
}

/// "3 min ago", "never".
pub(crate) fn relative(value: Option<OffsetDateTime>) -> String {
    let Some(then) = value else {
        return "never".to_string();
    };
    if then == OffsetDateTime::UNIX_EPOCH {
        return "never".to_string();
    }
    let seconds = (OffsetDateTime::now_utc() - then).whole_seconds().max(0);
    if seconds < 60 {
        return "just now".to_string();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes} min ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours} h ago");
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{days} d ago");
    }
    then.date().to_string()
}

// ---------------------------------------------------------------------------
// The shell every page shares

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Toast {
    pub(crate) category: &'static str,
    pub(crate) title: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SidebarVm {
    pub(crate) uuid: String,
    pub(crate) name: String,
    pub(crate) state: String,
    pub(crate) shared: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UserCtx {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) email: String,
    pub(crate) operator: bool,
}

/// What the layout needs on every page: the viewer, the sidebar list, and
/// a one-shot toast carried over a redirect in the query string.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Shell {
    pub(crate) user: UserCtx,
    pub(crate) vms: Vec<SidebarVm>,
    pub(crate) base_domain: String,
    pub(crate) path: String,
    pub(crate) toast: Option<Toast>,
}

/// A rendered page: the shell plus the page's own fields, flattened.
#[derive(Debug, Serialize)]
pub(crate) struct Page<T: Serialize> {
    #[serde(flatten)]
    pub(crate) shell: Shell,
    #[serde(flatten)]
    pub(crate) data: T,
}

pub(crate) type Params = Query<HashMap<String, String>>;

pub(crate) fn toast_from(params: &HashMap<String, String>) -> Option<Toast> {
    if let Some(title) = params.get("toast").filter(|title| !title.is_empty()) {
        return Some(Toast {
            category: "success",
            title: title.clone(),
        });
    }
    if let Some(title) = params.get("warn").filter(|title| !title.is_empty()) {
        return Some(Toast {
            category: "error",
            title: title.clone(),
        });
    }
    None
}

/// Lists the viewer's own and shared instances, sorted by name: the
/// sidebar and the front-page table are the same list.
pub(crate) async fn visible_instances(
    state: &AppState,
    user: &User,
) -> Result<Vec<Instance>, BoxError> {
    let mut instances = state.0.store.instances_by_owner(user.id).await?;
    instances.extend(state.0.store.instances_shared_with(user.id).await?);
    instances.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(instances)
}

pub(crate) async fn shell(
    state: &AppState,
    user: &User,
    path: &str,
    params: &HashMap<String, String>,
) -> Result<Shell, BoxError> {
    let vms = visible_instances(state, user)
        .await?
        .into_iter()
        .map(|instance| SidebarVm {
            uuid: instance.uuid,
            name: instance.name,
            state: instance.state.to_string(),
            shared: instance.owner_id != user.id,
        })
        .collect();
    Ok(Shell {
        user: UserCtx {
            id: user.id,
            name: user.name.clone(),
            email: user.email.clone(),
            operator: is_operator(state, user),
        },
        vms,
        base_domain: state.0.base_domain.clone(),
        path: path.to_string(),
        toast: toast_from(params),
    })
}

/// A full page for an error: same shell, a status, and the message.
#[derive(Debug, Serialize)]
struct ErrorData {
    title: String,
    status: u16,
    message: String,
}

pub(crate) fn error_page(shell: Shell, status: StatusCode, message: impl Into<String>) -> Response {
    let data = ErrorData {
        title: status.canonical_reason().unwrap_or("Error").to_string(),
        status: status.as_u16(),
        message: message.into(),
    };
    render(status, "error.html", &Page { shell, data })
}

/// Maps an adapter error to an error page inside the shell. When even the
/// shell cannot be built (the store is down), a bare page still says so.
pub(crate) async fn failed(state: &AppState, user: &User, path: &str, error: BoxError) -> Response {
    let (status, message) = error_parts(&error);
    match shell(state, user, path, &HashMap::new()).await {
        Ok(shell) => error_page(shell, status, message),
        Err(_) => html(
            status,
            format!(
                "<!doctype html><title>Error</title><p>{}</p>",
                escape(&message)
            ),
        ),
    }
}

/// Redirects after a successful form post, carrying a toast.
pub(crate) fn done(path: &str, toast: &str) -> Response {
    Redirect::to(&format!("{path}?toast={}", urlencode(toast))).into_response()
}

pub(crate) fn urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// An instance as the templates see it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct VmView {
    pub(crate) uuid: String,
    pub(crate) name: String,
    pub(crate) owner: String,
    pub(crate) mine: bool,
    pub(crate) state: String,
    pub(crate) address: String,
    pub(crate) image: String,
    pub(crate) vcpu: u32,
    pub(crate) memory_mib: i64,
    pub(crate) memory_gib: f64,
    pub(crate) disk_gib: i64,
    pub(crate) nested: bool,
    pub(crate) ksm: bool,
    pub(crate) http_port: u16,
    pub(crate) visibility: String,
    pub(crate) created: String,
    pub(crate) last_seen: String,
    pub(crate) url: String,
    pub(crate) ssh: String,
}

pub(crate) async fn vm_view(state: &AppState, instance: Instance, viewer: &User) -> VmView {
    let owner = if instance.owner_id == viewer.id {
        viewer.name.clone()
    } else {
        state
            .0
            .store
            .user_by_id(instance.owner_id)
            .await
            .map(|user| user.name)
            .unwrap_or_default()
    };
    let domain = &state.0.base_domain;
    VmView {
        url: format!("https://{}.{domain}/", instance.name),
        ssh: format!("ssh {}@{domain}", instance.name),
        uuid: instance.uuid,
        mine: instance.owner_id == viewer.id,
        owner,
        name: instance.name,
        state: instance.state.to_string(),
        address: instance.address,
        image: instance.image_name,
        vcpu: instance.vcpu,
        memory_mib: instance.memory_mib,
        memory_gib: instance.memory_mib as f64 / 1024.0,
        disk_gib: instance.disk_gib,
        nested: instance.nested,
        ksm: instance.ksm,
        http_port: instance.http_port,
        visibility: instance.visibility.to_string(),
        created: relative(Some(instance.created_at)),
        last_seen: relative(instance.last_seen_at),
    }
}

/// Looks up an instance the viewer may read; `None` renders as 404 so a
/// stranger cannot tell an existing UUID from a missing one.
pub(crate) async fn readable_instance(
    state: &AppState,
    uuid: &str,
    user: &User,
) -> Result<Option<Instance>, BoxError> {
    let instance = match state.0.store.instance(uuid).await {
        Ok(instance) => instance,
        Err(error) if crate::is_not_found(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    if crate::can_read_instance(state, &instance, user).await {
        Ok(Some(instance))
    } else {
        Ok(None)
    }
}

/// Parses a name the way the API does, with the same message.
pub(crate) fn checked_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if valid_name(name) {
        Ok(name.to_string())
    } else {
        Err(crate::BAD_NAME.to_string())
    }
}

/// Form checkboxes arrive as `on` or not at all.
pub(crate) fn checked(value: &Option<String>) -> bool {
    value.is_some()
}

// Re-exported for the handlers' signatures.
pub(crate) type Viewer = Extension<User>;
