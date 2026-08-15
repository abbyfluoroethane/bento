use bento_types::Token;
use http::HeaderMap;
use http::header::AUTHORIZATION;
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

use crate::{Error, Result, Service, random_token, store_error};

/// Every Bento API token starts with this prefix. It makes a leaked token
/// recognizable in logs and secret scanners.
pub const TOKEN_PREFIX: &str = "bento_";

/// The result of a token-store lookup. An expired row travels with the
/// outcome because the auth service enforces expiry against its own clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenLookup {
    Found(Token),
    Expired(Token),
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
        let expires_at = if ttl.is_positive() {
            (self.now)() + ttl
        } else {
            OffsetDateTime::UNIX_EPOCH
        };
        let token = self
            .tokens
            .create_token(user_id, &hash_token(&plaintext), expires_at)
            .await
            .map_err(|error| store_error("store token", error))?;
        Ok((plaintext, token))
    }

    /// Checks a plaintext bearer token and returns its row. Returns
    /// [`Error::Unauthenticated`] for an unknown token and
    /// [`Error::TokenExpired`] for a known token past its expiry.
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
            TokenLookup::Found(token) | TokenLookup::Expired(token) => token,
            TokenLookup::NotFound => return Err(Error::Unauthenticated),
        };
        // UNIX_EPOCH is the persistent zero-time sentinel for a token
        // that never expires.
        if token.expires_at != OffsetDateTime::UNIX_EPOCH && token.expires_at <= (self.now)() {
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
        assert_eq!(token.expires_at, TEST_EPOCH + Duration::HOUR);
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
    async fn store_expiry_is_rechecked_against_the_auth_clock() {
        let clock = FakeClock::new(TEST_EPOCH);
        let (service, _, _, tokens) = new_test_service(&clock);
        let (plaintext, token) = service.mint_token(7, Duration::HOUR).await.unwrap();
        tokens.report_expired(token.id);
        assert_eq!(service.authenticate_token(&plaintext).await.unwrap(), token);
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
