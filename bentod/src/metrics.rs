//! The measurements behind the dashboard charts (SPEC 14.4).
//!
//! A task in `serve` takes one reading every 30 seconds and pushes it
//! into a ring buffer for each series. The charts poll on the same
//! period and ask for the last hour by default, so a buffer of 24 hours
//! covers the widest window the API accepts.
//!
//! One deployment has more than one machine (MULTI-NODE 20), so the
//! series are kept per machine and per instance. The controller reads
//! its own machine through libvirt and every other machine through the
//! runner protocol, and both paths arrive here as the same reading. A
//! machine sends counters and byte counts; the rate arithmetic happens
//! once, here, so a remote guest is charted the same way as a local
//! one.
//!
//! Nothing is written to the database. A restart therefore empties the
//! charts, and they refill over the following hour. That is the price of
//! keeping the schema to one file and the write load off the WAL.
//!
//! The host-touching part is [`SamplerTask`], which stays thin. Every
//! calculation in [`Sampler`] takes readings as arguments, so the
//! deltas, the ring buffers, and the aggregation are tested without a
//! host.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bento_api::{BoxError, HostMetrics, InstanceMetrics, Metrics, Point, UserMetrics};
use bento_hostinfo::CpuTimes;
use bento_hypervisor::{DomainSample, DomainSampler};
use bento_lifecycle::Manager;
use bento_store::Store;
use bento_types::Instance;

/// How often [`SamplerTask::tick`] runs. The charts poll on the same
/// period.
pub(crate) const INTERVAL: Duration = Duration::from_secs(30);
/// Points kept for each series: 24 hours, the widest window the API
/// accepts (`window_from` clamps to 86 400 seconds).
const CAPACITY: usize = 24 * 60 * 60 / 30;

const MIB: f64 = 1024.0 * 1024.0;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
/// One virtual processor, fully busy, spends this much processor time
/// each second.
const NS_PER_SEC: f64 = 1_000_000_000.0;

/// A bounded series, oldest first.
#[derive(Debug, Default)]
struct Series(VecDeque<Point>);

impl Series {
    fn push(&mut self, at: i64, value: f64) {
        if self.0.len() == CAPACITY {
            self.0.pop_front();
        }
        self.0.push_back(Point { at, value });
    }

    /// The points inside `window`, oldest first.
    fn window(&self, window: Duration, now: i64) -> Vec<Point> {
        let first = now - window.as_secs() as i64;
        self.0
            .iter()
            .filter(|point| point.at >= first)
            .copied()
            .collect()
    }

    fn last(&self) -> f64 {
        self.0.back().map_or(0.0, |point| point.value)
    }
}

/// What one instance is using.
#[derive(Debug, Default)]
struct InstanceState {
    owner_id: i64,
    /// Which machine runs it. The account total measures processor time
    /// against the cores of the machine the instance is really on.
    host_id: i64,
    /// Percentage of the instance's own virtual processors.
    cpu_pct: Series,
    memory_used_mib: Series,
    storage_used_gib: f64,
    /// Processor time consumed each wall-clock second, in nanoseconds.
    /// One busy virtual processor is 1e9. Kept apart from `cpu_pct`
    /// because the user total measures against host cores, not against
    /// the virtual processors of one instance.
    cpu_ns_per_sec: f64,
    /// The previous reading, which the next delta is measured from.
    previous: Option<(u64, i64)>,
}

/// The series of one machine.
#[derive(Debug, Default)]
struct HostState {
    /// A label for the charts. The host row id is the key (MULTI-NODE 16).
    name: String,
    cpu_pct: Series,
    memory_used_mib: Series,
    memory_total_mib: i64,
    storage_used_gib: f64,
    storage_total_gib: i64,
    cpu_count: i64,
    previous_cpu: Option<CpuTimes>,
}

#[derive(Debug, Default)]
struct State {
    /// Keyed by host row id, and ordered by it, so the dashboard lists
    /// the machines in the same order on every poll.
    hosts: BTreeMap<i64, HostState>,
    instances: HashMap<String, InstanceState>,
}

