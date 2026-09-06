//! The measurements behind the dashboard charts (SPEC 14.4).
//!
//! A task in `serve` takes one reading every 30 seconds and pushes it
//! into a ring buffer for each series. The charts poll on the same
//! period and ask for the last hour by default, so a buffer of 24 hours
//! covers the widest window the API accepts.
//!
//! Nothing is written to the database. A restart therefore empties the
//! charts, and they refill over the following hour. That is the price of
//! keeping the schema to one file and the write load off the WAL.
//!
//! The host-touching part is [`SamplerTask`], which stays thin. Every
//! calculation in [`Sampler`] takes readings as arguments, so the
//! deltas, the ring buffers, and the aggregation are tested without a
//! host.

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

#[derive(Debug, Default)]
struct State {
    host_cpu_pct: Series,
    host_memory_used_mib: Series,
    host_memory_total_mib: i64,
    host_storage_used_gib: f64,
    host_storage_total_gib: i64,
    cpu_count: i64,
    previous_cpu: Option<CpuTimes>,
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
    /// Folds one host reading into the series.
    ///
    /// The processor share needs two readings, so the first tick after
    /// startup records memory and storage but no processor point.
    pub(crate) fn record_host(&self, reading: HostReading, at: i64) {
        let mut state = self.state.lock().expect("metrics state");
        if let (Some(before), Some(after)) = (state.previous_cpu, reading.cpu)
            && let Some(busy) = bento_hostinfo::busy_fraction(before, after)
        {
            state.host_cpu_pct.push(at, busy * 100.0);
        }
        if reading.cpu.is_some() {
            state.previous_cpu = reading.cpu;
        }
        let used = reading
            .memory_total_bytes
            .saturating_sub(reading.memory_available_bytes);
        state.host_memory_used_mib.push(at, used as f64 / MIB);
        state.host_memory_total_mib = (reading.memory_total_bytes as f64 / MIB) as i64;
        let storage_used = reading
            .storage_total_bytes
            .saturating_sub(reading.storage_available_bytes);
        state.host_storage_used_gib = storage_used as f64 / GIB;
        state.host_storage_total_gib = (reading.storage_total_bytes as f64 / GIB) as i64;
        state.cpu_count = reading.cpu_count;
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
        sample: &DomainSample,
        storage_used_gib: f64,
        at: i64,
    ) {
        let mut state = self.state.lock().expect("metrics state");
        let entry = state.instances.entry(uuid.to_owned()).or_default();
        entry.owner_id = owner_id;
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

/// Reads the host and every running domain, and feeds a [`Sampler`].
/// This is the only part that touches the host, so it stays thin.
pub(crate) struct SamplerTask {
    pub(crate) sampler: Arc<Sampler>,
    pub(crate) store: Store,
    pub(crate) manager: Arc<Manager>,
    pub(crate) domains: Arc<dyn DomainSampler>,
    pub(crate) storage_dir: String,
}

impl SamplerTask {
    /// Takes one reading of the host and of every running instance.
    pub(crate) async fn tick(&self) {
        let at = time::OffsetDateTime::now_utc().unix_timestamp();
        let memory = bento_hostinfo::read_memory().unwrap_or_default();
        let storage = bento_hostinfo::disk_usage(Path::new(&self.storage_dir)).unwrap_or_default();
        self.sampler.record_host(
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
        for instance in instances {
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
                &sample,
                storage_used_gib,
                at,
            );
        }
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
    async fn host(&self, window: Duration) -> Result<HostMetrics, BoxError> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let state = self.state.lock().expect("metrics state");
        Ok(HostMetrics {
            cpu_pct: state.host_cpu_pct.window(window, now),
            memory_used_mib: state.host_memory_used_mib.window(window, now),
            memory_total_mib: state.host_memory_total_mib,
            storage_used_gib: state.host_storage_used_gib,
            storage_total_gib: state.host_storage_total_gib,
            cpu_count: state.cpu_count,
            placeholder: false,
        })
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
        let mut cpu_ns_per_sec = 0.0;
        for entry in mine {
            cpu_ns_per_sec += entry.cpu_ns_per_sec;
            totals.memory_used_mib += entry.memory_used_mib.last() as i64;
            totals.storage_used_gib += entry.storage_used_gib;
        }
        // The account total measures against every core of the host, not
        // against the virtual processors of one instance, so that two
        // accounts on one host can be compared.
        let cores = state.cpu_count.max(1) as f64;
        totals.cpu_pct = (cpu_ns_per_sec / (cores * NS_PER_SEC) * 100.0).clamp(0.0, 100.0);
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

    fn sampler() -> Sampler {
        Sampler::default()
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
        sampler.record_host(reading(1000, 850), t);
        let host = sampler.host(Duration::from_secs(3600)).await.unwrap();
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

        sampler.record_host(reading(2000, 1600), t + 30);
        let host = sampler.host(Duration::from_secs(3600)).await.unwrap();
        assert_eq!(host.cpu_pct.len(), 1);
        assert_eq!(host.cpu_pct[0].value, 25.0);
    }

    #[tokio::test]
    async fn instance_processor_time_becomes_a_share_of_its_own_vcpus() {
        let sampler = sampler();
        let at = now();
        // Two virtual processors, both idle to start.
        sampler.record_instance("uuid-a", 1, &sample(0, 2, Some(512 * 1024)), 1.5, at);
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
        sampler.record_instance("uuid-a", 1, &sample(90_000_000_000, 2, None), 0.0, t);
        // The domain restarted, so its cumulative counter went backwards.
        sampler.record_instance("uuid-a", 1, &sample(5_000_000_000, 2, None), 0.0, t + 30);
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
        sampler.record_instance("uuid-a", 1, &sample(35_000_000_000, 2, None), 0.0, t + 60);
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
        sampler.record_instance("uuid-a", 1, &sample(0, 1, None), 0.0, t);
        sampler.record_instance("uuid-a", 1, &sample(15_000_000_000, 1, None), 0.0, t + 30);
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
        sampler.record_host(reading(1000, 850), t);
        // Two instances of one user, one of another.
        for (uuid, owner) in [("a", 1), ("b", 1), ("c", 2)] {
            sampler.record_instance(uuid, owner, &sample(0, 2, Some(1024 * 1024)), 2.0, t);
            sampler.record_instance(
                uuid,
                owner,
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
        sampler.record_instance("gone", 1, &sample(0, 1, Some(1024)), 1.0, t);
        sampler.record_instance("kept", 1, &sample(0, 1, Some(1024)), 1.0, t);
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
