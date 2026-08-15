use std::sync::Arc;

use async_trait::async_trait;
use http::header::HeaderValue;
use http::{HeaderMap, StatusCode, Uri};
use jsonwebtoken::errors::ErrorKind as JwtErrorKind;
use jsonwebtoken::jwk::{Jwk, JwkSet, KeyAlgorithm};
use jsonwebtoken::{DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use tokio::sync::RwLock;
use url::Url;

use crate::session::host_cookie;
use crate::{
    BoxError, Service, append_cookie, cookie_value, query_value, random_token, redirect_response,
    text_response,
};

/// The ID token claims Bento uses. The subject maps to the
/// `users.oidc_subject` column (SPEC 13).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Claims {
    #[serde(rename = "sub", default)]
    pub subject: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub nonce: String,
}

/// Checks a raw ID token and returns its claims. Tests use a fake and do
/// not contact an identity provider.
#[async_trait]
pub trait Verifier: Send + Sync {
    async fn verify(&self, raw_id_token: &str) -> std::result::Result<Claims, BoxError>;
}

/// Drives the OAuth2 authorization code flow. Tests use a fake and do not
/// contact an identity provider.
#[async_trait]
pub trait Exchanger: Send + Sync {
    /// Returns the provider URL to redirect the browser to.
    fn auth_code_url(&self, state: &str, nonce: &str) -> String;

    /// Redeems the authorization code and returns the raw ID token from
    /// the token response.
    async fn exchange(&self, code: &str) -> std::result::Result<String, BoxError>;
}

/// An OIDC discovery, exchange, JWKS, or verification failure.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ProviderError(String);

