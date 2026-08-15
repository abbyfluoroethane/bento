use bento_types::Token;
use rusqlite::{OptionalExtension, params};
use time::OffsetDateTime;

use crate::{Error, Result, Store, format_time, parse_time};

impl Store {
    /// Stores a programmatic access token (SPEC 13). Only the hash arrives
    /// here; the store never sees the token itself. The caller hashes the
    /// secret and hands out the plaintext once.
    pub async fn create_token(
        &self,
        user_id: i64,
        hash: impl Into<String>,
        expires_at: OffsetDateTime,
    ) -> Result<i64> {
        let hash = hash.into();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO tokens (user_id, hash, expires_at) VALUES (?, ?, ?)",
                params![user_id, hash, format_time(expires_at)?],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    /// Looks a token up by hash. Unknown hashes return [`Error::NotFound`]
    /// and tokens at or past their expiry return [`Error::TokenExpired`].
    pub async fn token_by_hash(&self, hash: impl Into<String>) -> Result<Token> {
        let hash = hash.into();
        let now = self.clock();
        self.with_conn(move |conn| {
            let token = conn
                .query_row(
                    "SELECT id, user_id, hash, expires_at FROM tokens WHERE hash = ?",
                    [hash],
                    scan_token,
                )
                .optional()?
                .ok_or(Error::NotFound)?;
            if now() >= token.expires_at {
                return Err(Error::TokenExpired(Box::new(token)));
            }
            Ok(token)
        })
        .await
    }

    /// Revokes one token belonging to one user.
    pub async fn delete_token(&self, user_id: i64, token_id: i64) -> Result<()> {
        self.with_conn(move |conn| {
            let changed = conn.execute(
                "DELETE FROM tokens WHERE id = ? AND user_id = ?",
                params![token_id, user_id],
            )?;
            if changed == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
        .await
    }

    /// Revokes a token regardless of owner. The dashboard path uses this
    /// after authorizing the caller and identifying the token by row id.
    pub async fn delete_token_by_id(&self, token_id: i64) -> Result<()> {
        self.with_conn(move |conn| {
            if conn.execute("DELETE FROM tokens WHERE id = ?", [token_id])? == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
        .await
    }
}

fn scan_token(row: &rusqlite::Row<'_>) -> rusqlite::Result<Token> {
    let expires: String = row.get(3)?;
    Ok(Token {
        id: row.get(0)?,
        user_id: row.get(1)?,
        hash: row.get(2)?,
        expires_at: parse_time(3, &expires)?,
    })
}

#[cfg(test)]
mod tests {
    use time::Duration;

    use crate::Error;
    use crate::tests::{
        FakeClock, new_test_store, new_test_store_with_clock, seed_store, test_range,
    };

    #[tokio::test]
    async fn tokens() {
        let clock = FakeClock::new();
        let store = new_test_store_with_clock(clock.clone()).await;
        let (user, _) = seed_store(&store).await;
        let expiry = clock.now() + Duration::HOUR;
        let id = store
            .create_token(user.id, "hash-of-secret", expiry)
            .await
            .unwrap();
        let token = store.token_by_hash("hash-of-secret").await.unwrap();
        assert_eq!(token.id, id);
        assert_eq!(token.user_id, user.id);
        assert_eq!(token.expires_at, expiry);
        assert!(matches!(
            store.token_by_hash("unknown").await,
            Err(Error::NotFound)
        ));
        clock.advance(Duration::hours(2));
        // The rejected row travels with the error: the auth service
        // enforces expiry against its own clock (SPEC 13).
        let expired = store.token_by_hash("hash-of-secret").await;
        let Err(Error::TokenExpired(row)) = expired else {
            panic!("expected TokenExpired, got {expired:?}");
        };
        assert_eq!(row.id, id);
        assert_eq!(row.expires_at, expiry);
        store.delete_token(user.id, id).await.unwrap();
        assert!(matches!(
            store.token_by_hash("hash-of-secret").await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn delete_token_by_id() {
        let store = new_test_store().await;
        let user = store
            .register_user("amber", "amber@example.org", None, test_range())
            .await
            .unwrap();
        let id = store
            .create_token(user.id, "hash-by-id", time::OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap();
        store.delete_token_by_id(id).await.unwrap();
        assert!(matches!(
            store.token_by_hash("hash-by-id").await,
            Err(Error::NotFound)
        ));
        assert!(matches!(
            store.delete_token_by_id(id).await,
            Err(Error::NotFound)
        ));
    }
}
