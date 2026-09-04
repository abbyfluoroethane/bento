//! Generated metrics for development. Nothing here measures anything: the
//! series are smooth functions of time seeded by the instance UUID, so a
//! chart looks alive and stays continuous across polls, while every
//! figure is flagged `placeholder` so the page can say so.
//!
//! The real sampler is tracked in the repository issues; when it lands it
//! implements [`Metrics`] and replaces this in `bentod`.

use std::time::Duration;

use async_trait::async_trait;

use crate::{BoxError, HostMetrics, InstanceMetrics, Metrics, Point, UserMetrics};

/// Placeholder host sizes. A small home server.
const HOST_MEMORY_MIB: i64 = 16 * 1024;
const HOST_STORAGE_GIB: i64 = 128;
const HOST_CPUS: i64 = 8;
/// Spacing between generated samples.
const STEP_SECONDS: i64 = 30;

/// The development [`Metrics`] implementation.
#[derive(Debug, Clone, Default)]
pub struct PlaceholderMetrics;

fn hash(text: &str) -> u64 {
    // FNV-1a: deterministic, dependency-free, good enough for a seed.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// A smooth, bounded signal: three sines with seeded phases plus a little
/// per-bucket noise, mapped into `lo..=hi`.
fn signal(seed: u64, at: i64, lo: f64, hi: f64) -> f64 {
    let t = at as f64;
    let phase = |shift: u64| ((seed.rotate_left((shift % 64) as u32) % 6283) as f64) / 1000.0;
    let slow = (t / 1900.0 + phase(1)).sin();
    let medium = (t / 410.0 + phase(17)).sin();
    let fast = (t / 95.0 + phase(33)).sin();
    let bucket = hash(&format!("{seed}:{}", at / STEP_SECONDS));
    let noise = ((bucket % 1000) as f64 / 1000.0) - 0.5;
    let unit = 0.5 + 0.28 * slow + 0.14 * medium + 0.06 * fast + 0.04 * noise;
    lo + (hi - lo) * unit.clamp(0.0, 1.0)
}

fn series(seed: u64, window: Duration, lo: f64, hi: f64) -> Vec<Point> {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let start = now - window.as_secs() as i64;
    let first = start - start.rem_euclid(STEP_SECONDS);
    (first..=now)
        .step_by(STEP_SECONDS as usize)
        .map(|at| Point {
            at,
            value: signal(seed, at, lo, hi),
        })
        .collect()
}

#[async_trait]
impl Metrics for PlaceholderMetrics {
    async fn host(&self, window: Duration) -> Result<HostMetrics, BoxError> {
        let seed = hash("host");
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        Ok(HostMetrics {
            cpu_pct: series(seed, window, 4.0, 88.0),
            memory_used_mib: series(seed ^ 0xa5a5, window, 5_200.0, 13_800.0),
            memory_total_mib: HOST_MEMORY_MIB,
            storage_used_gib: signal(seed ^ 0x5a5a, now, 60.0, 90.0),
            storage_total_gib: HOST_STORAGE_GIB,
            cpu_count: HOST_CPUS,
            placeholder: true,
        })
    }

    async fn instance(&self, uuid: &str, window: Duration) -> Result<InstanceMetrics, BoxError> {
        let seed = hash(uuid);
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        Ok(InstanceMetrics {
            cpu_pct: series(seed, window, 1.0, 95.0),
            memory_used_mib: series(seed ^ 0xa5a5, window, 300.0, 1_700.0),
            storage_used_gib: signal(seed ^ 0x5a5a, now, 2.0, 16.0),
            placeholder: true,
        })
    }

    async fn user(&self, user_id: i64) -> Result<UserMetrics, BoxError> {
        let seed = hash(&format!("user:{user_id}"));
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        Ok(UserMetrics {
            cpu_pct: signal(seed, now, 2.0, 60.0),
            memory_used_mib: signal(seed ^ 0xa5a5, now, 500.0, 9_000.0) as i64,
            storage_used_gib: signal(seed ^ 0x5a5a, now, 4.0, 40.0),
            placeholder: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn series_are_continuous_and_bounded() {
        let metrics = PlaceholderMetrics;
        let host = metrics.host(Duration::from_secs(3600)).await.unwrap();
        assert!(host.placeholder);
        assert!(host.cpu_pct.len() >= 120);
        assert!(
            host.cpu_pct
                .iter()
                .all(|p| (0.0..=100.0).contains(&p.value))
        );
        // The same bucket yields the same value on the next poll.
        let again = metrics.host(Duration::from_secs(3600)).await.unwrap();
        assert_eq!(host.cpu_pct[10], again.cpu_pct[10]);
        // Different instances get different series.
        let a = metrics
            .instance("a", Duration::from_secs(600))
            .await
            .unwrap();
        let b = metrics
            .instance("b", Duration::from_secs(600))
            .await
            .unwrap();
        assert_ne!(a.cpu_pct[3].value, b.cpu_pct[3].value);
    }
}
