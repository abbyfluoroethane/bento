use bento_types::Pairing;
use rusqlite::{OptionalExtension, params};
use time::OffsetDateTime;

use crate::{Error, Result, Store, format_time, parse_time};

impl Store {
    /// Records a pending SSH key link and returns the row (SPEC 13). Only
    /// the hash of the link token arrives here; the store never sees the
    /// token itself, which exists once in the URL handed to the user.
    ///
    /// Rows that expired without being used are dropped on the way past,
    /// so the table stays the size of the links currently in flight
    /// rather than growing with every unknown key that ever connected.
    pub async fn create_pairing(
        &self,
        token_hash: impl Into<String>,
        public_key: impl Into<String>,
        fingerprint: impl Into<String>,
        comment: impl Into<String>,
        expires_at: OffsetDateTime,
    ) -> Result<Pairing> {
        let token_hash = token_hash.into();
        let public_key = public_key.into();
        let fingerprint = fingerprint.into();
        let comment = comment.into();
        let now = self.clock();
        self.with_tx(move |tx| {
            let created_at = now().to_offset(time::UtcOffset::UTC);
            tx.execute(
                "DELETE FROM pairings WHERE linked_user_id IS NULL AND expires_at <= ?",
                [format_time(created_at)?],
            )?;
            tx.execute(
                "INSERT INTO pairings \
                 (token_hash, public_key, fingerprint, comment, created_at, expires_at) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    token_hash,
                    public_key,
                    fingerprint,
                    comment,
                    format_time(created_at)?,
                    format_time(expires_at)?,
                ],
            )?;
            Ok(Pairing {
                id: tx.last_insert_rowid(),
                token_hash,
                public_key,
                fingerprint,
                comment,
                created_at,
                expires_at,
                linked_user_id: None,
            })
        })
        .await
    }

    /// Looks a pairing up by the hash of its link token.
    ///
    /// Expired and already-used rows are returned rather than hidden: the
    /// caller renders a different page for each, and telling a stale link
    /// apart from a forged one needs the row. The same reasoning applies
    /// to [`Store::token_by_hash`].
    pub async fn pairing_by_token_hash(&self, token_hash: impl Into<String>) -> Result<Pairing> {
        let token_hash = token_hash.into();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT id, token_hash, public_key, fingerprint, comment, \
                 created_at, expires_at, linked_user_id \
                 FROM pairings WHERE token_hash = ?",
                [token_hash],
                scan_pairing,
            )
            .optional()?
            .ok_or(Error::NotFound)
        })
        .await
    }

    /// Returns one pairing by primary key. The SSH session that minted the
    /// pairing polls this to learn that the link was used.
    pub async fn pairing(&self, id: i64) -> Result<Pairing> {
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT id, token_hash, public_key, fingerprint, comment, \
                 created_at, expires_at, linked_user_id \
                 FROM pairings WHERE id = ?",
                [id],
                scan_pairing,
            )
            .optional()?
            .ok_or(Error::NotFound)
        })
        .await
    }

    /// Links the pairing's key to a user: one `ssh_keys` row, and the
    /// pairing marked used, in one transaction.
    ///
    /// The claim is a conditional update, so two browser tabs confirming
    /// the same link produce one key and one [`Error::NotFound`] rather
    /// than two keys. An expired or already-used pairing is
    /// [`Error::NotFound`] for the same reason: the check and the write
    /// cannot be separated.
    pub async fn link_pairing(&self, id: i64, user_id: i64) -> Result<i64> {
        let now = self.clock();
        self.with_tx(move |tx| {
            let claimed = tx.execute(
                "UPDATE pairings SET linked_user_id = ? \
                 WHERE id = ? AND linked_user_id IS NULL AND expires_at > ?",
                params![user_id, id, format_time(now())?],
            )?;
            if claimed == 0 {
                return Err(Error::NotFound);
            }
            let (public_key, fingerprint, comment) = tx.query_row(
                "SELECT public_key, fingerprint, comment FROM pairings WHERE id = ?",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )?;
            tx.execute(
                "INSERT INTO ssh_keys \
                 (user_id, public_key, fingerprint, comment, created_at) VALUES (?, ?, ?, ?, ?)",
                params![
                    user_id,
                    public_key,
                    fingerprint,
                    comment,
                    format_time(now())?
                ],
            )?;
            Ok(tx.last_insert_rowid())
        })
        .await
    }
}

