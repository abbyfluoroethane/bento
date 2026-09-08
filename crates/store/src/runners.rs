//! Runners, slots, and the controller lease (MULTI-NODE 7.2, 11.3, 19).
//!
//! Three things live here that version 1 had no need of. The deployment
//! row holds the runner prefix, which says how many slots a user `/24`
//! divides into. The slot rows say which runner owns each subprefix. The
//! lease row says which controller process may dispatch at all.
//!
//! The lease is the fence. A runner records the highest controller epoch
//! it has accepted and refuses every lower one after that, so a second
//! controller cannot drive a runner behind the first one's back. Every
//! acquisition raises the epoch, which is why acquiring is a write and
//! not a read.

use std::time::Duration;

use bento_types::{Deployment, Host, Lease, Placing, Slot};
use rusqlite::{OptionalExtension, Transaction, params};

use crate::{Error, Result, Store, format_time, parse_time};

/// Every column of `hosts`, in the order [`crate::hosts::scan_host`] reads.
pub(crate) const HOST_COLUMNS: &str = "id, machine_id, name, libvirt_uri, endpoint, underlay, \
     enabled, placement, created_at";

const SLOT_COLUMNS: &str = "slot, state, owner_host_id, ownership_epoch, \
     source_host_id, destination_host_id, operation_id";

impl Store {
    /// The deployment settings. The row always exists; `schema.sql`
    /// writes it when the database is created.
    pub async fn deployment(&self) -> Result<Deployment> {
        self.with_conn(move |conn| {
            let prefix: i64 = conn.query_row(
                "SELECT runner_prefix FROM deployment WHERE id = 1",
                [],
                |row| row.get(0),
            )?;
            Ok(Deployment {
                runner_prefix: prefix as u8,
            })
        })
        .await
    }

    /// Persists the runner prefix (MULTI-NODE 19).
    ///
    /// Setup calls this once. A later change is the whole workflow of
    /// section 17, so the one thing refused here is a prefix that would
    /// leave an existing slot with no number: dropping from `/26` to
    /// `/25` while slot 3 is owned would strand slot 3 and the addresses
    /// it supplied.
    pub async fn set_runner_prefix(&self, prefix: u8) -> Result<Deployment> {
        if !(24..=27).contains(&prefix) {
            return Err(Error::RunnerPrefix(prefix));
        }
        self.with_tx(move |tx| {
            let deployment = Deployment {
                runner_prefix: prefix,
            };
            let highest: Option<i64> =
                tx.query_row("SELECT MAX(slot) FROM runner_slots", [], |row| row.get(0))?;
            if let Some(highest) = highest
                && highest >= deployment.slot_count()
            {
                return Err(Error::RunnerPrefixStrandsSlot {
                    prefix,
                    slot: highest,
                });
            }
            tx.execute(
                "UPDATE deployment SET runner_prefix = ? WHERE id = 1",
                [prefix],
            )?;
            Ok(deployment)
        })
        .await
    }

    /// Takes the controller lease, or reports who holds it.
    ///
    /// The write transaction is what makes this safe: the check and the
    /// claim cannot interleave with another process. An unexpired lease
    /// held by somebody else is refused. Anything else is taken, and the
    /// epoch goes up, which fences every runner against the process that
    /// held it before.
    pub async fn acquire_lease(
        &self,
        holder_id: impl Into<String>,
        ttl: Duration,
    ) -> Result<Lease> {
        let holder_id = holder_id.into();
        let now = self.clock();
        self.with_tx(move |tx| {
            let now = now();
            let held: Option<(Option<String>, Option<String>)> = tx
                .query_row(
                    "SELECT holder_id, expires_at FROM controller_lease WHERE id = 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((Some(holder), Some(expires))) = held {
                let expires_at = parse_time(1, &expires)?;
                if holder != holder_id && expires_at > now {
                    return Err(Error::LeaseHeld { holder, expires_at });
                }
            }

            let expires_at = now + ttl;
            tx.execute(
                "UPDATE controller_lease \
                 SET holder_id = ?, epoch = epoch + 1, expires_at = ? WHERE id = 1",
                params![holder_id, format_time(expires_at)?],
            )?;
            let epoch: i64 = tx.query_row(
                "SELECT epoch FROM controller_lease WHERE id = 1",
                [],
                |row| row.get(0),
            )?;
            Ok(Lease {
                holder_id,
                epoch,
                expires_at,
            })
        })
        .await
    }

    /// Extends a lease this process still holds.
    ///
    /// The epoch does not move. A renewal is the same controller saying
    /// it is still alive, and raising the epoch would fence runners
    /// against work this same process has in flight.
    /// Returns the next generation to dispatch for one object
    /// (MULTI-NODE 11.3).
    ///
    /// A runner refuses two orders that claim one generation but want
    /// different things. The generation must therefore increase across
    /// every process that dispatches, not within one of them: `serve`
    /// and the SSH frontend both change instances, and two in-memory
    /// counters would both start at one and collide on their first order
    /// for the same object.
    ///
    /// The epoch floors it, so a controller that restored an older
    /// database still dispatches above whatever a runner already
    /// accepted from the controller before it.
    pub async fn next_dispatch_generation(
        &self,
        object_id: impl Into<String>,
        epoch: i64,
    ) -> Result<i64> {
        let object_id = object_id.into();
        let floor = epoch.saturating_mul(1_000_000);
        self.with_tx(move |tx| {
            let generation: i64 = tx.query_row(
                "INSERT INTO dispatch_generations (object_id, generation) VALUES (?, ?) \
                 ON CONFLICT(object_id) DO UPDATE SET \
                   generation = max(dispatch_generations.generation + 1, excluded.generation) \
                 RETURNING generation",
                params![object_id, floor + 1],
                |row| row.get(0),
            )?;
            Ok(generation)
        })
        .await
    }

    /// Reads the controller lease without taking it.
    ///
    /// A process that is not the controller still has to name the current
    /// controller and its epoch when it sends a runner a change, because
    /// a runner refuses an epoch older than the highest it has accepted
    /// (MULTI-NODE 11.3). Reading the lease is how such a process speaks
    /// as the deployment rather than as a second controller.
    ///
    /// Returns `None` when no controller holds it.
    pub async fn lease(&self) -> Result<Option<Lease>> {
        self.with_conn(move |conn| {
            let row: Option<(Option<String>, i64, Option<String>)> = conn
                .query_row(
                    "SELECT holder_id, epoch, expires_at FROM controller_lease WHERE id = 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((Some(holder_id), epoch, Some(expires_at))) = row else {
                return Ok(None);
            };
            Ok(Some(Lease {
                holder_id,
                epoch,
                expires_at: parse_time(2, &expires_at)?,
            }))
        })
        .await
    }

