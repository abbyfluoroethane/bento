use http::HeaderMap;

use crate::{Error, Result, SESSION_COOKIE_NAME, Service, cookie_value, store_error};

impl Service {
    /// Resolves a session ID and checks access to one instance, identified
    /// by UUID. This is the check the HTTP proxy runs on every request for
    /// a private instance (SPEC 9.2, 13).
    ///
    /// Returns the user ID on success, [`Error::Unauthenticated`] when the
    /// session is missing, unknown, or expired, and [`Error::Forbidden`]
    /// when the session is valid but the user neither owns the instance
    /// nor holds a share on its UUID. Because the check keys on the UUID,
    /// a session held from before a name changed hands grants nothing on
    /// the new instance.
    pub async fn authorize(&self, session_id: &str, instance_uuid: &str) -> Result<i64> {
        let session = self
            .session(session_id)
            .await
            .ok_or(Error::Unauthenticated)?;
        self.authorize_user_id(session.user_id, instance_uuid).await
    }

    /// [`Service::authorize`] with the session ID read from the request's
    /// session cookie.
    pub async fn authorize_request(&self, headers: &HeaderMap, instance_uuid: &str) -> Result<i64> {
        let session_id =
            cookie_value(headers, SESSION_COOKIE_NAME).ok_or(Error::Unauthenticated)?;
        self.authorize(&session_id, instance_uuid).await
    }

    /// Checks whether an already-authenticated user, such as the holder of
    /// an API token, may act on the instance.
    pub async fn authorize_user(&self, user_id: i64, instance_uuid: &str) -> Result<()> {
        self.authorize_user_id(user_id, instance_uuid)
            .await
            .map(|_| ())
    }

    async fn authorize_user_id(&self, user_id: i64, instance_uuid: &str) -> Result<i64> {
        let allowed = self
            .access
            .has_access(instance_uuid, user_id)
            .await
            .map_err(|error| {
                store_error(format!("access check for instance {instance_uuid}"), error)
            })?;
        if !allowed {
            return Err(Error::Forbidden);
        }
        Ok(user_id)
    }
}

#[cfg(test)]
mod tests {
    use http::header::{COOKIE, HeaderValue};

    use super::*;
    use crate::test_support::{FakeClock, TEST_EPOCH, new_test_service};

    #[tokio::test]
    async fn stale_session_after_name_changes_hands() {
        // Authorization keys on the instance UUID, so a session held from
        // before a name changed hands grants nothing on the new instance
        // with that name (SPEC 13).
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, access, _) = new_test_service(&clock);
        const ALICE: i64 = 1;
        const BOB: i64 = 2;
        const CAROL: i64 = 3;
        const OLD_UUID: &str = "uuid-old";
        const NEW_UUID: &str = "uuid-new";

        access.grant(OLD_UUID, ALICE);
        access.grant(OLD_UUID, BOB);
        let bob_session = service.new_session(BOB).await.unwrap();
        assert_eq!(
            service.authorize(&bob_session.id, OLD_UUID).await.unwrap(),
            BOB
        );

        // Alice deletes "web". The name cools down and Carol creates a
        // new instance with the same name but a new UUID. Shares key on
        // the UUID and die with the old instance (SPEC 12).
        access.revoke(OLD_UUID, ALICE);
        access.revoke(OLD_UUID, BOB);
        access.grant(NEW_UUID, CAROL);

        assert!(matches!(
            service.authorize(&bob_session.id, NEW_UUID).await,
            Err(Error::Forbidden)
        ));
        assert!(matches!(
            service.authorize(&bob_session.id, OLD_UUID).await,
            Err(Error::Forbidden)
        ));
        let carol_session = service.new_session(CAROL).await.unwrap();
        assert_eq!(
            service
                .authorize(&carol_session.id, NEW_UUID)
                .await
                .unwrap(),
            CAROL
        );
    }

    #[tokio::test]
    async fn authorize() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, access, _) = new_test_service(&clock);
        access.grant("uuid-1", 7);
        let session = service.new_session(7).await.unwrap();
        for (session_id, uuid, expected) in [
            (session.id.as_str(), "uuid-1", Ok(7)),
            (session.id.as_str(), "uuid-2", Err(Error::Forbidden)),
            ("no-such-session", "uuid-1", Err(Error::Unauthenticated)),
            ("", "uuid-1", Err(Error::Unauthenticated)),
        ] {
            let actual = service.authorize(session_id, uuid).await;
            match (actual, expected) {
                (Ok(actual), Ok(expected)) => assert_eq!(actual, expected),
                (Err(actual), Err(expected)) => {
                    assert_eq!(actual.to_string(), expected.to_string());
                }
                (actual, expected) => panic!("got {actual:?}, want {expected:?}"),
            }
        }
    }

    #[tokio::test]
    async fn authorize_store_error() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, access, _) = new_test_service(&clock);
        access.fail();
        let session = service.new_session(7).await.unwrap();
        let error = service.authorize(&session.id, "uuid-1").await.unwrap_err();
        assert!(matches!(error, Error::Store { .. }));
    }

    #[tokio::test]
    async fn authorize_request() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, access, _) = new_test_service(&clock);
        access.grant("uuid-1", 7);
        let session = service.new_session(7).await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            HeaderValue::from_str(&format!("{SESSION_COOKIE_NAME}={}", session.id)).unwrap(),
        );
        assert_eq!(
            service.authorize_request(&headers, "uuid-1").await.unwrap(),
            7
        );
        assert!(matches!(
            service.authorize_request(&HeaderMap::new(), "uuid-1").await,
            Err(Error::Unauthenticated)
        ));
    }

    #[tokio::test]
    async fn authorize_user() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, access, _) = new_test_service(&clock);
        access.grant("uuid-1", 7);
        service.authorize_user(7, "uuid-1").await.unwrap();
        assert!(matches!(
            service.authorize_user(8, "uuid-1").await,
            Err(Error::Forbidden)
        ));
    }
}
