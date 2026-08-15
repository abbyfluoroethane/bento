use std::time::Duration;

use bento_types::ReleasedName;
use rusqlite::{OptionalExtension, Transaction, params};
use time::OffsetDateTime;

use crate::{Error, Result, Store, format_time, parse_time};

impl Store {
    /// Checks whether a user may take a name under SPEC 7.2:
    ///
    /// 1. The previous owner may retake a released name immediately.
    /// 2. Another user waits out the cooldown; the error carries the
    ///    remaining time for the CLI message (SPEC 15).
    /// 3. A name held by a live instance is never available.
    ///
    /// Released rows remain after expiry (SPEC 12); timestamp comparison
    /// gates the claim. This check alone is advisory: create and rename
    /// repeat it inside their own transactions.
    pub async fn claim_name(
        &self,
        name: impl Into<String>,
        user_id: i64,
        cooldown: Duration,
    ) -> Result<()> {
        let name = name.into();
        let now = self.clock();
        self.with_tx(move |tx| claim_name_tx(tx, &name, user_id, cooldown, now()))
            .await
    }

    /// Returns a name's release record, which remains after cooldown expiry
    /// (SPEC 12).
    pub async fn released_name(&self, name: impl Into<String>) -> Result<ReleasedName> {
        let name = name.into();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT name, previous_owner_id, released_at FROM released_names WHERE name = ?",
                [name],
                scan_released_name,
            )
            .optional()?
            .ok_or(Error::NotFound)
        })
        .await
    }
}