    pub async fn renew_lease(&self, holder_id: impl Into<String>, ttl: Duration) -> Result<Lease> {
        let holder_id = holder_id.into();
        let now = self.clock();
        self.with_tx(move |tx| {
            let now = now();
            let expires_at = now + ttl;
            let changed = tx.execute(
                "UPDATE controller_lease SET expires_at = ? \
                 WHERE id = 1 AND holder_id = ? AND expires_at > ?",
                params![format_time(expires_at)?, holder_id, format_time(now)?],
            )?;
            if changed == 0 {
                return Err(Error::LeaseLost);
            }
            let epoch: i64 = tx.query_row(
                "SELECT epoch FROM controller_lease WHERE id = 1",
                [],
                |row| row.get(0),
            )?;
            Ok(Lease {
                holder_id,
                epoch,
                expires_at,
            })
        })
        .await
    }

    /// Gives up the lease, so the next controller need not wait it out.
    /// The epoch stays where it is; the next acquisition raises it.
    pub async fn release_lease(&self, holder_id: impl Into<String>) -> Result<()> {
        let holder_id = holder_id.into();
        self.with_tx(move |tx| {
            tx.execute(
                "UPDATE controller_lease SET holder_id = NULL, expires_at = NULL \
                 WHERE id = 1 AND holder_id = ?",
                [holder_id],
            )?;
            Ok(())
        })
        .await
    }

    /// Every host, oldest first.
    pub async fn hosts(&self) -> Result<Vec<Host>> {
        self.with_conn(move |conn| {
            let mut statement =
                conn.prepare(&format!("SELECT {HOST_COLUMNS} FROM hosts ORDER BY id"))?;
            let rows = statement.query_map([], crate::hosts::scan_host)?;
            Ok(rows.collect::<rusqlite::Result<Vec<Host>>>()?)
        })
        .await
    }

    /// Records a runner named in the configuration (MULTI-NODE 19).
    ///
    /// The row exists before the machine is reachable, so it carries no
    /// machine ID yet. The runner service on that machine reports one,
    /// and from then on the machine ID is the identity and this name is
    /// a label.
    pub async fn register_runner(
        &self,
        name: impl Into<String>,
        endpoint: impl Into<String>,
        underlay: impl Into<String>,
    ) -> Result<Host> {
        let name = name.into();
        let endpoint = endpoint.into();
        let underlay = underlay.into();
        let now = self.clock();
        self.with_tx(move |tx| {
            tx.execute(
                "INSERT INTO hosts (name, libvirt_uri, endpoint, underlay, created_at) \
                 VALUES (?, '', ?, ?, ?) \
                 ON CONFLICT(name) DO UPDATE SET endpoint = excluded.endpoint, \
                   underlay = excluded.underlay",
                params![name, endpoint, underlay, format_time(now())?],
            )?;
            Ok(tx.query_row(
                &format!("SELECT {HOST_COLUMNS} FROM hosts WHERE name = ?"),
                [&name],
                crate::hosts::scan_host,
            )?)
        })
        .await
    }

    /// Every slot, lowest first.
    pub async fn slots(&self) -> Result<Vec<Slot>> {
        self.with_conn(move |conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {SLOT_COLUMNS} FROM runner_slots ORDER BY slot"
            ))?;
            let rows = statement.query_map([], scan_slot)?;
            Ok(rows.collect::<rusqlite::Result<Vec<Slot>>>()?)
        })
        .await
    }

    /// Gives a slot to a host and raises its ownership epoch.
    ///
    /// This is the plain assignment, used by setup and by an operator
    /// adding a runner to an empty slot. Moving a slot that already holds
    /// instances is the fenced workflow of section 17, not this.
    pub async fn claim_slot(&self, slot: i64, host_id: i64) -> Result<Slot> {
        self.with_tx(move |tx| {
            let prefix: i64 = tx.query_row(
                "SELECT runner_prefix FROM deployment WHERE id = 1",
                [],
                |row| row.get(0),
            )?;
            let count = Deployment {
                runner_prefix: prefix as u8,
            }
            .slot_count();
            if slot < 0 || slot >= count {
                return Err(Error::NoSuchSlot { slot, count });
            }

            // Refuse a claim that would take the slot away from a host
            // whose instances still hold addresses in it. Section 17 is
            // the way to move a slot that is in use.
            let occupied: i64 = tx.query_row(
                "SELECT COUNT(*) FROM instances WHERE slot = ? AND host_id <> ?",
                params![slot, host_id],
                |row| row.get(0),
            )?;
            if occupied > 0 {
                return Err(Error::SlotInUse { slot, occupied });
            }

            tx.execute(
                "INSERT INTO runner_slots (slot, owner_host_id) VALUES (?, ?) \
                 ON CONFLICT(slot) DO UPDATE SET \
                   owner_host_id = excluded.owner_host_id, \
                   ownership_epoch = ownership_epoch + 1, \
                   state = 'active', \
                   source_host_id = NULL, \
                   destination_host_id = NULL, \
                   operation_id = NULL \
                 WHERE owner_host_id <> excluded.owner_host_id",
                params![slot, host_id],
            )?;
            Ok(tx.query_row(
                &format!("SELECT {SLOT_COLUMNS} FROM runner_slots WHERE slot = ?"),
                [slot],
                scan_slot,
            )?)
        })
        .await
    }
}

