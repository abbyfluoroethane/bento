use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bento_types::{DesiredState, Instance, State, Visibility};
use bytes::Bytes;
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::BodyExt;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use time::OffsetDateTime;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::listen::listen_all;

const TEST_BASE: &str = "bento.example.org";
const REMOTE: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 42424);

struct FakeSource {
    instances: HashMap<String, Instance>,
    error: bool,
}

impl FakeSource {
    fn empty() -> Arc<Self> {
        Arc::new(Self {
            instances: HashMap::new(),
            error: false,
        })
    }

    fn with(instance: Instance) -> Arc<Self> {
        Arc::new(Self {
            instances: HashMap::from([(instance.name.clone(), instance)]),
            error: false,
        })
    }
}

#[async_trait]
impl InstanceSource for FakeSource {
    async fn instance_by_name(&self, name: &str) -> Result<Option<Instance>, BoxError> {
        if self.error {
            return Err(io::Error::other("db locked").into());
        }
        Ok(self.instances.get(name).cloned())
    }
}

struct FakeSessions {
    access: Access,
    checked: Mutex<Vec<String>>,
}

impl FakeSessions {
    fn new(access: Access) -> Arc<Self> {
        Arc::new(Self {
            access,
            checked: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl SessionChecker for FakeSessions {
    async fn access(&self, _: &Request<ProxyBody>, uuid: &str) -> Access {
        self.checked.lock().unwrap().push(uuid.to_owned());
        self.access
    }
}

#[derive(Debug, Clone)]
struct SeenRequest {
    uri: Uri,
    headers: http::HeaderMap,
}

struct FakeTransport {
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    error: bool,
    panic_if_called: bool,
}

#[async_trait]
impl Transport for FakeTransport {
    async fn send(&self, request: Request<ProxyBody>) -> Result<Response<ProxyBody>, BoxError> {
        assert!(!self.panic_if_called, "transport must not be called");
        if self.error {
            return Err(
                io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused").into(),
            );
        }
        self.seen.lock().unwrap().push(SeenRequest {
            uri: request.uri().clone(),
            headers: request.headers().clone(),
        });
        Ok(Response::new(full_body("backend")))
    }
}

fn ok_transport() -> (Arc<dyn Transport>, Arc<Mutex<Vec<SeenRequest>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    (
        Arc::new(FakeTransport {
            seen: seen.clone(),
            error: false,
            panic_if_called: false,
        }),
        seen,
    )
}

fn transport_error() -> Arc<dyn Transport> {
    Arc::new(FakeTransport {
        seen: Arc::new(Mutex::new(Vec::new())),
        error: true,
        panic_if_called: false,
    })
}

fn panic_transport() -> Arc<dyn Transport> {
    Arc::new(FakeTransport {
        seen: Arc::new(Mutex::new(Vec::new())),
        error: false,
        panic_if_called: true,
    })
}

fn builder(source: Arc<dyn InstanceSource>) -> ProxyBuilder {
    Proxy::builder(TEST_BASE, source)
}

fn request(host: &str) -> Request<ProxyBody> {
    Request::builder()
        .method(Method::GET)
        .uri(format!("https://{host}/"))
        .header(header::HOST, host)
        .body(full_body(Bytes::new()))
        .unwrap()
}

fn running_instance(name: &str, visibility: Visibility) -> Instance {
    Instance {
        uuid: format!("uuid-{name}"),
        name: name.to_owned(),
        owner_id: 42,
        host_id: 1,
        image_name: "debian".to_owned(),
        base_checksum: "checksum".to_owned(),
        state: State::Running,
        desired_state: DesiredState::Running,
        address: "10.42.1.2".to_owned(),
        mac: "02:00:00:00:00:01".to_owned(),
        vcpu: 2,
        memory_mib: 2048,
        disk_gib: 20,
        nested: false,
        ksm: false,
        http_port: 0,
        visibility,
        created_at: OffsetDateTime::UNIX_EPOCH,
        last_seen_at: None,
        slot: None,
    }
}

async fn response_parts(response: Response<ProxyBody>) -> (StatusCode, http::HeaderMap, Bytes) {
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body)
}

#[tokio::test]
async fn base_domain_routing() {
    let control = control_handler(|_| async { Response::new(full_body("control")) });
    let proxy = builder(FakeSource::empty())
        .with_control(control)
        .build()
        .unwrap();

    let (status, _, body) =
        response_parts(proxy.handle(request(TEST_BASE), DEFAULT_PORT, REMOTE).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "control");

    let (status, _, _) = response_parts(proxy.handle(request(TEST_BASE), 3456, REMOTE).await).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The operator can publish instances under a parent of the base domain
/// (SPEC 7.1). The control plane keeps the base domain, instances answer one
/// label under the instance domain, and the apex belongs to neither.
#[tokio::test]
async fn split_control_and_instance_domains() {
    let mut instances = HashMap::new();
    instances.insert(
        "web".to_owned(),
        running_instance("web", Visibility::Public),
    );
    let source = Arc::new(FakeSource {
        instances,
        error: false,
    });
    let control = control_handler(|_| async { Response::new(full_body("control")) });
    let (transport, _seen) = ok_transport();
    let proxy = Proxy::builder("bento.foid.space", source)
        .with_instance_domain("foid.space")
        .with_control(control)
        .with_transport(transport)
        .build()
        .unwrap();

    // The base domain reaches the control plane even though it is itself a
    // name under the instance domain.
    let (status, _, body) = response_parts(
        proxy
            .handle(request("bento.foid.space"), DEFAULT_PORT, REMOTE)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "control");

    // An instance answers one label under the instance domain.
    let (status, _, _) = response_parts(
        proxy
            .handle(request("web.foid.space"), DEFAULT_PORT, REMOTE)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The apex is not the base domain and strips to nothing, so Bento does
    // not answer it. The operator can leave it to another service.
    for host in ["foid.space", "web.bento.foid.space", "a.b.foid.space"] {
        let (status, _, _) =
            response_parts(proxy.handle(request(host), DEFAULT_PORT, REMOTE).await).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{host}");
    }
}

/// A name that never existed, a name in release cooldown, an instance with
/// visibility off, and an invalid nested label produce byte-identical
/// responses (SPEC 9.2).
#[tokio::test]
async fn not_found_responses_are_identical() {
    let mut instances = HashMap::new();
    instances.insert(
        "hidden".to_owned(),
        running_instance("hidden", Visibility::Off),
    );
    let source = Arc::new(FakeSource {
        instances,
        error: false,
    });
    let proxy = builder(source)
        .with_sessions(FakeSessions::new(Access::Granted))
        .build()
        .unwrap();

    let mut reference = None;
    for host in [
        format!("never-existed.{TEST_BASE}"),
        format!("cooling-down.{TEST_BASE}"),
        format!("hidden.{TEST_BASE}"),
        format!("a.b.{TEST_BASE}"),
    ] {
        let result = response_parts(proxy.handle(request(&host), DEFAULT_PORT, REMOTE).await).await;
        assert_eq!(result.0, StatusCode::NOT_FOUND, "{host}");
        if let Some((headers, body)) = &reference {
            assert_eq!(&result.1, headers, "headers differ for {host}");
            assert_eq!(&result.2, body, "body differs for {host}");
        } else {
            reference = Some((result.1, result.2));
        }
    }
    assert!(!String::from_utf8_lossy(&reference.unwrap().1).contains("hidden"));
}

#[tokio::test]
async fn visibility_matrix() {
    let cases = [
        (
            "off unauthenticated",
            Visibility::Off,
            DEFAULT_PORT,
            Access::Unauthenticated,
            StatusCode::NOT_FOUND,
        ),
        (
            "off authorized",
            Visibility::Off,
            DEFAULT_PORT,
            Access::Granted,
            StatusCode::NOT_FOUND,
        ),
        (
            "off high authorized",
            Visibility::Off,
            3456,
            Access::Granted,
            StatusCode::NOT_FOUND,
        ),
        (
            "private unauthenticated",
            Visibility::Private,
            DEFAULT_PORT,
            Access::Unauthenticated,
            StatusCode::FOUND,
        ),
        (
            "private authorized",
            Visibility::Private,
            DEFAULT_PORT,
            Access::Granted,
            StatusCode::OK,
        ),
        (
            "private forbidden",
            Visibility::Private,
            DEFAULT_PORT,
            Access::Forbidden,
            StatusCode::NOT_FOUND,
        ),
        (
            "public unauthenticated",
            Visibility::Public,
            DEFAULT_PORT,
            Access::Unauthenticated,
            StatusCode::OK,
        ),
        (
            "public authorized",
            Visibility::Public,
            DEFAULT_PORT,
            Access::Granted,
            StatusCode::OK,
        ),
        (
            "public high unauthenticated",
            Visibility::Public,
            3456,
            Access::Unauthenticated,
            StatusCode::FOUND,
        ),
        (
            "public high authorized",
            Visibility::Public,
            3456,
            Access::Granted,
            StatusCode::OK,
        ),
        (
            "public high forbidden",
            Visibility::Public,
            3456,
            Access::Forbidden,
            StatusCode::NOT_FOUND,
        ),
        (
            "private high unauthenticated",
            Visibility::Private,
            9999,
            Access::Unauthenticated,
            StatusCode::FOUND,
        ),
        (
            "private high authorized",
            Visibility::Private,
            9999,
            Access::Granted,
            StatusCode::OK,
        ),
    ];
    for (name, visibility, port, access, expected) in cases {
        let (transport, _) = ok_transport();
        let proxy = builder(FakeSource::with(running_instance("box", visibility)))
            .with_sessions(FakeSessions::new(access))
            .with_transport(transport)
            .build()
            .unwrap();
        let response = proxy
            .handle(request(&format!("box.{TEST_BASE}")), port, REMOTE)
            .await;
        assert_eq!(response.status(), expected, "{name}");
    }
}

/// Authorization keys on UUID on every private request. An authenticated
/// but unauthorized stale credential gets the identical missing-name 404.
#[tokio::test]
async fn authorization_runs_per_request_and_keys_on_uuid() {
    let source = FakeSource::with(running_instance("box", Visibility::Private));
    let sessions = FakeSessions::new(Access::Granted);
    let (transport, _) = ok_transport();
    let proxy = builder(source.clone())
        .with_sessions(sessions.clone())
        .with_transport(transport)
        .build()
        .unwrap();
    assert_eq!(
        proxy
            .handle(request(&format!("box.{TEST_BASE}")), DEFAULT_PORT, REMOTE)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(&*sessions.checked.lock().unwrap(), &["uuid-box"]);

    let (transport, _) = ok_transport();
    let forbidden_proxy = builder(source)
        .with_sessions(FakeSessions::new(Access::Forbidden))
        .with_transport(transport)
        .build()
        .unwrap();
    let forbidden = response_parts(
        forbidden_proxy
            .handle(request(&format!("box.{TEST_BASE}")), DEFAULT_PORT, REMOTE)
            .await,
    )
    .await;
    let missing = response_parts(
        forbidden_proxy
            .handle(request(&format!("ghost.{TEST_BASE}")), DEFAULT_PORT, REMOTE)
            .await,
    )
    .await;
    assert_eq!(forbidden.0, StatusCode::NOT_FOUND);
    assert_eq!(forbidden.1, missing.1);
    assert_eq!(forbidden.2, missing.2);
}

#[tokio::test]
async fn public_default_port_skips_authorization() {
    let sessions = FakeSessions::new(Access::Forbidden);
    let (transport, _) = ok_transport();
    let proxy = builder(FakeSource::with(running_instance(
        "pub",
        Visibility::Public,
    )))
    .with_sessions(sessions.clone())
    .with_transport(transport)
    .build()
    .unwrap();
    assert_eq!(
        proxy
            .handle(request(&format!("pub.{TEST_BASE}")), DEFAULT_PORT, REMOTE)
            .await
            .status(),
        StatusCode::OK
    );
    assert!(sessions.checked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn redirect_to_login_carries_the_original_url() {
    let proxy = builder(FakeSource::with(running_instance(
        "box",
        Visibility::Private,
    )))
    .with_sessions(FakeSessions::new(Access::Unauthenticated))
    .build()
    .unwrap();
    let mut request = request(&format!("box.{TEST_BASE}"));
    *request.uri_mut() = format!("https://box.{TEST_BASE}/admin?x=1")
        .parse()
        .unwrap();
    let response = proxy.handle(request, DEFAULT_PORT, REMOTE).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    let location = url::Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    assert_eq!(
        format!(
            "{}://{}{}",
            location.scheme(),
            location.host_str().unwrap(),
            location.path()
        ),
        format!("https://{TEST_BASE}/login")
    );
    assert_eq!(
        location
            .query_pairs()
            .find(|(key, _)| key == "next")
            .unwrap()
            .1,
        format!("https://box.{TEST_BASE}/admin?x=1")
    );
}

#[tokio::test]
async fn port_selection() {
    let cases = [
        ("default port 80", 0, DEFAULT_PORT, "10.42.1.2:80"),
        ("port command", 8080, DEFAULT_PORT, "10.42.1.2:8080"),
        ("high overrides", 8080, 3456, "10.42.1.2:3456"),
        ("high end", 0, 9999, "10.42.1.2:9999"),
    ];
    for (name, http_port, listener_port, target) in cases {
        let mut instance = running_instance("box", Visibility::Public);
        instance.http_port = http_port;
        let (transport, seen) = ok_transport();
        let proxy = builder(FakeSource::with(instance))
            .with_sessions(FakeSessions::new(Access::Granted))
            .with_transport(transport)
            .build()
            .unwrap();
        assert_eq!(
            proxy
                .handle(request(&format!("box.{TEST_BASE}")), listener_port, REMOTE)
                .await
                .status(),
            StatusCode::OK,
            "{name}"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].uri.scheme_str(), Some("http"));
        assert_eq!(seen[0].uri.authority().unwrap().as_str(), target);
    }
}

#[tokio::test]
async fn forwarded_headers_are_set() {
    let (transport, seen) = ok_transport();
    let proxy = builder(FakeSource::with(running_instance(
        "box",
        Visibility::Public,
    )))
    .with_transport(transport)
    .build()
    .unwrap();
    let remote: SocketAddr = "192.0.2.7:55555".parse().unwrap();
    let mut request = request(&format!("box.{TEST_BASE}"));
    request.headers_mut().insert(
        HeaderName::from_static("x-forwarded-for"),
        HeaderValue::from_static("203.0.113.99"),
    );
    proxy.handle(request, DEFAULT_PORT, remote).await;
    let seen = seen.lock().unwrap();
    let headers = &seen[0].headers;
    assert_eq!(headers["x-forwarded-for"], "192.0.2.7");
    assert_eq!(headers["x-forwarded-host"], format!("box.{TEST_BASE}"));
    assert_eq!(headers["x-forwarded-proto"], "https");
    assert_eq!(headers[header::HOST], format!("box.{TEST_BASE}"));
}

/// What the guest receives for these request headers.
async fn forwarded_with(headers: &[(&str, &str)]) -> http::HeaderMap {
    let (transport, seen) = ok_transport();
    let proxy = builder(FakeSource::with(running_instance(
        "box",
        Visibility::Public,
    )))
    .with_transport(transport)
    .build()
    .unwrap();
    let mut request = request(&format!("box.{TEST_BASE}"));
    for (name, value) in headers {
        request.headers_mut().append(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    proxy.handle(request, DEFAULT_PORT, REMOTE).await;
    let seen = seen.lock().unwrap();
    seen[0].headers.clone()
}

#[tokio::test]
async fn the_guest_gets_its_own_cookies_and_not_the_bento_ones() {
    let headers = forwarded_with(&[("cookie", "a=1; bento_session=x; b=2")]).await;
    assert_eq!(headers[header::COOKIE], "a=1; b=2");
    // Spacing, several headers, and the OIDC flow cookies.
    let headers = forwarded_with(&[
        ("cookie", "a=1;bento_oauth_state=s ;  b=2"),
        ("cookie", "bento_login_next=/x; c=3"),
    ])
    .await;
    let all: Vec<_> = headers.get_all(header::COOKIE).iter().collect();
    assert_eq!(all, ["a=1; b=2; c=3"]);
}

#[tokio::test]
async fn a_lone_bento_cookie_removes_the_header() {
    let headers = forwarded_with(&[("cookie", "bento_session=x")]).await;
    assert!(!headers.contains_key(header::COOKIE));
    let headers = forwarded_with(&[("cookie", "a=1")]).await;
    assert_eq!(headers[header::COOKIE], "a=1");
}

#[tokio::test]
async fn a_bento_token_is_not_forwarded_but_a_guest_authorization_is() {
    let headers = forwarded_with(&[("authorization", "Bearer bento_abc123")]).await;
    assert!(!headers.contains_key(header::AUTHORIZATION));
    let headers = forwarded_with(&[("authorization", "bearer  bento_abc123")]).await;
    assert!(!headers.contains_key(header::AUTHORIZATION));
    for own in ["Basic dXNlcjpwYXNz", "Bearer guest-token"] {
        let headers = forwarded_with(&[("authorization", own)]).await;
        assert_eq!(headers[header::AUTHORIZATION], own);
    }
}

struct HeaderSpy(Mutex<Vec<(Option<String>, Option<String>)>>);

#[async_trait]
impl SessionChecker for HeaderSpy {
    async fn access(&self, request: &Request<ProxyBody>, _: &str) -> Access {
        let get = |name| {
            request
                .headers()
                .get(name)
                .map(|value| value.to_str().unwrap().to_owned())
        };
        self.0
            .lock()
            .unwrap()
            .push((get(header::COOKIE), get(header::AUTHORIZATION)));
        Access::Granted
    }
}

#[tokio::test]
async fn the_session_check_sees_the_original_headers() {
    let (transport, seen) = ok_transport();
    let spy = Arc::new(HeaderSpy(Mutex::new(Vec::new())));
    let proxy = builder(FakeSource::with(running_instance(
        "box",
        Visibility::Private,
    )))
    .with_transport(transport)
    .with_sessions(spy.clone())
    .build()
    .unwrap();
    let mut request = request(&format!("box.{TEST_BASE}"));
    request.headers_mut().insert(
        header::COOKIE,
        HeaderValue::from_static("a=1; bento_session=x"),
    );
    request.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer bento_t"),
    );
    proxy.handle(request, DEFAULT_PORT, REMOTE).await;
    let checked = spy.0.lock().unwrap();
    assert_eq!(
        checked[0],
        (
            Some("a=1; bento_session=x".to_owned()),
            Some("Bearer bento_t".to_owned())
        )
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].headers[header::COOKIE], "a=1");
    assert!(!seen[0].headers.contains_key(header::AUTHORIZATION));
}

/// A real ephemeral loopback backend proves the complete streaming path,
/// rather than only the injected transport seam.
#[tokio::test]
async fn forward_end_to_end() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let backend = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        http1::Builder::new()
            .serve_connection(
                TokioIo::new(stream),
                service_fn(|request: Request<hyper::body::Incoming>| async move {
                    let host = request.headers()[header::HOST].to_str().unwrap().to_owned();
                    Ok::<_, Infallible>(Response::new(full_body(format!("hello from {host}"))))
                }),
            )
            .await
            .unwrap();
    });
    let mut instance = running_instance("box", Visibility::Public);
    instance.address = address.ip().to_string();
    instance.http_port = address.port();
    let proxy = builder(FakeSource::with(instance)).build().unwrap();
    let (status, _, body) = response_parts(
        proxy
            .handle(request(&format!("box.{TEST_BASE}")), DEFAULT_PORT, REMOTE)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, format!("hello from box.{TEST_BASE}"));
    backend.abort();
    let _ = backend.await;
}

#[tokio::test]
async fn stopped_instance_is_unavailable_without_dialing() {
    let mut instance = running_instance("dbbox", Visibility::Public);
    instance.state = State::Stopped;
    let proxy = builder(FakeSource::with(instance))
        .with_transport(panic_transport())
        .build()
        .unwrap();
    let (status, headers, body) = response_parts(
        proxy
            .handle(request(&format!("dbbox.{TEST_BASE}")), DEFAULT_PORT, REMOTE)
            .await,
    )
    .await;
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers[header::RETRY_AFTER], "5");
    assert!(body.contains("dbbox"));
    assert!(body.contains("stopped"));
    assert!(!body.contains("42"));
    assert!(!body.contains("<script"));
}

#[tokio::test]
async fn starting_instance_is_unavailable_immediately() {
    let mut instance = running_instance("slowbox", Visibility::Public);
    instance.state = State::Starting;
    let proxy = builder(FakeSource::with(instance))
        .with_transport(panic_transport())
        .build()
        .unwrap();
    let start = Instant::now();
    let (status, _, body) = response_parts(
        proxy
            .handle(
                request(&format!("slowbox.{TEST_BASE}")),
                DEFAULT_PORT,
                REMOTE,
            )
            .await,
    )
    .await;
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(String::from_utf8_lossy(&body).contains("starting"));
}

#[tokio::test]
async fn refused_connection_is_unavailable() {
    let proxy = builder(FakeSource::with(running_instance(
        "box",
        Visibility::Public,
    )))
    .with_transport(transport_error())
    .build()
    .unwrap();
    let (status, headers, body) = response_parts(
        proxy
            .handle(request(&format!("box.{TEST_BASE}")), DEFAULT_PORT, REMOTE)
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers[header::RETRY_AFTER], "5");
    assert!(String::from_utf8_lossy(&body).contains("box"));
}

#[tokio::test]
async fn unavailable_page_escapes_instance_name() {
    let mut instance = running_instance("evil", Visibility::Public);
    instance.name = "<script>alert(1)</script>".to_owned();
    instance.state = State::Stopped;
    let mut instances = HashMap::new();
    instances.insert("evil".to_owned(), instance);
    let proxy = builder(Arc::new(FakeSource {
        instances,
        error: false,
    }))
    .build()
    .unwrap();
    let (_, _, body) = response_parts(
        proxy
            .handle(request(&format!("evil.{TEST_BASE}")), DEFAULT_PORT, REMOTE)
            .await,
    )
    .await;
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(!body.contains("<script>alert(1)</script>"));
    assert!(body.contains("&lt;script&gt;"));
}

struct FakeLastSeen(Mutex<Vec<String>>);

#[async_trait]
impl LastSeenRecorder for FakeLastSeen {
    async fn touch_last_seen(&self, uuid: &str) -> Result<(), BoxError> {
        self.0.lock().unwrap().push(uuid.to_owned());
        Ok(())
    }
}

/// A forwarded request touches last_seen_at; a 503 that never reaches the
/// instance does not (SPEC 12).
#[tokio::test]
async fn last_seen_is_touched_only_on_forward() {
    let running = running_instance("box", Visibility::Public);
    let mut stopped = running_instance("idle", Visibility::Public);
    stopped.state = State::Stopped;
    let source = Arc::new(FakeSource {
        instances: HashMap::from([("box".to_owned(), running), ("idle".to_owned(), stopped)]),
        error: false,
    });
    let recorder = Arc::new(FakeLastSeen(Mutex::new(Vec::new())));
    let (transport, _) = ok_transport();
    let proxy = builder(source)
        .with_sessions(FakeSessions::new(Access::Granted))
        .with_transport(transport)
        .with_last_seen(recorder.clone())
        .build()
        .unwrap();
    assert_eq!(
        proxy
            .handle(request(&format!("box.{TEST_BASE}")), DEFAULT_PORT, REMOTE)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(&*recorder.0.lock().unwrap(), &["uuid-box"]);
    assert_eq!(
        proxy
            .handle(request(&format!("idle.{TEST_BASE}")), DEFAULT_PORT, REMOTE)
            .await
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(&*recorder.0.lock().unwrap(), &["uuid-box"]);
}

#[tokio::test]
async fn source_error_is_internal_server_error() {
    let source = Arc::new(FakeSource {
        instances: HashMap::new(),
        error: true,
    });
    let proxy = builder(source).build().unwrap();
    assert_eq!(
        proxy
            .handle(request(&format!("box.{TEST_BASE}")), DEFAULT_PORT, REMOTE)
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[test]
fn request_host_normalization() {
    let cases = [
        (
            "plain",
            format!("box.{TEST_BASE}"),
            format!("box.{TEST_BASE}"),
        ),
        (
            "port",
            format!("box.{TEST_BASE}:3456"),
            format!("box.{TEST_BASE}"),
        ),
        (
            "uppercase",
            format!("BOX.{}", TEST_BASE.to_ascii_uppercase()),
            format!("box.{TEST_BASE}"),
        ),
        (
            "trailing dot",
            format!("box.{TEST_BASE}."),
            format!("box.{TEST_BASE}"),
        ),
        ("empty", String::new(), String::new()),
    ];
    for (name, host, expected) in cases {
        let request = Request::builder()
            .uri("http://placeholder/")
            .header(header::HOST, host)
            .body(full_body(Bytes::new()))
            .unwrap();
        assert_eq!(request_host(&request), expected, "{name}");
    }
}

/// Routing reads the listener port supplied by the accept loop, never the
/// Host header's port. When the main listener moves, the control-plane and
/// default-port rules move with it.
#[tokio::test]
async fn routing_uses_the_listener_port_and_follows_a_moved_main_port() {
    let control = control_handler(|_| async { Response::new(full_body("control")) });
    let proxy = builder(FakeSource::empty())
        .with_control(control)
        .with_ports(10443, 3000, 9999)
        .build()
        .unwrap();
    let request_with_misleading_header_port = request(&format!("{TEST_BASE}:3456"));
    let response = proxy
        .handle(request_with_misleading_header_port, 10443, REMOTE)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = proxy.handle(request(TEST_BASE), DEFAULT_PORT, REMOTE).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[test]
fn builder_validation_and_normalization() {
    assert!(matches!(
        Proxy::builder("", FakeSource::empty()).build(),
        Err(Error::EmptyBaseDomain)
    ));
    let proxy = Proxy::builder("Bento.Example.Org.", FakeSource::empty())
        .build()
        .unwrap();
    assert_eq!(proxy.base_domain, TEST_BASE);
}

#[test]
fn ports_cover_the_spec_range() {
    let proxy = builder(FakeSource::empty()).build().unwrap();
    let ports = proxy.ports();
    assert_eq!(ports.len(), 7001);
    assert_eq!(ports[0], DEFAULT_PORT);
    assert_eq!(ports[1], HIGH_PORT_MIN);
    assert_eq!(ports.last(), Some(&HIGH_PORT_MAX));
}

#[test]
fn with_ports_moves_and_validates_the_listener_layout() {
    let proxy = builder(FakeSource::empty())
        .with_ports(10443, 3000, 3002)
        .build()
        .unwrap();
    assert_eq!(proxy.ports(), [10443, 3000, 3001, 3002]);
    let proxy = builder(FakeSource::empty())
        .with_ports(0, 3000, 3001)
        .build()
        .unwrap();
    assert_eq!(proxy.ports()[0], DEFAULT_PORT);
    assert!(matches!(
        builder(FakeSource::empty())
            .with_ports(3500, 3000, 9999)
            .build(),
        Err(Error::MainPortInHighRange { .. })
    ));
    assert!(matches!(
        builder(FakeSource::empty())
            .with_ports(0, 9999, 3000)
            .build(),
        Err(Error::EmptyHighPortRange { .. })
    ));
}

struct FakeListener {
    closed: Arc<AtomicBool>,
}

impl Drop for FakeListener {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl Listener for FakeListener {
    async fn accept(&self) -> io::Result<(BoxIo, SocketAddr)> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn listen_all_binds_every_port() {
    let addresses = Arc::new(Mutex::new(Vec::new()));
    let recorded = addresses.clone();
    let listen = listen_fn(move |network, address| {
        let recorded = recorded.clone();
        async move {
            assert_eq!(network, "tcp");
            recorded.lock().unwrap().push(address);
            Ok(FakeListener {
                closed: Arc::new(AtomicBool::new(false)),
            })
        }
    });
    let proxy = builder(FakeSource::empty()).build().unwrap();
    let listeners = listen_all("0.0.0.0", &proxy.ports(), listen).await.unwrap();
    assert_eq!(listeners.len(), 7001);
    let addresses = addresses.lock().unwrap();
    assert_eq!(addresses[0], "0.0.0.0:443");
    assert_eq!(addresses.last().unwrap(), "0.0.0.0:9999");
}

#[tokio::test]
async fn listen_all_failure_closes_every_bound_listener() {
    let opened = Arc::new(Mutex::new(Vec::<Arc<AtomicBool>>::new()));
    let states = opened.clone();
    let listen = listen_fn(move |_, address| {
        let states = states.clone();
        async move {
            if address.ends_with(":3001") {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "address in use"));
            }
            let closed = Arc::new(AtomicBool::new(false));
            states.lock().unwrap().push(closed.clone());
            Ok(FakeListener { closed })
        }
    });
    let error = match listen_all("127.0.0.1", &[443, 3000, 3001, 3002], listen).await {
        Ok(_) => panic!("listen_all succeeded, want error"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("3001"));
    let opened = opened.lock().unwrap();
    assert_eq!(opened.len(), 2);
    assert!(opened.iter().all(|state| state.load(Ordering::SeqCst)));
}

#[tokio::test]
async fn serve_stops_when_shutdown_is_cancelled() {
    let proxy = Arc::new(
        builder(FakeSource::empty())
            .with_ports(10443, 13000, 13002)
            .build()
            .unwrap(),
    );
    let listen = listen_fn(|_, _| async {
        Ok(FakeListener {
            closed: Arc::new(AtomicBool::new(false)),
        })
    });
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(proxy.serve(
        "127.0.0.1",
        Some(listen),
        shutdown.clone().cancelled_owned(),
    ));
    tokio::task::yield_now().await;
    shutdown.cancel();
    let error = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("serve did not return after shutdown")
        .unwrap()
        .expect_err("serve returned success after shutdown");
    assert!(matches!(error, Error::Shutdown));
}

/// Lets a test change an instance while a tunnel is open.
struct SwitchSource(Mutex<Instance>);

#[async_trait]
impl InstanceSource for SwitchSource {
    async fn instance_by_name(&self, name: &str) -> Result<Option<Instance>, BoxError> {
        let instance = self.0.lock().unwrap().clone();
        Ok((instance.name == name).then_some(instance))
    }
}

struct SwitchSessions(Mutex<Access>);

#[async_trait]
impl SessionChecker for SwitchSessions {
    async fn access(&self, _: &Request<ProxyBody>, _: &str) -> Access {
        *self.0.lock().unwrap()
    }
}

/// An upstream that agrees to an `echo` upgrade and then echoes bytes.
async fn echo_upstream() -> SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let mut byte = [0_u8; 1];
                    if stream.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    head.push(byte[0]);
                }
                let reply = "HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\n\
                             Upgrade: echo\r\n\r\n";
                if stream.write_all(reply.as_bytes()).await.is_err() {
                    return;
                }
                let (mut read, mut write) = stream.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    address
}

/// A listener that hands each connection to `proxy`, with upgrades enabled.
async fn serve_front(proxy: Arc<Proxy>) -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let proxy = proxy.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                    let proxy = proxy.clone();
                    async move {
                        let request = request.map(|body| {
                            body.map_err(|error| -> BoxError { Box::new(error) })
                                .boxed()
                        });
                        Ok::<_, Infallible>(
                            proxy
                                .handle_connection(request, DEFAULT_PORT, REMOTE, false)
                                .await,
                        )
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    address
}

/// Opens an `echo` upgrade through the proxy and proves it carries bytes.
async fn open_tunnel(front: SocketAddr, name: &str) -> tokio::net::TcpStream {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut client = tokio::net::TcpStream::connect(front).await.unwrap();
    let head = format!(
        "GET /socket HTTP/1.1\r\nHost: {name}.{TEST_BASE}\r\n\
         Connection: upgrade\r\nUpgrade: echo\r\n\r\n"
    );
    client.write_all(head.as_bytes()).await.unwrap();
    let mut reply = Vec::new();
    while !reply.ends_with(b"\r\n\r\n") {
        let mut byte = [0_u8; 1];
        client.read_exact(&mut byte).await.unwrap();
        reply.push(byte[0]);
    }
    let reply = String::from_utf8(reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 101"), "{reply}");
    echo_once(&mut client).await;
    client
}

async fn echo_once(client: &mut tokio::net::TcpStream) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    client.write_all(b"ping").await.unwrap();
    let mut echoed = [0_u8; 4];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut echoed))
        .await
        .expect("the tunnel carries bytes")
        .unwrap();
    assert_eq!(&echoed, b"ping");
}

async fn assert_tunnel_closes(client: &mut tokio::net::TcpStream) {
    use tokio::io::AsyncReadExt;
    let mut byte = [0_u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), client.read(&mut byte))
        .await
        .expect("the proxy must end the tunnel");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
}

fn instance_at(upstream: SocketAddr, visibility: Visibility) -> Instance {
    let mut instance = running_instance("box", visibility);
    instance.address = upstream.ip().to_string();
    instance.http_port = upstream.port();
    instance
}

/// The upgrade request is authorized once, so the tunnel must be checked
/// again: an owner who turns visibility off cuts the open connection
/// (SPEC 9.2).
#[tokio::test]
async fn tunnel_ends_when_visibility_turns_off() {
    let upstream = echo_upstream().await;
    let source = Arc::new(SwitchSource(Mutex::new(instance_at(
        upstream,
        Visibility::Public,
    ))));
    let proxy = builder(source.clone())
        .with_tunnel_revalidation(Duration::from_millis(50))
        .build()
        .unwrap();
    let front = serve_front(Arc::new(proxy)).await;

    let mut client = open_tunnel(front, "box").await;
    // Still allowed after several checks.
    tokio::time::sleep(Duration::from_millis(200)).await;
    echo_once(&mut client).await;

    source.0.lock().unwrap().visibility = Visibility::Off;
    assert_tunnel_closes(&mut client).await;
}

/// A revoked session or a removed share cuts a private tunnel (SPEC 13).
#[tokio::test]
async fn tunnel_ends_when_access_is_revoked() {
    let upstream = echo_upstream().await;
    let source = Arc::new(SwitchSource(Mutex::new(instance_at(
        upstream,
        Visibility::Private,
    ))));
    let sessions = Arc::new(SwitchSessions(Mutex::new(Access::Granted)));
    let proxy = builder(source)
        .with_sessions(sessions.clone())
        .with_tunnel_revalidation(Duration::from_millis(50))
        .build()
        .unwrap();
    let front = serve_front(Arc::new(proxy)).await;

    let mut client = open_tunnel(front, "box").await;
    *sessions.0.lock().unwrap() = Access::Forbidden;
    assert_tunnel_closes(&mut client).await;
}

/// The UUID is the key: a name that now belongs to another instance does
/// not keep the old tunnel.
#[tokio::test]
async fn tunnel_ends_when_the_name_changes_hands() {
    let upstream = echo_upstream().await;
    let source = Arc::new(SwitchSource(Mutex::new(instance_at(
        upstream,
        Visibility::Public,
    ))));
    let proxy = builder(source.clone())
        .with_tunnel_revalidation(Duration::from_millis(50))
        .build()
        .unwrap();
    let front = serve_front(Arc::new(proxy)).await;

    let mut client = open_tunnel(front, "box").await;
    source.0.lock().unwrap().uuid = "uuid-other".to_owned();
    assert_tunnel_closes(&mut client).await;
}

/// Shutdown ends a tunnel that no longer belongs to any connection task.
#[tokio::test]
async fn tunnel_ends_on_shutdown() {
    let (inbound, _inbound_peer) = tokio::io::duplex(64);
    let (outbound, _outbound_peer) = tokio::io::duplex(64);
    let shutdown = CancellationToken::new();
    let guard = TunnelGuard {
        shutdown: Some(shutdown.clone()),
        ..TunnelGuard::default()
    };
    let tunnel = tokio::spawn(run_tunnel(inbound, outbound, guard));
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), tunnel)
        .await
        .expect("shutdown ends the tunnel")
        .unwrap();
}
