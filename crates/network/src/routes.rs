use std::net::Ipv4Addr;
use std::process::{Output, Stdio};

use async_trait::async_trait;
use tokio::process::Command;

use crate::{DynError, Error, Ipv4Prefix, Result, invalid};

/// One route to a guest subprefix owned by another machine
/// (MULTI-NODE 8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub destination: Ipv4Prefix,
    pub next_hop: Ipv4Addr,
}

/// Applies routes for remotely owned guest subprefixes and interface
/// flags needed to reach them (MULTI-NODE 8.2 and 8.3).
#[async_trait]
pub trait RouteApplier: Send + Sync {
    /// Every route Bento currently has installed for guest subprefixes.
    async fn installed(&self) -> std::result::Result<Vec<Route>, DynError>;
    async fn add(&self, route: &Route) -> std::result::Result<(), DynError>;
    async fn remove(&self, route: &Route) -> std::result::Result<(), DynError>;
    /// Sets a per-interface sysctl, such as proxy ARP on a bridge
    /// (MULTI-NODE 8.3).
    async fn set_interface_flag(
        &self,
        interface: &str,
        flag: &str,
        value: &str,
    ) -> std::result::Result<(), DynError>;
}

/// Applies guest routes by running the `ip` command (MULTI-NODE 8.2).
#[derive(Debug, Clone)]
pub struct IpRouteApplier {
    /// Overrides the ip binary path. An empty string means `ip` from
    /// `PATH`.
    pub path: String,
    /// The whole Bento private range.
    ///
    /// [`RouteApplier::installed`] reports only routes inside it, which
    /// is what makes [`converge`] safe to run on a machine that is not
    /// Bento's alone. Without this bound, converge would read every
    /// static route on the host as one of its own and delete the ones it
    /// did not plan. A machine on a LAN commonly carries such routes.
    pub private_range: Ipv4Prefix,
}

#[derive(Debug, thiserror::Error)]
#[error("network: {command}: {cause}: {output}")]
struct IpRouteApplyError {
    command: String,
    cause: String,
    output: String,
}

#[async_trait]
impl RouteApplier for IpRouteApplier {
    async fn installed(&self) -> std::result::Result<Vec<Route>, DynError> {
        let output = self.run_ip(&["-4", "route", "show"]).await?;
        Ok(parse_routes(
            &String::from_utf8_lossy(&output.stdout),
            self.private_range,
        ))
    }

    async fn add(&self, route: &Route) -> std::result::Result<(), DynError> {
        let destination = format_destination(route.destination);
        let next_hop = route.next_hop.to_string();
        self.run_ip(&["route", "replace", &destination, "via", &next_hop])
            .await?;
        Ok(())
    }

    async fn remove(&self, route: &Route) -> std::result::Result<(), DynError> {
        let destination = format_destination(route.destination);
        let next_hop = route.next_hop.to_string();
        self.run_ip(&["route", "del", &destination, "via", &next_hop])
            .await?;
        Ok(())
    }

    async fn set_interface_flag(
        &self,
        interface: &str,
        flag: &str,
        value: &str,
    ) -> std::result::Result<(), DynError> {
        // These values form a sysctl key, so reject command-line syntax
        // before enabling proxy ARP on a bridge (MULTI-NODE 8.3).
        if interface.is_empty() || !is_sysctl_component(interface) {
            return Err(Box::new(invalid(format!(
                "invalid interface name {interface:?}"
            ))));
        }
        if !is_sysctl_component(flag) {
            return Err(Box::new(invalid(format!(
                "invalid interface flag {flag:?}"
            ))));
        }

        let assignment = format!("net.ipv4.conf.{interface}.{flag}={value}");
        run_command("sysctl", &["-w", &assignment]).await?;
        Ok(())
    }
}

impl IpRouteApplier {
    async fn run_ip(&self, args: &[&str]) -> std::result::Result<Output, DynError> {
        let path = if self.path.is_empty() {
            "ip"
        } else {
            &self.path
        };
        run_command(path, args).await
    }
}

async fn run_command(path: &str, args: &[&str]) -> std::result::Result<Output, DynError> {
    let command = format!("{} {}", path, args.join(" "));
    let child = Command::new(path)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            Box::new(IpRouteApplyError {
                command: command.clone(),
                cause: error.to_string(),
                output: String::new(),
            }) as DynError
        })?;

    let output = child.wait_with_output().await.map_err(|error| {
        Box::new(IpRouteApplyError {
            command: command.clone(),
            cause: error.to_string(),
            output: String::new(),
        }) as DynError
    })?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = combined.trim().to_string();

    if !output.status.success() {
        return Err(Box::new(IpRouteApplyError {
            command,
            cause: output.status.to_string(),
            output: combined,
        }));
    }
    Ok(output)
}