impl ProviderError {
    fn message(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[derive(Debug, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

/// Implements [`Exchanger`] and [`Verifier`] against a standard OIDC
/// provider. Its discovered JWKS is cached and refreshed when a token
/// names a new key or fails signature verification.
pub struct ProviderClient {
    client: reqwest::Client,
    issuer: String,
    client_id: String,
    client_secret: String,
    redirect_url: String,
    authorization_endpoint: Url,
    token_endpoint: Url,
    jwks_uri: Url,
    jwks: RwLock<Option<Arc<JwkSet>>>,
}

impl ProviderClient {
    /// Discovers the issuer and returns a client for the authorization
    /// code flow. `redirect_url` is the absolute URL of the `/callback`
    /// handler on the base domain.
    pub async fn discover(
        issuer: &str,
        client_id: &str,
        client_secret: &str,
        redirect_url: &str,
    ) -> std::result::Result<Self, ProviderError> {
        install_tls_provider();
        let issuer = issuer.to_string();
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let client = reqwest::Client::new();
        let discovery: Discovery = client
            .get(&discovery_url)
            .send()
            .await
            .map_err(|error| {
                ProviderError::message(format!("oidc discovery for {issuer}: {error}"))
            })?
            .error_for_status()
            .map_err(|error| {
                ProviderError::message(format!("oidc discovery for {issuer}: {error}"))
            })?
            .json()
            .await
            .map_err(|error| {
                ProviderError::message(format!("oidc discovery for {issuer}: {error}"))
            })?;
        if discovery.issuer != issuer {
            return Err(ProviderError::message(format!(
                "oidc discovery for {issuer}: issuer was {:?}",
                discovery.issuer
            )));
        }
        let parse_endpoint = |name: &str, endpoint: String| {
            Url::parse(&endpoint).map_err(|error| {
                ProviderError::message(format!(
                    "oidc discovery for {issuer}: invalid {name} {endpoint:?}: {error}"
                ))
            })
        };
        let authorization_endpoint =
            parse_endpoint("authorization_endpoint", discovery.authorization_endpoint)?;
        let token_endpoint = parse_endpoint("token_endpoint", discovery.token_endpoint)?;
        let jwks_uri = parse_endpoint("jwks_uri", discovery.jwks_uri)?;
        Ok(Self {
            client,
            issuer,
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            redirect_url: redirect_url.to_string(),
            authorization_endpoint,
            token_endpoint,
            jwks_uri,
            jwks: RwLock::new(None),
        })
    }

    async fn fetch_jwks(&self) -> std::result::Result<Arc<JwkSet>, ProviderError> {
        let keys = self
            .client
            .get(self.jwks_uri.clone())
            .send()
            .await
            .map_err(|error| ProviderError::message(format!("fetch OIDC JWKS: {error}")))?
            .error_for_status()
            .map_err(|error| ProviderError::message(format!("fetch OIDC JWKS: {error}")))?
            .json::<JwkSet>()
            .await
            .map_err(|error| ProviderError::message(format!("decode OIDC JWKS: {error}")))?;
        let keys = Arc::new(keys);
        *self.jwks.write().await = Some(Arc::clone(&keys));
        Ok(keys)
    }

    async fn cached_or_fetch_jwks(
        &self,
    ) -> std::result::Result<(Arc<JwkSet>, bool), ProviderError> {
        if let Some(keys) = self.jwks.read().await.clone() {
            return Ok((keys, true));
        }
        self.fetch_jwks().await.map(|keys| (keys, false))
    }

    async fn exchange_request(
        &self,
        code: &str,
        credentials_in_header: bool,
    ) -> std::result::Result<TokenResponse, ProviderError> {
        let form = {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            serializer
                .append_pair("grant_type", "authorization_code")
                .append_pair("code", code)
                .append_pair("redirect_uri", &self.redirect_url);
            if !credentials_in_header {
                serializer
                    .append_pair("client_id", &self.client_id)
                    .append_pair("client_secret", &self.client_secret);
            }
            serializer.finish()
        };
        let mut request = self
            .client
            .post(self.token_endpoint.clone())
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form);
        if credentials_in_header {
            request = request.basic_auth(&self.client_id, Some(&self.client_secret));
        }
        request
            .send()
            .await
            .map_err(|error| ProviderError::message(format!("oauth2 exchange: {error}")))?
            .error_for_status()
            .map_err(|error| ProviderError::message(format!("oauth2 exchange: {error}")))?
            .json::<TokenResponse>()
            .await
            .map_err(|error| ProviderError::message(format!("oauth2 exchange: {error}")))
    }

    fn verify_with_jwks(&self, raw: &str, keys: &JwkSet) -> VerifyAttempt {
        let header = match decode_header(raw) {
            Ok(header) => header,
            Err(error) => return VerifyAttempt::Final(error.to_string()),
        };
        let candidates: Vec<&Jwk> = keys
            .keys
            .iter()
            .filter(|key| {
                header
                    .kid
                    .as_ref()
                    .is_none_or(|kid| key.common.key_id.as_ref() == Some(kid))
            })
            .filter(|key| {
                key.common
                    .key_algorithm
                    .is_none_or(|algorithm| algorithm == KeyAlgorithm::from(header.alg))
            })
            .collect();
        if candidates.is_empty() {
            return VerifyAttempt::Refresh("no matching key in the OIDC JWKS".into());
        }

        let mut signature_error = None;
        for key in candidates {
            let decoding_key = match DecodingKey::from_jwk(key) {
                Ok(key) => key,
                Err(error) => {
                    signature_error = Some(error.to_string());
                    continue;
                }
            };
            let mut validation = Validation::new(header.alg);
            validation.leeway = 0;
            validation.set_issuer(&[&self.issuer]);
            validation.set_audience(&[&self.client_id]);
            match decode::<Claims>(raw, &decoding_key, &validation) {
                Ok(token) => return VerifyAttempt::Verified(token.claims),
                Err(error) if matches!(error.kind(), JwtErrorKind::InvalidSignature) => {
                    signature_error = Some(error.to_string());
                }
                Err(error) => return VerifyAttempt::Final(error.to_string()),
            }
        }
        VerifyAttempt::Refresh(
            signature_error.unwrap_or_else(|| "ID token signature did not verify".into()),
        )
    }

    #[cfg(test)]
    fn with_cached_jwks_for_test(issuer: &str, client_id: &str, keys: JwkSet) -> Self {
        install_tls_provider();
        Self {
            client: reqwest::Client::new(),
            issuer: issuer.into(),
            client_id: client_id.into(),
            client_secret: "secret".into(),
            redirect_url: "https://bento.example.org/callback".into(),
            authorization_endpoint: Url::parse("https://id.example.org/authorize").unwrap(),
            token_endpoint: Url::parse("https://id.example.org/token").unwrap(),
            jwks_uri: Url::parse("https://id.example.org/jwks").unwrap(),
            jwks: RwLock::new(Some(Arc::new(keys))),
        }
    }
}

fn install_tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

enum VerifyAttempt {
    Verified(Claims),
    Refresh(String),
    Final(String),
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    id_token: String,
}

#[async_trait]
impl Exchanger for ProviderClient {
    fn auth_code_url(&self, state: &str, nonce: &str) -> String {
        let mut url = self.authorization_endpoint.clone();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", &self.redirect_url)
            .append_pair("scope", "openid profile email")
            .append_pair("state", state)
            .append_pair("nonce", nonce);
        url.into()
    }

    async fn exchange(&self, code: &str) -> std::result::Result<String, BoxError> {
        // OAuth2 providers disagree on whether client credentials belong
        // in Basic auth or the form body. Probe the standards-preferred
        // header first, then the body, matching the established flow.
        let response = match self.exchange_request(code, true).await {
            Ok(response) => response,
            Err(_) => self
                .exchange_request(code, false)
                .await
                .map_err(|error| Box::new(error) as BoxError)?,
        };
        if response.id_token.is_empty() {
            return Err(Box::new(ProviderError::message(
                "token response has no id_token",
            )));
        }
        Ok(response.id_token)
    }
}

#[async_trait]
impl Verifier for ProviderClient {
    async fn verify(&self, raw_id_token: &str) -> std::result::Result<Claims, BoxError> {
        let (keys, was_cached) = self
            .cached_or_fetch_jwks()
            .await
            .map_err(|error| Box::new(error) as BoxError)?;
        match self.verify_with_jwks(raw_id_token, &keys) {
            VerifyAttempt::Verified(claims) => Ok(claims),
            VerifyAttempt::Final(error) => Err(Box::new(ProviderError::message(format!(
                "verify id token: {error}"
            )))),
            VerifyAttempt::Refresh(_error) if was_cached => {
                let fresh = self
                    .fetch_jwks()
                    .await
                    .map_err(|error| Box::new(error) as BoxError)?;
                match self.verify_with_jwks(raw_id_token, &fresh) {
                    VerifyAttempt::Verified(claims) => Ok(claims),
                    VerifyAttempt::Refresh(fresh_error) | VerifyAttempt::Final(fresh_error) => {
                        Err(Box::new(ProviderError::message(format!(
                            "verify id token: {fresh_error}"
                        ))))
                    }
                }
            }
            VerifyAttempt::Refresh(error) => Err(Box::new(ProviderError::message(format!(
                "verify id token: {error}"
            )))),
        }
    }
}

const STATE_COOKIE_NAME: &str = "bento_oauth_state";
const NONCE_COOKIE_NAME: &str = "bento_oauth_nonce";
const NEXT_COOKIE_NAME: &str = "bento_login_next";
const FLOW_COOKIE_TTL_SECONDS: i64 = 10 * 60;

fn flow_cookie(name: &str, value: &str) -> HeaderValue {
    let max_age = if value.is_empty() {
        -1
    } else {
        FLOW_COOKIE_TTL_SECONDS
    };
    host_cookie(name, value, max_age)
}

impl Service {
    /// Starts the OIDC authorization code flow: fresh state and nonce
    /// values go into short-lived cookies and the browser is redirected to
    /// the provider. An optional `?next=` query parameter names the path
    /// or same-site URL to return to after login.
    pub fn login_response(&self, uri: &Uri) -> crate::HttpResponse {
        let Some(oauth) = self.oauth.as_ref() else {
            return text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "OIDC is not configured\n",
            );
        };
        let state = random_token();
        let nonce = random_token();
        let next = self.safe_next(query_value(uri, "next").as_deref().unwrap_or(""));
        let mut response = redirect_response(&oauth.auth_code_url(&state, &nonce));
        append_cookie(&mut response, flow_cookie(STATE_COOKIE_NAME, &state));
        append_cookie(&mut response, flow_cookie(NONCE_COOKIE_NAME, &nonce));
        append_cookie(&mut response, flow_cookie(NEXT_COOKIE_NAME, &next));
        response
    }

    /// Finishes the OIDC flow: checks state, redeems the code, verifies
    /// the ID token and nonce, maps the subject to a users row, and issues
    /// the base-domain session cookie. A verified login with no matching
    /// users row gets 403: registration happens over SSH (SPEC 13).
    pub async fn callback_response(&self, headers: &HeaderMap, uri: &Uri) -> crate::HttpResponse {
        let (Some(oauth), Some(verifier)) = (self.oauth.as_ref(), self.verifier.as_ref()) else {
            return text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "OIDC is not configured\n",
            );
        };
        if let Some(error) = query_value(uri, "error").filter(|error| !error.is_empty()) {
            tracing::warn!(error = %error, "login refused by the provider");
            return text_response(
                StatusCode::FORBIDDEN,
                format!("login failed at the provider: {error}\n"),
            );
        }
        let Some(state_cookie) = cookie_value(headers, STATE_COOKIE_NAME).filter(|v| !v.is_empty())
        else {
            tracing::warn!(
                hint = "the flow cookies are SameSite=Lax and expire in 10 minutes",
                "login rejected: no state cookie on the callback"
            );
            return text_response(
                StatusCode::BAD_REQUEST,
                "missing login state; start again at /login\n",
            );
        };
        if query_value(uri, "state").as_deref() != Some(&state_cookie) {
            tracing::warn!("login rejected: state mismatch");
            return text_response(StatusCode::BAD_REQUEST, "state mismatch\n");
        }
        let code = query_value(uri, "code").unwrap_or_default();
        let raw_id_token = match oauth.exchange(&code).await {
            Ok(token) => token,
            Err(error) => {
                tracing::error!(error = %error, "login rejected: code exchange failed");
                return text_response(StatusCode::BAD_GATEWAY, "code exchange failed\n");
            }
        };
        let claims = match verifier.verify(&raw_id_token).await {
            Ok(claims) => claims,
            Err(error) => {
                tracing::error!(error = %error, "login rejected: ID token did not verify");
                return text_response(StatusCode::UNAUTHORIZED, "invalid ID token\n");
            }
        };
        let nonce_cookie = cookie_value(headers, NONCE_COOKIE_NAME);
        if nonce_cookie.as_deref().filter(|value| !value.is_empty()) != Some(&claims.nonce) {
            tracing::warn!(
                cookie_present = nonce_cookie.is_some(),
                "login rejected: nonce mismatch"
            );
            return text_response(StatusCode::BAD_REQUEST, "nonce mismatch\n");
        }
        let user = match self.users.user_by_oidc_subject(&claims.subject).await {
            Ok(Some(user)) => user,
            Ok(None) => {
                // The operator has to copy this subject into the users row
                // by hand (SPEC 13), and the provider is the only other
                // place it can be read from, so name it here.
                tracing::warn!(
                    subject = %claims.subject,
                    email = %claims.email,
                    hint = "set oidc_subject on the user's row to this value",
                    "login rejected: no users row carries this OIDC subject"
                );
                return text_response(
                    StatusCode::FORBIDDEN,
                    "no Bento account for this login; register by connecting with ssh\n",
                );
            }
            Err(error) => {
                tracing::error!(error = %error, "login rejected: user lookup failed");
                return text_response(StatusCode::INTERNAL_SERVER_ERROR, "user lookup failed\n");
            }
        };
        tracing::info!(user = %user.name, subject = %claims.subject, "dashboard login");
        let session = match self.new_session(user.id).await {
            Ok(session) => session,
            Err(_) => {
                return text_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "session creation failed\n",
                );
            }
        };
        let next = cookie_value(headers, NEXT_COOKIE_NAME)
            .map_or_else(|| "/".to_string(), |next| self.safe_next(&next));
        let mut response = redirect_response(&next);
        append_cookie(&mut response, flow_cookie(STATE_COOKIE_NAME, ""));
        append_cookie(&mut response, flow_cookie(NONCE_COOKIE_NAME, ""));
        append_cookie(&mut response, flow_cookie(NEXT_COOKIE_NAME, ""));
        append_cookie(&mut response, self.session_cookie(&session));
        response
    }

