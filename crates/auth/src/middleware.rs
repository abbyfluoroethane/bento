use http::StatusCode;
use http::header::{HeaderValue, WWW_AUTHENTICATE};
use http::request::Parts;

use crate::{HttpResponse, Service, bearer_token, redirect_response, text_response};

/// The authenticated Bento user ID stored in request extensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserId(pub i64);

/// Returns the authenticated user ID placed in request extensions, if
/// any.
pub fn user_id_from_parts(parts: &Parts) -> Option<i64> {
    parts.extensions.get::<UserId>().map(|user| user.0)
}

fn unauthorized() -> Box<HttpResponse> {
    let mut response = text_response(StatusCode::UNAUTHORIZED, "unauthorized\n");
    response.headers_mut().insert(
        WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"bento\""),
    );
    Box::new(response)
}

impl Service {
    /// Requires a live browser session. On success the user ID is placed
    /// in request extensions. A request without one is redirected to the
    /// login page with the original path in `?next=`.
    pub async fn require_session(
        &self,
        parts: &mut Parts,
    ) -> std::result::Result<(), Box<HttpResponse>> {
        let Some(session) = self.session_from_headers(&parts.headers).await else {
            let request_uri = parts
                .uri
                .path_and_query()
                .map_or("/", http::uri::PathAndQuery::as_str);
            let encoded: String =
                url::form_urlencoded::byte_serialize(request_uri.as_bytes()).collect();
            let target = format!("{}?next={encoded}", self.login_path);
            return Err(Box::new(redirect_response(&target)));
        };
        parts.extensions.insert(UserId(session.user_id));
        Ok(())
    }

    /// Requires a valid bearer token. On success the token owner's user
    /// ID is placed in request extensions. Failure returns 401 with a
    /// `WWW-Authenticate` header.
    pub async fn require_token(
        &self,
        parts: &mut Parts,
    ) -> std::result::Result<(), Box<HttpResponse>> {
        let Some(plaintext) = bearer_token(&parts.headers) else {
            return Err(unauthorized());
        };
        let token = self
            .authenticate_token(plaintext)
            .await
            .map_err(|_| unauthorized())?;
        parts.extensions.insert(UserId(token.user_id));
        Ok(())
    }

    /// Accepts either credential: a bearer token first, then the session
    /// cookie. Browser requests without either are redirected to login;
    /// requests carrying a bad bearer token get 401.
    pub async fn require_session_or_token(
        &self,
        parts: &mut Parts,
    ) -> std::result::Result<(), Box<HttpResponse>> {
        if bearer_token(&parts.headers).is_some() {
            self.require_token(parts).await
        } else {
            self.require_session(parts).await
        }
    }
}

#[cfg(test)]
mod tests {
    use http::Request;
    use http::header::{AUTHORIZATION, COOKIE, LOCATION};
    use time::Duration;

    use super::*;
    use crate::SESSION_COOKIE_NAME;
    use crate::test_support::{FakeClock, TEST_EPOCH, new_test_service};

    fn parts(uri: &str) -> Parts {
        Request::builder().uri(uri).body(()).unwrap().into_parts().0
    }

    #[tokio::test]
    async fn require_session() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, _, _) = new_test_service(&clock);
        let service = service.with_session_ttl(Duration::HOUR);
        let session = service.new_session(7).await.unwrap();

        let mut request = parts("/instances/web?tab=ports");
        let response = service.require_session(&mut request).await.unwrap_err();
        assert_eq!(response.status(), StatusCode::FOUND);
        let location = response.headers()[LOCATION].to_str().unwrap();
        assert!(location.starts_with("/login?next="));
        assert!(location.contains("%2Finstances%2Fweb"));

        let mut request = parts("/");
        request.headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!("{SESSION_COOKIE_NAME}={}", session.id)).unwrap(),
        );
        service.require_session(&mut request).await.unwrap();
        assert_eq!(user_id_from_parts(&request), Some(7));

        clock.advance(Duration::hours(2));
        let response = service.require_session(&mut request).await.unwrap_err();
        assert_eq!(response.status(), StatusCode::FOUND);
    }

    #[tokio::test]
    async fn require_token() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, _, _) = new_test_service(&clock);
        let (plaintext, _) = service.mint_token(3, Duration::HOUR).await.unwrap();

        let mut request = parts("/api/instances");
        let response = service.require_token(&mut request).await.unwrap_err();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().contains_key(WWW_AUTHENTICATE));

        request.headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {plaintext}")).unwrap(),
        );
        service.require_token(&mut request).await.unwrap();
        assert_eq!(user_id_from_parts(&request), Some(3));

        clock.advance(Duration::hours(2));
        let response = service.require_token(&mut request).await.unwrap_err();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn require_session_or_token() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, _, _) = new_test_service(&clock);
        let service = service.with_session_ttl(Duration::HOUR);
        let session = service.new_session(7).await.unwrap();
        let (plaintext, _) = service.mint_token(3, Duration::HOUR).await.unwrap();

        let mut request = parts("/");
        request.headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!("{SESSION_COOKIE_NAME}={}", session.id)).unwrap(),
        );
        request.headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {plaintext}")).unwrap(),
        );
        service
            .require_session_or_token(&mut request)
            .await
            .unwrap();
        assert_eq!(user_id_from_parts(&request), Some(3));

        request.headers.remove(AUTHORIZATION);
        service
            .require_session_or_token(&mut request)
            .await
            .unwrap();
        assert_eq!(user_id_from_parts(&request), Some(7));

        request.headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer bento_forged"),
        );
        let response = service
            .require_session_or_token(&mut request)
            .await
            .unwrap_err();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        request.headers.clear();
        let response = service
            .require_session_or_token(&mut request)
            .await
            .unwrap_err();
        assert_eq!(response.status(), StatusCode::FOUND);
    }

    #[test]
    fn user_id_from_request_parts() {
        let mut request = parts("/");
        assert_eq!(user_id_from_parts(&request), None);
        request.extensions.insert(UserId(9));
        assert_eq!(user_id_from_parts(&request), Some(9));
    }
}
