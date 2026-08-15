//! OIDC login for the dashboard, base-domain session cookies,
//! per-request authorization against owner and shares, and API tokens
//! (SPEC sections 9.2 and 13).
//!
//! The session cookie identifies the user and nothing else. Authorization
//! runs on every request against the owner and the shares of the instance,
//! keyed by instance UUID. A cookie held from before a name changed hands
//! therefore grants nothing on the new instance.

use std::error::Error as StdError;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use bento_types::{Token, User};
use http::header::{CONTENT_TYPE, COOKIE, HeaderValue, LOCATION, SET_COOKIE};
use http::{HeaderMap, Response, StatusCode};
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use time::{Duration, OffsetDateTime};

mod authorize;
mod middleware;
mod oidc;
mod session;
mod token;

pub use middleware::{UserId, user_id_from_parts};
pub use oidc::{Claims, Exchanger, ProviderClient, ProviderError, Verifier};
pub use session::{MemorySessionStore, SESSION_COOKIE_NAME, Session, SessionStore};
pub use token::{TOKEN_PREFIX, TokenLookup, bearer_token, hash_token};

/// The body type used by framework-neutral authentication responses.
pub type HttpResponse = Response<String>;

/// An error returned by a consumer-side store implementation.
pub type BoxError = Box<dyn StdError + Send + Sync + 'static>;

/// Errors returned by authentication and authorization.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No valid session or token was presented.
    #[error("auth: unauthenticated")]
    Unauthenticated,
    /// The caller is authenticated but neither owns the instance nor
    /// holds a share on its UUID.
    #[error("auth: forbidden")]
    Forbidden,
    /// The presented API token exists but its expiry time has passed.
    #[error("auth: token expired")]
    TokenExpired,
    /// The OIDC login succeeded but no users row has the presented
    /// subject. Registration happens over SSH (SPEC 13).
    #[error("auth: no account for OIDC subject")]
    NoAccount,
    /// A consumer-side persistence operation failed.
    #[error("{operation}: {source}")]
    Store {
        operation: String,
        #[source]
        source: BoxError,
    },
}

/// Results returned by this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Resolves an OIDC subject to a Bento user. The store package satisfies
/// it through a thin adapter over `users.oidc_subject`.
#[async_trait]
pub trait UserStore: Send + Sync {
    /// Returns the user whose `oidc_subject` column matches `subject`, or
    /// `None` when no such user exists.
    async fn user_by_oidc_subject(
        &self,
        subject: &str,
    ) -> std::result::Result<Option<User>, BoxError>;
}

/// Answers the per-request authorization question of SPEC 13.
#[async_trait]
pub trait AccessStore: Send + Sync {
    /// Reports whether the user owns the instance with the given UUID or
    /// holds a shares row keyed on that UUID. It must key on the UUID,
    /// never on the name (SPEC 12).
    async fn has_access(
        &self,
        instance_uuid: &str,
        user_id: i64,
    ) -> std::result::Result<bool, BoxError>;
}

/// Persists API tokens. Only the hash of a token is stored (SPEC 13).
#[async_trait]
pub trait TokenStore: Send + Sync {
    /// Inserts a token row. [`OffsetDateTime::UNIX_EPOCH`] means the token
    /// does not expire.
    async fn create_token(
        &self,
        user_id: i64,
        hash: &str,
        expires_at: OffsetDateTime,
    ) -> std::result::Result<Token, BoxError>;

    /// Returns the lookup outcome for `hash`. The expired outcome carries
    /// its row because this crate enforces expiry against its own clock.
    async fn token_by_hash(&self, hash: &str) -> std::result::Result<TokenLookup, BoxError>;

    /// Removes a token row.
    async fn delete_token(&self, id: i64) -> std::result::Result<(), BoxError>;
}

type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// Ties sessions, OIDC login, authorization, and API tokens together.
pub struct Service {
    base_domain: String,
    sessions: Arc<dyn SessionStore>,
    users: Arc<dyn UserStore>,
    access: Arc<dyn AccessStore>,
    tokens: Arc<dyn TokenStore>,
    oauth: Option<Arc<dyn Exchanger>>,
    verifier: Option<Arc<dyn Verifier>>,
    now: Clock,
    session_ttl: Duration,
    login_path: String,
}

