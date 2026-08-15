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
fn request_host_normalization_and_sni_precedence() {
    let cases = [
        (
            "plain",
            format!("box.{TEST_BASE}"),
            None,
            format!("box.{TEST_BASE}"),
        ),
        (
            "port",
            format!("box.{TEST_BASE}:3456"),
            None,
            format!("box.{TEST_BASE}"),
        ),
        (
            "uppercase",
            format!("BOX.{}", TEST_BASE.to_ascii_uppercase()),
            None,
            format!("box.{TEST_BASE}"),
        ),
        (
            "trailing dot",
            format!("box.{TEST_BASE}."),
            None,
            format!("box.{TEST_BASE}"),
        ),
        (
            "sni",
            "other.example.net".to_owned(),
            Some(format!("box.{TEST_BASE}")),
            format!("box.{TEST_BASE}"),
        ),
        ("empty", String::new(), None, String::new()),
    ];
    for (name, host, sni, expected) in cases {
        let request = Request::builder()
            .uri("http://placeholder/")
            .header(header::HOST, host)
            .body(full_body(Bytes::new()))
            .unwrap();
        assert_eq!(request_host(&request, sni.as_deref()), expected, "{name}");
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
        None,
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