/// One host reading, as [`Sampler::record_host`] wants it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HostReading {
    pub(crate) cpu: Option<CpuTimes>,
    pub(crate) memory_total_bytes: u64,
    pub(crate) memory_available_bytes: u64,
    pub(crate) storage_total_bytes: u64,
    pub(crate) storage_available_bytes: u64,
    pub(crate) cpu_count: i64,
}

/// The live [`Metrics`] implementation: it holds the series and answers
/// the API. It touches no host, so every calculation below is tested by
/// pushing readings straight in.
#[derive(Debug, Default)]
pub(crate) struct Sampler {
    state: Mutex<State>,
}

impl Sampler {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Notes that a machine exists and how big it is, without a usage
    /// reading.
    ///
    /// A machine the controller could not reach still belongs on the
    /// dashboard: that is exactly when an operator looks. Its card shows
    /// what the machine has and what is provisioned on it, and its
    /// charts stay empty until it answers again.
    pub(crate) fn record_known(
        &self,
        host_id: i64,
        name: &str,
        cpu_count: i64,
        memory_total_mib: i64,
        storage_total_gib: i64,
    ) {
        let mut state = self.state.lock().expect("metrics state");
        let entry = state.hosts.entry(host_id).or_default();
        entry.name = name.to_owned();
        // A live reading is better than a remembered one, so remembered
        // figures never overwrite what a machine reported.
        if entry.memory_total_mib == 0 {
            entry.memory_total_mib = memory_total_mib;
        }
        if entry.storage_total_gib == 0 {
            entry.storage_total_gib = storage_total_gib;
        }
        if entry.cpu_count == 0 {
            entry.cpu_count = cpu_count;
        }
    }

    /// Drops the series of a machine that is no longer a host row.
    pub(crate) fn retain_hosts(&self, live: &[i64]) {
        let mut state = self.state.lock().expect("metrics state");
        state.hosts.retain(|host_id, _| live.contains(host_id));
    }

    /// Folds one host reading into the series.
    ///
    /// The processor share needs two readings, so the first tick after
    /// startup records memory and storage but no processor point.
    pub(crate) fn record_host(&self, host_id: i64, name: &str, reading: HostReading, at: i64) {
        let mut state = self.state.lock().expect("metrics state");
        let host = state.hosts.entry(host_id).or_default();
        host.name = name.to_owned();
        if let (Some(before), Some(after)) = (host.previous_cpu, reading.cpu)
            && let Some(busy) = bento_hostinfo::busy_fraction(before, after)
        {
            host.cpu_pct.push(at, busy * 100.0);
        }
        if reading.cpu.is_some() {
            host.previous_cpu = reading.cpu;
        }
        let used = reading
            .memory_total_bytes
            .saturating_sub(reading.memory_available_bytes);
        host.memory_used_mib.push(at, used as f64 / MIB);
        host.memory_total_mib = (reading.memory_total_bytes as f64 / MIB) as i64;
        let storage_used = reading
            .storage_total_bytes
            .saturating_sub(reading.storage_available_bytes);
        host.storage_used_gib = storage_used as f64 / GIB;
        host.storage_total_gib = (reading.storage_total_bytes as f64 / GIB) as i64;
        host.cpu_count = reading.cpu_count;
    }

    /// Folds one domain reading into the series of an instance.
    ///
    /// Processor time is cumulative, so a percentage needs the previous
    /// reading and the seconds between the two. The first reading of an
    /// instance therefore records memory but no processor point. A
    /// counter that went backwards means the domain restarted, so the
    /// reading becomes the new baseline instead of a huge spike.
    pub(crate) fn record_instance(
        &self,
        uuid: &str,
        owner_id: i64,
        host_id: i64,
        sample: &DomainSample,
        storage_used_gib: f64,
        at: i64,
    ) {
        let mut state = self.state.lock().expect("metrics state");
        let entry = state.instances.entry(uuid.to_owned()).or_default();
        entry.owner_id = owner_id;
        entry.host_id = host_id;
        entry.storage_used_gib = storage_used_gib;
        if let Some(rss_kib) = sample.rss_kib {
            entry.memory_used_mib.push(at, rss_kib as f64 / 1024.0);
        }
        if let Some((previous_ns, previous_at)) = entry.previous {
            let elapsed = at - previous_at;
            let spent = sample.cpu_time_ns.checked_sub(previous_ns);
            if let (Some(spent), true) = (spent, elapsed > 0) {
                let rate = spent as f64 / elapsed as f64;
                entry.cpu_ns_per_sec = rate;
                let vcpus = sample.vcpus.max(1) as f64;
                entry
                    .cpu_pct
                    .push(at, (rate / (vcpus * NS_PER_SEC) * 100.0).clamp(0.0, 100.0));
            }
        }
        entry.previous = Some((sample.cpu_time_ns, at));
    }