/// The default session lifetime is seven days.
pub const DEFAULT_SESSION_TTL: Duration = Duration::days(7);

impl Service {
    /// Returns a service for the given base domain. The session cookie is
    /// issued for the base domain and is therefore valid on every
    /// subdomain (SPEC 13).
    pub fn new(
        base_domain: impl Into<String>,
        users: Arc<dyn UserStore>,
        access: Arc<dyn AccessStore>,
        tokens: Arc<dyn TokenStore>,
    ) -> Self {
        Self {
            base_domain: base_domain
                .into()
                .trim_end_matches('.')
                .to_ascii_lowercase(),
            sessions: Arc::new(MemorySessionStore::new()),
            users,
            access,
            tokens,
            oauth: None,
            verifier: None,
            now: Arc::new(OffsetDateTime::now_utc),
            session_ttl: DEFAULT_SESSION_TTL,
            login_path: "/login".to_string(),
        }
    }

    /// Replaces the default in-memory session store.
    #[must_use]
    pub fn with_session_store(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.sessions = store;
        self
    }

    /// Injects the time source, for deterministic callers and tests.
    #[must_use]
    pub fn with_clock<F>(mut self, now: F) -> Self
    where
        F: Fn() -> OffsetDateTime + Send + Sync + 'static,
    {
        self.now = Arc::new(now);
        self
    }

    /// Sets the session lifetime.
    #[must_use]
    pub fn with_session_ttl(mut self, duration: Duration) -> Self {
        self.session_ttl = duration;
        self
    }

    /// Injects the OAuth2 exchanger and ID token verifier. Wire one
    /// [`ProviderClient`] for both in production; tests pass fakes.
    #[must_use]
    pub fn with_oidc(mut self, exchanger: Arc<dyn Exchanger>, verifier: Arc<dyn Verifier>) -> Self {
        self.oauth = Some(exchanger);
        self.verifier = Some(verifier);
        self
    }

    /// Sets the path unauthenticated browser requests redirect to.
    #[must_use]
    pub fn with_login_path(mut self, path: impl Into<String>) -> Self {
        self.login_path = path.into();
        self
    }
}

/// Returns a 256-bit opaque random value in URL-safe base64.
fn random_token() -> String {
    // Authentication cannot continue safely without cryptographic
    // randomness; the RNG surfaces an OS entropy failure as a
    // process-stopping panic.
    let mut bytes = [0_u8; 32];
    rand::fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

const COOKIE_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'%')
    .add(b',')
    .add(b';')
    .add(b'\\');

fn encode_cookie_value(value: &str) -> String {
    utf8_percent_encode(value, COOKIE_ENCODE_SET).to_string()
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(candidate, value)| {
            (candidate == name)
                .then(|| {
                    percent_decode_str(value)
                        .decode_utf8()
                        .ok()
                        .map(|v| v.into_owned())
                })
                .flatten()
        })
}

fn query_value(uri: &http::Uri, name: &str) -> Option<String> {
    url::form_urlencoded::parse(uri.query()?.as_bytes())
        .find_map(|(key, value)| (key == name).then(|| value.into_owned()))
}

fn text_response(status: StatusCode, message: impl Into<String>) -> HttpResponse {
    let mut response = Response::new(message.into());
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn redirect_response(location: &str) -> HttpResponse {
    let Ok(location) = HeaderValue::from_str(location) else {
        return text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid redirect target\n",
        );
    };
    let mut response = Response::new(String::new());
    *response.status_mut() = StatusCode::FOUND;
    response.headers_mut().insert(LOCATION, location);
    response
}

fn append_cookie(response: &mut HttpResponse, cookie: HeaderValue) {
    response.headers_mut().append(SET_COOKIE, cookie);
}

fn store_error(operation: impl Into<String>, source: BoxError) -> Error {
    Error::Store {
        operation: operation.into(),
        source,
    }
}

#[cfg(test)]
mod test_support;
