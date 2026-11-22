//! The cross-site request check for the whole control plane (SPEC 13).
//!
//! The session cookie is valid for the base domain and so for every
//! subdomain. A guest page at `<name>.<instance_domain>` is same-site with
//! the dashboard, and `SameSite=Lax` does not stop it from sending an
//! authenticated POST. Only the `Origin` header tells the two apart.
//! [`guard_origin`] wraps a finished router, so a route added later is
//! covered without any work in its handler.

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;

use crate::{BoxError, StatusError, error_parts, error_response};

/// The origin of the dashboard as a browser writes it in an `Origin`
/// header. The scheme is always `https`: behind a TLS terminator the
/// browser still sees the public `https` name (DEPLOYING 5).
pub(crate) fn dashboard_origin(base_domain: &str) -> String {
    format!("https://{base_domain}")
}

/// Whether a request may change state. `base_domain` is the dashboard
/// host as the service knows it.
fn permitted(base_domain: &str, method: &Method, headers: &HeaderMap) -> bool {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return true;
    }
    if let Some(origin) = headers.get(header::ORIGIN) {
        return origin.to_str().ok() == Some(dashboard_origin(base_domain).as_str());
    }
    // No `Origin`. A browser that sends `Sec-Fetch-Site` says how the
    // request began. A client that sends no cookie has no ambient
    // credential to forge (a script with `Authorization: Bearer`).
    let site = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok());
    matches!(site, Some("same-origin" | "none")) || !headers.contains_key(header::COOKIE)
}

async fn check(State(base_domain): State<String>, request: Request<Body>, next: Next) -> Response {
    if permitted(&base_domain, request.method(), request.headers()) {
        return next.run(request).await;
    }
    tracing::warn!(
        method = %request.method(),
        path = request.uri().path(),
        origin = ?request.headers().get(header::ORIGIN),
        "cross-site request refused"
    );
    let error: BoxError = Box::new(StatusError::new(
        StatusCode::FORBIDDEN,
        "a request that changes state must come from the dashboard",
    ));
    let (status, message) = error_parts(&error);
    error_response(status, message)
}

/// Refuses every request that changes state and does not come from the
/// dashboard origin (SPEC 13). Apply it to the final router.
pub fn guard_origin(router: Router, base_domain: &str) -> Router {
    router.layer(middleware::from_fn_with_state(
        base_domain.to_owned(),
        check,
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::routing::{get, post};
    use tower::ServiceExt;

    use super::*;
    use crate::tests::fixture;

    const DASHBOARD: &str = "https://bento.example";
    const SIBLING: &str = "https://evil.bento.example";

    async fn send(
        app: &Router,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
    ) -> StatusCode {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        app.clone()
            .oneshot(builder.body(Body::from("{}")).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// The real API router, with the fixture's signed-in user.
    fn guarded_api() -> (crate::tests::Fixture, Router) {
        let fx = fixture();
        let app = guard_origin(fx.app.clone(), "bento.example");
        (fx, app)
    }

    #[tokio::test]
    async fn a_sibling_subdomain_cannot_post_and_nothing_changes() {
        let (fx, app) = guarded_api();
        let before = fx.store.data.lock().unwrap().keys.len();
        let status = send(
            &app,
            Method::POST,
            "/api/ssh-keys",
            &[("origin", SIBLING), ("cookie", "bento_session=sid")],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(fx.store.data.lock().unwrap().keys.len(), before);
        let status = send(
            &app,
            Method::POST,
            "/api/instances/uuid-web/stop",
            &[("origin", SIBLING), ("cookie", "bento_session=sid")],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(fx.lifecycle.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_dashboard_origin_can_post() {
        let (_fx, app) = guarded_api();
        let status = send(
            &app,
            Method::POST,
            "/api/instances/uuid-web/stop",
            &[("origin", DASHBOARD), ("cookie", "bento_session=sid")],
        )
        .await;
        assert!(status.is_success(), "{status}");
    }

    #[tokio::test]
    async fn a_bearer_request_with_no_origin_and_no_cookie_can_post() {
        let (_fx, app) = guarded_api();
        let status = send(
            &app,
            Method::POST,
            "/api/instances/uuid-web/stop",
            &[("authorization", "Bearer bento_x")],
        )
        .await;
        assert!(status.is_success(), "{status}");
    }

    #[tokio::test]
    async fn no_origin_with_a_cookie_needs_a_same_origin_fetch_site() {
        let (_fx, app) = guarded_api();
        let path = "/api/instances/uuid-web/stop";
        let cookie = ("cookie", "bento_session=sid");
        for (site, allowed) in [
            (Some("same-origin"), true),
            (Some("none"), true),
            (Some("same-site"), false),
            (Some("cross-site"), false),
            (None, false),
        ] {
            let mut headers = vec![cookie];
            if let Some(site) = site {
                headers.push(("sec-fetch-site", site));
            }
            let status = send(&app, Method::POST, path, &headers).await;
            assert_eq!(status != StatusCode::FORBIDDEN, allowed, "{site:?}");
        }
    }

    #[tokio::test]
    async fn the_origin_null_and_a_wrong_scheme_are_refused() {
        let (_fx, app) = guarded_api();
        for origin in ["null", "http://bento.example", "https://bento.example:8443"] {
            let status = send(
                &app,
                Method::POST,
                "/api/instances/uuid-web/stop",
                &[("origin", origin)],
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{origin}");
        }
    }

    #[tokio::test]
    async fn get_is_not_checked() {
        let (_fx, app) = guarded_api();
        let status = send(
            &app,
            Method::GET,
            "/api/whoami",
            &[("origin", SIBLING), ("cookie", "bento_session=sid")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    /// `/link/{token}` and `/logout` live in the auth service, not in this
    /// crate. Stand-ins prove the guard covers any route it wraps.
    #[tokio::test]
    async fn link_and_logout_posts_are_covered() {
        let hits = Arc::new(AtomicUsize::new(0));
        let count = |hits: Arc<AtomicUsize>| {
            move || async move {
                hits.fetch_add(1, Ordering::SeqCst);
            }
        };
        let app = guard_origin(
            Router::new()
                .route("/link/{token}", get(|| async {}).post(count(hits.clone())))
                .route("/logout", post(count(hits.clone()))),
            "bento.example",
        );
        for path in ["/link/abc", "/logout"] {
            let evil = [("origin", SIBLING), ("cookie", "bento_session=sid")];
            assert_eq!(
                send(&app, Method::POST, path, &evil).await,
                StatusCode::FORBIDDEN,
                "{path}"
            );
        }
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        for path in ["/link/abc", "/logout"] {
            let good = [("origin", DASHBOARD), ("cookie", "bento_session=sid")];
            assert_eq!(send(&app, Method::POST, path, &good).await, StatusCode::OK);
        }
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }
}