    /// Drops the series of an instance that no longer exists, so that a
    /// long-running control plane does not hold a deleted machine.
    pub(crate) fn retain(&self, live: &[String]) {
        let mut state = self.state.lock().expect("metrics state");
        state.instances.retain(|uuid, _| live.contains(uuid));
    }
}

/// How the controller reads a machine that is not its own.
///
/// The real implementation sends [`bento_runner::Operation::Sample`] over
/// the runner protocol. Tests use a fake, so every calculation above is
/// exercised for a remote machine without a second machine.
#[async_trait]
pub(crate) trait RemoteSampler: Send + Sync {
    async fn sample(&self, host: &bento_types::Host) -> Result<bento_runner::Samples, String>;
}

/// Reads every machine and every running domain, and feeds a
/// [`Sampler`]. This is the only part that touches a host or the
/// network, so it stays thin.
pub(crate) struct SamplerTask {
    pub(crate) sampler: Arc<Sampler>,
    pub(crate) store: Store,
    pub(crate) manager: Arc<Manager>,
    pub(crate) domains: Arc<dyn DomainSampler>,
    pub(crate) storage_dir: String,
    /// The machine this controller runs on. Its domains are read through
    /// the local libvirt socket; every other machine is asked.
    pub(crate) local_host_id: i64,
    /// Reads the other machines. `None` in a one-machine deployment and
    /// in tests that only exercise the local path.
    pub(crate) remote: Option<Arc<dyn RemoteSampler>>,
}

impl SamplerTask {
    /// Takes one reading of every machine and of every running instance.
    pub(crate) async fn tick(&self) {
        let at = time::OffsetDateTime::now_utc().unix_timestamp();
        let instances = match self.store.instances().await {
            Ok(instances) => instances,
            Err(error) => {
                tracing::warn!(%error, "metrics: listing instances");
                return;
            }
        };
        self.sampler.retain(
            &instances
                .iter()
                .map(|instance| instance.uuid.clone())
                .collect::<Vec<_>>(),
        );

        let hosts = match self.store.hosts().await {
            Ok(hosts) => hosts,
            Err(error) => {
                tracing::warn!(%error, "metrics: listing machines");
                return;
            }
        };
        // A retired machine keeps its row for the audit trail and holds
        // no guests, so it is not charted (MULTI-NODE 18).
        let hosts: Vec<_> = hosts
            .into_iter()
            .filter(|host| host.placement != bento_types::Placement::Removed)
            .collect();
        self.sampler
            .retain_hosts(&hosts.iter().map(|host| host.id).collect::<Vec<_>>());

        for host in &hosts {
            if host.id == self.local_host_id {
                self.read_local(host, &instances, at).await;
            } else {
                self.read_remote(host, &instances, at).await;
            }
        }
    }