impl Store {
    /// Records what a runner said, and learns its machine on first
    /// contact (MULTI-NODE 11.1, 16).
    ///
    /// A runner named in the configuration has a name and an endpoint but
    /// no machine ID: the operator wrote the endpoint, not the machine.
    /// The first answer from that endpoint supplies it. Bento accepts it
    /// because the network is the trust boundary (MULTI-NODE 11.1); there
    /// is no enrollment secret to check.
    ///
    /// Once learned, the value never changes silently. A runner that
    /// answers with a different machine ID means the endpoint now points
    /// at another machine, which is an operator mistake and not a
    /// migration. That host is marked `mismatched` and keeps its old
    /// identity, so nothing is placed on the wrong machine.
    pub async fn observe_host(&self, host_id: i64, seen: HostSeen) -> Result<HostHealth> {
        let now = self.clock();
        self.with_tx(move |tx| {
            let known: Option<String> = tx.query_row(
                "SELECT machine_id FROM hosts WHERE id = ?",
                [host_id],
                |row| row.get(0),
            )?;
            let health = match (&known, &seen.machine_id) {
                (_, None) => HostHealth::Unreachable,
                (None, Some(reported)) => {
                    tx.execute(
                        "UPDATE hosts SET machine_id = ? WHERE id = ?",
                        params![reported, host_id],
                    )?;
                    HostHealth::Ok
                }
                (Some(stored), Some(reported)) if stored == reported => HostHealth::Ok,
                (Some(_), Some(_)) => HostHealth::Mismatched,
            };

            let contact = match health {
                HostHealth::Ok => Some(format_time(now())?),
                _ => None,
            };
            tx.execute(
                "INSERT INTO host_observations (host_id, health, last_contact_at, \
                   accepted_epoch, arch, cpu_count, memory_total_mib, storage_total_gib, \
                   storage_available_gib, hypervisor_version, last_error) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(host_id) DO UPDATE SET \
                   health = excluded.health, \
                   accepted_epoch = excluded.accepted_epoch, \
                   arch = COALESCE(excluded.arch, arch), \
                   cpu_count = COALESCE(excluded.cpu_count, cpu_count), \
                   memory_total_mib = COALESCE(excluded.memory_total_mib, memory_total_mib), \
                   storage_total_gib = COALESCE(excluded.storage_total_gib, storage_total_gib), \
                   storage_available_gib = \
                     COALESCE(excluded.storage_available_gib, storage_available_gib), \
                   hypervisor_version = \
                     COALESCE(excluded.hypervisor_version, hypervisor_version), \
                   last_error = excluded.last_error, \
                   -- A failed call must not erase when the runner was
                   -- last well. That time is how an operator sees how
                   -- long it has been gone.
                   last_contact_at = COALESCE(excluded.last_contact_at, last_contact_at)",
                params![
                    host_id,
                    health.as_str(),
                    contact,
                    seen.accepted_epoch,
                    seen.arch,
                    seen.cpu_count,
                    seen.memory_total_mib,
                    seen.storage_total_gib,
                    seen.storage_available_gib,
                    seen.hypervisor_version,
                    seen.error,
                ],
            )?;
            Ok(health)
        })
        .await
    }

    /// What the controller last saw of one host, if it has ever called it.
    pub async fn host_observation(&self, host_id: i64) -> Result<Option<Observation>> {
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT health, last_contact_at, accepted_epoch, arch, cpu_count, \
                       memory_total_mib, storage_total_gib, storage_available_gib, \
                       hypervisor_version, last_error \
                     FROM host_observations WHERE host_id = ?",
                    [host_id],
                    scan_observation,
                )
                .optional()?)
        })
        .await
    }
}

/// What one call to a runner produced. A failed call carries `error` and
/// leaves the rest empty.
#[derive(Debug, Clone, Default)]
pub struct HostSeen {
    pub machine_id: Option<String>,
    pub accepted_epoch: i64,
    pub arch: Option<String>,
    pub cpu_count: Option<i64>,
    pub memory_total_mib: Option<i64>,
    pub storage_total_gib: Option<i64>,
    pub storage_available_gib: Option<i64>,
    pub hypervisor_version: Option<String>,
    pub error: Option<String>,
}

/// How a host looked when the controller last called it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HostHealth {
    /// Never called.
    #[default]
    Unknown,
    Ok,
    Unreachable,
    /// The endpoint answered as a different machine than the one this row
    /// records. Bento places nothing there until an operator fixes it.
    Mismatched,
}

impl HostHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            HostHealth::Unknown => "unknown",
            HostHealth::Ok => "ok",
            HostHealth::Unreachable => "unreachable",
            HostHealth::Mismatched => "mismatched",
        }
    }

    /// Whether the controller may place a new instance here. Only a host
    /// that answered as itself qualifies.
    pub fn placeable(self) -> bool {
        matches!(self, HostHealth::Ok)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub health: HostHealth,
    pub last_contact_at: Option<time::OffsetDateTime>,
    pub accepted_epoch: i64,
    pub arch: Option<String>,
    pub cpu_count: Option<i64>,
    pub memory_total_mib: Option<i64>,
    pub storage_total_gib: Option<i64>,
    pub storage_available_gib: Option<i64>,
    pub hypervisor_version: Option<String>,
    pub last_error: Option<String>,
}

fn scan_observation(row: &rusqlite::Row<'_>) -> rusqlite::Result<Observation> {
    let health: String = row.get(0)?;
    let contact: Option<String> = row.get(1)?;
    Ok(Observation {
        health: match health.as_str() {
            "ok" => HostHealth::Ok,
            "unreachable" => HostHealth::Unreachable,
            "mismatched" => HostHealth::Mismatched,
            _ => HostHealth::Unknown,
        },
        last_contact_at: contact.as_deref().map(|t| parse_time(1, t)).transpose()?,
        accepted_epoch: row.get(2)?,
        arch: row.get(3)?,
        cpu_count: row.get(4)?,
        memory_total_mib: row.get(5)?,
        storage_total_gib: row.get(6)?,
        storage_available_gib: row.get(7)?,
        hypervisor_version: row.get(8)?,
        last_error: row.get(9)?,
    })
}

/// The one shape a checksum is stored in: lowercase hex, no prefix.
///
/// `image_versions.checksum` has always been bare hex, and the runner
/// protocol reports `sha256-<hex>`. Both reach `host_images`, so both are
/// reduced here. Without this the same version reads as two, and the
/// fleet gate refuses a machine that is in fact ready (MULTI-NODE 13.2).
fn bare_checksum(checksum: &str) -> String {
    checksum
        .strip_prefix("sha256-")
        .or_else(|| checksum.strip_prefix("sha256:"))
        .unwrap_or(checksum)
        .to_ascii_lowercase()
}