    /// Returns `next` when it is a same-site relative path or an absolute
    /// HTTPS URL on the base domain or one of its subdomains, otherwise
    /// `/`. The subdomain form is what the HTTP proxy sends when a private
    /// instance redirects to login (SPEC 9.2, 13): the session cookie is
    /// valid for every subdomain, so the flow can return there. Everything
    /// else stops open redirects through the login flow.
    fn safe_next(&self, next: &str) -> String {
        if next.is_empty() {
            return "/".into();
        }
        if next.starts_with('/') {
            return if next.starts_with("//") {
                "/".into()
            } else {
                next.into()
            };
        }
        let Ok(url) = Url::parse(next) else {
            return "/".into();
        };
        if url.scheme() != "https" {
            return "/".into();
        }
        let Some(host) = url
            .host_str()
            .map(|host| host.trim_end_matches('.').to_ascii_lowercase())
        else {
            return "/".into();
        };
        if host == self.base_domain || host.ends_with(&format!(".{}", self.base_domain)) {
            next.into()
        } else {
            "/".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use http::header::{COOKIE, LOCATION, SET_COOKIE};
    use jsonwebtoken::jwk::JwkSet;
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use serde::Serialize;
    use std::collections::HashMap;
    use time::OffsetDateTime;

    use super::*;
    use crate::SESSION_COOKIE_NAME;
    use crate::test_support::{FakeClock, TEST_EPOCH, TestOidc, new_oidc_service};

    fn response_cookies(response: &crate::HttpResponse) -> Vec<String> {
        response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap().to_string())
            .collect()
    }

    fn cookie(cookies: &[String], name: &str) -> Option<String> {
        cookies.iter().find_map(|cookie| {
            let (candidate, rest) = cookie.split_once('=')?;
            (candidate == name).then(|| rest.split(';').next().unwrap_or_default().to_string())
        })
    }

    fn callback_headers(cookies: &[String], drop: Option<&str>) -> HeaderMap {
        let values = cookies
            .iter()
            .filter_map(|cookie| cookie.split(';').next())
            .filter(|cookie| !drop.is_some_and(|name| cookie.starts_with(&format!("{name}="))))
            .collect::<Vec<_>>()
            .join("; ");
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, HeaderValue::from_str(&values).unwrap());
        headers
    }

