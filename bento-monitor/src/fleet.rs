//! Every machine of the deployment, not only the one the screen runs on
//! (MULTI-NODE 20).
//!
//! A runner answers only the controller. Its one endpoint is fenced: a
//! request must carry the controller epoch and an unexpired lease, and a
//! runner refuses everything else (MULTI-NODE 11.3). The monitor holds no
//! lease and must never take one, because taking one raises the epoch and
//! fences the running control plane out of its own fleet.
//!
//! So the monitor does not call runners. It reads what the controller
//! already recorded about them. The controller polls every runner on a
//! thirty-second tick and writes what it saw into `host_observations`
//! (MULTI-NODE 20), which is exactly the per-runner report an operator
//! wants and costs the fleet nothing to read.
//!
//! On a machine that runs no control plane there is no such database. That
//! machine reads its own fence instead: the controller epoch it has
//! accepted is what says which controller it is following.

use std::path::Path;

use bento_config::Config;
use bento_store::{HostHealth, Observation, Store};
use bento_types::{Deployment, Host, Lease, Placement, Slot, SlotState};
use time::OffsetDateTime;

use crate::role::Role;

/// What the Fleet screen shows, which depends on what this machine is.
#[derive(Debug)]
pub enum View {
    /// The controller's record of every machine.
    Controller(Result<Fleet, String>),
    /// A runner has no fleet record. It knows which controller it follows.
    Runner(Result<Fence, String>),
}

impl View {
    /// Reads the fleet for `role`. The libvirt census and the host sample
    /// are read elsewhere; this is only what the deployment knows.
    pub fn read(
        runtime: &tokio::runtime::Runtime,
        role: Role,
        config: Option<&Config>,
        machine_id: Option<&str>,
    ) -> View {
        let Some(config) = config else {
            let reason = "the configuration has not loaded, so nothing here knows where \
                          the deployment state is. The Config tab says why."
                .to_string();
            return match role {
                Role::Controller => View::Controller(Err(reason)),
                Role::Runner => View::Runner(Err(reason)),
            };
        };
        match role {
            Role::Controller => View::Controller(Fleet::read(runtime, &config.db_path, machine_id)),
            Role::Runner => View::Runner(Fence::read(config, machine_id)),
        }
    }
}

/// The deployment as the controller last saw it.
#[derive(Debug)]
pub struct Fleet {
    pub deployment: Deployment,
    pub lease: Option<Lease>,
    pub runners: Vec<Runner>,
    /// Instances across every machine. This is a count of rows, not a
    /// capacity: memory added across machines that cannot share it
    /// describes a machine that does not exist (MULTI-NODE 20).
    pub instances: usize,
    /// Instances whose `host_id` matches no host row.
    pub orphans: usize,
    /// Allowlist entries that have a current version to hold.
    pub images: usize,
}

impl Fleet {
    /// Reads one snapshot from the controller database.
    ///
    /// The database is opened read-only for each snapshot and closed
    /// again, the same way the libvirt census reconnects each time: a
    /// monitor left open overnight must hold nothing of the control
    /// plane's.
    pub fn read(
        runtime: &tokio::runtime::Runtime,
        db_path: &str,
        machine_id: Option<&str>,
    ) -> Result<Fleet, String> {
        if !Path::new(db_path).is_file() {
            return Err(format!(
                "no controller database at {db_path}.\n\
                 A machine that holds only guests has none: the controller keeps it."
            ));
        }
        runtime.block_on(async move {
            let store = Store::open_read_only(db_path)
                .await
                .map_err(|error| format!("{db_path}: {error}"))?;
            let fleet = collect(&store, machine_id).await;
            let _ = store.close().await;
            fleet
        })
    }

    /// The runner the cursor sits on, if the fleet has any.
    pub fn selected(&self, cursor: usize) -> Option<&Runner> {
        self.runners
            .get(cursor.min(self.runners.len().saturating_sub(1)))
    }
}

async fn collect(store: &Store, machine_id: Option<&str>) -> Result<Fleet, String> {
    let text = |error: bento_store::Error| error.to_string();

    let deployment = store.deployment().await.map_err(text)?;
    let lease = store.lease().await.map_err(text)?;
    let hosts = store.hosts().await.map_err(text)?;
    let slots = store.slots().await.map_err(text)?;
    let instances = store.instances().await.map_err(text)?;
    let images = store.images().await.map_err(text)?;

    let wanted: Vec<(String, String)> = images
        .iter()
        .filter_map(|image| {
            image
                .current_checksum
                .clone()
                .map(|checksum| (image.name.clone(), checksum))
        })
        .collect();

    let mut runners = Vec::with_capacity(hosts.len());
    for host in &hosts {
        let observation = store.host_observation(host.id).await.map_err(text)?;
        let held = store.host_images(host.id).await.map_err(text)?;
        let ready = wanted
            .iter()
            .filter(|(name, checksum)| {
                held.iter().any(|(held_name, held_checksum)| {
                    held_name == name && held_checksum == checksum
                })
            })
            .count();
        runners.push(Runner::new(
            host,
            observation,
            &slots,
            &instances,
            ready,
            wanted.len(),
            machine_id,
        ));
    }

    let known: Vec<i64> = hosts.iter().map(|host| host.id).collect();
    Ok(Fleet {
        deployment,
        lease,
        instances: instances.len(),
        orphans: instances
            .iter()
            .filter(|instance| !known.contains(&instance.host_id))
            .count(),
        images: wanted.len(),
        runners,
    })
}

