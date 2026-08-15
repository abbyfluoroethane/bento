use async_trait::async_trait;
use http::HeaderMap;
use http::header::HeaderValue;
use std::collections::HashMap;
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};
use tokio::sync::RwLock;

use crate::{BoxError, Service, cookie_value, encode_cookie_value, random_token, store_error};

/// The name of the base-domain session cookie.
pub const SESSION_COOKIE_NAME: &str = "bento_session";

/// One server-side login session. The cookie carries only the opaque ID;
/// everything else lives on the server (SPEC 13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub user_id: i64,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

/// Holds sessions server side. The default implementation is in memory;
/// a restart logs everyone out, which is acceptable for a dashboard
/// session.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Stores or replaces a session.
    async fn put(&self, session: Session) -> std::result::Result<(), BoxError>;
    /// Returns the session with the given ID.
    async fn get(&self, id: &str) -> Option<Session>;
    /// Removes the session with the given ID. Deleting a missing session
    /// is a no-op.
    async fn delete(&self, id: &str);
}

/// A lock-guarded in-memory [`SessionStore`].
#[derive(Debug, Default)]
pub struct MemorySessionStore {
    sessions: RwLock<HashMap<String, Session>>,
}

impl MemorySessionStore {
    /// Returns an empty in-memory session store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Removes every session that expired at or before `now`. Callers may
    /// run it periodically; expiry is also enforced on every lookup, so
    /// the sweep only reclaims memory.
    pub async fn delete_expired(&self, now: OffsetDateTime) {
        self.sessions
            .write()
            .await
            .retain(|_, session| session.expires_at > now);
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    async fn put(&self, session: Session) -> std::result::Result<(), BoxError> {
        self.sessions
            .write()
            .await
            .insert(session.id.clone(), session);
        Ok(())
    }

    async fn get(&self, id: &str) -> Option<Session> {
        self.sessions.read().await.get(id).cloned()
    }

    async fn delete(&self, id: &str) {
        self.sessions.write().await.remove(id);
    }
}

impl Service {
    /// Creates and stores a new session for `user_id`.
    pub(crate) async fn new_session(&self, user_id: i64) -> crate::Result<Session> {
        let now = (self.now)();
        let session = Session {
            id: random_token(),
            user_id,
            created_at: now,
            expires_at: now + self.session_ttl,
        };
        self.sessions
            .put(session.clone())
            .await
            .map_err(|error| store_error("store session", error))?;
        Ok(session)
    }

    /// Resolves a session ID to a live session, enforcing expiry.
    pub(crate) async fn session(&self, id: &str) -> Option<Session> {
        if id.is_empty() {
            return None;
        }
        let session = self.sessions.get(id).await?;
        if session.expires_at <= (self.now)() {
            self.sessions.delete(id).await;
            return None;
        }
        Some(session)
    }

    /// Returns the live session identified by the request's session
    /// cookie, if any.
    pub async fn session_from_headers(&self, headers: &HeaderMap) -> Option<Session> {
        let id = cookie_value(headers, SESSION_COOKIE_NAME)?;
        self.session(&id).await
    }

    /// Deletes the server-side session, expires the browser cookie, and
    /// returns a redirect to `/`.
    pub async fn logout_response(&self, headers: &HeaderMap) -> crate::HttpResponse {
        if let Some(id) = cookie_value(headers, SESSION_COOKIE_NAME).filter(|id| !id.is_empty()) {
            self.sessions.delete(&id).await;
        }
        let mut response = crate::redirect_response("/");
        crate::append_cookie(&mut response, self.clear_session_cookie());
        response
    }

    pub(crate) fn session_cookie(&self, session: &Session) -> HeaderValue {
        // Domain is set to the base domain so the cookie is sent to every
        // subdomain; HttpOnly, Secure, and SameSite=Lax per SPEC 13.
        cookie_header(
            SESSION_COOKIE_NAME,
            &session.id,
            Some(&self.base_domain),
            Some(session.expires_at),
            None,
        )
    }