    /// Reads this machine through its own libvirt socket and filesystem.
    async fn read_local(&self, host: &bento_types::Host, instances: &[Instance], at: i64) {
        let memory = bento_hostinfo::read_memory().unwrap_or_default();
        let storage = bento_hostinfo::disk_usage(Path::new(&self.storage_dir)).unwrap_or_default();
        self.sampler.record_host(
            host.id,
            &host.name,
            HostReading {
                cpu: bento_hostinfo::read_cpu().ok().flatten(),
                memory_total_bytes: memory.total,
                memory_available_bytes: memory.available,
                storage_total_bytes: storage.total,
                storage_available_bytes: storage.available,
                cpu_count: bento_hostinfo::cpu_count() as i64,
            },
            at,
        );

        for instance in instances.iter().filter(|i| i.host_id == host.id) {
            // A domain that is not running has no sample, and a libvirt
            // failure for one instance must not stop the rest.
            let sample = match self.domains.sample(&instance.name).await {
                Ok(Some(sample)) => sample,
                Ok(None) => continue,
                Err(error) => {
                    tracing::debug!(name = %instance.name, %error, "metrics: sampling domain");
                    continue;
                }
            };
            let overlay = self.manager.overlay_path(&instance.uuid);
            let storage_used_gib = overlay_size_gib(&overlay);
            self.sampler.record_instance(
                &instance.uuid,
                instance.owner_id,
                host.id,
                &sample,
                storage_used_gib,
                at,
            );
        }
    }

    /// Asks another machine what it and its guests are using.
    ///
    /// A machine that cannot be reached is still listed, with the size it
    /// last reported, so the operator sees a machine with flat charts
    /// rather than a machine that vanished (MULTI-NODE 20).
    async fn read_remote(&self, host: &bento_types::Host, instances: &[Instance], at: i64) {
        let samples = match &self.remote {
            Some(remote) => match remote.sample(host).await {
                Ok(samples) => Some(samples),
                Err(error) => {
                    tracing::debug!(runner = %host.name, %error, "metrics: sampling a machine");
                    None
                }
            },
            None => None,
        };
        let Some(samples) = samples else {
            self.remember(host).await;
            return;
        };

        self.sampler.record_host(
            host.id,
            &host.name,
            HostReading {
                cpu: samples.host.cpu.map(|cpu| CpuTimes {
                    total: cpu.total,
                    idle: cpu.idle,
                }),
                memory_total_bytes: samples.host.memory_total_bytes,
                memory_available_bytes: samples.host.memory_available_bytes,
                storage_total_bytes: samples.host.storage_total_bytes,
                storage_available_bytes: samples.host.storage_available_bytes,
                cpu_count: samples.host.cpu_count,
            },
            at,
        );

        let owners: HashMap<&str, i64> = instances
            .iter()
            .map(|instance| (instance.uuid.as_str(), instance.owner_id))
            .collect();
        for usage in samples.domains {
            // A domain the controller has no row for is not charted.
            // Only the controller can say who owns an instance, and a
            // reading with no owner would land in nobody's total.
            let Some(owner_id) = owners.get(usage.uuid.as_str()).copied() else {
                continue;
            };
            self.sampler.record_instance(
                &usage.uuid,
                owner_id,
                host.id,
                &DomainSample {
                    cpu_time_ns: usage.cpu_time_ns,
                    vcpus: usage.vcpus,
                    rss_kib: usage.rss_kib,
                },
                usage.storage_used_bytes as f64 / GIB,
                at,
            );
        }
    }

    /// Lists a machine that did not answer, using the size it reported
    /// the last time it did.
    async fn remember(&self, host: &bento_types::Host) {
        let observed = match self.store.host_observation(host.id).await {
            Ok(observed) => observed,
            Err(error) => {
                tracing::debug!(runner = %host.name, %error, "metrics: reading an observation");
                None
            }
        };
        let (cpu_count, memory_total_mib, storage_total_gib) = observed.map_or((0, 0, 0), |o| {
            (
                o.cpu_count.unwrap_or_default(),
                o.memory_total_mib.unwrap_or_default(),
                o.storage_total_gib.unwrap_or_default(),
            )
        });
        self.sampler.record_known(
            host.id,
            &host.name,
            cpu_count,
            memory_total_mib,
            storage_total_gib,
        );
    }
}

/// The space an overlay really occupies, not its virtual size. A qcow2
/// overlay is thin, so the two differ by a lot (SPEC 19).
fn overlay_size_gib(path: &Path) -> f64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).map_or(0.0, |meta| (meta.blocks() * 512) as f64 / GIB)
}

