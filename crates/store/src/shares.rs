use bento_types::Share;
use rusqlite::{OptionalExtension, params};

use crate::{Error, Result, Store, format_time, parse_time};

impl Store {
    /// Grants `user_id` access to an instance. Shares key on UUID, never
    /// name (SPEC 7.2, 12). Adding an existing share is a no-op.
    pub async fn add_share(&self, instance_uuid: impl Into<String>, user_id: i64) -> Result<()> {
        let instance_uuid = instance_uuid.into();
        let now = self.clock();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO shares (instance_uuid, user_id, created_at) VALUES (?, ?, ?) \
                 ON CONFLICT(instance_uuid, user_id) DO NOTHING",
                params![instance_uuid, user_id, format_time(now())?],
            )?;
            Ok(())
        })
        .await
    }

    /// Revokes a share. Removing a share that does not exist returns
    /// [`Error::NotFound`].
    pub async fn remove_share(&self, instance_uuid: impl Into<String>, user_id: i64) -> Result<()> {
        let instance_uuid = instance_uuid.into();
        self.with_conn(move |conn| {
            let changed = conn.execute(
                "DELETE FROM shares WHERE instance_uuid = ? AND user_id = ?",
                params![instance_uuid, user_id],
            )?;
            if changed == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
        .await
    }

    /// Lists the shares of one instance.
    pub async fn shares_for(&self, instance_uuid: impl Into<String>) -> Result<Vec<Share>> {
        let instance_uuid = instance_uuid.into();
        self.with_conn(move |conn| {
            let mut statement = conn.prepare(
                "SELECT instance_uuid, user_id, created_at FROM shares \
                 WHERE instance_uuid = ? ORDER BY user_id",
            )?;
            let rows = statement.query_map([instance_uuid], scan_share)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Lists the instances shared with one user, oldest first.
    pub async fn instances_shared_with(&self, user_id: i64) -> Result<Vec<bento_types::Instance>> {
        self.list_instances(
            "WHERE uuid IN (SELECT instance_uuid FROM shares WHERE user_id = ?) \
             ORDER BY created_at, uuid",
            vec![rusqlite::types::Value::Integer(user_id)],
        )
        .await
    }

    /// Reports whether `user_id` owns the instance or holds a share on it.
    /// Authorization runs this check on every request (SPEC 13).
    pub async fn has_access(&self, instance_uuid: impl Into<String>, user_id: i64) -> Result<bool> {
        let instance_uuid = instance_uuid.into();
        self.with_conn(move |conn| {
            let found = conn
                .query_row(
                    "SELECT 1 FROM instances WHERE uuid = ? AND owner_id = ? \
                     UNION SELECT 1 FROM shares WHERE instance_uuid = ? AND user_id = ?",
                    params![instance_uuid, user_id, instance_uuid, user_id],
                    |_| Ok(()),
                )
                .optional()?;
            Ok(found.is_some())
        })
        .await
    }
}

fn scan_share(row: &rusqlite::Row<'_>) -> rusqlite::Result<Share> {
    let created: String = row.get(2)?;
    Ok(Share {
        instance_uuid: row.get(0)?,
        user_id: row.get(1)?,
        created_at: parse_time(2, &created)?,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::Error;
    use crate::tests::{new_test_store, seed_store, test_instance, test_range};

    #[tokio::test]
    async fn shares_and_access() {
        let store = new_test_store().await;
        let (owner, host) = seed_store(&store).await;
        let friend = store
            .register_user("bob", "bob@example.org", None, test_range())
            .await
            .unwrap();
        let stranger = store
            .register_user("carol", "carol@example.org", None, test_range())
            .await
            .unwrap();
        let instance = test_instance(1, "web", &owner, &host);
        store
            .create_instance(instance.clone(), Duration::ZERO)
            .await
            .unwrap();
        store.add_share(&instance.uuid, friend.id).await.unwrap();
        store.add_share(&instance.uuid, friend.id).await.unwrap();
        for (label, user_id, expected) in [
            ("owner", owner.id, true),
            ("shared user", friend.id, true),
            ("stranger", stranger.id, false),
        ] {
            assert_eq!(
                store.has_access(&instance.uuid, user_id).await.unwrap(),
                expected,
                "{label}"
            );
        }
        let shared = store.instances_shared_with(friend.id).await.unwrap();
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].uuid, instance.uuid);
        store.remove_share(&instance.uuid, friend.id).await.unwrap();
        assert!(matches!(
            store.remove_share(&instance.uuid, friend.id).await,
            Err(Error::NotFound)
        ));
    }
}