/// One machine, as the controller last saw it (MULTI-NODE 20).
#[derive(Debug, Clone)]
pub struct Runner {
    pub id: i64,
    pub name: String,
    pub machine_id: Option<String>,
    pub endpoint: Option<String>,
    pub underlay: Option<String>,
    pub enabled: bool,
    pub placement: Placement,
    pub health: HostHealth,
    pub last_contact_at: Option<OffsetDateTime>,
    pub accepted_epoch: i64,
    pub arch: Option<String>,
    pub cpu_count: Option<i64>,
    pub memory_total_mib: Option<i64>,
    pub storage_total_gib: Option<i64>,
    pub storage_available_gib: Option<i64>,
    pub hypervisor_version: Option<String>,
    pub last_error: Option<String>,
    /// The slots this machine owns, and what each one is doing.
    pub slots: Vec<(i64, SlotState)>,
    pub instances: usize,
    pub running: usize,
    /// What the instance rows on this machine reserve (MULTI-NODE 12).
    pub vcpu: i64,
    pub memory_mib: i64,
    pub disk_gib: i64,
    pub images_ready: usize,
    pub images_wanted: usize,
    /// Whether this row describes the machine the screen runs on.
    pub is_local: bool,
}

impl Runner {
    fn new(
        host: &Host,
        observation: Option<Observation>,
        slots: &[Slot],
        instances: &[bento_types::Instance],
        images_ready: usize,
        images_wanted: usize,
        machine_id: Option<&str>,
    ) -> Runner {
        let observation = observation.unwrap_or(Observation {
            health: HostHealth::Unknown,
            last_contact_at: None,
            accepted_epoch: 0,
            arch: None,
            cpu_count: None,
            memory_total_mib: None,
            storage_total_gib: None,
            storage_available_gib: None,
            hypervisor_version: None,
            last_error: None,
        });
        let mine: Vec<&bento_types::Instance> = instances
            .iter()
            .filter(|instance| instance.host_id == host.id)
            .collect();
        Runner {
            id: host.id,
            name: host.name.clone(),
            machine_id: host.machine_id.clone(),
            endpoint: host.endpoint.clone(),
            underlay: host.underlay.clone(),
            enabled: host.enabled,
            placement: host.placement,
            health: observation.health,
            last_contact_at: observation.last_contact_at,
            accepted_epoch: observation.accepted_epoch,
            arch: observation.arch,
            cpu_count: observation.cpu_count,
            memory_total_mib: observation.memory_total_mib,
            storage_total_gib: observation.storage_total_gib,
            storage_available_gib: observation.storage_available_gib,
            hypervisor_version: observation.hypervisor_version,
            last_error: observation.last_error,
            slots: slots
                .iter()
                .filter(|slot| slot.owner_host_id == host.id)
                .map(|slot| (slot.slot, slot.state))
                .collect(),
            instances: mine.len(),
            running: mine
                .iter()
                .filter(|instance| instance.state == bento_types::State::Running)
                .count(),
            vcpu: mine.iter().map(|instance| i64::from(instance.vcpu)).sum(),
            memory_mib: mine.iter().map(|instance| instance.memory_mib).sum(),
            disk_gib: mine.iter().map(|instance| instance.disk_gib).sum(),
            images_ready,
            images_wanted,
            is_local: match (&host.machine_id, machine_id) {
                (Some(host), Some(local)) => host == local,
                _ => false,
            },
        }
    }

    /// Whether the controller may place a new instance here, and why not
    /// when it may not (MULTI-NODE 18).
    pub fn refusal(&self) -> Option<&'static str> {
        if !self.enabled {
            return Some("disabled");
        }
        match self.placement {
            Placement::Draining => return Some("draining"),
            Placement::Removed => return Some("removed"),
            Placement::Active => {}
        }
        match self.health {
            HostHealth::Ok => None,
            HostHealth::Unknown => Some("never answered"),
            HostHealth::Unreachable => Some("unreachable"),
            HostHealth::Mismatched => Some("answers as another machine"),
        }
    }

    /// How the endpoint reads when the operator has not set one.
    pub fn endpoint_text(&self) -> String {
        self.endpoint
            .clone()
            .unwrap_or_else(|| "none (driven through the local libvirt socket)".to_string())
    }
}

