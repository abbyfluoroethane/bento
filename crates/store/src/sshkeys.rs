use bento_types::SshKey;
use rusqlite::{OptionalExtension, params};

use crate::{Error, Result, Store, format_time, parse_time};

impl Store {
    /// Registers a public key for a user and returns its id.
    pub async fn add_ssh_key(
        &self,
        user_id: i64,
        public_key: impl Into<String>,
        fingerprint: impl Into<String>,
        comment: impl Into<String>,
    ) -> Result<i64> {
        let public_key = public_key.into();
        let fingerprint = fingerprint.into();
        let comment = comment.into();
        let now = self.clock();
        self.with_conn(move |conn| {
            conn.execute(
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
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    /// Hot-path lookup run by the SSH frontend on every connection;
    /// `idx_ssh_keys_fingerprint` backs it (SPEC 12).
    pub async fn ssh_key_by_fingerprint(&self, fingerprint: impl Into<String>) -> Result<SshKey> {
        let fingerprint = fingerprint.into();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT id, user_id, public_key, fingerprint, comment, created_at \
                 FROM ssh_keys WHERE fingerprint = ? ORDER BY id LIMIT 1",
                [fingerprint],
                scan_ssh_key,
            )
            .optional()?
            .ok_or(Error::NotFound)
        })
        .await
    }

    /// Lists one user's keys in insertion order.
    pub async fn ssh_keys_for_user(&self, user_id: i64) -> Result<Vec<SshKey>> {
        self.with_conn(move |conn| {
            let mut statement = conn.prepare(
                "SELECT id, user_id, public_key, fingerprint, comment, created_at \
                 FROM ssh_keys WHERE user_id = ? ORDER BY id",
            )?;
            let rows = statement.query_map([user_id], scan_ssh_key)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Removes one key belonging to one user. The user scope prevents a
    /// caller deleting another user's key by id.
    pub async fn delete_ssh_key(&self, user_id: i64, key_id: i64) -> Result<()> {
        self.with_conn(move |conn| {
            let changed = conn.execute(
                "DELETE FROM ssh_keys WHERE id = ? AND user_id = ?",
                params![key_id, user_id],
            )?;
            if changed == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
        .await
    }
}

fn scan_ssh_key(row: &rusqlite::Row<'_>) -> rusqlite::Result<SshKey> {
    let created: String = row.get(5)?;
    Ok(SshKey {
        id: row.get(0)?,
        user_id: row.get(1)?,
        public_key: row.get(2)?,
        fingerprint: row.get(3)?,
        comment: row.get(4)?,
        created_at: parse_time(5, &created)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::Error;
    use crate::tests::{new_test_store, seed_store};

    #[tokio::test]
    async fn ssh_key_fingerprint_lookup() {
        let store = new_test_store().await;
        let (user, _) = seed_store(&store).await;
        let id = store
            .add_ssh_key(
                user.id,
                "ssh-ed25519 AAAA... alice@laptop",
                "SHA256:abcdef",
                "laptop",
            )
            .await
            .unwrap();
        store
            .add_ssh_key(
                user.id,
                "ssh-ed25519 BBBB... alice@desk",
                "SHA256:ghijkl",
                "desk",
            )
            .await
            .unwrap();
        let key = store.ssh_key_by_fingerprint("SHA256:abcdef").await.unwrap();
        assert_eq!(key.id, id);
        assert_eq!(key.user_id, user.id);
        assert_eq!(key.comment, "laptop");
        assert!(matches!(
            store.ssh_key_by_fingerprint("SHA256:missing").await,
            Err(Error::NotFound)
        ));
        assert_eq!(store.ssh_keys_for_user(user.id).await.unwrap().len(), 2);
        assert!(matches!(
            store.delete_ssh_key(user.id + 1, id).await,
            Err(Error::NotFound)
        ));
        store.delete_ssh_key(user.id, id).await.unwrap();
        assert!(matches!(
            store.ssh_key_by_fingerprint("SHA256:abcdef").await,
            Err(Error::NotFound)
        ));
    }
}