fn scan_pairing(row: &rusqlite::Row<'_>) -> rusqlite::Result<Pairing> {
    let created: String = row.get(5)?;
    let expires: String = row.get(6)?;
    Ok(Pairing {
        id: row.get(0)?,
        token_hash: row.get(1)?,
        public_key: row.get(2)?,
        fingerprint: row.get(3)?,
        comment: row.get(4)?,
        created_at: parse_time(5, &created)?,
        expires_at: parse_time(6, &expires)?,
        linked_user_id: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use time::Duration;

    use crate::Error;
    use crate::tests::{FakeClock, new_test_store_with_clock, seed_store};

    #[tokio::test]
    async fn pairing_links_one_key_once() {
        let clock = FakeClock::new();
        let store = new_test_store_with_clock(clock.clone()).await;
        let (user, _) = seed_store(&store).await;

        let pairing = store
            .create_pairing(
                "hash-of-link-token",
                "ssh-ed25519 AAAA... alice@laptop",
                "SHA256:abcdef",
                "alice@laptop",
                clock.now() + Duration::minutes(3),
            )
            .await
            .unwrap();
        assert_eq!(pairing.linked_user_id, None);
        let found = store
            .pairing_by_token_hash("hash-of-link-token")
            .await
            .unwrap();
        assert_eq!(found, pairing);
        assert!(matches!(
            store.pairing_by_token_hash("forged").await,
            Err(Error::NotFound)
        ));

        let key_id = store.link_pairing(pairing.id, user.id).await.unwrap();
        let key = store.ssh_key_by_fingerprint("SHA256:abcdef").await.unwrap();
        assert_eq!(key.id, key_id);
        assert_eq!(key.user_id, user.id);
        assert_eq!(key.comment, "alice@laptop");
        assert_eq!(
            store.pairing(pairing.id).await.unwrap().linked_user_id,
            Some(user.id)
        );

        // A second confirmation of the same link adds no second key.
        assert!(matches!(
            store.link_pairing(pairing.id, user.id).await,
            Err(Error::NotFound)
        ));
        assert_eq!(store.ssh_keys_for_user(user.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_expired_pairing_links_nothing_and_is_still_readable() {
        let clock = FakeClock::new();
        let store = new_test_store_with_clock(clock.clone()).await;
        let (user, _) = seed_store(&store).await;
        let pairing = store
            .create_pairing(
                "hash-of-stale-token",
                "ssh-ed25519 AAAA...",
                "SHA256:stale",
                "",
                clock.now() + Duration::minutes(3),
            )
            .await
            .unwrap();

        clock.advance(Duration::minutes(4));
        assert!(matches!(
            store.link_pairing(pairing.id, user.id).await,
            Err(Error::NotFound)
        ));
        // The row survives its expiry so the page can say "expired"
        // rather than "no such link".
        let found = store
            .pairing_by_token_hash("hash-of-stale-token")
            .await
            .unwrap();
        assert_eq!(found.linked_user_id, None);
        assert!(found.expires_at < clock.now());
        assert!(
            store
                .ssh_key_by_fingerprint("SHA256:stale")
                .await
                .is_err_and(|error| matches!(error, Error::NotFound))
        );
    }

    #[tokio::test]
    async fn creating_a_pairing_sweeps_expired_ones() {
        let clock = FakeClock::new();
        let store = new_test_store_with_clock(clock.clone()).await;
        let (user, _) = seed_store(&store).await;
        let stale = store
            .create_pairing(
                "stale",
                "key",
                "SHA256:a",
                "",
                clock.now() + Duration::MINUTE,
            )
            .await
            .unwrap();
        let used = store
            .create_pairing(
                "used",
                "key",
                "SHA256:b",
                "",
                clock.now() + Duration::MINUTE,
            )
            .await
            .unwrap();
        store.link_pairing(used.id, user.id).await.unwrap();

        clock.advance(Duration::minutes(2));
        store
            .create_pairing(
                "fresh",
                "key",
                "SHA256:c",
                "",
                clock.now() + Duration::MINUTE,
            )
            .await
            .unwrap();

        assert!(matches!(
            store.pairing(stale.id).await,
            Err(Error::NotFound)
        ));
        // A used pairing is kept: its key row references what happened.
        assert_eq!(
            store.pairing(used.id).await.unwrap().linked_user_id,
            Some(user.id)
        );
    }
}