    #[tokio::test]
    async fn login_redirects_to_provider() {
        let TestOidc {
            service, exchanger, ..
        } = new_oidc_service();
        let response = service.login_response(&"/login".parse().unwrap());
        assert_eq!(response.status(), StatusCode::FOUND);
        assert!(
            response.headers()[LOCATION]
                .to_str()
                .unwrap()
                .starts_with("https://id.example.org/authorize")
        );
        let cookies = response_cookies(&response);
        let state = cookie(&cookies, STATE_COOKIE_NAME).unwrap();
        let nonce = cookie(&cookies, NONCE_COOKIE_NAME).unwrap();
        let seen = exchanger.seen();
        assert_eq!(state, seen.0);
        assert_eq!(nonce, seen.1);
        for name in [STATE_COOKIE_NAME, NONCE_COOKIE_NAME] {
            let value = cookies
                .iter()
                .find(|cookie| cookie.starts_with(name))
                .unwrap();
            assert!(value.contains("HttpOnly"));
            assert!(value.contains("Secure"));
            assert!(value.contains("SameSite=Lax"));
            assert!(value.contains("Max-Age=600"));
            assert!(!value.contains("Domain="));
        }
    }

    #[tokio::test]
    async fn callback_happy_path() {
        let TestOidc {
            service,
            exchanger,
            verifier,
            ..
        } = new_oidc_service();
        let login = service.login_response(&"/login?next=/instances/web".parse().unwrap());
        let cookies = response_cookies(&login);
        let (state, nonce) = exchanger.seen();
        verifier.allow(
            "raw-token",
            Claims {
                subject: "subject-1".into(),
                email: "shaun@example.org".into(),
                nonce,
            },
        );
        let uri: Uri = format!("/callback?code=good-code&state={state}")
            .parse()
            .unwrap();
        let response = service
            .callback_response(&callback_headers(&cookies, None), &uri)
            .await;
        assert_eq!(response.status(), StatusCode::FOUND, "{}", response.body());
        assert_eq!(response.headers()[LOCATION], "/instances/web");
        let response_cookies = response_cookies(&response);
        let session_id = cookie(&response_cookies, SESSION_COOKIE_NAME).unwrap();
        let session_cookie = response_cookies
            .iter()
            .find(|cookie| cookie.starts_with(SESSION_COOKIE_NAME))
            .unwrap();
        assert!(session_cookie.contains("Domain=bento.example.org"));
        assert!(session_cookie.contains("HttpOnly"));
        assert!(session_cookie.contains("Secure"));
        assert_eq!(service.sessions.get(&session_id).await.unwrap().user_id, 42);
        for name in [STATE_COOKIE_NAME, NONCE_COOKIE_NAME, NEXT_COOKIE_NAME] {
            let cleared = response_cookies
                .iter()
                .find(|cookie| cookie.starts_with(name))
                .unwrap();
            assert!(cleared.contains("Max-Age=-1"));
        }
    }

