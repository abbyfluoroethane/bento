//! What libvirt holds. The monitor connects with the same client the
//! control plane uses (SPEC 4.1), so what it counts is what `bentod`
//! would see, and a connection the monitor cannot make is a connection
//! `bentod` could not make either.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bento_hypervisor::{Client, Hypervisor};
use bento_types::State;
use url::Url;

/// A libvirt connection is answered on a local socket, so it is normally
/// immediate. This bound keeps a hung `virtqemud` from stopping the
/// screen from drawing.
const CENSUS_TIMEOUT: Duration = Duration::from_secs(3);

/// The domains libvirt knows, by state. These are every domain of the
/// connection, not only the ones Bento defined: a domain another tool
/// left behind still takes memory from the host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Census {
    pub running: usize,
    pub stopped: usize,
    pub starting: usize,
    pub names: Vec<(String, State)>,
}

impl Census {
    pub fn total(&self) -> usize {
        self.running + self.stopped + self.starting
    }
}

/// The socket of a libvirt URI, as `bentod` resolves it: only an explicit
/// `?socket=` overrides the built-in default (`bentod/src/setup.rs`).
pub fn socket_path(uri: &str) -> PathBuf {
    if uri.is_empty() {
        return PathBuf::new();
    }
    Url::parse(uri)
        .ok()
        .and_then(|url| {
            url.query_pairs()
                .find(|(key, value)| key == "socket" && !value.is_empty())
                .map(|(_, value)| PathBuf::from(value.into_owned()))
        })
        .unwrap_or_default()
}

/// Connects, counts, and closes. The connection is not held between
/// frames: a monitor left open overnight must not keep a libvirt client
/// alive, and reconnecting costs one local socket round trip.
pub async fn census(socket: &Path) -> Result<Census, String> {
    let work = async {
        let client = Client::connect(socket)
            .await
            .map_err(|error| error.to_string())?;
        let domains = client.list().await.map_err(|error| error.to_string());
        let _ = client.close().await;
        domains
    };
    let domains = tokio::time::timeout(CENSUS_TIMEOUT, work)
        .await
        .map_err(|_| format!("libvirt did not answer in {}s", CENSUS_TIMEOUT.as_secs()))??;

    let mut census = Census::default();
    for domain in domains {
        match domain.state {
            State::Running => census.running += 1,
            State::Stopped => census.stopped += 1,
            State::Starting => census.starting += 1,
        }
        census.names.push((domain.name, domain.state));
    }
    census.names.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(census)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_socket_query_overrides_the_default() {
        assert_eq!(socket_path("qemu:///system"), PathBuf::new());
        assert_eq!(
            socket_path("qemu:///system?socket=/run/libvirt/virtqemud-sock"),
            PathBuf::from("/run/libvirt/virtqemud-sock")
        );
        assert_eq!(socket_path(""), PathBuf::new());
    }

    #[tokio::test]
    async fn a_missing_socket_reports_the_reason_rather_than_hanging() {
        let error = census(Path::new("/no/such/libvirt-sock"))
            .await
            .expect_err("no socket");
        assert!(!error.is_empty());
    }
}
