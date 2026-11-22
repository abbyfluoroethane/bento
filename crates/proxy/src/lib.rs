//! Hostname-based routing, visibility enforcement, and streaming HTTP
//! forwarding for instances (SPEC sections 9 and 14.5).
//!
//! The proxy resolves the instance name on every request (SPEC 7.1), reads
//! the instance address assigned at creation (SPEC 6.2), and forwards over
//! plain HTTP on the private network. The listeners speak plain HTTP too:
//! a TLS terminator in front of Bento owns the certificate (SPEC 8).

use std::convert::Infallible;
use std::error::Error as StdError;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bento_types::{Instance, State, Visibility};
use bytes::Bytes;
use http::header::{self, HeaderName, HeaderValue};
use http::{HeaderMap, Request, Response, StatusCode, Uri};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

mod listen;
mod pages;

use tokio_util::sync::CancellationToken;

pub use listen::{BoxIo, ListenFunc, Listener, listen_fn};

/// The main port. A request on this port goes to the instance's default
/// HTTP port, set with the `port` command (SPEC 9.1). The operator moves
/// the listener with [`ProxyBuilder::with_ports`] to where the TLS
/// terminator forwards (SPEC 8); every routing rule follows the listener,
/// not this number.
pub const DEFAULT_PORT: u16 = 443;

/// The extra listener range. A request on port N goes to port N on the
/// instance and is always private, regardless of visibility (SPEC 9.1,
/// 9.2).
pub const HIGH_PORT_MIN: u16 = 3000;
pub const HIGH_PORT_MAX: u16 = 9999;

/// How often an upgraded connection, such as a WebSocket, is authorized
/// again (SPEC 9.2, 13). The upgrade request is checked once; without this
/// the tunnel would outlive a visibility change, a removed share, or a
/// revoked session.
pub const TUNNEL_REVALIDATE_INTERVAL: Duration = Duration::from_secs(30);

/// An error that can cross a proxy integration seam.
pub type BoxError = Box<dyn StdError + Send + Sync>;

/// The streaming body type accepted and returned by proxy handlers.
pub type ProxyBody = BoxBody<Bytes, BoxError>;

/// Resolves an instance name to its current row. The address is assigned
/// at creation (SPEC 6.2), so a lookup never waits for boot. `None` makes a
/// name that never existed, a deleted name, and a name in release cooldown
/// indistinguishable, allowing the uniform response required by SPEC 9.2.
#[async_trait]
pub trait InstanceSource: Send + Sync + 'static {
    async fn instance_by_name(&self, name: &str) -> Result<Option<Instance>, BoxError>;
}

/// One request's authorization standing on one instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// The request carries no valid session or token. Private instances
    /// redirect this request to the login page (SPEC 9.2).
    Unauthenticated,
    /// The caller is authenticated but neither owns the instance nor holds
    /// a share keyed on its UUID (SPEC 13). The uniform 404 hides it.
    Forbidden,
    /// The caller owns the instance or holds a share.
    Granted,
}

/// Answers the SPEC 13 authorization question for one request. It checks
/// both whether valid credentials are present and whether that identity
/// owns the instance or holds a share keyed on its UUID. Authorization runs
/// on every request, so a credential held from before a name changed hands
/// grants nothing. The proxy never interprets credentials itself.
#[async_trait]
pub trait SessionChecker: Send + Sync + 'static {
    async fn access(&self, request: &Request<ProxyBody>, instance_uuid: &str) -> Access;
}

/// Records a forwarded HTTP request against `instances.last_seen_at`
/// (SPEC 12: the column records the last SSH connection or HTTP request).
#[async_trait]
pub trait LastSeenRecorder: Send + Sync + 'static {
    async fn touch_last_seen(&self, uuid: &str) -> Result<(), BoxError>;
}

/// An outbound streaming HTTP transport. Production uses a pooled Hyper
/// client with a deliberately short connection timeout; tests can inject a
/// deterministic implementation through [`ProxyBuilder::with_transport`].
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    async fn send(&self, request: Request<ProxyBody>) -> Result<Response<ProxyBody>, BoxError>;
}