fn is_sysctl_component(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

fn format_destination(prefix: Ipv4Prefix) -> String {
    format!("{}/{}", prefix.addr, prefix.bits)
}

/// Reads `ip -4 route show` and keeps only Bento's guest routes.
///
/// A route qualifies when its destination is a runner-slot prefix inside
/// `private_range` and it has a next hop. That excludes the default
/// route, every connected route, and every route the operator installed
/// for something else (MULTI-NODE 8.2).
fn parse_routes(text: &str, private_range: Ipv4Prefix) -> Vec<Route> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let destination = parse_destination(fields.next()?)?;
            if !crate::subnet::contains(private_range, destination.addr) {
                return None;
            }
            let fields: Vec<_> = fields.collect();
            let via = fields.iter().position(|field| *field == "via")?;
            let next_hop = fields.get(via + 1)?.parse().ok()?;
            Some(Route {
                destination,
                next_hop,
            })
        })
        .collect()
}

fn parse_destination(text: &str) -> Option<Ipv4Prefix> {
    let (addr, bits) = text.split_once('/')?;
    let addr = addr.parse().ok()?;
    let bits = bits.parse().ok()?;
    if !(24..=27).contains(&bits) {
        return None;
    }
    Some(Ipv4Prefix { addr, bits })
}

/// The changes made while converging guest routes (MULTI-NODE 8.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Converged {
    pub added: Vec<Route>,
    pub removed: Vec<Route>,
    pub unchanged: usize,
}

/// Converges installed guest routes to `desired` (MULTI-NODE 8.2).
pub async fn converge<A: RouteApplier + ?Sized>(
    applier: &A,
    desired: &[Route],
) -> Result<Converged> {
    for route in desired {
        if !(24..=27).contains(&route.destination.bits) {
            return Err(invalid(format!(
                "route destination {} is not a runner-slot prefix",
                format_destination(route.destination)
            )));
        }
    }
    let installed = applier.installed().await.map_err(Error::Apply)?;
    let mut converged = Converged {
        added: Vec::new(),
        removed: Vec::new(),
        unchanged: 0,
    };

    for route in unique(&installed) {
        if !desired.contains(route) {
            applier.remove(route).await.map_err(Error::Apply)?;
            converged.removed.push(*route);
        }
    }
    for route in unique(desired) {
        if installed.contains(route) {
            converged.unchanged += 1;
        } else {
            applier.add(route).await.map_err(Error::Apply)?;
            converged.added.push(*route);
        }
    }
    Ok(converged)
}