/// What a runner machine knows without a controller database
/// (MULTI-NODE 11.3).
#[derive(Debug, Clone, Default)]
pub struct Fence {
    pub machine_id: Option<String>,
    pub listen: String,
    pub fence_db: String,
    pub accepted_epoch: i64,
    /// Objects this runner has accepted a generation for: instances and
    /// user networks it has been told to build.
    pub objects: i64,
    /// Mutations it has finished and remembers the outcome of.
    pub outcomes: i64,
    pub last_change_at: Option<OffsetDateTime>,
}

impl Fence {
    pub fn read(config: &Config, machine_id: Option<&str>) -> Result<Fence, String> {
        let path = config.runner.fence_db.clone();
        if !Path::new(&path).is_file() {
            return Err(format!(
                "no fence database at {path}.\n\
                 The runner service writes it when it first answers a controller."
            ));
        }
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_URI
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = rusqlite::Connection::open_with_flags(&path, flags)
            .map_err(|error| format!("{path}: {error}"))?;
        // The runner owns this file. The monitor only reads it, and
        // SQLite is told so rather than trusted to be asked politely.
        conn.pragma_update(None, "query_only", true)
            .map_err(|error| format!("{path}: {error}"))?;

        let count = |table: &str| -> i64 {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap_or(0)
        };
        let last: Option<String> = conn
            .query_row("SELECT MAX(finished_at) FROM request_outcomes", [], |row| {
                row.get(0)
            })
            .unwrap_or(None);
        Ok(Fence {
            machine_id: machine_id.map(str::to_owned),
            listen: config.runner.listen.clone(),
            accepted_epoch: conn
                .query_row("SELECT accepted_epoch FROM fence WHERE id = 1", [], |row| {
                    row.get(0)
                })
                .map_err(|error| format!("{path}: {error}"))?,
            objects: count("object_generations"),
            outcomes: count("request_outcomes"),
            last_change_at: last.and_then(|text| {
                OffsetDateTime::parse(&text, &time::format_description::well_known::Rfc3339).ok()
            }),
            fence_db: path,
        })
    }
}