#[async_trait]
impl Metrics for Sampler {
    async fn hosts(&self, window: Duration) -> Result<Vec<HostMetrics>, BoxError> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let state = self.state.lock().expect("metrics state");
        // One entry for each machine in the deployment, in host row
        // order (MULTI-NODE 20).
        Ok(state
            .hosts
            .iter()
            .map(|(host_id, host)| HostMetrics {
                host_id: *host_id,
                host_name: host.name.clone(),
                cpu_pct: host.cpu_pct.window(window, now),
                memory_used_mib: host.memory_used_mib.window(window, now),
                memory_total_mib: host.memory_total_mib,
                storage_used_gib: host.storage_used_gib,
                storage_total_gib: host.storage_total_gib,
                cpu_count: host.cpu_count,
                placeholder: false,
            })
            .collect())
    }

    async fn instance(&self, uuid: &str, window: Duration) -> Result<InstanceMetrics, BoxError> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let state = self.state.lock().expect("metrics state");
        let Some(entry) = state.instances.get(uuid) else {
            // A stopped or brand new instance has no readings yet. The
            // charts say "No samples yet" rather than showing a zero
            // that reads as a measurement.
            return Ok(InstanceMetrics {
                placeholder: false,
                ..InstanceMetrics::default()
            });
        };
        Ok(InstanceMetrics {
            cpu_pct: entry.cpu_pct.window(window, now),
            memory_used_mib: entry.memory_used_mib.window(window, now),
            storage_used_gib: entry.storage_used_gib,
            placeholder: false,
        })
    }

    async fn user(&self, user_id: i64) -> Result<UserMetrics, BoxError> {
        let state = self.state.lock().expect("metrics state");
        let mine = state
            .instances
            .values()
            .filter(|entry| entry.owner_id == user_id);
        let mut totals = UserMetrics {
            placeholder: false,
            ..UserMetrics::default()
        };
        let mut cpu_pct = 0.0;
        for entry in mine {
            // The account total measures against every core of the
            // machine, not against the virtual processors of one
            // instance, so that two accounts can be compared. An account
            // whose instances sit on different machines is measured
            // against each machine in turn and the shares are added: a
            // core is a core wherever it is.
            let cores = state
                .hosts
                .get(&entry.host_id)
                .map_or(0, |host| host.cpu_count)
                .max(1) as f64;
            cpu_pct += entry.cpu_ns_per_sec / (cores * NS_PER_SEC) * 100.0;
            totals.memory_used_mib += entry.memory_used_mib.last() as i64;
            totals.storage_used_gib += entry.storage_used_gib;
        }
        totals.cpu_pct = cpu_pct.clamp(0.0, 100.0);
        Ok(totals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(busy_total: u64, busy_idle: u64) -> HostReading {
        HostReading {
            cpu: Some(CpuTimes {
                total: busy_total,
                idle: busy_idle,
            }),
            memory_total_bytes: 16 * 1024 * 1024 * 1024,
            memory_available_bytes: 4 * 1024 * 1024 * 1024,
            storage_total_bytes: 160 * 1024 * 1024 * 1024,
            storage_available_bytes: 60 * 1024 * 1024 * 1024,
            cpu_count: 8,
        }
    }

    /// The machine every test below records against, unless it names a
    /// second one.
    const HOST: i64 = 7;
    const HOST_NAME: &str = "runner.example.org";

    fn sampler() -> Sampler {
        Sampler::new()
    }

    /// The series are read against the wall clock, so a test that pushes
    /// a point has to place it near now for a window to contain it.
    fn now() -> i64 {
        time::OffsetDateTime::now_utc().unix_timestamp()
    }

    #[tokio::test]
    async fn the_first_host_reading_has_no_processor_point() {
        let sampler = sampler();
        let t = now();
        sampler.record_host(HOST, HOST_NAME, reading(1000, 850), t);
        let hosts = sampler.hosts(Duration::from_secs(3600)).await.unwrap();
        assert_eq!(hosts.len(), 1);
        let host = &hosts[0];
        assert_eq!(host.host_id, 7);
        assert_eq!(host.host_name, "runner.example.org");
        // Memory and storage are absolute, so one reading is enough.
        assert_eq!(host.memory_used_mib.len(), 1);
        assert_eq!(host.memory_used_mib[0].value, 12.0 * 1024.0);
        assert_eq!(host.memory_total_mib, 16 * 1024);
        assert_eq!(host.storage_used_gib, 100.0);
        assert_eq!(host.storage_total_gib, 160);
        assert_eq!(host.cpu_count, 8);
        // Processor time is a counter, so a share needs two readings.
        assert!(host.cpu_pct.is_empty());
        assert!(!host.placeholder);

        sampler.record_host(HOST, HOST_NAME, reading(2000, 1600), t + 30);
        let hosts = sampler.hosts(Duration::from_secs(3600)).await.unwrap();
        let host = &hosts[0];
        assert_eq!(host.cpu_pct.len(), 1);
        assert_eq!(host.cpu_pct[0].value, 25.0);
    }

    #[tokio::test]
    async fn instance_processor_time_becomes_a_share_of_its_own_vcpus() {
        let sampler = sampler();
        let at = now();
        // Two virtual processors, both idle to start.
        sampler.record_instance("uuid-a", 1, HOST, &sample(0, 2, Some(512 * 1024)), 1.5, at);
        let metrics = sampler
            .instance("uuid-a", Duration::from_secs(3600))
            .await
            .unwrap();
        assert!(metrics.cpu_pct.is_empty(), "one reading is not a rate");
        assert_eq!(metrics.memory_used_mib.len(), 1);
        assert_eq!(metrics.memory_used_mib[0].value, 512.0);
        assert_eq!(metrics.storage_used_gib, 1.5);

        // 30 seconds later one of the two processors was fully busy:
        // 30e9 nanoseconds of processor time over 30 seconds of wall
        // clock, against two virtual processors, is 50 percent.
        sampler.record_instance(
            "uuid-a",
            1,
            HOST,
            &sample(30_000_000_000, 2, Some(512 * 1024)),
            1.5,
            at + 30,
        );
        let metrics = sampler
            .instance("uuid-a", Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(metrics.cpu_pct.len(), 1);
        assert_eq!(metrics.cpu_pct[0].value, 50.0);
    }

    fn sample(cpu_time_ns: u64, vcpus: u32, rss_kib: Option<u64>) -> DomainSample {
        DomainSample {
            cpu_time_ns,
            vcpus,
            rss_kib,
        }
    }

    #[tokio::test]
    async fn a_restarted_domain_resets_rather_than_spiking() {
        let sampler = sampler();
        let t = now();
        sampler.record_instance("uuid-a", 1, HOST, &sample(90_000_000_000, 2, None), 0.0, t);
        // The domain restarted, so its cumulative counter went backwards.
        sampler.record_instance(
            "uuid-a",
            1,
            HOST,
            &sample(5_000_000_000, 2, None),
            0.0,
            t + 30,
        );
        let metrics = sampler
            .instance("uuid-a", Duration::from_secs(3600))
            .await
            .unwrap();
        assert!(
            metrics.cpu_pct.is_empty(),
            "a backwards counter must not produce a point: {:?}",
            metrics.cpu_pct
        );
        // The next reading measures from the new baseline.
        sampler.record_instance(
            "uuid-a",
            1,
            HOST,
            &sample(35_000_000_000, 2, None),
            0.0,
            t + 60,
        );
        let metrics = sampler
            .instance("uuid-a", Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(metrics.cpu_pct.len(), 1);
        assert_eq!(metrics.cpu_pct[0].value, 50.0);
    }

    #[tokio::test]
    async fn a_guest_without_memory_statistics_still_charts_its_processor() {
        let sampler = sampler();
        let t = now();
        sampler.record_instance("uuid-a", 1, HOST, &sample(0, 1, None), 0.0, t);
        sampler.record_instance(
            "uuid-a",
            1,
            HOST,
            &sample(15_000_000_000, 1, None),
            0.0,
            t + 30,
        );
        let metrics = sampler
            .instance("uuid-a", Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(metrics.cpu_pct.len(), 1);
        assert_eq!(metrics.cpu_pct[0].value, 50.0);
        assert!(metrics.memory_used_mib.is_empty());
        // The figures that exist are real, so nothing is flagged.
        assert!(!metrics.placeholder);
    }

    #[tokio::test]
    async fn an_unknown_instance_answers_empty_rather_than_failing() {
        let sampler = sampler();
        let metrics = sampler
            .instance("no-such-uuid", Duration::from_secs(3600))
            .await
            .unwrap();
        assert!(metrics.cpu_pct.is_empty());
        assert_eq!(metrics.storage_used_gib, 0.0);
        assert!(!metrics.placeholder);
    }

    #[tokio::test]
    async fn a_user_total_measures_against_the_cores_of_the_host() {
        let sampler = sampler();
        let t = now();
        sampler.record_host(HOST, HOST_NAME, reading(1000, 850), t);
        // Two instances of one user, one of another.
        for (uuid, owner) in [("a", 1), ("b", 1), ("c", 2)] {
            sampler.record_instance(uuid, owner, HOST, &sample(0, 2, Some(1024 * 1024)), 2.0, t);
            sampler.record_instance(
                uuid,
                owner,
                HOST,
                &sample(30_000_000_000, 2, Some(1024 * 1024)),
                2.0,
                t + 30,
            );
        }
        let user = sampler.user(1).await.unwrap();
        // Each instance burned one core. Two cores of the host's eight
        // is 25 percent, not the 50 percent each instance shows against
        // its own two virtual processors.
        assert_eq!(user.cpu_pct, 25.0);
        assert_eq!(user.memory_used_mib, 2048);
        assert_eq!(user.storage_used_gib, 4.0);
        assert!(!user.placeholder);

        let other = sampler.user(2).await.unwrap();
        assert_eq!(other.memory_used_mib, 1024);
    }

    #[tokio::test]
    async fn a_deleted_instance_is_dropped_from_the_series() {
        let sampler = sampler();
        let t = now();
        sampler.record_instance("gone", 1, HOST, &sample(0, 1, Some(1024)), 1.0, t);
        sampler.record_instance("kept", 1, HOST, &sample(0, 1, Some(1024)), 1.0, t);
        sampler.retain(&["kept".to_string()]);
        assert_eq!(
            sampler
                .instance("gone", Duration::from_secs(3600))
                .await
                .unwrap()
                .storage_used_gib,
            0.0
        );
        assert_eq!(
            sampler
                .instance("kept", Duration::from_secs(3600))
                .await
                .unwrap()
                .storage_used_gib,
            1.0
        );
    }

    #[tokio::test]
    async fn each_machine_keeps_its_own_series() {
        let sampler = sampler();
        let t = now();
        // Two machines of different sizes, read at the same instant.
        sampler.record_host(HOST, HOST_NAME, reading(1000, 850), t);
        sampler.record_host(HOST, HOST_NAME, reading(2000, 1600), t + 30);
        let small = |total, idle| {
            let mut small = reading(total, idle);
            small.memory_total_bytes = 8 * 1024 * 1024 * 1024;
            small.memory_available_bytes = 6 * 1024 * 1024 * 1024;
            small.cpu_count = 4;
            small
        };
        sampler.record_host(3, "tsukasa.example.org", small(1000, 900), t);
        // Every tick of processor time was idle, so this machine did
        // nothing while the other one was a quarter busy.
        sampler.record_host(3, "tsukasa.example.org", small(2000, 1900), t + 30);

        let hosts = sampler.hosts(Duration::from_secs(3600)).await.unwrap();
        assert_eq!(hosts.len(), 2, "both machines are charted");
        // Ordered by host row id, so the list does not shuffle between
        // polls.
        assert_eq!(hosts[0].host_id, 3);
        assert_eq!(hosts[0].host_name, "tsukasa.example.org");
        assert_eq!(hosts[0].memory_total_mib, 8 * 1024);
        assert_eq!(hosts[0].cpu_count, 4);
        // One machine's reading must not become another's.
        assert_eq!(hosts[0].cpu_pct[0].value, 0.0);
        assert_eq!(hosts[1].host_id, HOST);
        assert_eq!(hosts[1].memory_total_mib, 16 * 1024);
        assert_eq!(hosts[1].cpu_pct[0].value, 25.0);
    }

    #[tokio::test]
    async fn a_machine_that_did_not_answer_is_still_listed() {
        let sampler = sampler();
        sampler.record_known(3, "tsukasa.example.org", 4, 7549, 165);
        let hosts = sampler.hosts(Duration::from_secs(3600)).await.unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].host_name, "tsukasa.example.org");
        assert_eq!(hosts[0].memory_total_mib, 7549);
        assert_eq!(hosts[0].storage_total_gib, 165);
        // Nothing was measured, so nothing is charted. The card shows
        // the machine and what is provisioned on it.
        assert!(hosts[0].cpu_pct.is_empty());
        assert!(hosts[0].memory_used_mib.is_empty());

        // A reading is worth more than a memory, and replaces it.
        sampler.record_host(3, "tsukasa.example.org", reading(1000, 850), now());
        let hosts = sampler.hosts(Duration::from_secs(3600)).await.unwrap();
        assert_eq!(hosts[0].memory_total_mib, 16 * 1024);
        assert_eq!(hosts[0].memory_used_mib.len(), 1);
        // And the memory never overwrites the reading.
        sampler.record_known(3, "tsukasa.example.org", 4, 7549, 165);
        let hosts = sampler.hosts(Duration::from_secs(3600)).await.unwrap();
        assert_eq!(hosts[0].memory_total_mib, 16 * 1024);
    }

    #[tokio::test]
    async fn an_account_measures_each_instance_against_its_own_machine() {
        let sampler = sampler();
        let t = now();
        sampler.record_host(HOST, HOST_NAME, reading(1000, 850), t);
        let mut small = reading(1000, 900);
        small.cpu_count = 4;
        sampler.record_host(3, "tsukasa.example.org", small, t);

        // One instance of the same account on each machine, each burning
        // one core.
        for (uuid, host_id) in [("here", HOST), ("there", 3)] {
            sampler.record_instance(uuid, 1, host_id, &sample(0, 2, Some(1024 * 1024)), 2.0, t);
            sampler.record_instance(
                uuid,
                1,
                host_id,
                &sample(30_000_000_000, 2, Some(1024 * 1024)),
                2.0,
                t + 30,
            );
        }
        let user = sampler.user(1).await.unwrap();
        // One core of eight is 12.5 percent; one core of four is 25.
        // Measuring both against one machine would report the wrong
        // figure for the instance that is not on it.
        assert_eq!(user.cpu_pct, 37.5);
        assert_eq!(user.memory_used_mib, 2048);
        assert_eq!(user.storage_used_gib, 4.0);
    }

    #[tokio::test]
    async fn a_machine_that_is_no_longer_a_host_row_leaves_the_charts() {
        let sampler = sampler();
        let t = now();
        sampler.record_host(HOST, HOST_NAME, reading(1000, 850), t);
        sampler.record_host(3, "retired.example.org", reading(1000, 850), t);
        sampler.retain_hosts(&[HOST]);
        let hosts = sampler.hosts(Duration::from_secs(3600)).await.unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].host_id, HOST);
    }

    #[test]
    fn a_series_holds_a_day_and_then_drops_its_oldest_point() {
        let mut series = Series::default();
        for step in 0..(CAPACITY as i64 + 10) {
            series.push(step * 30, step as f64);
        }
        assert_eq!(series.0.len(), CAPACITY);
        assert_eq!(series.0.front().unwrap().value, 10.0);
        assert_eq!(series.last(), (CAPACITY as f64) + 9.0);
    }

    #[test]
    fn a_window_returns_only_the_points_inside_it() {
        let mut series = Series::default();
        for step in 0..10 {
            series.push(1_000 + step * 30, step as f64);
        }
        // The last 100 seconds of a series that spans 270.
        let points = series.window(Duration::from_secs(100), 1_270);
        assert_eq!(points.len(), 4);
        assert_eq!(points[0].at, 1_180);
    }
}
