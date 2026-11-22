//! Whether the deployment answers where the configuration says it
//! listens, and whether the libvirt socket accepts a client.
//!
//! These are the only connections the install screen makes. They open a
//! local socket and close it again: no request is sent, so nothing on
//! the other side changes. They sit behind [`Connector`] so that the step
//! table can be tested without the host (CLAUDE.md).

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use bento_config::Config;

/// A listener that does not accept in this time is reported as not
/// answering. A local listener answers or refuses at once; only a
/// firewall that drops the packet makes the wait this long.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// Opens and closes connections, and reports only whether they opened.
pub trait Connector {
    /// Whether a TCP connection to `address` opens in [`CONNECT_TIMEOUT`].
    fn tcp(&self, address: &str) -> bool;
    /// Whether the Unix socket at `path` accepts a connection.
    fn unix(&self, path: &Path) -> bool;
}

/// The host's own network stack.
pub struct SystemConnector;

impl Connector for SystemConnector {
    fn tcp(&self, address: &str) -> bool {
        let Ok(addresses) = address.to_socket_addrs() else {
            return false;
        };
        let addresses: Vec<SocketAddr> = addresses.collect();
        addresses
            .iter()
            .any(|address| TcpStream::connect_timeout(address, CONNECT_TIMEOUT).is_ok())
    }

    fn unix(&self, path: &Path) -> bool {
        UnixStream::connect(path).is_ok()
    }
}

/// A connector that answers from two lists.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct FakeConnector {
    pub tcp_open: Vec<String>,
    pub unix_open: Vec<std::path::PathBuf>,
}

#[cfg(test)]
impl Connector for FakeConnector {
    fn tcp(&self, address: &str) -> bool {
        self.tcp_open.iter().any(|open| open == address)
    }

    fn unix(&self, path: &Path) -> bool {
        self.unix_open.iter().any(|open| open == path)
    }
}

/// One listen address of the configuration, and whether it answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    /// `http`, `https`, `ssh`, or `runner`: the configuration key.
    pub name: &'static str,
    /// The address as the configuration writes it.
    pub listen: String,
    /// The address the probe connects to. A wildcard bind is reached on
    /// loopback, because every address of the host reaches it and
    /// loopback is the one that needs no route.
    pub dial: String,
    pub reachable: bool,
}

/// The name of the runner listener, the one address a runner machine
/// has (MULTI-NODE 19).
pub const RUNNER: &str = "runner";

/// Connects to every listen address of `config`. The controller
/// listeners and the runner listener are all probed, and the step
/// table picks the ones of the role.
pub fn listeners(config: &Config, connector: &dyn Connector) -> Vec<Listener> {
    [
        ("http", config.listen.http.as_str()),
        ("https", config.listen.https.as_str()),
        ("ssh", config.listen.ssh.as_str()),
        (RUNNER, config.runner.listen.as_str()),
    ]
    .into_iter()
    .map(|(name, listen)| {
        let dial = dial_address(listen);
        Listener {
            name,
            listen: listen.to_string(),
            reachable: dial.as_deref().is_some_and(|dial| connector.tcp(dial)),
            dial: dial.unwrap_or_default(),
        }
    })
    .collect()
}

/// The address to connect to for a listen address. `":443"`,
/// `"0.0.0.0:443"`, and `"[::]:443"` bind every address, so they are
/// reached on `127.0.0.1`. `None` for a value with no port.
pub fn dial_address(listen: &str) -> Option<String> {
    let (host, port) = listen.rsplit_once(':')?;
    if port.is_empty() || port.parse::<u16>().is_err() {
        return None;
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let wildcard = bare.is_empty()
        || bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_unspecified());
    Some(if wildcard {
        format!("127.0.0.1:{port}")
    } else {
        listen.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_bind_is_reached_on_loopback() {
        assert_eq!(dial_address(":443").as_deref(), Some("127.0.0.1:443"));
        assert_eq!(dial_address("0.0.0.0:22").as_deref(), Some("127.0.0.1:22"));
        assert_eq!(dial_address("[::]:443").as_deref(), Some("127.0.0.1:443"));
        assert_eq!(
            dial_address("127.0.0.1:10080").as_deref(),
            Some("127.0.0.1:10080")
        );
        assert_eq!(
            dial_address("10.0.0.97:10443").as_deref(),
            Some("10.0.0.97:10443")
        );
        assert_eq!(dial_address("[::1]:22").as_deref(), Some("[::1]:22"));
        assert_eq!(dial_address("10443"), None);
        assert_eq!(dial_address(""), None);
    }

    #[test]
    fn every_listen_address_is_probed_and_only_open_ones_answer() {
        let config = Config::default();
        let connector = FakeConnector {
            tcp_open: vec!["127.0.0.1:10080".to_string(), "127.0.0.1:443".to_string()],
            ..Default::default()
        };
        let found = listeners(&config, &connector);
        let names: Vec<&str> = found.iter().map(|listener| listener.name).collect();
        assert_eq!(names, vec!["http", "https", "ssh", RUNNER]);
        let reachable: Vec<bool> = found.iter().map(|listener| listener.reachable).collect();
        assert_eq!(reachable, vec![true, true, false, false]);
        assert_eq!(found[1].listen, ":443");
        assert_eq!(found[1].dial, "127.0.0.1:443");
    }

    #[test]
    fn the_system_connector_reaches_a_local_listener_and_nothing_else() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address").to_string();
        assert!(SystemConnector.tcp(&address));
        drop(listener);
        assert!(!SystemConnector.tcp("not an address"));

        let directory = tempfile::tempdir().expect("tempdir");
        let socket = directory.path().join("sock");
        assert!(!SystemConnector.unix(&socket));
        let _server = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        assert!(SystemConnector.unix(&socket));
    }
}
