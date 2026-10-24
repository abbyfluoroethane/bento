use bento_types::Token;
use http::HeaderMap;
use http::header::AUTHORIZATION;
use sha2::{Digest, Sha256};
use time::Duration;

use crate::{Error, Result, Service, random_token, store_error};

/// Every Bento API token starts with this prefix. It makes a leaked token
/// recognizable in logs and secret scanners.
pub const TOKEN_PREFIX: &str = "bento_";

/// The result of a token-store lookup. The store returns the row whatever
/// its expiry; the auth service enforces expiry against its own clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenLookup {
    Found(Token),
    NotFound,
}

/// Returns the hex SHA-256 of a plaintext token. Only this hash is stored
/// (SPEC 13).
pub fn hash_token(plaintext: &str) -> String {
    hex::encode(Sha256::digest(plaintext.as_bytes()))
}

/// Extracts the token from an `Authorization: Bearer` header. Returns
/// `None` when the header is absent or not a bearer credential.
pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let header = headers.get(AUTHORIZATION)?.to_str().ok()?;
    const SCHEME: &str = "bearer ";
    if header.len() <= SCHEME.len() || !header[..SCHEME.len()].eq_ignore_ascii_case(SCHEME) {
        return None;
    }
    let token = header[SCHEME.len()..].trim();
    (!token.is_empty()).then_some(token)
}

impl Service {
    /// Creates an API token for the user and returns the plaintext exactly
    /// once. Only the hash reaches the store; the plaintext cannot be
    /// recovered later. A TTL of zero or less mints a token that does not
    /// expire.
    pub async fn mint_token(&self, user_id: i64, ttl: Duration) -> Result<(String, Token)> {
        let plaintext = format!("{TOKEN_PREFIX}{}", random_token());
        let expires_at = ttl.is_positive().then(|| (self.now)() + ttl);
        let token = self
            .tokens
            .create_token(user_id, &hash_token(&plaintext), expires_at)
            .await
            .map_err(|error| store_error("store token", error))?;
        Ok((plaintext, token))
    }

    /// Checks a plaintext bearer token and returns its row. Returns
    /// [`Error::Unauthenticated`] for an unknown token and
    /// [`Error::TokenExpired`] for a known token past its expiry. A token
    /// with no expiry stays valid.
    pub async fn authenticate_token(&self, plaintext: &str) -> Result<Token> {
        if plaintext.is_empty() {
            return Err(Error::Unauthenticated);
        }
        let lookup = self
            .tokens
            .token_by_hash(&hash_token(plaintext))
            .await
            .map_err(|error| store_error("token lookup", error))?;
        let token = match lookup {
            TokenLookup::Found(token) => token,
            TokenLookup::NotFound => return Err(Error::Unauthenticated),
        };
        if token.expires_at.is_some_and(|at| at <= (self.now)()) {
            return Err(Error::TokenExpired);
        }
        Ok(token)
    }

    /// Deletes a token row. The plaintext stops working at once.
    pub async fn revoke_token(&self, id: i64) -> Result<()> {
        self.tokens
            .delete_token(id)
            .await
            .map_err(|error| store_error("delete token", error))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use http::header::HeaderValue;

    use super::*;
    use crate::test_support::{FakeClock, TEST_EPOCH, new_test_service};

    #[tokio::test]
    async fn mint_token_stores_only_the_hash() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, _, tokens) = new_test_service(&clock);
        let (plaintext, token) = service.mint_token(7, Duration::HOUR).await.unwrap();
        assert!(plaintext.starts_with(TOKEN_PREFIX));
        assert_eq!(token.user_id, 7);
        assert_eq!(token.expires_at, Some(TEST_EPOCH + Duration::HOUR));
        let created = tokens.created();
        assert_eq!(created.len(), 1);
        assert_ne!(created[0].hash, plaintext);
        assert!(!created[0].hash.contains(&plaintext));
        assert_eq!(created[0].hash, hash_token(&plaintext));
    }

    #[tokio::test]
    async fn mint_token_plaintexts_are_unique() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, _, _) = new_test_service(&clock);
        let mut seen = HashSet::new();
        for _ in 0..50 {
            let (plaintext, _) = service.mint_token(1, Duration::ZERO).await.unwrap();
            assert!(seen.insert(plaintext));
        }
    }

    #[tokio::test]
    async fn authenticate_token() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, _, _) = new_test_service(&clock);
        let (expiring, _) = service.mint_token(7, Duration::HOUR).await.unwrap();
        let (forever, _) = service.mint_token(8, Duration::ZERO).await.unwrap();

        assert_eq!(
            service.authenticate_token(&expiring).await.unwrap().user_id,
            7
        );
        assert!(matches!(
            service.authenticate_token("bento_forged").await,
            Err(Error::Unauthenticated)
        ));
        assert!(matches!(
            service.authenticate_token("").await,
            Err(Error::Unauthenticated)
        ));

        clock.advance(Duration::HOUR + Duration::SECOND);
        assert!(matches!(
            service.authenticate_token(&expiring).await,
            Err(Error::TokenExpired)
        ));
        clock.advance(Duration::hours(1000));
        assert_eq!(
            service.authenticate_token(&forever).await.unwrap().user_id,
            8
        );
    }

    #[tokio::test]
    async fn a_token_without_expiry_has_none_and_stays_valid() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, _, _) = new_test_service(&clock);
        let (plaintext, token) = service.mint_token(7, Duration::ZERO).await.unwrap();
        assert_eq!(token.expires_at, None);
        clock.advance(Duration::hours(100_000));
        assert_eq!(service.authenticate_token(&plaintext).await.unwrap(), token);
    }

    #[tokio::test]
    async fn a_token_is_rejected_from_its_expiry_instant() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, _, _) = new_test_service(&clock);
        let (plaintext, _) = service.mint_token(7, Duration::HOUR).await.unwrap();
        clock.advance(Duration::HOUR - Duration::SECOND);
        assert!(service.authenticate_token(&plaintext).await.is_ok());
        clock.advance(Duration::SECOND);
        assert!(matches!(
            service.authenticate_token(&plaintext).await,
            Err(Error::TokenExpired)
        ));
    }

    #[tokio::test]
    async fn revoke_token() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, _, _) = new_test_service(&clock);
        let (plaintext, token) = service.mint_token(7, Duration::HOUR).await.unwrap();
        service.revoke_token(token.id).await.unwrap();
        assert!(matches!(
            service.authenticate_token(&plaintext).await,
            Err(Error::Unauthenticated)
        ));
    }

    #[test]
    fn bearer_token_from_headers() {
        for (header, expected) in [
            (None, None),
            (Some("Bearer bento_abc"), Some("bento_abc")),
            (Some("bearer bento_abc"), Some("bento_abc")),
            (Some("Bearer   bento_abc  "), Some("bento_abc")),
            (Some("Basic dXNlcjpwYXNz"), None),
            (Some("Bearer"), None),
            (Some("Bearer "), None),
        ] {
            let mut headers = HeaderMap::new();
            if let Some(header) = header {
                headers.insert(AUTHORIZATION, HeaderValue::from_str(header).unwrap());
            }
            assert_eq!(bearer_token(&headers), expected);
        }
    }

    #[test]
    fn hash_token_is_stable() {
        // Pinned so a refactor cannot silently orphan every stored token.
        assert_eq!(
            hash_token("bento_test-token"),
            "6534dfb43d2797ced449eb69034d35aa68c7a93ae1580da72db3f7050a1acb79"
        );
    }
}