/// Future returned by a control-plane handler.
pub type HandlerFuture = Pin<Box<dyn Future<Output = Response<ProxyBody>> + Send>>;

/// The handler used for the dashboard and OIDC flow on the base domain.
pub type ControlHandler = Arc<dyn Fn(Request<ProxyBody>) -> HandlerFuture + Send + Sync>;

/// Boxes an async function as a [`ControlHandler`].
pub fn control_handler<F, Fut>(handler: F) -> ControlHandler
where
    F: Fn(Request<ProxyBody>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<ProxyBody>> + Send + 'static,
{
    Arc::new(move |request| Box::pin(handler(request)))
}

/// Builds a proxy while preserving the integration seams used by the
/// control plane and tests.
pub struct ProxyBuilder {
    base_domain: String,
    instance_domain: Option<String>,
    instances: Arc<dyn InstanceSource>,
    sessions: Option<Arc<dyn SessionChecker>>,
    control: Option<ControlHandler>,
    login_url: Option<String>,
    transport: Option<Arc<dyn Transport>>,
    last_seen: Option<Arc<dyn LastSeenRecorder>>,
    tunnel_interval: Duration,
    main_port: u16,
    high_min: u16,
    high_max: u16,
}

impl ProxyBuilder {
    /// Sets the domain instances publish under (SPEC 7.1). An empty value
    /// or an absent call leaves it equal to the base domain, which is what a
    /// single-domain deployment wants.
    pub fn with_instance_domain(mut self, domain: impl Into<String>) -> Self {
        self.instance_domain = Some(domain.into());
        self
    }

    /// Supplies the per-request authorization implementation.
    pub fn with_sessions(mut self, sessions: Arc<dyn SessionChecker>) -> Self {
        self.sessions = Some(sessions);
        self
    }

    /// Supplies the dashboard and OIDC handler for the base domain.
    pub fn with_control(mut self, control: ControlHandler) -> Self {
        self.control = Some(control);
        self
    }

    /// Replaces the outbound transport. Production keeps the default
    /// short-dial Hyper client.
    pub fn with_transport(mut self, transport: Arc<dyn Transport>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Replaces the dashboard login URL. The default is
    /// `https://<base domain>/login`.
    pub fn with_login_url(mut self, login_url: impl Into<String>) -> Self {
        self.login_url = Some(login_url.into());
        self
    }

    /// Records every forwarded request against the instance (SPEC 12).
    pub fn with_last_seen(mut self, recorder: Arc<dyn LastSeenRecorder>) -> Self {
        self.last_seen = Some(recorder);
        self
    }

    /// Sets how often an upgraded connection is authorized again. The
    /// default is [`TUNNEL_REVALIDATE_INTERVAL`].
    pub fn with_tunnel_revalidation(mut self, interval: Duration) -> Self {
        self.tunnel_interval = interval;
        self
    }

    /// Moves listeners off the SPEC 9.1 defaults. `main` carries the base
    /// domain and an instance's default HTTP port; `high_min..=high_max`
    /// are always-private extra ports. Zero leaves a setting at its default.
    ///
    /// The main port moves to where the TLS terminator in front of Bento
    /// forwards (SPEC 8). Everything decided from the listener port — including the
    /// control plane and `public` applying only to the default port — moves
    /// with it (SPEC 9.2).
    pub fn with_ports(mut self, main: u16, high_min: u16, high_max: u16) -> Self {
        if main != 0 {
            self.main_port = main;
        }
        if high_min != 0 {
            self.high_min = high_min;
        }
        if high_max != 0 {
            self.high_max = high_max;
        }
        self
    }

    /// Validates the listener layout and creates the proxy.
    pub fn build(self) -> Result<Proxy, Error> {
        let base_domain = self
            .base_domain
            .to_ascii_lowercase()
            .trim_end_matches('.')
            .to_owned();
        if base_domain.is_empty() {
            return Err(Error::EmptyBaseDomain);
        }
        if self.high_min > self.high_max {
            return Err(Error::EmptyHighPortRange {
                min: self.high_min,
                max: self.high_max,
            });
        }
        if (self.high_min..=self.high_max).contains(&self.main_port) {
            return Err(Error::MainPortInHighRange {
                main: self.main_port,
                min: self.high_min,
                max: self.high_max,
            });
        }
        let login_url = self
            .login_url
            .unwrap_or_else(|| format!("https://{base_domain}/login"));
        let instance_domain = self
            .instance_domain
            .filter(|domain| !domain.is_empty())
            .unwrap_or_else(|| base_domain.clone());
        Ok(Proxy {
            base_domain,
            instance_domain,
            instances: self.instances,
            sessions: self.sessions,
            control: self.control,
            login_url,
            transport: self.transport.unwrap_or_else(default_transport),
            last_seen: self.last_seen,
            tunnel_interval: self.tunnel_interval,
            main_port: self.main_port,
            high_min: self.high_min,
            high_max: self.high_max,
        })
    }
}

/// Hostname router and streaming reverse proxy.
pub struct Proxy {
    base_domain: String,
    instance_domain: String,
    instances: Arc<dyn InstanceSource>,
    sessions: Option<Arc<dyn SessionChecker>>,
    control: Option<ControlHandler>,
    login_url: String,
    transport: Arc<dyn Transport>,
    last_seen: Option<Arc<dyn LastSeenRecorder>>,
    tunnel_interval: Duration,
    main_port: u16,
    high_min: u16,
    high_max: u16,
}

impl Proxy {
    /// Starts a builder. Requests for `base_domain` go to the configured
    /// control handler; `<name>.<instance domain>` resolves through
    /// `instances`. The instance domain defaults to `base_domain`.
    pub fn builder(
        base_domain: impl Into<String>,
        instances: Arc<dyn InstanceSource>,
    ) -> ProxyBuilder {
        ProxyBuilder {
            base_domain: base_domain.into(),
            instance_domain: None,
            instances,
            sessions: None,
            control: None,
            login_url: None,
            transport: None,
            last_seen: None,
            tunnel_interval: TUNNEL_REVALIDATE_INTERVAL,
            main_port: DEFAULT_PORT,
            high_min: HIGH_PORT_MIN,
            high_max: HIGH_PORT_MAX,
        }
    }

    /// Every port bound by [`Proxy::serve`]: the main port followed by the
    /// high range (SPEC 9, 9.1).
    pub fn ports(&self) -> Vec<u16> {
        let mut ports = Vec::with_capacity(usize::from(self.high_max - self.high_min) + 2);
        ports.push(self.main_port);
        ports.extend(self.high_min..=self.high_max);
        ports
    }

    /// Routes one request using the actual listener port. This is useful
    /// for embedding the proxy in a custom accept loop; [`Proxy::serve`]
    /// supplies these values directly from each accepted connection.
    pub async fn handle(
        &self,
        request: Request<ProxyBody>,
        listener_port: u16,
        remote_addr: SocketAddr,
    ) -> Response<ProxyBody> {
        let tls = request.uri().scheme_str() == Some("https");
        self.handle_connection(request, listener_port, remote_addr, tls)
            .await
    }

    pub(crate) async fn handle_connection(
        &self,
        mut request: Request<ProxyBody>,
        listener_port: u16,
        remote_addr: SocketAddr,
        tls: bool,
    ) -> Response<ProxyBody> {
        let host = request_host(&request);

        // The base domain is tested first because it can itself be a name
        // under the instance domain, as `bento.foid.space` is under
        // `foid.space`. Reversing these two would route the control plane to
        // an instance lookup (SPEC 9).
        if host == self.base_domain {
            // The control plane answers only on the main port. The high
            // ports bind nothing for the base domain.
            if listener_port == self.main_port
                && let Some(control) = &self.control
            {
                return control(request).await;
            }
            return pages::not_found();
        }

        let Some(name) = host.strip_suffix(&format!(".{}", self.instance_domain)) else {
            return pages::not_found();
        };
        if name.is_empty() || name.contains('.') {
            return pages::not_found();
        }

        let instance = match self.instances.instance_by_name(name).await {
            Ok(Some(instance)) => instance,
            Ok(None) => return pages::not_found(),
            Err(_) => return internal_error(),
        };
        let verdict = authorize(
            self.sessions.as_ref(),
            self.main_port,
            &instance,
            listener_port,
            &request,
        )
        .await;
        match verdict {
            Verdict::Allowed => {}
            Verdict::NotFound => return pages::not_found(),
            Verdict::Login => return self.redirect_to_login(&request),
            // Answer at once; never hold the request until the instance is
            // up (SPEC 9.3).
            Verdict::Unavailable => return pages::unavailable(&instance),
        }
        let target_port = if listener_port == self.main_port {
            instance.effective_http_port()
        } else {
            listener_port
        };

        // SPEC 12: last_seen_at records the last HTTP request. A recording
        // failure must not take a healthy instance offline.
        if let Some(last_seen) = &self.last_seen {
            let _ = last_seen.touch_last_seen(&instance.uuid).await;
        }

        let guard = self.tunnel_guard(&request, &instance, name, listener_port);
        self.forward(
            &mut request,
            &instance,
            target_port,
            remote_addr,
            tls,
            guard,
        )
        .await
    }

    /// Builds the hook that authorizes an upgraded connection again, with
    /// the headers of the request that opened it. The instance is found by
    /// name and must still have the same UUID: the name is a label (SPEC 7.1).
    /// A lookup failure keeps the tunnel, because a database blip is not a
    /// revoked grant.
    fn tunnel_guard(
        &self,
        request: &Request<ProxyBody>,
        instance: &Instance,
        name: &str,
        listener_port: u16,
    ) -> TunnelGuard {
        let mut guard = TunnelGuard {
            interval: self.tunnel_interval,
            shutdown: request.extensions().get::<Shutdown>().map(|s| s.0.clone()),
            ..TunnelGuard::default()
        };
        if upgrade_protocol(request.headers()).is_none() {
            return guard;
        }
        let instances = self.instances.clone();
        let sessions = self.sessions.clone();
        let main_port = self.main_port;
        let name = name.to_owned();
        let uuid = instance.uuid.clone();
        let method = request.method().clone();
        let uri = request.uri().clone();
        let headers = request.headers().clone();
        guard.revalidate = Some(Arc::new(move || {
            let instances = instances.clone();
            let sessions = sessions.clone();
            let (name, uuid) = (name.clone(), uuid.clone());
            let (method, uri, headers) = (method.clone(), uri.clone(), headers.clone());
            Box::pin(async move {
                let instance = match instances.instance_by_name(&name).await {
                    Ok(Some(instance)) if instance.uuid == uuid => instance,
                    Ok(_) => return false,
                    Err(_) => return true,
                };
                let mut original = Request::new(full_body(Bytes::new()));
                *original.method_mut() = method;
                *original.uri_mut() = uri;
                *original.headers_mut() = headers;
                authorize(
                    sessions.as_ref(),
                    main_port,
                    &instance,
                    listener_port,
                    &original,
                )
                .await
                    == Verdict::Allowed
            })
        }));
        guard
    }

    fn redirect_to_login(&self, request: &Request<ProxyBody>) -> Response<ProxyBody> {
        let host = original_host(request);
        let path = request
            .uri()
            .path_and_query()
            .map_or("/", http::uri::PathAndQuery::as_str);
        let next = format!("https://{host}{path}");
        let encoded: String = url::form_urlencoded::byte_serialize(next.as_bytes()).collect();
        let location = format!("{}?next={encoded}", self.login_url);
        let mut response = Response::new(full_body(Bytes::new()));
        *response.status_mut() = StatusCode::FOUND;
        if let Ok(value) = HeaderValue::from_str(&location) {
            response.headers_mut().insert(header::LOCATION, value);
        }
        response
    }

    async fn forward(
        &self,
        request: &mut Request<ProxyBody>,
        instance: &Instance,
        target_port: u16,
        remote_addr: SocketAddr,
        tls: bool,
        guard: TunnelGuard,
    ) -> Response<ProxyBody> {
        let original_host = original_host(request);
        let path = request
            .uri()
            .path_and_query()
            .cloned()
            .unwrap_or_else(|| http::uri::PathAndQuery::from_static("/"));
        let target = format_authority(&instance.address, target_port);
        let uri = Uri::builder()
            .scheme("http")
            .authority(target.as_str())
            .path_and_query(path)
            .build();
        let Ok(uri) = uri else {
            return pages::unavailable(instance);
        };

        let prepared = send_guarded(
            self.transport.as_ref(),
            take_request(request),
            guard,
            |request| {
                *request.uri_mut() = uri;
                // Forwarding headers are trust-boundary data. Only the TLS
                // terminator on this machine, which reaches the proxy from
                // loopback, may say what the client used (SPEC 8, 9). From
                // any other peer they are client data and are replaced.
                let trusted = forwarded_by_terminator(remote_addr.ip());
                let proto = trusted
                    .then(|| forwarded_proto(request.headers()))
                    .flatten()
                    .unwrap_or(if tls { "https" } else { "http" });
                let chain = trusted
                    .then(|| forwarded_for_chain(request.headers()))
                    .flatten();
                for name in [
                    HeaderName::from_static("forwarded"),
                    HeaderName::from_static("x-forwarded-for"),
                    HeaderName::from_static("x-forwarded-host"),
                    HeaderName::from_static("x-forwarded-proto"),
                ] {
                    request.headers_mut().remove(name);
                }
                if let Ok(host) = HeaderValue::from_str(&original_host) {
                    request.headers_mut().insert(header::HOST, host.clone());
                    request
                        .headers_mut()
                        .insert(HeaderName::from_static("x-forwarded-host"), host);
                }
                request.headers_mut().insert(
                    HeaderName::from_static("x-forwarded-proto"),
                    HeaderValue::from_static(proto),
                );
                set_forwarded_for(request.headers_mut(), chain, remote_addr.ip());
                // Only the forwarded copy loses the credentials. The session
                // check and the tunnel revalidation hold the originals.
                strip_bento_credentials(request.headers_mut());
            },
        )
        .await;
        match prepared {
            Ok(response) => response,
            Err(_) => pages::unavailable(instance),
        }
    }
}

/// Sends `request` through `transport`. When the client and the upstream
/// agree on an upgrade, such as a WebSocket, this joins the two upgraded
/// connections. Instance traffic and the control plane both need that; the
/// web terminal is a WebSocket on the base domain (SPEC 14.6).
///
/// `prepare` runs after the hop-by-hop headers are removed, so a header it
/// sets cannot be named away by the client's `Connection` header.
pub async fn send_upgradable(
    transport: &dyn Transport,
    request: Request<ProxyBody>,
    prepare: impl FnOnce(&mut Request<ProxyBody>),
) -> Result<Response<ProxyBody>, BoxError> {
    send_guarded(transport, request, TunnelGuard::default(), prepare).await
}

/// The future that answers whether an upgraded connection may stay open.
pub type Revalidate = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// Limits the life of an upgraded connection (SPEC 9.2, 13).
#[derive(Clone)]
pub struct TunnelGuard {
    /// Called every `interval`. The tunnel ends when it answers `false`.
    /// `None` leaves the tunnel open until a side closes it, which is the
    /// trust model of the control plane: the handler authorized the
    /// request itself.
    pub revalidate: Option<Revalidate>,
    pub interval: Duration,
    /// Ends the tunnel when cancelled, for example on shutdown.
    pub shutdown: Option<CancellationToken>,
}

impl Default for TunnelGuard {
    fn default() -> Self {
        Self {
            revalidate: None,
            interval: TUNNEL_REVALIDATE_INTERVAL,
            shutdown: None,
        }
    }
}

/// Marks a request that arrived on a listener with a shutdown signal.
#[derive(Clone)]
pub(crate) struct Shutdown(pub(crate) CancellationToken);

/// Joins two upgraded connections until one side closes, `guard` ends the
/// tunnel, or the shutdown signal fires.
async fn run_tunnel(
    mut inbound: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    mut outbound: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    guard: TunnelGuard,
) {
    let copy = tokio::io::copy_bidirectional(&mut inbound, &mut outbound);
    tokio::pin!(copy);
    let shutdown = async {
        match &guard.shutdown {
            Some(token) => token.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(shutdown);
    loop {
        let Some(revalidate) = &guard.revalidate else {
            tokio::select! {
                _ = &mut copy => {}
                () = &mut shutdown => {}
            }
            return;
        };
        tokio::select! {
            _ = &mut copy => return,
            () = &mut shutdown => return,
            () = tokio::time::sleep(guard.interval) => {}
        }
        // The data keeps flowing while the check runs.
        tokio::select! {
            _ = &mut copy => return,
            () = &mut shutdown => return,
            allowed = revalidate() => {
                if !allowed {
                    return;
                }
            }
        }
    }
}

async fn send_guarded(
    transport: &dyn Transport,
    mut request: Request<ProxyBody>,
    guard: TunnelGuard,
    prepare: impl FnOnce(&mut Request<ProxyBody>),
) -> Result<Response<ProxyBody>, BoxError> {
    let requested_upgrade = upgrade_protocol(request.headers()).cloned();
    remove_hop_by_hop_headers(request.headers_mut());
    if let Some(protocol) = &requested_upgrade {
        request
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        request
            .headers_mut()
            .insert(header::UPGRADE, protocol.clone());
    }
    prepare(&mut request);

    let request_upgrade = requested_upgrade
        .as_ref()
        .map(|_| hyper::upgrade::on(&mut request));
    let mut response = transport.send(request).await?;
    let response_upgrade = if requested_upgrade.is_some()
        && response.status() == StatusCode::SWITCHING_PROTOCOLS
        && upgrade_protocol(response.headers()) == requested_upgrade.as_ref()
    {
        Some(hyper::upgrade::on(&mut response))
    } else {
        None
    };
    remove_hop_by_hop_headers(response.headers_mut());
    if let Some(protocol) = requested_upgrade
        && response_upgrade.is_some()
    {
        response
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        response.headers_mut().insert(header::UPGRADE, protocol);
    }

    if let (Some(inbound), Some(outbound)) = (request_upgrade, response_upgrade) {
        tokio::spawn(async move {
            if let (Ok(inbound), Ok(outbound)) = (inbound.await, outbound.await) {
                run_tunnel(
                    hyper_util::rt::TokioIo::new(inbound),
                    hyper_util::rt::TokioIo::new(outbound),
                    guard,
                )
                .await;
            }
        });
    }
    Ok(response)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Allowed,
    /// The uniform 404 (SPEC 9.2).
    NotFound,
    Login,
    Unavailable,
}

/// The per-request decision (SPEC 9.2, 13), shared by a new request and by
/// the later checks of an upgraded connection.
async fn authorize(
    sessions: Option<&Arc<dyn SessionChecker>>,
    main_port: u16,
    instance: &Instance,
    listener_port: u16,
    request: &Request<ProxyBody>,
) -> Verdict {
    if instance.visibility == Visibility::Off {
        // A name that does not exist, a name in the release cooldown,
        // and an instance with visibility off answer byte-identically,
        // so a visitor cannot probe which names exist (SPEC 9.2, 7.3).
        return Verdict::NotFound;
    }

    // Ports 3000-9999 are always private; `public` applies only to the
    // default HTTP port (SPEC 9.2). A private request is authorized
    // against the owner and shares on every request (SPEC 13). An
    // authenticated user without access gets the same 404 as a missing
    // name.
    let private = instance.visibility == Visibility::Private || listener_port != main_port;
    if private {
        let access = match sessions {
            Some(sessions) => sessions.access(request, &instance.uuid).await,
            None => Access::Unauthenticated,
        };
        match access {
            Access::Granted => {}
            Access::Unauthenticated => return Verdict::Login,
            Access::Forbidden => return Verdict::NotFound,
        }
    }
    if instance.state != State::Running {
        return Verdict::Unavailable;
    }
    Verdict::Allowed
}

/// Every Bento cookie name and API token starts with this. The proxy must
/// not depend on the auth crate, so the prefix is repeated here. It is
/// `bento_auth::TOKEN_PREFIX` and the start of `SESSION_COOKIE_NAME` and of
/// the OIDC flow cookies (`bento_oauth_state`, `bento_oauth_nonce`,
/// `bento_login_next`). Keep the four in step (SPEC 9.2, 13).
const BENTO_CREDENTIAL_PREFIX: &str = "bento_";

/// Removes what Bento issued from a request bound for a guest, so a guest
/// app cannot read or replay a visitor's Bento session (SPEC 9.2, 13).
/// This drops every cookie whose name starts with `bento_`, and the
/// `Authorization` header when it carries a Bento API token. The guest's own
/// cookies and its own `Authorization` (Basic auth, for example) pass.
fn strip_bento_credentials(headers: &mut HeaderMap) {
    let mut removed = false;
    let mut kept: Vec<String> = Vec::new();
    let mut opaque: Vec<HeaderValue> = Vec::new();
    for value in headers.get_all(header::COOKIE) {
        match value.to_str() {
            Ok(text) => {
                for pair in text
                    .split(';')
                    .map(str::trim)
                    .filter(|pair| !pair.is_empty())
                {
                    let name = pair.split_once('=').map_or(pair, |(name, _)| name).trim();
                    if name.starts_with(BENTO_CREDENTIAL_PREFIX) {
                        removed = true;
                    } else {
                        kept.push(pair.to_owned());
                    }
                }
            }
            // Not text, so it cannot name a Bento cookie. Leave it be.
            Err(_) => opaque.push(value.clone()),
        }
    }
    if removed {
        headers.remove(header::COOKIE);
        for value in opaque {
            headers.append(header::COOKIE, value);
        }
        if !kept.is_empty() {
            // Built from parts of valid header values joined by `; `.
            if let Ok(value) = HeaderValue::from_str(&kept.join("; ")) {
                headers.append(header::COOKIE, value);
            }
        }
    }
    let bento_token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(char::is_whitespace))
        .is_some_and(|(scheme, token)| {
            scheme.eq_ignore_ascii_case("bearer")
                && token.trim_start().starts_with(BENTO_CREDENTIAL_PREFIX)
        });
    if bento_token {
        headers.remove(header::AUTHORIZATION);
    }
}

fn take_request(request: &mut Request<ProxyBody>) -> Request<ProxyBody> {
    let replacement = Request::new(full_body(Bytes::new()));
    std::mem::replace(request, replacement)
}

/// The hyper client the proxy uses by default. The control plane handler
/// uses it too, with [`send_upgradable`].
pub fn http_transport() -> Arc<dyn Transport> {
    default_transport()
}

fn default_transport() -> Arc<dyn Transport> {
    let mut connector = HttpConnector::new();
    // The short timeout is intentional: a dead target must answer with the
    // 503 page quickly instead of holding the request (SPEC 9.3).
    connector.set_connect_timeout(Some(Duration::from_secs(3)));
    connector.set_keepalive(Some(Duration::from_secs(90)));
    let client = Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(8)
        .build(connector);
    Arc::new(HyperTransport { client })
}

struct HyperTransport {
    client: Client<HttpConnector, ProxyBody>,
}

#[async_trait]
impl Transport for HyperTransport {
    async fn send(&self, request: Request<ProxyBody>) -> Result<Response<ProxyBody>, BoxError> {
        let response = self.client.request(request).await?;
        Ok(response.map(|body| {
            body.map_err(|error| -> BoxError { Box::new(error) })
                .boxed()
        }))
    }
}

/// Errors produced while configuring, binding, or serving the proxy.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("proxy: base domain is empty")]
    EmptyBaseDomain,
    #[error("proxy: high port range {min}-{max} is empty")]
    EmptyHighPortRange { min: u16, max: u16 },
    #[error("proxy: main port {main} falls inside the high port range {min}-{max}")]
    MainPortInHighRange { main: u16, min: u16, max: u16 },
    #[error("proxy: bind port {port}")]
    Bind {
        port: u16,
        #[source]
        source: std::io::Error,
    },
    #[error("proxy: serve port {port}")]
    Serve {
        port: u16,
        #[source]
        source: std::io::Error,
    },
    #[error("proxy: shutdown requested")]
    Shutdown,
}

pub(crate) fn full_body(body: impl Into<Bytes>) -> ProxyBody {
    Full::new(body.into())
        .map_err(|never: Infallible| match never {})
        .boxed()
}

fn internal_error() -> Response<ProxyBody> {
    let mut response = Response::new(full_body("internal error\n"));
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response
}

// The Host header names the instance (SPEC 9). Port, case, and one trailing
// dot are stripped.
fn request_host(request: &Request<ProxyBody>) -> String {
    let host = original_host(request);
    strip_host_port(&host)
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn original_host(request: &Request<ProxyBody>) -> String {
    request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| request.uri().authority().map(ToString::to_string))
        .unwrap_or_default()
}

fn strip_host_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host.find(']').map_or(host, |end| &host[1..end]);
    }
    match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.parse::<u16>().is_ok() => name,
        _ => host,
    }
}

