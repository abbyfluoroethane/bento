use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use bento_types::{DesiredState, Instance, Quota, State, Visibility};
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Transaction, params, params_from_iter};

use crate::names::{claim_name_tx, release_name_tx};
use crate::{Error, Result, Store, format_time, parse_time};

const INSTANCE_COLUMNS: &str = "uuid, name, owner_id, host_id, image_name, base_checksum, \
     state, desired_state, address, mac, vcpu, memory, disk, nested, ksm, \
     http_port, visibility, created_at, last_seen_at";

impl Store {
    /// Inserts an instance after checking name cooldown (SPEC 7.2) and all
    /// four quota limits (SPEC 6.1) in the same transaction. Concurrent
    /// creates cannot both pass when only one fits. No quota row means
    /// unlimited.
    pub async fn create_instance(&self, instance: Instance, name_cooldown: Duration) -> Result<()> {
        let now = self.clock();
        self.with_tx(move |tx| {
            claim_name_tx(tx, &instance.name, instance.owner_id, name_cooldown, now())?;
            check_quota_tx(
                tx,
                instance.owner_id,
                "",
                1,
                i64::from(instance.vcpu),
                instance.memory_mib,
                instance.disk_gib,
            )?;
            let created_at = if instance.created_at == time::OffsetDateTime::UNIX_EPOCH {
                now()
            } else {
                instance.created_at
            };
            tx.execute(
                "INSERT INTO instances \
                 (uuid, name, owner_id, host_id, image_name, base_checksum, \
                  state, desired_state, address, mac, vcpu, memory, disk, \
                  nested, ksm, http_port, visibility, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    instance.uuid,
                    instance.name,
                    instance.owner_id,
                    instance.host_id,
                    instance.image_name,
                    instance.base_checksum,
                    instance.state.as_str(),
                    instance.desired_state.as_str(),
                    instance.address,
                    instance.mac,
                    instance.vcpu,
                    instance.memory_mib,
                    instance.disk_gib,
                    instance.nested,
                    instance.ksm,
                    instance.http_port,
                    instance.visibility.as_str(),
                    format_time(created_at)?
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Deletes an instance and releases its name in one transaction. Shares
    /// cascade with the row (SPEC 7.2). The returned instance lets the
    /// caller clean up its domain and overlay.
    pub async fn delete_instance(&self, uuid: impl Into<String>) -> Result<Instance> {
        let uuid = uuid.into();
        let now = self.clock();
        self.with_tx(move |tx| {
            let instance = get_instance_tx(tx, "uuid", &uuid)?;
            tx.execute("DELETE FROM instances WHERE uuid = ?", [&uuid])?;
            release_name_tx(tx, &instance.name, instance.owner_id, now())?;
            Ok(instance)
        })
        .await
    }

    /// Changes an instance label (SPEC 7.3). Claiming the new name,
    /// updating the row, and releasing the old name share one transaction.
    /// Bento never redirects the old name.
    pub async fn rename_instance(
        &self,
        uuid: impl Into<String>,
        new_name: impl Into<String>,
        name_cooldown: Duration,
    ) -> Result<()> {
        let uuid = uuid.into();
        let new_name = new_name.into();
        let now = self.clock();
        self.with_tx(move |tx| {
            let instance = get_instance_tx(tx, "uuid", &uuid)?;
            if instance.name == new_name {
                return Ok(());
            }
            claim_name_tx(tx, &new_name, instance.owner_id, name_cooldown, now())?;
            tx.execute(
                "UPDATE instances SET name = ? WHERE uuid = ?",
                params![new_name, uuid],
            )?;
            release_name_tx(tx, &instance.name, instance.owner_id, now())
        })
        .await
    }

    /// Updates vCPU, memory, disk, and nested virtualization, rerunning the
    /// quota check with the instance's own old use excluded (SPEC 6.1,
    /// 11.1).
    pub async fn resize(
        &self,
        uuid: impl Into<String>,
        vcpu: u32,
        memory_mib: i64,
        disk_gib: i64,
        nested: bool,
    ) -> Result<()> {
        let uuid = uuid.into();
        self.with_tx(move |tx| {
            let instance = get_instance_tx(tx, "uuid", &uuid)?;
            check_quota_tx(
                tx,
                instance.owner_id,
                &uuid,
                1,
                i64::from(vcpu),
                memory_mib,
                disk_gib,
            )?;
            tx.execute(
                "UPDATE instances SET vcpu = ?, memory = ?, disk = ?, nested = ? WHERE uuid = ?",
                params![vcpu, memory_mib, disk_gib, nested, uuid],
            )?;
            Ok(())
        })
        .await
    }

    /// Returns one instance by UUID, its identifier (SPEC 7.2).
    pub async fn instance(&self, uuid: impl Into<String>) -> Result<Instance> {
        self.get_instance("uuid", uuid.into()).await
    }

    /// Returns one instance by its current label. The proxy and SSH
    /// frontend resolve the name on every request (SPEC 7.1).
    pub async fn instance_by_name(&self, name: impl Into<String>) -> Result<Instance> {
        self.get_instance("name", name.into()).await
    }

    /// Lists one user's instances, oldest first.
    pub async fn instances_by_owner(&self, owner_id: i64) -> Result<Vec<Instance>> {
        self.list_instances(
            "WHERE owner_id = ? ORDER BY created_at, uuid",
            vec![Value::Integer(owner_id)],
        )
        .await
    }

    /// Lists every instance, oldest first. The reconcile report compares
    /// this list with host domains (SPEC 6.1).
    pub async fn instances(&self) -> Result<Vec<Instance>> {
        self.list_instances("ORDER BY created_at, uuid", Vec::new())
            .await
    }

    /// Lists instances whose desired state is running while observed state
    /// is stopped: the reboot-restore batch input (SPEC 11.2).
    pub async fn instances_to_restore(&self) -> Result<Vec<Instance>> {
        self.list_instances(
            "WHERE desired_state = 'running' AND state = 'stopped' ORDER BY created_at, uuid",
            Vec::new(),
        )
        .await
    }

    /// Records the last user action. Bento is authoritative for desired
    /// state (SPEC 11.1).
    pub async fn set_desired_state(
        &self,
        uuid: impl Into<String>,
        state: DesiredState,
    ) -> Result<()> {
        self.update_instance(uuid.into(), "desired_state", Value::Text(state.to_string()))
            .await
    }

    /// Records what libvirt reports. Libvirt is authoritative for observed
    /// state (SPEC 11.1).
    pub async fn set_observed_state(&self, uuid: impl Into<String>, state: State) -> Result<()> {
        self.update_instance(uuid.into(), "state", Value::Text(state.to_string()))
            .await
    }

    /// Applies the 30-second domain poll in one transaction (SPEC 12).
    /// UUIDs without rows are skipped; the reconcile report covers them.
    pub async fn update_observed_states(&self, states: HashMap<String, State>) -> Result<()> {
        if states.is_empty() {
            return Ok(());
        }
        self.with_tx(move |tx| {
            let mut statement = tx.prepare("UPDATE instances SET state = ? WHERE uuid = ?")?;
            for (uuid, state) in states {
                statement.execute(params![state.as_str(), uuid])?;
            }
            Ok(())
        })
        .await
    }

    /// Sets proxy visibility (SPEC 9.2).
    pub async fn set_visibility(
        &self,
        uuid: impl Into<String>,
        visibility: Visibility,
    ) -> Result<()> {
        self.update_instance(
            uuid.into(),
            "visibility",
            Value::Text(visibility.to_string()),
        )
        .await
    }

    /// Sets the default HTTP port targeted by the proxy (SPEC 9.1).
    pub async fn set_http_port(&self, uuid: impl Into<String>, port: u16) -> Result<()> {
        self.update_instance(uuid.into(), "http_port", Value::Integer(i64::from(port)))
            .await
    }

    /// Records an SSH connection or HTTP request (SPEC 12). Bento never
    /// acts on this column; it only feeds instance listings.
    pub async fn touch_last_seen(&self, uuid: impl Into<String>) -> Result<()> {
        let value = format_time((self.now)())?;
        self.update_instance(uuid.into(), "last_seen_at", Value::Text(value))
            .await
    }

    async fn update_instance(
        &self,
        uuid: String,
        column: &'static str,
        value: Value,
    ) -> Result<()> {
        self.with_conn(move |conn| {
            let sql = format!("UPDATE instances SET {column} = ? WHERE uuid = ?");
            if conn.execute(&sql, params![value, uuid])? == 0 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
        .await
    }

    async fn get_instance(&self, column: &'static str, value: String) -> Result<Instance> {
        self.with_conn(move |conn| {
            let sql = format!("SELECT {INSTANCE_COLUMNS} FROM instances WHERE {column} = ?");
            conn.query_row(&sql, [value], scan_instance)
                .optional()?
                .ok_or(Error::NotFound)
        })
        .await
    }

    pub(crate) async fn list_instances(
        &self,
        tail: &'static str,
        arguments: Vec<Value>,
    ) -> Result<Vec<Instance>> {
        self.with_conn(move |conn| {
            let sql = format!("SELECT {INSTANCE_COLUMNS} FROM instances {tail}");
            let mut statement = conn.prepare(&sql)?;
            let rows = statement.query_map(params_from_iter(arguments), scan_instance)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }
}

fn get_instance_tx(tx: &Transaction<'_>, column: &str, value: &str) -> Result<Instance> {
    let sql = format!("SELECT {INSTANCE_COLUMNS} FROM instances WHERE {column} = ?");
    tx.query_row(&sql, [value], scan_instance)
        .optional()?
        .ok_or(Error::NotFound)
}

fn check_quota_tx(
    tx: &Transaction<'_>,
    owner_id: i64,
    exclude_uuid: &str,
    add_instances: i64,
    add_vcpu: i64,
    add_memory: i64,
    add_disk: i64,
) -> Result<()> {
    let quota = tx
        .query_row(
            "SELECT user_id, max_instances, max_vcpu, max_memory, max_disk \
             FROM quotas WHERE user_id = ?",
            [owner_id],
            |row| {
                Ok(Quota {
                    user_id: row.get(0)?,
                    max_instances: row.get(1)?,
                    max_vcpu: row.get(2)?,
                    max_memory_mib: row.get(3)?,
                    max_disk_gib: row.get(4)?,
                })
            },
        )
        .optional()?;
    let Some(quota) = quota else {
        return Ok(());
    };

    let (instances, vcpu, memory, disk) = tx.query_row(
        "SELECT COUNT(*), COALESCE(SUM(vcpu), 0), COALESCE(SUM(memory), 0), \
         COALESCE(SUM(disk), 0) FROM instances WHERE owner_id = ? AND uuid != ?",
        params![owner_id, exclude_uuid],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    for (limit, used, requested, max) in [
        ("instances", instances, add_instances, quota.max_instances),
        ("vcpu", vcpu, add_vcpu, quota.max_vcpu),
        ("memory", memory, add_memory, quota.max_memory_mib),
        ("disk", disk, add_disk, quota.max_disk_gib),
    ] {
        if used + requested > max {
            return Err(Error::Quota {
                limit,
                used,
                requested,
                max,
            });
        }
    }
    Ok(())
}

pub(crate) fn scan_instance(row: &rusqlite::Row<'_>) -> rusqlite::Result<Instance> {
    let state_text: String = row.get(6)?;
    let desired_text: String = row.get(7)?;
    let visibility_text: String = row.get(16)?;
    let created_text: String = row.get(17)?;
    let last_seen_text: Option<String> = row.get(18)?;
    Ok(Instance {
        uuid: row.get(0)?,
        name: row.get(1)?,
        owner_id: row.get(2)?,
        host_id: row.get(3)?,
        image_name: row.get(4)?,
        base_checksum: row.get(5)?,
        state: parse_enum(6, &state_text)?,
        desired_state: parse_enum(7, &desired_text)?,
        address: row.get(8)?,
        mac: row.get(9)?,
        vcpu: row.get(10)?,
        memory_mib: row.get(11)?,
        disk_gib: row.get(12)?,
        nested: row.get(13)?,
        ksm: row.get(14)?,
        http_port: row.get(15)?,
        visibility: parse_enum(16, &visibility_text)?,
        created_at: parse_time(17, &created_text)?,
        last_seen_at: last_seen_text
            .as_deref()
            .map(|text| parse_time(18, text))
            .transpose()?,
    })
}

fn parse_enum<T>(column: usize, value: &str) -> rusqlite::Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    value.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use bento_types::{DesiredState, Quota, State, Visibility};
    use time::Duration as TimeDuration;

    use crate::Error;
    use crate::tests::{
        FakeClock, new_test_store, new_test_store_with_clock, seed_store, test_instance, test_range,
    };

    #[tokio::test]
    async fn create_instance_quota_limits() {
        for (case, expected_limit) in [
            ("fits", None),
            ("vcpu", Some("vcpu")),
            ("memory", Some("memory")),
            ("disk", Some("disk")),
        ] {
            let store = new_test_store().await;
            let (owner, host) = seed_store(&store).await;
            store
                .set_quota(Quota {
                    user_id: owner.id,
                    max_instances: 2,
                    max_vcpu: 4,
                    max_memory_mib: 4096,
                    max_disk_gib: 50,
                })
                .await
                .unwrap();
            let mut first = test_instance(1, "first", &owner, &host);
            first.memory_mib = 1024;
            let mut second = test_instance(2, "second", &owner, &host);
            second.memory_mib = 1024;
            match case {
                "vcpu" => second.vcpu = 4,
                "memory" => second.memory_mib = 4000,
                "disk" => second.disk_gib = 41,
                _ => {}
            }
            store.create_instance(first, Duration::ZERO).await.unwrap();
            let result = store.create_instance(second, Duration::ZERO).await;
            match expected_limit {
                None => assert!(result.is_ok(), "{case}: {result:?}"),
                Some(expected) => match result.unwrap_err() {
                    Error::Quota { limit, .. } => assert_eq!(limit, expected, "{case}"),
                    error => panic!("{case}: got {error}, want quota error"),
                },
            }
        }
    }

    #[tokio::test]
    async fn create_instance_count_limit() {
        let store = new_test_store().await;
        let (owner, host) = seed_store(&store).await;
        store
            .set_quota(Quota {
                user_id: owner.id,
                max_instances: 1,
                max_vcpu: 100,
                max_memory_mib: 1 << 20,
                max_disk_gib: 1 << 20,
            })
            .await
            .unwrap();
        store
            .create_instance(test_instance(1, "first", &owner, &host), Duration::ZERO)
            .await
            .unwrap();
        assert!(matches!(
            store
                .create_instance(test_instance(2, "second", &owner, &host), Duration::ZERO)
                .await,
            Err(Error::Quota {
                limit: "instances",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn create_instance_concurrent_quota() {
        let store = new_test_store().await;
        let (owner, host) = seed_store(&store).await;
        store
            .set_quota(Quota {
                user_id: owner.id,
                max_instances: 1,
                max_vcpu: 100,
                max_memory_mib: 1 << 20,
                max_disk_gib: 1 << 20,
            })
            .await
            .unwrap();
        let mut tasks = Vec::new();
        for number in 0..8 {
            let store = store.store.clone();
            let owner = owner.clone();
            let host = host.clone();
            tasks.push(tokio::spawn(async move {
                store
                    .create_instance(
                        test_instance(number, &format!("racer-{number}"), &owner, &host),
                        Duration::ZERO,
                    )
                    .await
            }));
        }
        let mut successes = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(()) => successes += 1,
                Err(Error::Quota { .. }) => {}
                Err(error) => panic!("loser got {error}, want quota error"),
            }
        }
        assert_eq!(successes, 1);
        assert_eq!(store.usage_for(owner.id).await.unwrap().instances, 1);
    }

    #[tokio::test]
    async fn create_instance_no_quota_row_is_unlimited() {
        let store = new_test_store().await;
        let (owner, host) = seed_store(&store).await;
        for number in 0..3 {
            store
                .create_instance(
                    test_instance(number, &format!("inst-{number}"), &owner, &host),
                    Duration::ZERO,
                )
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn instance_round_trip() {
        let clock = FakeClock::new();
        let store = new_test_store_with_clock(clock.clone()).await;
        let (owner, host) = seed_store(&store).await;
        let mut expected = test_instance(1, "web", &owner, &host);
        expected.nested = true;
        expected.ksm = false;
        expected.http_port = 3000;
        expected.visibility = Visibility::Public;
        store
            .create_instance(expected.clone(), Duration::ZERO)
            .await
            .unwrap();
        expected.created_at = clock.now();
        assert_eq!(store.instance(&expected.uuid).await.unwrap(), expected);
        assert_eq!(store.instance_by_name("web").await.unwrap(), expected);
    }

    #[tokio::test]
    async fn delete_instance_releases_name_and_shares() {
        let store = new_test_store().await;
        let (owner, host) = seed_store(&store).await;
        let other = store
            .register_user("bob", "bob@example.org", None, test_range())
            .await
            .unwrap();
        let instance = test_instance(1, "web", &owner, &host);
        store
            .create_instance(instance.clone(), Duration::ZERO)
            .await
            .unwrap();
        store.add_share(&instance.uuid, other.id).await.unwrap();
        let deleted = store.delete_instance(&instance.uuid).await.unwrap();
        assert_eq!(deleted.uuid, instance.uuid);
        assert_eq!(deleted.name, "web");
        assert!(matches!(
            store.instance(&instance.uuid).await,
            Err(Error::NotFound)
        ));
        assert!(store.shares_for(&instance.uuid).await.unwrap().is_empty());
        assert_eq!(
            store.released_name("web").await.unwrap().previous_owner_id,
            owner.id
        );
        assert!(matches!(
            store.delete_instance("no-such-uuid").await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn observed_state_batch_and_restore_list() {
        let store = new_test_store().await;
        let (owner, host) = seed_store(&store).await;
        for (number, name) in ["a", "b", "c"].into_iter().enumerate() {
            store
                .create_instance(test_instance(number, name, &owner, &host), Duration::ZERO)
                .await
                .unwrap();
        }
        store
            .set_desired_state("uuid-001", DesiredState::Stopped)
            .await
            .unwrap();
        store
            .update_observed_states(HashMap::from([
                ("uuid-000".into(), State::Stopped),
                ("uuid-001".into(), State::Stopped),
                ("uuid-002".into(), State::Running),
                ("unknown".into(), State::Running),
            ]))
            .await
            .unwrap();
        let restore = store.instances_to_restore().await.unwrap();
        assert_eq!(restore.len(), 1);
        assert_eq!(restore[0].uuid, "uuid-000");
        assert_eq!(
            store.instance("uuid-002").await.unwrap().state,
            State::Running
        );
    }

    #[tokio::test]
    async fn listings_and_setters() {
        let store = new_test_store().await;
        let (owner, host) = seed_store(&store).await;
        let other = store
            .register_user("bob", "bob@example.org", None, test_range())
            .await
            .unwrap();
        let mine = test_instance(1, "mine", &owner, &host);
        let theirs = test_instance(2, "theirs", &other, &host);
        store
            .create_instance(mine.clone(), Duration::ZERO)
            .await
            .unwrap();
        store.create_instance(theirs, Duration::ZERO).await.unwrap();
        assert_eq!(store.instances().await.unwrap().len(), 2);
        let owned = store.instances_by_owner(owner.id).await.unwrap();
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].uuid, mine.uuid);
        store
            .set_visibility(&mine.uuid, Visibility::Private)
            .await
            .unwrap();
        store.set_http_port(&mine.uuid, 8080).await.unwrap();
        let updated = store.instance(&mine.uuid).await.unwrap();
        assert_eq!(updated.visibility, Visibility::Private);
        assert_eq!(updated.http_port, 8080);
        assert!(matches!(
            store.set_visibility("no-such", Visibility::Off).await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn touch_last_seen() {
        let clock = FakeClock::new();
        let store = new_test_store_with_clock(clock.clone()).await;
        let (owner, host) = seed_store(&store).await;
        let instance = test_instance(1, "web", &owner, &host);
        store
            .create_instance(instance.clone(), Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(
            store.instance(&instance.uuid).await.unwrap().last_seen_at,
            None
        );
        clock.advance(TimeDuration::minutes(90));
        store.touch_last_seen(&instance.uuid).await.unwrap();
        assert_eq!(
            store.instance(&instance.uuid).await.unwrap().last_seen_at,
            Some(clock.now())
        );
    }

    #[tokio::test]
    async fn resize_quota() {
        let store = new_test_store().await;
        let (owner, host) = seed_store(&store).await;
        store
            .set_quota(Quota {
                user_id: owner.id,
                max_instances: 2,
                max_vcpu: 4,
                max_memory_mib: 4096,
                max_disk_gib: 40,
            })
            .await
            .unwrap();
        let mut instance = test_instance(1, "web", &owner, &host);
        instance.vcpu = 2;
        instance.memory_mib = 2048;
        instance.disk_gib = 20;
        store
            .create_instance(instance.clone(), Duration::ZERO)
            .await
            .unwrap();
        store
            .resize(&instance.uuid, 4, 4096, 40, true)
            .await
            .unwrap();
        let resized = store.instance(&instance.uuid).await.unwrap();
        assert_eq!(resized.vcpu, 4);
        assert_eq!(resized.memory_mib, 4096);
        assert_eq!(resized.disk_gib, 40);
        assert!(resized.nested);
        assert!(matches!(
            store.resize(&instance.uuid, 5, 4096, 40, true).await,
            Err(Error::Quota { limit: "vcpu", .. })
        ));
    }
}