    #[tokio::test]
    async fn callback_rejections() {
        enum Break {
            State,
            MissingState,
            Provider,
            Code,
            Token,
            Nonce,
            Subject,
        }
        for (case, expected) in [
            (Break::State, StatusCode::BAD_REQUEST),
            (Break::MissingState, StatusCode::BAD_REQUEST),
            (Break::Provider, StatusCode::FORBIDDEN),
            (Break::Code, StatusCode::BAD_GATEWAY),
            (Break::Token, StatusCode::UNAUTHORIZED),
            (Break::Nonce, StatusCode::BAD_REQUEST),
            (Break::Subject, StatusCode::FORBIDDEN),
        ] {
            let TestOidc {
                service,
                exchanger,
                verifier,
                ..
            } = new_oidc_service();
            let login = service.login_response(&"/login".parse().unwrap());
            let cookies = response_cookies(&login);
            let (state, nonce) = exchanger.seen();
            verifier.allow(
                "raw-token",
                Claims {
                    subject: "subject-1".into(),
                    email: String::new(),
                    nonce,
                },
            );
            let (uri, drop_state): (String, bool) = match case {
                Break::State => ("/callback?code=good-code&state=forged".into(), false),
                Break::MissingState => (format!("/callback?code=good-code&state={state}"), true),
                Break::Provider => (
                    format!("/callback?error=access_denied&state={state}"),
                    false,
                ),
                Break::Code => (format!("/callback?code=wrong&state={state}"), false),
                Break::Token => {
                    verifier.clear();
                    (format!("/callback?code=good-code&state={state}"), false)
                }
                Break::Nonce => {
                    verifier.allow(
                        "raw-token",
                        Claims {
                            subject: "subject-1".into(),
                            email: String::new(),
                            nonce: "replayed".into(),
                        },
                    );
                    (format!("/callback?code=good-code&state={state}"), false)
                }
                Break::Subject => {
                    verifier.allow(
                        "raw-token",
                        Claims {
                            subject: "stranger".into(),
                            email: String::new(),
                            nonce: exchanger.seen().1,
                        },
                    );
                    (format!("/callback?code=good-code&state={state}"), false)
                }
            };
            let headers = callback_headers(&cookies, drop_state.then_some(STATE_COOKIE_NAME));
            let response = service
                .callback_response(&headers, &uri.parse().unwrap())
                .await;
            assert_eq!(response.status(), expected, "body {:?}", response.body());
            assert!(cookie(&response_cookies(&response), SESSION_COOKIE_NAME).is_none());
        }
    }