/// The machine this screen runs on (MULTI-NODE 16). The hostname is a
/// label and drifts; `/etc/machine-id` is the key.
pub fn machine_id() -> Option<String> {
    let text = std::fs::read_to_string("/etc/machine-id").ok()?;
    let id = text.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// How long ago something happened, for a screen that redraws every two
/// seconds. `None` reads as never.
pub fn ago(then: Option<OffsetDateTime>, now: OffsetDateTime) -> String {
    let Some(then) = then else {
        return "never".to_string();
    };
    let elapsed = now - then;
    if elapsed.is_negative() {
        // The controller wrote a time this machine has not reached. That
        // is a clock disagreement, and saying "in 4s" hides it.
        return format!("{}s ahead", elapsed.abs().whole_seconds());
    }
    match std::time::Duration::try_from(elapsed) {
        Ok(duration) => format!("{} ago", crate::host::human_duration(duration)),
        Err(_) => "never".to_string(),
    }
}

/// How long until something happens, for a deadline rather than a past
/// event. A deadline already gone reads as gone, not as a negative wait.
pub fn until(deadline: OffsetDateTime, now: OffsetDateTime) -> String {
    let left = deadline - now;
    match std::time::Duration::try_from(left) {
        Ok(duration) => format!("expires in {}", crate::host::human_duration(duration)),
        Err(_) => format!("ran out {}", ago(Some(deadline), now)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn host(id: i64, name: &str, machine_id: Option<&str>) -> Host {
        Host {
            id,
            machine_id: machine_id.map(str::to_owned),
            name: name.to_string(),
            libvirt_uri: String::new(),
            endpoint: Some(format!("http://10.0.0.{id}:10443")),
            underlay: Some(format!("10.0.0.{id}")),
            enabled: true,
            placement: Placement::Active,
            created_at: datetime!(2026-01-01 0:00 UTC),
        }
    }

    fn instance(host_id: i64, state: bento_types::State) -> bento_types::Instance {
        bento_types::Instance {
            uuid: format!("uuid-{host_id}-{}", state.as_str()),
            name: "vm".to_string(),
            owner_id: 1,
            host_id,
            image_name: "debian-13".to_string(),
            base_checksum: "abc".to_string(),
            state,
            desired_state: bento_types::DesiredState::Running,
            address: "10.100.1.10".to_string(),
            mac: "52:54:00:00:00:01".to_string(),
            vcpu: 2,
            memory_mib: 2048,
            disk_gib: 20,
            nested: false,
            ksm: false,
            http_port: 0,
            visibility: bento_types::Visibility::Private,
            created_at: datetime!(2026-01-01 0:00 UTC),
            last_seen_at: None,
            slot: Some(0),
        }
    }

    fn slot(number: i64, owner: i64, state: SlotState) -> Slot {
        Slot {
            slot: number,
            state,
            owner_host_id: owner,
            ownership_epoch: 1,
            source_host_id: None,
            destination_host_id: None,
            operation_id: None,
        }
    }

    fn runner(host: &Host, observation: Option<Observation>) -> Runner {
        Runner::new(
            host,
            observation,
            &[slot(0, 1, SlotState::Active), slot(1, 2, SlotState::Moving)],
            &[
                instance(1, bento_types::State::Running),
                instance(1, bento_types::State::Stopped),
                instance(2, bento_types::State::Running),
            ],
            1,
            2,
            Some("ebb80f403ef641deaa486417f2b6992a"),
        )
    }

    fn observation(health: HostHealth) -> Observation {
        Observation {
            health,
            last_contact_at: Some(datetime!(2026-01-01 0:00 UTC)),
            accepted_epoch: 7,
            arch: Some("aarch64".to_string()),
            cpu_count: Some(8),
            memory_total_mib: Some(15685),
            storage_total_gib: Some(400),
            storage_available_gib: Some(120),
            hypervisor_version: Some("libvirt local RPC".to_string()),
            last_error: None,
        }
    }

    #[test]
    fn a_runner_row_counts_only_what_sits_on_that_machine() {
        let host = host(1, "konata", Some("ebb80f403ef641deaa486417f2b6992a"));
        let runner = runner(&host, Some(observation(HostHealth::Ok)));
        assert_eq!(runner.instances, 2);
        assert_eq!(runner.running, 1);
        assert_eq!(runner.memory_mib, 4096);
        assert_eq!(runner.disk_gib, 40);
        assert_eq!(runner.vcpu, 4);
        assert_eq!(runner.slots, vec![(0, SlotState::Active)]);
        assert!(runner.is_local, "the machine ids match");
        assert_eq!(runner.refusal(), None);
    }

    #[test]
    fn a_machine_the_controller_never_reached_says_so_rather_than_showing_zeroes() {
        let host = host(2, "tsukasa", None);
        let runner = runner(&host, None);
        assert_eq!(runner.health, HostHealth::Unknown);
        assert_eq!(runner.arch, None);
        assert_eq!(runner.refusal(), Some("never answered"));
        assert!(!runner.is_local);
        // The rows are still counted: the controller placed them, and a
        // machine it cannot reach still holds them.
        assert_eq!(runner.instances, 1);
        assert_eq!(runner.slots, vec![(1, SlotState::Moving)]);
    }

    #[test]
    fn a_machine_that_takes_nothing_new_says_which_reason_stopped_it() {
        let mut host = host(1, "konata", None);
        host.placement = Placement::Draining;
        assert_eq!(
            runner(&host, Some(observation(HostHealth::Ok))).refusal(),
            Some("draining")
        );
        host.placement = Placement::Active;
        host.enabled = false;
        assert_eq!(
            runner(&host, Some(observation(HostHealth::Ok))).refusal(),
            Some("disabled")
        );
        host.enabled = true;
        assert_eq!(
            runner(&host, Some(observation(HostHealth::Mismatched))).refusal(),
            Some("answers as another machine")
        );
    }

    #[test]
    fn a_missing_database_names_the_path_rather_than_reporting_an_empty_fleet() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let error = Fleet::read(&runtime, "/no/such/bento.db", None).expect_err("no database");
        assert!(error.contains("/no/such/bento.db"), "{error}");
        assert!(error.contains("only guests"), "{error}");

        let mut config = Config::default();
        config.runner.fence_db = "/no/such/runner.db".to_string();
        let error = Fence::read(&config, None).expect_err("no fence");
        assert!(error.contains("/no/such/runner.db"), "{error}");
    }

    #[test]
    fn a_lease_reads_as_the_time_it_has_left_or_as_gone() {
        let now = datetime!(2026-01-01 1:00 UTC);
        assert_eq!(
            until(datetime!(2026-01-01 1:00:40 UTC), now),
            "expires in 40s"
        );
        assert_eq!(
            until(datetime!(2026-01-01 0:58 UTC), now),
            "ran out 2m 0s ago"
        );
    }

    #[test]
    fn a_contact_time_reads_as_an_age_and_a_clock_disagreement_reads_as_one() {
        let now = datetime!(2026-01-01 1:00 UTC);
        assert_eq!(ago(None, now), "never");
        assert_eq!(ago(Some(datetime!(2026-01-01 0:59 UTC)), now), "1m 0s ago");
        assert_eq!(
            ago(Some(datetime!(2026-01-01 1:00:30 UTC)), now),
            "30s ahead"
        );
    }
}