pub(crate) fn claim_name_tx(
    tx: &Transaction<'_>,
    name: &str,
    user_id: i64,
    cooldown: Duration,
    now: OffsetDateTime,
) -> Result<()> {
    if tx
        .query_row("SELECT 1 FROM instances WHERE name = ?", [name], |_| Ok(()))
        .optional()?
        .is_some()
    {
        return Err(Error::NameTaken);
    }

    let release = tx
        .query_row(
            "SELECT previous_owner_id, released_at FROM released_names WHERE name = ?",
            [name],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((previous_owner_id, released_text)) = release else {
        return Ok(());
    };
    if previous_owner_id == user_id {
        return Ok(());
    }
    let released_at = parse_time(1, &released_text)?;
    if let Some(remaining) = remaining_cooldown(released_at, now, cooldown) {
        return Err(Error::NameCooldown {
            name: name.to_string(),
            remaining,
        });
    }
    Ok(())
}

/// Records a release from delete or rename (SPEC 7.2). Releasing the same
/// name again replaces the row so cooldown restarts at the newest release.
pub(crate) fn release_name_tx(
    tx: &Transaction<'_>,
    name: &str,
    previous_owner_id: i64,
    now: OffsetDateTime,
) -> Result<()> {
    tx.execute(
        "INSERT INTO released_names (name, previous_owner_id, released_at) VALUES (?, ?, ?) \
         ON CONFLICT(name) DO UPDATE SET \
            previous_owner_id = excluded.previous_owner_id, \
            released_at = excluded.released_at",
        params![name, previous_owner_id, format_time(now)?],
    )?;
    Ok(())
}

fn remaining_cooldown(
    released_at: OffsetDateTime,
    now: OffsetDateTime,
    cooldown: Duration,
) -> Option<Duration> {
    let remaining_nanos = cooldown.as_nanos() as i128 - (now - released_at).whole_nanoseconds();
    if remaining_nanos <= 0 {
        return None;
    }
    let seconds = u64::try_from(remaining_nanos / 1_000_000_000).ok()?;
    let nanos = u32::try_from(remaining_nanos % 1_000_000_000).ok()?;
    Some(Duration::new(seconds, nanos))
}

fn scan_released_name(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReleasedName> {
    let released: String = row.get(2)?;
    Ok(ReleasedName {
        name: row.get(0)?,
        previous_owner_id: row.get(1)?,
        released_at: parse_time(2, &released)?,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use time::Duration as TimeDuration;

    use crate::Error;
    use crate::tests::{
        FakeClock, new_test_store, new_test_store_with_clock, seed_store, test_instance, test_range,
    };

    const COOLDOWN: Duration = Duration::from_secs(24 * 60 * 60);

    #[tokio::test]
    async fn claim_name_covers_all_cooldown_rules() {
        let cases = [
            ("rule 1: previous owner retakes at once", 0, false, None),
            (
                "rule 2: another user inside cooldown is refused",
                1,
                true,
                Some(Duration::from_secs(23 * 60 * 60)),
            ),
            (
                "rule 3: another user after cooldown succeeds",
                24,
                true,
                None,
            ),
        ];
        for (case, advance_hours, use_other, expected_remaining) in cases {
            let clock = FakeClock::new();
            let store = new_test_store_with_clock(clock.clone()).await;
            let (owner, host) = seed_store(&store).await;
            let other = store
                .register_user("bob", "bob@example.org", None, test_range())
                .await
                .unwrap();
            store
                .create_instance(test_instance(1, "web", &owner, &host), COOLDOWN)
                .await
                .unwrap();
            store.delete_instance("uuid-001").await.unwrap();
            clock.advance(TimeDuration::hours(advance_hours));
            let claimant = if use_other { other.id } else { owner.id };
            let result = store.claim_name("web", claimant, COOLDOWN).await;
            match expected_remaining {
                None => assert!(result.is_ok(), "{case}: {result:?}"),
                Some(expected) => match result.unwrap_err() {
                    Error::NameCooldown { name, remaining } => {
                        assert_eq!(name, "web", "{case}");
                        assert_eq!(remaining, expected, "{case}");
                    }
                    error => panic!("{case}: got {error}, want NameCooldown"),
                },
            }
        }
    }

    #[tokio::test]
    async fn claim_name_live_instance() {
        let store = new_test_store().await;
        let (owner, host) = seed_store(&store).await;
        store
            .create_instance(test_instance(1, "web", &owner, &host), COOLDOWN)
            .await
            .unwrap();
        assert!(matches!(
            store.claim_name("web", owner.id, COOLDOWN).await,
            Err(Error::NameTaken)
        ));
    }

    #[tokio::test]
    async fn released_name_row_kept_after_expiry() {
        let clock = FakeClock::new();
        let store = new_test_store_with_clock(clock.clone()).await;
        let (owner, host) = seed_store(&store).await;
        let other = store
            .register_user("bob", "bob@example.org", None, test_range())
            .await
            .unwrap();
        store
            .create_instance(test_instance(1, "web", &owner, &host), COOLDOWN)
            .await
            .unwrap();
        store.delete_instance("uuid-001").await.unwrap();
        let released_at = clock.now();
        clock.advance(TimeDuration::hours(25));
        store.claim_name("web", other.id, COOLDOWN).await.unwrap();
        let record = store.released_name("web").await.unwrap();
        assert_eq!(record.previous_owner_id, owner.id);
        assert_eq!(record.released_at, released_at);
    }

    #[tokio::test]
    async fn rename_releases_old_name() {
        let clock = FakeClock::new();
        let store = new_test_store_with_clock(clock).await;
        let (owner, host) = seed_store(&store).await;
        let other = store
            .register_user("bob", "bob@example.org", None, test_range())
            .await
            .unwrap();
        store
            .create_instance(test_instance(1, "old-name", &owner, &host), COOLDOWN)
            .await
            .unwrap();
        store
            .rename_instance("uuid-001", "new-name", COOLDOWN)
            .await
            .unwrap();
        assert_eq!(store.instance("uuid-001").await.unwrap().name, "new-name");
        assert!(matches!(
            store.instance_by_name("old-name").await,
            Err(Error::NotFound)
        ));
        assert!(matches!(
            store.claim_name("old-name", other.id, COOLDOWN).await,
            Err(Error::NameCooldown { .. })
        ));
        store
            .claim_name("old-name", owner.id, COOLDOWN)
            .await
            .unwrap();

        let other_instance = test_instance(2, "bob-web", &other, &host);
        store
            .create_instance(other_instance.clone(), COOLDOWN)
            .await
            .unwrap();
        store.delete_instance(other_instance.uuid).await.unwrap();
        assert!(matches!(
            store.rename_instance("uuid-001", "bob-web", COOLDOWN).await,
            Err(Error::NameCooldown { .. })
        ));
    }

    #[tokio::test]
    async fn create_instance_respects_cooldown() {
        let clock = FakeClock::new();
        let store = new_test_store_with_clock(clock.clone()).await;
        let (owner, host) = seed_store(&store).await;
        let other = store
            .register_user("bob", "bob@example.org", None, test_range())
            .await
            .unwrap();
        store
            .create_instance(test_instance(1, "web", &owner, &host), COOLDOWN)
            .await
            .unwrap();
        store.delete_instance("uuid-001").await.unwrap();
        clock.advance(TimeDuration::minutes(30));

        let instance = test_instance(2, "web", &other, &host);
        match store
            .create_instance(instance.clone(), COOLDOWN)
            .await
            .unwrap_err()
        {
            Error::NameCooldown { remaining, .. } => {
                assert_eq!(remaining, COOLDOWN - Duration::from_secs(30 * 60));
            }
            error => panic!("got {error}, want NameCooldown"),
        }
        assert!(matches!(
            store.instance(instance.uuid).await,
            Err(Error::NotFound)
        ));
        store
            .create_instance(test_instance(3, "web", &owner, &host), COOLDOWN)
            .await
            .unwrap();
    }
}
