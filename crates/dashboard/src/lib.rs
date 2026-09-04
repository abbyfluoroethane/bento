//! Serves the dashboard's static assets (SPEC 14).
//!
//! The pages themselves are rendered by `bento_api::pages`; this crate
//! carries what they link to: the Basecoat stylesheet and scripts, HTMX,
//! uPlot, the self-hosted IBM Plex fonts (SPEC 14.3), and the branding.
//! Everything is embedded at compile time, so the deployed artifact stays
//! one binary with no Node runtime and no build step (SPEC 14.1).
//!
//! The control plane mounts this router beside the pages and `/api/`. The
//! sign-in gate skips paths that look like files, so these are served to
//! anyone; none of them carries session meaning.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// The asset tree, checked in under `crates/dashboard/assets`.
#[derive(RustEmbed)]
#[folder = "assets"]
struct Dist;

/// An asset source. The embedded tree satisfies it; tests substitute an
/// in-memory map so the routing rules are exercised on their own.
pub trait Assets: Send + Sync + 'static {
    /// The bytes of `path`, or `None` when no such file exists.
    fn get(&self, path: &str) -> Option<Vec<u8>>;
}

/// The asset tree embedded in this binary.
#[derive(Clone, Copy, Default)]
pub struct Embedded;

impl Assets for Embedded {
    fn get(&self, path: &str) -> Option<Vec<u8>> {
        Dist::get(path).map(|f| f.data.into_owned())
    }
}

/// The URL prefix every asset lives under.
pub const PREFIX: &str = "/assets/";

/// Serves the embedded assets under [`PREFIX`].
pub fn router() -> Router {
    router_from(Embedded)
}

/// Serves an asset tree from any [`Assets`]. Assets are not content
/// hashed, so each answer carries an entity tag derived from its bytes
/// and is revalidated on every load: a new deploy takes effect at once,
/// and an unchanged file costs one small round trip.
pub fn router_from(assets: impl Assets) -> Router {
    let assets = std::sync::Arc::new(assets);
    let for_icon = assets.clone();
    Router::new()
        // Browsers and tools ask for this regardless of the link tags.
        .route(
            "/favicon.ico",
            axum::routing::get(move |req: Request<Body>| {
                let assets = for_icon.clone();
                async move {
                    let uri: Uri = "/assets/branding/favicon.png".parse().expect("static uri");
                    serve(assets.as_ref(), req.method(), &uri, req.headers())
                }
            }),
        )
        .fallback(move |req: Request<Body>| {
            let assets = assets.clone();
            async move { serve(assets.as_ref(), req.method(), req.uri(), req.headers()) }
        })
}

/// The content type for a path, from its extension.
fn content_type(path: &str) -> HeaderValue {
    let guess = mime_guess::from_path(path).first_or_octet_stream();
    HeaderValue::from_str(guess.as_ref())
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}

/// Normalizes a request path to an asset path: the prefix and leading
/// slash removed, `.` and `..` segments resolved away so nothing can
/// escape the asset tree.
fn clean_path(uri: &Uri) -> Option<String> {
    let rest = uri.path().strip_prefix(PREFIX)?;
    let mut parts: Vec<&str> = Vec::new();
    for segment in rest.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// A weak entity tag over the bytes: FNV-1a, which is plenty for
/// distinguishing deploys and needs no dependency.
fn etag(bytes: &[u8]) -> HeaderValue {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    HeaderValue::from_str(&format!("W/\"{hash:016x}\"")).expect("hex is a valid header")
}

fn serve(assets: &dyn Assets, method: &Method, uri: &Uri, headers: &header::HeaderMap) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response();
    }
    let Some(path) = clean_path(uri) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Some(body) = assets.get(&path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let tag = etag(&body);
    if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|value| value == tag)
    {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, tag),
                (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            ],
        )
            .into_response();
    }
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type(&path)),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            (header::ETAG, tag),
        ],
        body,
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
            ("css/app.css", "body{}"),
            ("js/app.js", "console.log(1)"),
            ("branding/favicon.svg", "<svg/>"),
            ("branding/favicon.png", "<png>"),
        ]))
    }

    async fn get(
        app: Router,
        path: &str,
        if_none_match: Option<&str>,
    ) -> (StatusCode, HeaderMap, String) {
        let mut builder = Request::builder().uri(path);
        if let Some(tag) = if_none_match {
            builder = builder.header(header::IF_NONE_MATCH, tag);
        }
        let res = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    use axum::http::HeaderMap;

    #[tokio::test]
    async fn serves_files_with_type_and_etag() {
        let (status, headers, body) = get(app(), "/assets/css/app.css", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "body{}");
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/css")
        );
        assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
        assert!(headers[header::ETAG].to_str().unwrap().starts_with("W/\""));
    }

    #[tokio::test]
    async fn revalidation_answers_304() {
        let (_, headers, _) = get(app(), "/assets/js/app.js", None).await;
        let tag = headers[header::ETAG].to_str().unwrap().to_string();
        let (status, _, body) = get(app(), "/assets/js/app.js", Some(&tag)).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert_eq!(body, "");
        let (status, _, _) = get(app(), "/assets/js/app.js", Some("W/\"stale\"")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_and_escaping_paths_are_404() {
        assert_eq!(
            get(app(), "/assets/js/missing.js", None).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(get(app(), "/assets/", None).await.0, StatusCode::NOT_FOUND);
        assert_eq!(
            get(app(), "/assets/../etc/passwd", None).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(get(app(), "/other", None).await.0, StatusCode::NOT_FOUND);
        // `..` cannot climb above the tree: this resolves to css/app.css.
        assert_eq!(
            get(app(), "/assets/js/../css/app.css", None).await.0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn favicon_ico_is_the_png() {
        let (status, headers, body) = get(app(), "/favicon.ico", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "<png>");
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("image/png")
        );
    }

    #[tokio::test]
    async fn only_get_and_head() {
        let res = app()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/assets/css/app.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn embedded_tree_carries_the_essentials() {
        for path in [
            "css/basecoat-lyra.min.css",
            "css/uplot.min.css",
            "css/app.css",
            "js/basecoat.min.js",
            "js/htmx.min.js",
            "js/uplot.min.js",
            "js/app.js",
            "fonts/ibm-plex-sans-latin-400-normal.woff2",
            "fonts/ibm-plex-mono-latin-400-normal.woff2",
            "branding/favicon.svg",
        ] {
            assert!(Embedded.get(path).is_some(), "missing asset {path}");
        }
    }
}
