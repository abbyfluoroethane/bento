//! Serves the built web dashboard (SPEC 14).
//!
//! The assets are a single-page application built in `web/` and embedded
//! at compile time; the deployed artifact stays one binary with no Node
//! runtime (SPEC 14.1). The control plane mounts this router at `/` and
//! the API at `/api/`, so this router never sees an API request.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// The Vite build output, committed to the repository (see `web/README.md`).
#[derive(RustEmbed)]
#[folder = "../../web/dist"]
struct Dist;

/// An asset source. The embedded build satisfies it; tests substitute an
/// in-memory map so the routing rules are exercised without a Node build.
pub trait Assets: Send + Sync + 'static {
    /// The bytes of `path`, or `None` when no such file exists.
    fn get(&self, path: &str) -> Option<Vec<u8>>;
}

/// The dashboard build embedded in this binary.
#[derive(Clone, Copy, Default)]
pub struct Embedded;

impl Assets for Embedded {
    fn get(&self, path: &str) -> Option<Vec<u8>> {
        Dist::get(path).map(|f| f.data.into_owned())
    }
}

/// Serves the embedded dashboard build. When the build carries no
/// `index.html` — an asset tree that was never built — it serves a plain
/// placeholder that says how to build the assets.
pub fn router() -> Router {
    router_from(Embedded)
}

/// Serves a dashboard build from any [`Assets`]. Paths that name a file
/// are served as-is; every other path falls back to `index.html` so
/// client-side routes survive a reload. Hashed assets under `assets/`
/// are immutable and cached for a year; `index.html` is revalidated on
/// every load so a new deploy takes effect at once.
pub fn router_from(assets: impl Assets) -> Router {
    if assets.get("index.html").is_none() {
        return Router::new().fallback(|| async { placeholder() });
    }
    let assets = std::sync::Arc::new(assets);
    Router::new().fallback(move |req: Request<Body>| {
        let assets = assets.clone();
        async move { serve(assets.as_ref(), req.method(), req.uri()) }
    })
}

/// The content type for a path, from its extension.
fn content_type(path: &str) -> HeaderValue {
    let guess = mime_guess::from_path(path).first_or_octet_stream();
    HeaderValue::from_str(guess.as_ref())
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}

/// Normalizes a request path to an asset path: leading slash removed,
/// `.` and `..` segments resolved away so nothing can escape the asset
/// tree, and an empty path meaning `index.html`.
fn clean_path(uri: &Uri) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in uri.path().split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return "index.html".to_string();
    }
    parts.join("/")
}

fn serve(assets: &dyn Assets, method: &Method, uri: &Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response();
    }
    let path = clean_path(uri);

    if let Some(body) = assets.get(&path) {
        // A hashed name under assets/ never changes content, so it is
        // immutable for a year; everything else is revalidated on every
        // load so a new deploy takes effect at once.
        let cache = if path.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, content_type(&path)),
                (header::CACHE_CONTROL, HeaderValue::from_static(cache)),
            ],
            body,
        )
            .into_response();
    }

    // A path with an extension names a missing file: a real 404. A path
    // without one is a client-side route: serve the app shell.
    if path.rsplit('/').next().is_some_and(|f| f.contains('.')) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    match assets.get("index.html") {
        Some(body) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, HeaderValue::from_static("text/html")),
                (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            ],
            body,
        )
            .into_response(),
        None => placeholder(),
    }
}

/// Answers when no dashboard build is embedded. It returns 503: the API
/// still works, the UI is what is unavailable. This is not the proxy's
/// instance error page of SPEC 9.3 and 14.5 — that page belongs to the
/// HTTP proxy, never to this crate.
fn placeholder() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        concat!(
            "<!doctype html><title>Bento</title>",
            "<p>The dashboard assets are not embedded in this build. ",
            "Run <code>npm install &amp;&amp; npm run build</code> in <code>web/</code> ",
            "and rebuild.</p>"
        ),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use std::collections::HashMap;
    use tower::ServiceExt;

    struct Fake(HashMap<String, Vec<u8>>);

    impl Fake {
        fn new(files: &[(&str, &str)]) -> Self {
            Fake(
                files
                    .iter()
                    .map(|(p, b)| (p.to_string(), b.as_bytes().to_vec()))
                    .collect(),
            )
        }
    }

    impl Assets for Fake {
        fn get(&self, path: &str) -> Option<Vec<u8>> {
            self.0.get(path).cloned()
        }
    }

    fn app() -> Router {
        router_from(Fake::new(&[
            ("index.html", "<!doctype html>shell"),
            ("assets/app-abc123.js", "console.log(1)"),
            ("favicon.ico", "icon"),
        ]))
    }

    async fn get(app: Router, path: &str) -> (StatusCode, String, String) {
        let res = app
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let cache = res
            .headers()
            .get(header::CACHE_CONTROL)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, cache, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn root_serves_the_shell() {
        let (status, cache, body) = get(app(), "/").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cache, "no-cache");
        assert!(body.contains("shell"));
    }

    #[tokio::test]
    async fn hashed_assets_are_immutable() {
        let (status, cache, body) = get(app(), "/assets/app-abc123.js").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cache, "public, max-age=31536000, immutable");
        assert_eq!(body, "console.log(1)");
    }

    /// A client-side route has no extension, so it gets the app shell
    /// rather than a 404 — a reload on /instances must not break.
    #[tokio::test]
    async fn client_side_routes_fall_back_to_the_shell() {
        let (status, _, body) = get(app(), "/instances/web").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("shell"));
    }

    /// A path with an extension names a file, so a missing one is a real
    /// 404 rather than an HTML page a script tag would choke on.
    #[tokio::test]
    async fn missing_files_are_not_found() {
        let (status, _, _) = get(app(), "/assets/gone.js").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn traversal_cannot_escape_the_asset_tree() {
        for path in ["/../../etc/passwd", "/assets/../../secret.txt"] {
            let (status, _, body) = get(app(), path).await;
            assert!(
                status == StatusCode::NOT_FOUND || body.contains("shell"),
                "{path} leaked: {status}"
            );
        }
    }

    #[tokio::test]
    async fn writes_are_rejected() {
        let res = app()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    /// With no build embedded the API still works; the UI is what is
    /// unavailable, so this is a 503 and not the proxy's 9.3 page.
    #[tokio::test]
    async fn no_build_serves_the_placeholder() {
        let (status, _, body) = get(router_from(Fake::new(&[])), "/").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("not embedded"));
    }

    /// The real embedded build must carry an index.html, or every
    /// deployment would silently serve the placeholder.
    #[tokio::test]
    async fn the_embedded_build_is_present() {
        assert!(
            Embedded.get("index.html").is_some(),
            "web/dist/index.html is not embedded"
        );
    }
}