fn unique(routes: &[Route]) -> impl Iterator<Item = &Route> {
    routes
        .iter()
        .enumerate()
        .filter_map(|(index, route)| (!routes[..index].contains(route)).then_some(route))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct FakeRouteApplier {
        installed: Mutex<Vec<Route>>,
        added: Mutex<Vec<Route>>,
        removed: Mutex<Vec<Route>>,
        flags: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait]
    impl RouteApplier for FakeRouteApplier {
        async fn installed(&self) -> std::result::Result<Vec<Route>, DynError> {
            Ok(self.installed.lock().unwrap().clone())
        }

        async fn add(&self, route: &Route) -> std::result::Result<(), DynError> {
            self.added.lock().unwrap().push(*route);
            let mut installed = self.installed.lock().unwrap();
            if !installed.contains(route) {
                installed.push(*route);
            }
            Ok(())
        }

        async fn remove(&self, route: &Route) -> std::result::Result<(), DynError> {
            self.removed.lock().unwrap().push(*route);
            self.installed.lock().unwrap().retain(|item| item != route);
            Ok(())
        }

        async fn set_interface_flag(
            &self,
            interface: &str,
            flag: &str,
            value: &str,
        ) -> std::result::Result<(), DynError> {
            self.flags.lock().unwrap().push((
                interface.to_string(),
                flag.to_string(),
                value.to_string(),
            ));
            Ok(())
        }
    }

    fn prefix(text: &str) -> Ipv4Prefix {
        let (addr, bits) = text.split_once('/').unwrap();
        Ipv4Prefix {
            addr: addr.parse().unwrap(),
            bits: bits.parse().unwrap(),
        }
    }

    fn route(destination: &str, next_hop: &str) -> Route {
        Route {
            destination: prefix(destination),
            next_hop: next_hop.parse().unwrap(),
        }
    }

    #[tokio::test]
    async fn converge_installs_every_desired_route() {
        let fake = FakeRouteApplier::default();
        let desired = [
            route("10.100.0.128/25", "10.0.0.97"),
            route("10.100.1.128/25", "10.0.0.98"),
        ];

        let result = converge(&fake, &desired).await.unwrap();

        assert_eq!(result.added, desired);
        assert!(result.removed.is_empty());
        assert_eq!(result.unchanged, 0);
        assert_eq!(*fake.installed.lock().unwrap(), desired);
    }

    #[tokio::test]
    async fn converge_is_idempotent() {
        let fake = FakeRouteApplier::default();
        let desired = [route("10.100.0.128/25", "10.0.0.97")];
        converge(&fake, &desired).await.unwrap();
        fake.added.lock().unwrap().clear();
        fake.removed.lock().unwrap().clear();

        let result = converge(&fake, &desired).await.unwrap();

        assert!(result.added.is_empty());
        assert!(result.removed.is_empty());
        assert_eq!(result.unchanged, 1);
        assert!(fake.added.lock().unwrap().is_empty());
        assert!(fake.removed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn converge_removes_stale_route() {
        let stale = route("10.100.0.128/25", "10.0.0.97");
        let fake = FakeRouteApplier {
            installed: Mutex::new(vec![stale]),
            ..Default::default()
        };

        let result = converge(&fake, &[]).await.unwrap();

        assert!(result.added.is_empty());
        assert_eq!(result.removed, [stale]);
        assert_eq!(result.unchanged, 0);
        assert!(fake.installed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn converge_changes_next_hop() {
        let old = route("10.100.0.128/25", "10.0.0.97");
        let new = route("10.100.0.128/25", "10.0.0.98");
        let fake = FakeRouteApplier {
            installed: Mutex::new(vec![old]),
            ..Default::default()
        };

        let result = converge(&fake, &[new]).await.unwrap();

        assert_eq!(result.added, [new]);
        assert_eq!(result.removed, [old]);
        assert_eq!(*fake.installed.lock().unwrap(), [new]);
    }

    #[tokio::test]
    async fn interface_flag_rejects_invalid_interface_names() {
        let applier = IpRouteApplier {
            path: String::new(),
            private_range: prefix("10.100.0.0/16"),
        };
        for interface in ["bridge;reboot", "bridge name", "bridge/name", ""] {
            let error = applier
                .set_interface_flag(interface, "proxy_arp", "1")
                .await
                .unwrap_err();
            assert!(error.to_string().contains("invalid interface name"));
        }
    }

    #[test]
    fn parses_guest_routes() {
        // Taken from a real machine, plus the two guest routes.
        let text = "\
default via 10.0.0.1 dev end0 proto dhcp src 10.0.0.188 metric 100
10.0.0.0/24 dev end0 proto kernel scope link src 10.0.0.188 metric 100
192.168.122.0/24 dev virbr0 proto kernel scope link src 192.168.122.1 linkdown
10.100.0.0/24 dev bento0 proto kernel scope link src 10.100.0.1
10.100.0.128/25 via 10.0.0.97 dev end0
10.100.1.192/26 via 10.0.0.98 dev end0 proto static
";

        assert_eq!(
            parse_routes(text, prefix("10.100.0.0/16")),
            [
                route("10.100.0.128/25", "10.0.0.97"),
                route("10.100.1.192/26", "10.0.0.98"),
            ]
        );
    }

    #[test]
    fn a_route_outside_the_private_range_is_not_bentos_to_manage() {
        // converge deletes an installed route it did not plan. Reading a
        // route the operator installed for something else would therefore
        // delete it, so `installed` never reports one.
        let text = "\
192.168.5.0/24 via 10.0.0.1 dev end0 proto static
172.16.9.0/25 via 10.0.0.5 dev end0
10.100.0.128/25 via 10.0.0.97 dev end0
";
        assert_eq!(
            parse_routes(text, prefix("10.100.0.0/16")),
            [route("10.100.0.128/25", "10.0.0.97")],
            "converge would have deleted an operator route"
        );
    }

    #[tokio::test]
    async fn converge_refuses_a_destination_that_is_not_a_slot_prefix() {
        let fake = FakeRouteApplier::default();
        assert!(
            converge(&fake, &[route("10.100.0.0/16", "10.0.0.97")])
                .await
                .is_err()
        );
        assert!(fake.added.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ip_route_applier_exec() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let ok = directory.path().join("ip-ok");
        std::fs::write(&ok, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\n").unwrap();
        std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o755)).unwrap();
        let applier = IpRouteApplier {
            path: ok.to_string_lossy().into_owned(),
            private_range: prefix("10.100.0.0/16"),
        };

        applier
            .add(&route("10.100.0.128/25", "10.0.0.97"))
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(format!("{}.args", ok.display())).unwrap(),
            "route\nreplace\n10.100.0.128/25\nvia\n10.0.0.97\n"
        );
    }
}