    pub(crate) fn clear_session_cookie(&self) -> HeaderValue {
        cookie_header(
            SESSION_COOKIE_NAME,
            "",
            Some(&self.base_domain),
            None,
            Some(-1),
        )
    }
}

pub(crate) fn host_cookie(name: &str, value: &str, max_age: i64) -> HeaderValue {
    cookie_header(name, value, None, None, Some(max_age))
}

fn cookie_header(
    name: &str,
    value: &str,
    domain: Option<&str>,
    expires: Option<OffsetDateTime>,
    max_age: Option<i64>,
) -> HeaderValue {
    let mut cookie = format!("{name}={}; Path=/", encode_cookie_value(value));
    if let Some(domain) = domain {
        cookie.push_str("; Domain=");
        cookie.push_str(domain);
    }
    if let Some(expires) = expires {
        const FORMAT: &[time::format_description::FormatItem<'_>] = format_description!(
            "[weekday repr:short], [day padding:zero] [month repr:short] [year] [hour]:[minute]:[second] GMT"
        );
        let expires = expires
            .to_offset(UtcOffset::UTC)
            .format(FORMAT)
            .expect("the fixed cookie date format accepts every OffsetDateTime");
        cookie.push_str("; Expires=");
        cookie.push_str(&expires);
    }
    if let Some(max_age) = max_age {
        cookie.push_str("; Max-Age=");
        cookie.push_str(&max_age.to_string());
    }
    cookie.push_str("; HttpOnly; Secure; SameSite=Lax");
    HeaderValue::from_str(&cookie).expect("cookie components are validated or encoded")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use time::Duration;

    use super::*;
    use crate::Error;
    use crate::test_support::{FakeClock, TEST_EPOCH, new_test_service};

    #[tokio::test]
    async fn memory_session_store() {
        let store = MemorySessionStore::new();
        let session = Session {
            id: "abc".into(),
            user_id: 7,
            created_at: TEST_EPOCH,
            expires_at: TEST_EPOCH + Duration::HOUR,
        };
        store.put(session.clone()).await.unwrap();
        assert_eq!(store.get("abc").await, Some(session));
        assert_eq!(store.get("missing").await, None);
        store.delete("abc").await;
        assert_eq!(store.get("abc").await, None);
        store.delete("abc").await;
    }

    #[tokio::test]
    async fn memory_session_store_delete_expired() {
        let store = MemorySessionStore::new();
        for (id, expires_at) in [
            ("live", TEST_EPOCH + Duration::HOUR),
            ("dead", TEST_EPOCH - Duration::HOUR),
            ("edge", TEST_EPOCH),
        ] {
            store
                .put(Session {
                    id: id.into(),
                    user_id: 0,
                    created_at: TEST_EPOCH,
                    expires_at,
                })
                .await
                .unwrap();
        }
        store.delete_expired(TEST_EPOCH).await;
        assert!(store.get("live").await.is_some());
        assert!(store.get("dead").await.is_none());
        assert!(store.get("edge").await.is_none());
    }

    #[tokio::test]
    async fn session_expiry() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, access, _) = new_test_service(&clock);
        access.grant("uuid-1", 7);
        let service = service.with_session_ttl(Duration::HOUR);

        let session = service.new_session(7).await.unwrap();
        assert_eq!(session.expires_at, TEST_EPOCH + Duration::HOUR);
        assert_eq!(service.authorize(&session.id, "uuid-1").await.unwrap(), 7);
        clock.advance(Duration::HOUR + Duration::SECOND);
        assert!(matches!(
            service.authorize(&session.id, "uuid-1").await,
            Err(Error::Unauthenticated)
        ));
        assert!(service.sessions.get(&session.id).await.is_none());
    }

    #[tokio::test]
    async fn session_ids_are_opaque_and_unique() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, _, _) = new_test_service(&clock);
        let mut seen = HashSet::new();
        for _ in 0..100 {
            let session = service.new_session(1).await.unwrap();
            assert!(session.id.len() >= 40);
            assert!(seen.insert(session.id));
        }
    }

    #[tokio::test]
    async fn session_cookie_attributes() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, _, _) = new_test_service(&clock);
        let session = Session {
            id: "sid".into(),
            user_id: 1,
            created_at: TEST_EPOCH,
            expires_at: TEST_EPOCH + Duration::HOUR,
        };
        let cookie_header = service.session_cookie(&session);
        let cookie = cookie_header.to_str().unwrap();
        assert!(cookie.starts_with("bento_session=sid; Path=/; Domain=bento.example.org"));
        assert!(cookie.contains("Expires=Mon, 10 Aug 2026 13:00:00 GMT"));
        assert!(cookie.contains("; HttpOnly"));
        assert!(cookie.contains("; Secure"));
        assert!(cookie.contains("; SameSite=Lax"));

        let clear_header = service.clear_session_cookie();
        let clear = clear_header.to_str().unwrap();
        assert!(clear.contains("Max-Age=-1"));
        assert!(clear.contains("Domain=bento.example.org"));

        let memory: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
        let _ = service.with_session_store(memory);
    }
}