fn format_authority(address: &str, port: u16) -> String {
    if address.contains(':') && !address.starts_with('[') {
        format!("[{address}]:{port}")
    } else {
        format!("{address}:{port}")
    }
}

/// Whether `peer` is the TLS terminator in front of the proxy. The
/// terminator runs on this machine and forwards to a loopback listener
/// (SPEC 8), so only a loopback peer may supply forwarding headers.
fn forwarded_by_terminator(peer: IpAddr) -> bool {
    match peer {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or(v6.is_loopback(), |v4| v4.is_loopback()),
    }
}

/// The scheme the client used, as the terminator reports it. The first
/// entry is the hop nearest the client. Any value other than `http` or
/// `https` counts as no report.
fn forwarded_proto(headers: &http::HeaderMap) -> Option<&'static str> {
    let first = headers
        .get("x-forwarded-proto")?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim();
    if first.eq_ignore_ascii_case("https") {
        Some("https")
    } else if first.eq_ignore_ascii_case("http") {
        Some("http")
    } else {
        None
    }
}

/// The `X-Forwarded-For` chain that the terminator sent, as one list.
fn forwarded_for_chain(headers: &http::HeaderMap) -> Option<String> {
    let entries: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    (!entries.is_empty()).then(|| entries.join(", "))
}

/// Writes `X-Forwarded-For`: the trusted `chain` with the peer appended,
/// or the peer alone.
fn set_forwarded_for(headers: &mut http::HeaderMap, chain: Option<String>, remote: IpAddr) {
    let name = HeaderName::from_static("x-forwarded-for");
    let value = match chain {
        Some(chain) => format!("{chain}, {remote}"),
        None => remote.to_string(),
    };
    if let Ok(value) = HeaderValue::from_str(&value) {
        headers.insert(name, value);
    }
}

fn upgrade_protocol(headers: &http::HeaderMap) -> Option<&HeaderValue> {
    let connection_has_upgrade = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    connection_has_upgrade
        .then(|| headers.get(header::UPGRADE))
        .flatten()
}

fn remove_hop_by_hop_headers(headers: &mut http::HeaderMap) {
    let connection_headers: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in connection_headers {
        headers.remove(name);
    }
    for name in [
        header::CONNECTION,
        HeaderName::from_static("proxy-connection"),
        HeaderName::from_static("keep-alive"),
        HeaderName::from_static("proxy-authenticate"),
        HeaderName::from_static("proxy-authorization"),
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ] {
        headers.remove(name);
    }
}

#[cfg(test)]
mod tests;