fn scan_slot(row: &rusqlite::Row<'_>) -> rusqlite::Result<Slot> {
    let state: String = row.get(1)?;
    Ok(Slot {
        slot: row.get(0)?,
        state: crate::instances::parse_enum(1, &state)?,
        owner_host_id: row.get(2)?,
        ownership_epoch: row.get(3)?,
        source_host_id: row.get(4)?,
        destination_host_id: row.get(5)?,
        operation_id: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bento_types::SlotState;

    use crate::Error;
    use crate::tests::new_test_store;

    const KONATA: &str = "ebb80f403ef641deaa486417f2b6992a";
    const TSUKASA: &str = "167eeb6836c44115aa084e7780e4328c";
    const TTL: Duration = Duration::from_secs(30);

    #[tokio::test]
    async fn a_new_database_is_one_slot_wide() {
        let store = new_test_store().await;
        let deployment = store.deployment().await.unwrap();
        assert_eq!(deployment.runner_prefix, 24);
        assert_eq!(deployment.slot_count(), 1, "a /24 is one slot");

        for (prefix, count) in [(25, 2), (26, 4), (27, 8)] {
            let deployment = store.set_runner_prefix(prefix).await.unwrap();
            assert_eq!(deployment.slot_count(), count);
        }
        assert!(matches!(
            store.set_runner_prefix(28).await,
            Err(Error::RunnerPrefix(28))
        ));
    }

    #[tokio::test]
    async fn a_smaller_prefix_may_not_strand_an_owned_slot() {
        let store = new_test_store().await;
        let konata = store
            .ensure_host(KONATA, "konata", "qemu:///system")
            .await
            .unwrap();
        store.set_runner_prefix(26).await.unwrap();
        store.claim_slot(3, konata.id).await.unwrap();

        // /25 has slots 0 and 1. Slot 3 would have no number.
        let error = store.set_runner_prefix(25).await.unwrap_err();
        assert!(
            matches!(error, Error::RunnerPrefixStrandsSlot { slot: 3, .. }),
            "{error}"
        );
        assert_eq!(store.deployment().await.unwrap().runner_prefix, 26);
    }

    #[tokio::test]
    async fn only_one_controller_holds_the_lease_and_every_acquisition_raises_the_epoch() {
        let store = new_test_store().await;
        let first = store.acquire_lease("controller-a", TTL).await.unwrap();
        assert_eq!(first.epoch, 1);

        // A second process is refused while the lease is live.
        let error = store.acquire_lease("controller-b", TTL).await.unwrap_err();
        let Error::LeaseHeld { holder, .. } = &error else {
            panic!("expected the lease to be held: {error}");
        };
        assert_eq!(holder, "controller-a");

        // The holder may take it again, and renewing keeps the epoch.
        assert_eq!(
            store
                .acquire_lease("controller-a", TTL)
                .await
                .unwrap()
                .epoch,
            2
        );
        let renewed = store.renew_lease("controller-a", TTL).await.unwrap();
        assert_eq!(renewed.epoch, 2, "a renewal must not fence its own work");

        // Once released, the next controller takes it and the epoch goes
        // up, which fences every runner against the process before it.
        store.release_lease("controller-a").await.unwrap();
        let second = store.acquire_lease("controller-b", TTL).await.unwrap();
        assert_eq!(second.epoch, 3);
        assert!(matches!(
            store.renew_lease("controller-a", TTL).await,
            Err(Error::LeaseLost)
        ));
    }

    #[tokio::test]
    async fn an_expired_lease_is_taken_by_the_next_controller() {
        // The clock is the store's, so the test moves it rather than
        // waiting. `new_test_store` starts it at a fixed time.
        let store = new_test_store().await;
        store
            .acquire_lease("controller-a", Duration::from_secs(0))
            .await
            .unwrap();
        let taken = store.acquire_lease("controller-b", TTL).await.unwrap();
        assert_eq!(taken.holder_id, "controller-b");
        assert_eq!(taken.epoch, 2);
    }

    #[tokio::test]
    async fn a_slot_is_claimed_by_number_and_the_number_must_exist() {
        let store = new_test_store().await;
        let konata = store
            .ensure_host(KONATA, "konata", "qemu:///system")
            .await
            .unwrap();
        let tsukasa = store
            .register_runner("tsukasa", "https://10.0.0.97:10443", "10.0.0.97")
            .await
            .unwrap();
        assert_eq!(tsukasa.endpoint.as_deref(), Some("https://10.0.0.97:10443"));
        assert_eq!(tsukasa.machine_id, None, "it has not reported a machine");

        // A /24 has only slot 0.
        let error = store.claim_slot(1, tsukasa.id).await.unwrap_err();
        assert!(
            matches!(error, Error::NoSuchSlot { slot: 1, count: 1 }),
            "{error}"
        );

        store.set_runner_prefix(25).await.unwrap();
        let slot = store.claim_slot(0, konata.id).await.unwrap();
        assert_eq!(slot.state, SlotState::Active);
        assert_eq!(slot.owner_host_id, konata.id);
        assert_eq!(slot.ownership_epoch, 0, "the first owner is not a change");

        store.claim_slot(1, tsukasa.id).await.unwrap();
        let slots = store.slots().await.unwrap();
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[1].owner_host_id, tsukasa.id);

        // Handing a slot on raises its ownership epoch.
        let moved = store.claim_slot(1, konata.id).await.unwrap();
        assert_eq!(moved.owner_host_id, konata.id);
        assert_eq!(moved.ownership_epoch, 1);

        // Claiming it again for the same host changes nothing.
        let again = store.claim_slot(1, konata.id).await.unwrap();
        assert_eq!(again.ownership_epoch, 1);
    }

    #[tokio::test]
    async fn a_slot_that_still_holds_instances_of_another_host_is_not_reassigned() {
        let store = new_test_store().await;
        let (owner, konata) = crate::tests::seed_store(&store).await;
        let tsukasa = store
            .ensure_host(TSUKASA, "tsukasa", "qemu:///system")
            .await
            .unwrap();
        store.claim_slot(0, konata.id).await.unwrap();

        let mut instance = crate::tests::test_instance(1, "web", &owner, &konata);
        instance.slot = Some(0);
        store
            .create_instance(instance, Duration::ZERO, bento_types::Capacity::unbounded())
            .await
            .unwrap();

        // Moving a slot that is in use is the section 17 workflow, not a
        // plain reassignment.
        let error = store.claim_slot(0, tsukasa.id).await.unwrap_err();
        assert!(
            matches!(
                error,
                Error::SlotInUse {
                    slot: 0,
                    occupied: 1
                }
            ),
            "{error}"
        );
        assert_eq!(
            store.slots().await.unwrap()[0].owner_host_id,
            konata.id,
            "the refused claim left the slot alone"
        );
    }
}

#[cfg(test)]
mod observation_tests {
    use super::{HostHealth, HostSeen};
    use crate::tests::new_test_store;

    const TSUKASA: &str = "167eeb6836c44115aa084e7780e4328c";
    const KONATA: &str = "ebb80f403ef641deaa486417f2b6992a";

    fn seen(machine_id: &str) -> HostSeen {
        HostSeen {
            machine_id: Some(machine_id.to_string()),
            accepted_epoch: 3,
            arch: Some("aarch64".into()),
            cpu_count: Some(8),
            memory_total_mib: Some(7549),
            storage_total_gib: Some(164),
            storage_available_gib: Some(153),
            hypervisor_version: Some("libvirt local RPC".into()),
            error: None,
        }
    }

    #[tokio::test]
    async fn the_first_answer_from_an_endpoint_supplies_the_machine() {
        // The operator writes a name and an endpoint. Only the machine
        // itself can say which machine it is (MULTI-NODE 11.1).
        let store = new_test_store().await;
        let runner = store
            .register_runner("tsukasa", "http://10.0.0.97:10443", "10.0.0.97")
            .await
            .unwrap();
        assert_eq!(runner.machine_id, None);

        let health = store.observe_host(runner.id, seen(TSUKASA)).await.unwrap();
        assert_eq!(health, HostHealth::Ok);
        assert_eq!(
            store.host(runner.id).await.unwrap().machine_id.as_deref(),
            Some(TSUKASA)
        );

        let observed = store.host_observation(runner.id).await.unwrap().unwrap();
        assert_eq!(observed.accepted_epoch, 3);
        assert_eq!(observed.memory_total_mib, Some(7549));
        assert!(observed.last_contact_at.is_some());
        assert!(observed.health.placeable());
    }

    #[tokio::test]
    async fn an_endpoint_that_answers_as_another_machine_is_not_placeable() {
        // The endpoint was repointed, or two rows share one address. The
        // row keeps the machine it knew, so nothing lands on the wrong
        // machine (MULTI-NODE 16).
        let store = new_test_store().await;
        let runner = store
            .register_runner("tsukasa", "http://10.0.0.97:10443", "10.0.0.97")
            .await
            .unwrap();
        store.observe_host(runner.id, seen(TSUKASA)).await.unwrap();

        let health = store.observe_host(runner.id, seen(KONATA)).await.unwrap();
        assert_eq!(health, HostHealth::Mismatched);
        assert!(!health.placeable());
        assert_eq!(
            store.host(runner.id).await.unwrap().machine_id.as_deref(),
            Some(TSUKASA),
            "the row kept the machine it already knew"
        );
    }

    #[tokio::test]
    async fn a_failed_call_keeps_what_the_runner_last_reported() {
        // An unreachable runner is not an empty runner. The operator
        // needs its last known size and when it was last well.
        let store = new_test_store().await;
        let runner = store
            .register_runner("tsukasa", "http://10.0.0.97:10443", "10.0.0.97")
            .await
            .unwrap();
        store.observe_host(runner.id, seen(TSUKASA)).await.unwrap();
        let well = store.host_observation(runner.id).await.unwrap().unwrap();

        let health = store
            .observe_host(
                runner.id,
                HostSeen {
                    error: Some("connection refused".into()),
                    ..HostSeen::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(health, HostHealth::Unreachable);

        let after = store.host_observation(runner.id).await.unwrap().unwrap();
        assert_eq!(
            after.memory_total_mib,
            Some(7549),
            "its size is still known"
        );
        assert_eq!(
            after.last_contact_at, well.last_contact_at,
            "when it was last well"
        );
        assert_eq!(after.last_error.as_deref(), Some("connection refused"));
        assert!(!after.health.placeable());
    }
}

impl Store {
    /// Chooses the runner a new instance should go on (MULTI-NODE 12).
    ///
    /// A runner is eligible when it is enabled, its placement is active,
    /// the controller last saw it answer as itself, it owns at least one
    /// active slot, its architecture matches, and what it has left can
    /// hold the request.
    ///
    /// Among eligible runners it takes the lowest reserved-memory ratio,
    /// then the lowest reserved-vCPU ratio, then the lowest host id. That
    /// is deterministic and explainable rather than predictive: an
    /// operator can work out why a instance landed where it did
    /// (MULTI-NODE 12).
    pub async fn choose_host(&self, want: Placing, overcommit_ratio: f64) -> Result<Placement> {
        self.with_tx(move |tx| {
            let mut statement = tx.prepare(&format!(
                "SELECT {HOST_COLUMNS} FROM hosts WHERE enabled = 1 AND placement = 'active' \
                 ORDER BY id"
            ))?;
            let hosts = statement
                .query_map([], crate::hosts::scan_host)?
                .collect::<rusqlite::Result<Vec<Host>>>()?;
            drop(statement);

            let mut rejected = Vec::new();
            let mut best: Option<(f64, f64, i64, Host)> = None;
            for host in hosts {
                match weigh_host(tx, &host, &want, overcommit_ratio)? {
                    Ok(weight) => {
                        let candidate = (weight.memory_ratio, weight.vcpu_ratio, host.id, host);
                        if best.as_ref().is_none_or(|current| {
                            (candidate.0, candidate.1, candidate.2)
                                < (current.0, current.1, current.2)
                        }) {
                            best = Some(candidate);
                        }
                    }
                    Err(reason) => rejected.push(format!("{}: {reason}", host.name)),
                }
            }

            match best {
                Some((_, _, _, host)) => Ok(Placement { host }),
                // The refusal names every runner and why each one was
                // refused. "No room" alone tells an operator nothing
                // about which machine to fix.
                None => Err(Error::NoPlacement {
                    reasons: rejected.join("; "),
                }),
            }
        })
        .await
    }
}

/// Where an instance should go.
#[derive(Debug, Clone)]
pub struct Placement {
    pub host: Host,
}

struct Weight {
    memory_ratio: f64,
    vcpu_ratio: f64,
}

/// The columns [`weigh_host`] reads from the last observation.
struct Weighed {
    health: String,
    cpu_count: Option<i64>,
    memory_total: Option<i64>,
    storage_available: Option<i64>,
    arch: Option<String>,
}

/// Decides whether one host can take the request, and how loaded it is.
fn weigh_host(
    tx: &Transaction<'_>,
    host: &Host,
    want: &Placing,
    overcommit_ratio: f64,
) -> rusqlite::Result<std::result::Result<Weight, String>> {
    let observation = tx
        .query_row(
            "SELECT health, cpu_count, memory_total_mib, storage_available_gib, arch \
             FROM host_observations WHERE host_id = ?",
            [host.id],
            |row| {
                Ok(Weighed {
                    health: row.get(0)?,
                    cpu_count: row.get(1)?,
                    memory_total: row.get(2)?,
                    storage_available: row.get(3)?,
                    arch: row.get(4)?,
                })
            },
        )
        .optional()?;
    let Some(Weighed {
        health,
        cpu_count,
        memory_total,
        storage_available,
        arch,
    }) = observation
    else {
        return Ok(Err("never answered the controller".to_string()));
    };
    if health != "ok" {
        return Ok(Err(format!("last seen {health}")));
    }

    // A runner with no slot has no address to give the instance
    // (MULTI-NODE 12).
    let slots: i64 = tx.query_row(
        "SELECT COUNT(*) FROM runner_slots WHERE owner_host_id = ? AND state = 'active'",
        [host.id],
        |row| row.get(0),
    )?;
    if slots == 0 {
        return Ok(Err("owns no active slot".to_string()));
    }

    if let (Some(want_arch), Some(host_arch)) = (&want.arch, &arch)
        && want_arch != host_arch
    {
        return Ok(Err(format!("is {host_arch}, the image is {want_arch}")));
    }

    let (reserved_memory, reserved_vcpu): (i64, i64) = tx.query_row(
        "SELECT COALESCE(SUM(memory), 0), COALESCE(SUM(vcpu), 0) FROM instances WHERE host_id = ?",
        [host.id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let memory_total = memory_total.unwrap_or(0);
    if memory_total <= 0 {
        return Ok(Err("reported no memory".to_string()));
    }
    let memory_limit =
        bento_types::Capacity::from_host(memory_total, 0, overcommit_ratio).memory_mib;
    if reserved_memory + want.memory_mib > memory_limit {
        return Ok(Err(format!(
            "has {} MiB left of {memory_limit}, the instance wants {}",
            memory_limit - reserved_memory,
            want.memory_mib
        )));
    }
    if let Some(available) = storage_available
        && want.disk_gib > available
    {
        return Ok(Err(format!(
            "has {available} GiB free, the instance wants {}",
            want.disk_gib
        )));
    }

    let cpu_count = cpu_count.unwrap_or(0).max(1);
    Ok(Ok(Weight {
        memory_ratio: (reserved_memory + want.memory_mib) as f64 / memory_limit as f64,
        vcpu_ratio: (reserved_vcpu + want.vcpu) as f64 / cpu_count as f64,
    }))
}

#[cfg(test)]
mod placement_tests {
    use std::time::Duration;

    use super::{HostSeen, Placing};
    use crate::Error;
    use crate::tests::new_test_store;

    const KONATA: &str = "ebb80f403ef641deaa486417f2b6992a";
    const TSUKASA: &str = "167eeb6836c44115aa084e7780e4328c";

    fn seen(machine_id: &str, memory_total_mib: i64, storage_available_gib: i64) -> HostSeen {
        HostSeen {
            machine_id: Some(machine_id.to_string()),
            accepted_epoch: 1,
            arch: Some("aarch64".into()),
            cpu_count: Some(8),
            memory_total_mib: Some(memory_total_mib),
            storage_total_gib: Some(storage_available_gib + 10),
            storage_available_gib: Some(storage_available_gib),
            hypervisor_version: Some("QEMU".into()),
            error: None,
        }
    }

    fn want(memory_mib: i64, disk_gib: i64) -> Placing {
        Placing {
            vcpu: 2,
            memory_mib,
            disk_gib,
            arch: Some("aarch64".into()),
        }
    }

    /// Two runners, both healthy, both owning a slot.
    async fn two_runners() -> (crate::tests::TestStore, i64, i64) {
        let store = new_test_store().await;
        store.set_runner_prefix(25).await.unwrap();
        let konata = store
            .ensure_host(KONATA, "konata", "qemu:///system")
            .await
            .unwrap();
        let tsukasa = store
            .register_runner("tsukasa", "http://10.0.0.97:10443", "10.0.0.97")
            .await
            .unwrap();
        store.claim_slot(0, konata.id).await.unwrap();
        store.claim_slot(1, tsukasa.id).await.unwrap();
        store
            .observe_host(konata.id, seen(KONATA, 23527, 8))
            .await
            .unwrap();
        store
            .observe_host(tsukasa.id, seen(TSUKASA, 7549, 153))
            .await
            .unwrap();
        (store, konata.id, tsukasa.id)
    }

    #[tokio::test]
    async fn a_full_runner_gives_way_to_an_empty_one() {
        // This is the case that started the multi-node work: konata is
        // nearly full and tsukasa is empty.
        let (store, konata, tsukasa) = two_runners().await;
        let (owner, _) = crate::tests::seed_store(&store).await;

        // Fill konata to just under its ceiling.
        for index in 0..4 {
            let mut instance = crate::tests::test_instance(
                index + 10,
                &format!("big{index}"),
                &owner,
                &host_row(&store, konata).await,
            );
            instance.memory_mib = 5000;
            instance.disk_gib = 1;
            instance.slot = Some(0);
            store
                .create_instance(
                    instance,
                    std::time::Duration::ZERO,
                    bento_types::Capacity::unbounded(),
                )
                .await
                .unwrap();
        }

        let chosen = store.choose_host(want(2048, 20), 1.0).await.unwrap();
        assert_eq!(chosen.host.id, tsukasa, "the emptier runner takes it");
    }

    async fn host_row(store: &crate::tests::TestStore, id: i64) -> bento_types::Host {
        store.host(id).await.unwrap()
    }

    #[tokio::test]
    async fn a_runner_that_cannot_hold_it_says_why() {
        let (store, _, _) = two_runners().await;
        // More memory than either runner has.
        let error = store.choose_host(want(99_000, 1), 1.0).await.unwrap_err();
        let Error::NoPlacement { reasons } = &error else {
            panic!("expected no placement, got {error}");
        };
        assert!(reasons.contains("konata"), "{reasons}");
        assert!(reasons.contains("tsukasa"), "{reasons}");
        assert!(reasons.contains("MiB left"), "{reasons}");
    }

    #[tokio::test]
    async fn placement_and_creation_share_the_overcommitted_memory_ceiling() {
        let (store, first, second) = two_runners().await;
        let (owner, _) = crate::tests::seed_store(&store).await;
        for (index, host_id, machine) in [(20, first, KONATA), (21, second, TSUKASA)] {
            store
                .observe_host(host_id, seen(machine, 4097, 100))
                .await
                .unwrap();
            let mut instance = crate::tests::test_instance(
                index,
                &format!("existing-{index}"),
                &owner,
                &host_row(&store, host_id).await,
            );
            instance.memory_mib = 4096;
            instance.disk_gib = 1;
            store
                .create_instance(instance, Duration::ZERO, bento_types::Capacity::unbounded())
                .await
                .unwrap();
        }

        assert!(matches!(
            store.choose_host(want(2049, 1), 1.0).await,
            Err(Error::NoPlacement { .. })
        ));
        // The fractional MiB is truncated by both checks: 4097 * 1.5 = 6145.5.
        let chosen = store.choose_host(want(2049, 1), 1.5).await.unwrap();
        assert_eq!(
            chosen.host.id, first,
            "equal weights keep the stable host order"
        );
        let error = store.choose_host(want(2050, 1), 1.5).await.unwrap_err();
        let Error::NoPlacement { reasons } = error else {
            panic!("expected placement refusal")
        };
        assert!(
            reasons.contains("konata: has 2049 MiB left of 6145"),
            "{reasons}"
        );
        assert!(
            reasons.contains("tsukasa: has 2049 MiB left of 6145"),
            "{reasons}"
        );
        assert!(
            matches!(
                store.choose_host(want(2049, 101), 1.5).await,
                Err(Error::NoPlacement { .. })
            ),
            "overcommit does not increase free disk"
        );

        let mut instance = crate::tests::test_instance(22, "overcommitted", &owner, &chosen.host);
        instance.memory_mib = 2049;
        instance.disk_gib = 1;
        store
            .create_instance(
                instance,
                Duration::ZERO,
                bento_types::Capacity::from_host(4097, 110, 1.5),
            )
            .await
            .unwrap();
        assert_eq!(
            store.choose_host(want(2049, 1), 1.5).await.unwrap().host.id,
            second
        );
    }

    #[tokio::test]
    async fn an_unreachable_runner_is_not_chosen() {
        let (store, konata, tsukasa) = two_runners().await;
        store
            .observe_host(
                tsukasa,
                HostSeen {
                    error: Some("connection refused".into()),
                    ..HostSeen::default()
                },
            )
            .await
            .unwrap();
        let chosen = store.choose_host(want(1024, 1), 1.0).await.unwrap();
        assert_eq!(chosen.host.id, konata);
    }

    #[tokio::test]
    async fn a_runner_of_another_architecture_is_not_chosen() {
        let (store, _, tsukasa) = two_runners().await;
        let mut other = seen(TSUKASA, 7549, 153);
        other.arch = Some("x86_64".into());
        store.observe_host(tsukasa, other).await.unwrap();

        let chosen = store.choose_host(want(1024, 1), 1.0).await.unwrap();
        assert_ne!(
            chosen.host.id, tsukasa,
            "an aarch64 image needs an aarch64 runner"
        );
    }

    #[tokio::test]
    async fn a_dispatch_generation_never_repeats_across_processes() {
        // `serve` and the SSH frontend both change instances. Two
        // in-memory counters would both start at one and give the same
        // generation to two different orders, which a runner refuses as a
        // conflict because it cannot tell which is right
        // (MULTI-NODE 11.3).
        let store = new_test_store().await;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..5 {
            for _ in 0..2 {
                let generation = store.next_dispatch_generation("web", 3).await.unwrap();
                assert!(
                    seen.insert(generation),
                    "generation {generation} was handed out twice"
                );
            }
        }
        // Strictly increasing, and floored by the epoch.
        let mut sorted: Vec<_> = seen.into_iter().collect();
        sorted.sort_unstable();
        assert!(sorted[0] > 3 * 1_000_000);
        assert!(sorted.windows(2).all(|pair| pair[1] > pair[0]));

        // A different object keeps its own count.
        let other = store.next_dispatch_generation("db", 3).await.unwrap();
        assert_eq!(other, 3 * 1_000_000 + 1);
    }

    #[tokio::test]
    async fn a_later_epoch_lifts_the_generation_above_the_old_one() {
        // A controller that took the lease after another must dispatch
        // above whatever the runner already accepted from its
        // predecessor, even for an object it has never touched.
        let store = new_test_store().await;
        let first = store.next_dispatch_generation("web", 1).await.unwrap();
        let second = store.next_dispatch_generation("web", 9).await.unwrap();
        assert!(second > first, "{second} did not rise above {first}");
        assert!(second > 9 * 1_000_000);
    }

    #[tokio::test]
    async fn the_first_machine_takes_slot_zero_by_itself() {
        // A deployment with no slot has never divided its /24, so every
        // address belongs to slot 0 and the one machine running them owns
        // it (MULTI-NODE 7.2). Without this a new deployment could place
        // nothing: allocation asks which slot the machine owns.
        let store = new_test_store().await;
        let konata = store
            .ensure_host(KONATA, "konata", "qemu:///system")
            .await
            .unwrap();
        let slots = store.slots().await.unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!((slots[0].slot, slots[0].owner_host_id), (0, konata.id));

        // A second machine does not take a slot this way. It joins the
        // fleet with none until an operator gives it one, which is what
        // makes it ineligible for placement below.
        let tsukasa = store
            .ensure_host(TSUKASA, "tsukasa", "qemu:///system")
            .await
            .unwrap();
        let slots = store.slots().await.unwrap();
        assert_eq!(slots.len(), 1, "a second machine claimed a slot");
        assert_eq!(slots[0].owner_host_id, konata.id);

        for (host, machine) in [(konata.id, KONATA), (tsukasa.id, TSUKASA)] {
            store
                .observe_host(host, seen(machine, 23527, 100))
                .await
                .unwrap();
        }
        // Both machines are healthy, but only the one holding a slot has
        // addresses to give.
        for _ in 0..4 {
            let chosen = store.choose_host(want(1024, 1), 1.0).await.unwrap();
            assert_eq!(
                chosen.host.id, konata.id,
                "a machine with no slot was chosen"
            );
        }
    }
}

impl Store {
    /// Records that one machine holds one image version (MULTI-NODE 13.2).
    ///
    /// This adds; it never replaces. A machine keeps every version its
    /// own overlays are backed by, because a `base_checksum` is a qcow2
    /// backing file and deleting it breaks the instance built on it
    /// (SPEC 5.1).
    pub async fn record_host_image(
        &self,
        host_id: i64,
        image_name: impl Into<String>,
        checksum: impl Into<String>,
    ) -> Result<()> {
        let image_name = image_name.into();
        let checksum = bare_checksum(&checksum.into());
        let now = self.clock();
        self.with_tx(move |tx| {
            tx.execute(
                "INSERT INTO host_images (host_id, image_name, checksum, verified_at) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT(host_id, image_name, checksum) DO UPDATE SET \
                   verified_at = excluded.verified_at",
                params![host_id, image_name, checksum, format_time(now())?],
            )?;
            Ok(())
        })
        .await
    }

    /// The machines that should hold `checksum` for `image_name` and do
    /// not (MULTI-NODE 13.2).
    ///
    /// This is the fleet gate. A deployment has one current version of
    /// each image, and a create uses it, so every machine that can take
    /// an instance must hold it first. Until they all do, creates wait.
    /// That keeps `base_checksum` one value for the whole deployment
    /// rather than a different one on each machine.
    ///
    /// A disabled or draining machine is not counted: it takes no new
    /// instance, so it does not need the new version to arrive first.
    pub async fn hosts_missing_image(
        &self,
        image_name: impl Into<String>,
        checksum: impl Into<String>,
    ) -> Result<Vec<String>> {
        let image_name = image_name.into();
        let checksum = bare_checksum(&checksum.into());
        self.with_conn(move |conn| {
            let mut statement = conn.prepare(
                "SELECT h.name FROM hosts h \
                 WHERE h.enabled = 1 AND h.placement = 'active' \
                   AND NOT EXISTS ( \
                     SELECT 1 FROM host_images i \
                     WHERE i.host_id = h.id AND i.image_name = ? AND i.checksum = ? \
                   ) \
                 ORDER BY h.name",
            )?;
            let rows = statement.query_map(params![image_name, checksum], |row| row.get(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
        })
        .await
    }

    /// Records every image version the controller's own machine holds.
    ///
    /// The controller fetches images to its own image directory, so the
    /// rows in `image_versions` describe files on that machine. Without
    /// this the fleet gate would refuse every create on a deployment that
    /// has only ever had one host, because nothing would ever have said
    /// that host holds anything (MULTI-NODE 13.2).
    pub async fn record_local_image_versions(&self, host_id: i64) -> Result<usize> {
        let now = self.clock();
        self.with_tx(move |tx| {
            let recorded = tx.execute(
                "INSERT INTO host_images (host_id, image_name, checksum, verified_at) \
                 SELECT ?, image_name, checksum, ? FROM image_versions \
                 WHERE true \
                 ON CONFLICT(host_id, image_name, checksum) DO NOTHING",
                params![host_id, format_time(now())?],
            )?;
            Ok(recorded)
        })
        .await
    }

    /// Every image version one machine holds, newest record first.
    pub async fn host_images(&self, host_id: i64) -> Result<Vec<(String, String)>> {
        self.with_conn(move |conn| {
            let mut statement = conn.prepare(
                "SELECT image_name, checksum FROM host_images WHERE host_id = ? \
                 ORDER BY image_name, verified_at DESC",
            )?;
            let rows = statement.query_map([host_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<(String, String)>>>()?)
        })
        .await
    }
}

#[cfg(test)]
mod image_tests {
    use crate::tests::new_test_store;

    const KONATA: &str = "ebb80f403ef641deaa486417f2b6992a";
    const OLD: &str = "sha256-3ddb4bb4";
    const NEW: &str = "sha256-f580e185";

    #[tokio::test]
    async fn one_version_is_one_row_whichever_shape_it_arrives_in() {
        // The store keeps bare hex; the runner protocol reports
        // `sha256-<hex>`. Both must mean the same version.
        let store = new_test_store().await;
        let konata = store
            .ensure_host(KONATA, "konata", "qemu:///system")
            .await
            .unwrap();
        store
            .record_host_image(konata.id, "debian-13", "sha256-ABCD")
            .await
            .unwrap();
        store
            .record_host_image(konata.id, "debian-13", "abcd")
            .await
            .unwrap();
        assert_eq!(store.host_images(konata.id).await.unwrap().len(), 1);
        assert!(
            store
                .hosts_missing_image("debian-13", "sha256-abcd")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_machine_holds_a_set_of_versions_not_one() {
        // konata's running instances are backed by the older build. It
        // gains the newer one; it does not swap (SPEC 5.1).
        let store = new_test_store().await;
        let konata = store
            .ensure_host(KONATA, "konata", "qemu:///system")
            .await
            .unwrap();
        store
            .record_host_image(konata.id, "debian-13", OLD)
            .await
            .unwrap();
        store
            .record_host_image(konata.id, "debian-13", NEW)
            .await
            .unwrap();

        let held = store.host_images(konata.id).await.unwrap();
        assert_eq!(held.len(), 2, "the older build was replaced: {held:?}");
        // Stored bare, whichever shape they arrived in.
        assert!(held.iter().any(|(_, checksum)| checksum == "3ddb4bb4"));
        assert!(held.iter().any(|(_, checksum)| checksum == "f580e185"));
    }

    #[tokio::test]
    async fn creates_wait_until_every_machine_has_the_current_version() {
        let store = new_test_store().await;
        let konata = store
            .ensure_host(KONATA, "konata", "qemu:///system")
            .await
            .unwrap();
        let tsukasa = store
            .register_runner("tsukasa", "http://10.0.0.97:10443", "10.0.0.97")
            .await
            .unwrap();

        // Nobody has it yet.
        let missing = store.hosts_missing_image("debian-13", NEW).await.unwrap();
        assert_eq!(missing, vec!["konata", "tsukasa"]);

        store
            .record_host_image(tsukasa.id, "debian-13", NEW)
            .await
            .unwrap();
        assert_eq!(
            store.hosts_missing_image("debian-13", NEW).await.unwrap(),
            vec!["konata"],
            "the fleet is not ready while one machine lacks it"
        );

        store
            .record_host_image(konata.id, "debian-13", NEW)
            .await
            .unwrap();
        assert!(
            store
                .hosts_missing_image("debian-13", NEW)
                .await
                .unwrap()
                .is_empty(),
            "the fleet has converged"
        );
    }

    #[tokio::test]
    async fn a_machine_that_takes_no_instances_does_not_hold_the_fleet_back() {
        let store = new_test_store().await;
        let konata = store
            .ensure_host(KONATA, "konata", "qemu:///system")
            .await
            .unwrap();
        let draining = store
            .register_runner("old-node", "http://10.0.0.98:10443", "10.0.0.98")
            .await
            .unwrap();
        store
            .record_host_image(konata.id, "debian-13", NEW)
            .await
            .unwrap();

        assert_eq!(
            store.hosts_missing_image("debian-13", NEW).await.unwrap(),
            vec!["old-node"]
        );
        store
            .with_tx(move |tx| {
                tx.execute(
                    "UPDATE hosts SET placement = 'draining' WHERE id = ?",
                    [draining.id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            store
                .hosts_missing_image("debian-13", NEW)
                .await
                .unwrap()
                .is_empty(),
            "a draining machine takes no new instance, so it need not be ready"
        );
    }
}
