//! Hostname-based routing, visibility enforcement, and streaming HTTP
//! forwarding for instances (SPEC sections 9 and 14.5).
//!
//! The proxy resolves the instance name on every request (SPEC 7.1), reads
//! the instance address assigned at creation (SPEC 6.2), and forwards over
//! plain HTTP on the private network. TLS termination is optional and is
//! configured by the caller with a shared [`rustls::ServerConfig`].

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
use http::{Request, Response, StatusCode, Uri};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

mod listen;
mod pages;

pub use listen::{BoxIo, ListenFunc, Listener, listen_fn};

/// The main HTTPS port. A request on this port goes to the instance's
/// default HTTP port, set with the `port` command (SPEC 9.1). An operator
/// who terminates TLS in front of Bento can move the listener with
/// [`ProxyBuilder::with_ports`]; every routing rule follows the listener,
/// not this number.
pub const DEFAULT_PORT: u16 = 443;

/// The extra listener range. A request on port N goes to port N on the
/// instance and is always private, regardless of visibility (SPEC 9.1,
/// 9.2).
pub const HIGH_PORT_MIN: u16 = 3000;
pub const HIGH_PORT_MAX: u16 = 9999;

const DEFAULT_HTTP_PORT: u16 = 80;

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
    instances: Arc<dyn InstanceSource>,
    sessions: Option<Arc<dyn SessionChecker>>,
    control: Option<ControlHandler>,
    login_url: Option<String>,
    transport: Option<Arc<dyn Transport>>,
    last_seen: Option<Arc<dyn LastSeenRecorder>>,
    main_port: u16,
    high_min: u16,
    high_max: u16,
}

impl ProxyBuilder {
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

    /// Moves listeners off the SPEC 9.1 defaults. `main` carries the base
    /// domain and an instance's default HTTP port; `high_min..=high_max`
    /// are always-private extra ports. Zero leaves a setting at its default.
    ///
    /// The main port moves when something else terminates TLS in front of
    /// Bento. Everything decided from the listener port — including the
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
        Ok(Proxy {
            base_domain,
            instances: self.instances,
            sessions: self.sessions,
            control: self.control,
            login_url,
            transport: self.transport.unwrap_or_else(default_transport),
            last_seen: self.last_seen,
            main_port: self.main_port,
            high_min: self.high_min,
            high_max: self.high_max,
        })
    }
}

/// Hostname router and streaming reverse proxy.
pub struct Proxy {
    base_domain: String,
    instances: Arc<dyn InstanceSource>,
    sessions: Option<Arc<dyn SessionChecker>>,
    control: Option<ControlHandler>,
    login_url: String,
    transport: Arc<dyn Transport>,
    last_seen: Option<Arc<dyn LastSeenRecorder>>,
    main_port: u16,
    high_min: u16,
    high_max: u16,
}

impl Proxy {
    /// Starts a builder. Requests for `base_domain` go to the configured
    /// control handler; `<name>.<base_domain>` resolves through `instances`.
    pub fn builder(
        base_domain: impl Into<String>,
        instances: Arc<dyn InstanceSource>,
    ) -> ProxyBuilder {
        ProxyBuilder {
            base_domain: base_domain.into(),
            instances,
            sessions: None,
            control: None,
            login_url: None,
            transport: None,
            last_seen: None,
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
        self.handle_connection(request, listener_port, remote_addr, None, tls)
            .await
    }

    pub(crate) async fn handle_connection(
        &self,
        mut request: Request<ProxyBody>,
        listener_port: u16,
        remote_addr: SocketAddr,
        server_name: Option<&str>,
        tls: bool,
    ) -> Response<ProxyBody> {
        let host = request_host(&request, server_name);

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

        let Some(name) = host.strip_suffix(&format!(".{}", self.base_domain)) else {
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
        if instance.visibility == Visibility::Off {
            // A name that does not exist, a name in the release cooldown,
            // and an instance with visibility off answer byte-identically,
            // so a visitor cannot probe which names exist (SPEC 9.2, 7.3).
            return pages::not_found();
        }

        // Ports 3000-9999 are always private; `public` applies only to the
        // default HTTP port (SPEC 9.2). A private request is authorized
        // against the owner and shares on every request (SPEC 13). An
        // authenticated user without access gets the same 404 as a missing
        // name.
        let private = instance.visibility == Visibility::Private || listener_port != self.main_port;
        if private {
            let access = match &self.sessions {
                Some(sessions) => sessions.access(&request, &instance.uuid).await,
                None => Access::Unauthenticated,
            };
            match access {
                Access::Granted => {}
                Access::Unauthenticated => return self.redirect_to_login(&request),
                Access::Forbidden => return pages::not_found(),
            }
        }

        let target_port = if listener_port == self.main_port {
            if instance.http_port == 0 {
                DEFAULT_HTTP_PORT
            } else {
                instance.http_port
            }
        } else {
            listener_port
        };

        if instance.state != State::Running {
            // Answer at once; never hold the request until the instance is
            // up (SPEC 9.3).
            return pages::unavailable(&instance);
        }

        // SPEC 12: last_seen_at records the last HTTP request. A recording
        // failure must not take a healthy instance offline.
        if let Some(last_seen) = &self.last_seen {
            let _ = last_seen.touch_last_seen(&instance.uuid).await;
        }

        self.forward(&mut request, &instance, target_port, remote_addr, tls)
            .await
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
        *request.uri_mut() = uri;
        // Forwarding headers are trust-boundary data. Discard anything the
        // client supplied before writing the values observed by this proxy.
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
            HeaderValue::from_static(if tls { "https" } else { "http" }),
        );
        set_forwarded_for(request.headers_mut(), remote_addr.ip());

        let request_upgrade = requested_upgrade
            .as_ref()
            .map(|_| hyper::upgrade::on(&mut *request));
        let mut response = match self.transport.send(take_request(request)).await {
            Ok(response) => response,
            Err(_) => return pages::unavailable(instance),
        };

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
                    let mut inbound = hyper_util::rt::TokioIo::new(inbound);
                    let mut outbound = hyper_util::rt::TokioIo::new(outbound);
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
            });
        }
        response
    }
}

fn take_request(request: &mut Request<ProxyBody>) -> Request<ProxyBody> {
    let replacement = Request::new(full_body(Bytes::new()));
    std::mem::replace(request, replacement)
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
    #[error("proxy: bind port {port}: {source}")]
    Bind {
        port: u16,
        #[source]
        source: std::io::Error,
    },
    #[error("proxy: serve port {port}: {source}")]
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

// TLS Server Name Indication wins when present (SPEC 9); the Host header
// covers plain listeners. Port, case, and one trailing dot are stripped.
fn request_host(request: &Request<ProxyBody>, server_name: Option<&str>) -> String {
    let host = server_name
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| original_host(request));
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

fn set_forwarded_for(headers: &mut http::HeaderMap, remote: IpAddr) {
    let name = HeaderName::from_static("x-forwarded-for");
    let value = remote.to_string();
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