    #[test]
    fn safe_next() {
        let TestOidc { service, .. } = new_oidc_service();
        for (input, expected) in [
            ("", "/"),
            ("/", "/"),
            ("/instances/web", "/instances/web"),
            (
                "https://web.bento.example.org/admin?x=1",
                "https://web.bento.example.org/admin?x=1",
            ),
            (
                "https://web.bento.example.org:3456/",
                "https://web.bento.example.org:3456/",
            ),
            (
                "https://bento.example.org/instances",
                "https://bento.example.org/instances",
            ),
            ("https://evil.example.com/", "/"),
            ("https://evilbento.example.org/", "/"),
            ("http://web.bento.example.org/", "/"),
            ("//evil.example.com/", "/"),
            ("javascript:alert(1)", "/"),
            ("relative/path", "/"),
        ] {
            assert_eq!(service.safe_next(input), expected);
        }
    }

    #[tokio::test]
    async fn logout() {
        let TestOidc { service, .. } = new_oidc_service();
        let session = service.new_session(42).await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!("{SESSION_COOKIE_NAME}={}", session.id)).unwrap(),
        );
        let response = service.logout_response(&headers).await;
        assert_eq!(response.status(), StatusCode::FOUND);
        assert!(service.sessions.get(&session.id).await.is_none());
        assert!(
            response_cookies(&response)
                .iter()
                .any(|cookie| cookie.contains("Max-Age=-1"))
        );
    }

    #[test]
    fn responses_without_oidc_configured() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, _, _) = crate::test_support::new_test_service(&clock);
        assert_eq!(
            service.login_response(&"/".parse().unwrap()).status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn callback_without_oidc_configured() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, _, _) = crate::test_support::new_test_service(&clock);
        assert_eq!(
            service
                .callback_response(&HeaderMap::new(), &"/".parse().unwrap())
                .await
                .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn provider_authorization_url_has_the_oidc_parameters() {
        let provider = ProviderClient::with_cached_jwks_for_test(
            "https://id.example.org",
            "client-1",
            JwkSet::default(),
        );
        let url = Url::parse(&provider.auth_code_url("state-1", "nonce-1")).unwrap();
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["client_id"], "client-1");
        assert_eq!(query["scope"], "openid profile email");
        assert_eq!(query["state"], "state-1");
        assert_eq!(query["nonce"], "nonce-1");
    }

    #[derive(Serialize)]
    struct SignedClaims<'a> {
        iss: &'a str,
        aud: &'a str,
        exp: i64,
        sub: &'a str,
        nonce: &'a str,
    }

    #[tokio::test]
    async fn provider_verifies_signature_issuer_audience_and_expiry_without_email() {
        let secret = b"a sufficiently long test-only HMAC key";
        let encoded_secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
        let keys: JwkSet = serde_json::from_value(serde_json::json!({
            "keys": [{"kty": "oct", "alg": "HS256", "kid": "key-1", "k": encoded_secret}]
        }))
        .unwrap();
        let provider =
            ProviderClient::with_cached_jwks_for_test("https://id.example.org", "client-1", keys);
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("key-1".into());
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let raw = encode(
            &header,
            &SignedClaims {
                iss: "https://id.example.org",
                aud: "client-1",
                exp: now + 300,
                sub: "subject-1",
                nonce: "nonce-1",
            },
            &EncodingKey::from_secret(secret),
        )
        .unwrap();
        assert_eq!(
            provider.verify(&raw).await.unwrap(),
            Claims {
                subject: "subject-1".into(),
                email: String::new(),
                nonce: "nonce-1".into()
            }
        );

        for (issuer, audience, expiry) in [
            ("https://wrong.example.org", "client-1", now + 300),
            ("https://id.example.org", "wrong-client", now + 300),
            ("https://id.example.org", "client-1", now - 1),
        ] {
            let raw = encode(
                &header,
                &SignedClaims {
                    iss: issuer,
                    aud: audience,
                    exp: expiry,
                    sub: "subject-1",
                    nonce: "nonce-1",
                },
                &EncodingKey::from_secret(secret),
            )
            .unwrap();
            assert!(provider.verify(&raw).await.is_err());
        }
    }
}
