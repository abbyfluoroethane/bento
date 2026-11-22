//! The deployment preflight: a read-only list of what a host needs before
//! Bento can run on it (SPEC 4.2, DEPLOYING.md sections 1, 2, and 6).
//!
//! `serve` refuses to start on a failed SPEC 4.2 check, one at a time. The
//! preflight reports every requirement at once, with a fix for each, so an
//! operator (or `bento-monitor`) sees the whole list before the first
//! start. It changes no host setting. Three probes touch the host for a
//! moment: a port bind closes at once, `nft` runs in check mode, and the
//! directory write probe makes the file `.bento-write-probe` in each
//! checked directory and deletes it again (SPEC 4.2).
//!
//! # Fatal and not fatal
//!
//! A requirement is fatal only when a Bento process refuses to start
//! without it: `serve`, `runner`, `proxy`, or `sshd` stops at startup, or
//! the configuration does not load. The monitor blocks its Start step on
//! a fatal requirement, so a fatal flag on a setting that no process
//! enforces blocks a host that can run. A missing setting that only
//! disables a feature, for example OIDC (nobody can sign in) or the image
//! allowlist (nobody can make an instance), is a warning. Its detail says
//! what does not work.
//!
//! # Privilege
//!
//! The units run as root (DEPLOYING.md section 6). A check that runs as a
//! different user cannot test all that root can do. [`Privilege`] says
//! whether the result stands for this process or for the root units that
//! this process only observes.
//!
//! Every decision is a function of what a [`Probe`] reads. The real host
//! is [`HostProbe`]; tests use [`FakeProbe`].

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use bento_config::{Config, resolve_listen_addr};
use bento_hypervisor::{CheckConfig, CheckDeps, CheckReport, CheckResult};
use serde::Serialize;

mod probe;
pub use probe::{
    FakeProbe, Holder, HostProbe, NFT_PROBE_RULESET, NftOutcome, Probe, parse_listeners,
    parse_socket_link,
};

#[cfg(test)]
mod tests;

/// The outcome of one requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Met,
    /// Not met. A fatal requirement in this state stops Bento.
    Unmet,
    /// Not met, and Bento still starts.
    Warn,
    /// Not checked, because an earlier requirement failed or the run is
    /// offline.
    Skipped,
}

/// The section a requirement belongs to. The report lists groups in this
/// order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    Host,
    Libvirt,
    Network,
    Ports,
    Storage,
    Config,
    Images,
}

/// How to meet a requirement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Fix {
    pub summary: String,
    /// Argument vectors to run in order, with no shell. Empty means the
    /// operator does the fix by hand.
    pub commands: Vec<Vec<String>>,
    /// The commands need root.
    pub root: bool,
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Requirement {
    /// Stable kebab-case key, for example `nft-permitted`.
    pub id: String,
    pub group: Group,
    pub title: String,
    pub status: Status,
    /// An `Unmet` fatal requirement stops Bento.
    pub fatal: bool,
    pub detail: String,
    /// Present only when the status is `Unmet` or `Warn`.
    pub fix: Option<Fix>,
}

/// Every requirement for one role, in group order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub role: String,
    pub requirements: Vec<Requirement>,
}

impl Report {
    /// No fatal requirement is `Unmet`.
    pub fn ok(&self) -> bool {
        !self
            .requirements
            .iter()
            .any(|requirement| requirement.fatal && requirement.status == Status::Unmet)
    }

    /// The `Unmet` requirements, fatal first, otherwise in report order.
    pub fn unmet(&self) -> Vec<&Requirement> {
        let mut unmet: Vec<&Requirement> = self
            .requirements
            .iter()
            .filter(|requirement| requirement.status == Status::Unmet)
            .collect();
        unmet.sort_by_key(|requirement| !requirement.fatal);
        unmet
    }

    /// The number of `Met`, `Unmet`, and `Warn` requirements.
    pub fn counts(&self) -> (usize, usize, usize) {
        let count = |status| {
            self.requirements
                .iter()
                .filter(|requirement| requirement.status == status)
                .count()
        };
        (
            count(Status::Met),
            count(Status::Unmet),
            count(Status::Warn),
        )
    }

    /// The requirement with this id.
    pub fn get(&self, id: &str) -> Option<&Requirement> {
        self.requirements
            .iter()
            .find(|requirement| requirement.id == id)
    }
}

/// The part this machine plays (MULTI-NODE 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Controller,
    Runner,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Controller => "controller",
            Role::Runner => "runner",
        }
    }
}

/// Whose privileges the result stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Privilege {
    /// The result is the truth for this process. `bentod check` and
    /// `serve` use this: an operator who runs `bentod check` without root
    /// sees what that user can do, and a root run gives the full result.
    #[default]
    Actual,
    /// The checks run on behalf of the root units, but this process is not
    /// root. `bento-monitor` uses this when it runs as a user with sudo
    /// (DEPLOYING.md section 6). A check that fails only because this
    /// process lacks root is `Skipped`, not `Unmet`, so the monitor does
    /// not block a host that the root units can use. The detail of each
    /// such check says to run the check as root to test it.
    Observer,
}

/// What the preflight checks against.
pub struct Input<'a> {
    /// `None` runs only the checks that need no configuration, plus an
    /// `Unmet` fatal `config-loads` with `config_error` as its detail.
    pub config: Option<&'a Config>,
    pub config_error: Option<String>,
    /// Goes into fix commands such as `bentod -config <path> fetch-images`.
    pub config_path: Option<PathBuf>,
    pub role: Role,
    /// Permits network probes: OIDC discovery and DNS.
    pub online: bool,
    /// An instance requests nested virtualization (SPEC 4.2 item 7).
    pub nested_wanted: bool,
    pub privilege: Privilege,
    /// Permits the port binds. `false` leaves every port requirement
    /// `Skipped`. A unit that starts at the same moment as a bind of its
    /// port fails its own bind, so the monitor turns this off for a short
    /// time after it starts the units.
    pub binds: bool,
}

/// A machine with a runner listener on an underlay address and no runner
/// list is a runner. Anything else is a controller, which is also what a
/// fresh host installs as (MULTI-NODE 4).
pub fn detect_role(config: Option<&Config>) -> Role {
    match config {
        Some(config) if config.runners.is_empty() && is_underlay(&config.runner.listen) => {
            Role::Runner
        }
        _ => Role::Controller,
    }
}

fn is_underlay(listen: &str) -> bool {
    listen
        .parse::<SocketAddr>()
        .map(|address| !address.ip().is_loopback() && !address.ip().is_unspecified())
        .unwrap_or(false)
}

const OS_RELEASE: &str = "/etc/os-release";
const PROC_STATUS: &str = "/proc/self/status";
const UID_MAP: &str = "/proc/self/uid_map";
const PROC_LIMITS: &str = "/proc/self/limits";
const IP_FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";
const SYS_CLASS_NET: &str = "/sys/class/net";
const CONTAINER_MARKERS: [&str; 2] = ["/run/.containerenv", "/.dockerenv"];
const VIRTQEMUD_SOCKET: &str = "/run/libvirt/virtqemud-sock";
/// Where Fedora puts the libvirt daemons. A normal user's `PATH` often
/// does not hold it.
const SBIN: &str = "/usr/sbin";
/// The words of an `nft` refusal that means "not root" (EPERM).
const EPERM_WORDS: [&str; 2] = ["Operation not permitted", "EPERM"];
/// The text that a write probe gives for EACCES.
const EACCES_WORDS: [&str; 2] = ["Permission denied", "os error 13"];
/// The sentence that ends the detail of each check that an observer skips.
const RUN_AS_ROOT: &str = "Run the check as root to test this.";
const CAP_NET_BIND_SERVICE: u32 = 10;
const CAP_NET_ADMIN: u32 = 12;
/// File descriptors that the proxy uses besides one for each port.
const SPARE_FILES: u64 = 64;
/// The number of busy ports that a detail names.
const BUSY_PORTS_SHOWN: usize = 5;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
/// Interface prefixes of networks that forward through this host. The
/// Bento `forward` chain drops their traffic (DEPLOYING.md section 2).
const FOREIGN_BRIDGES: [&str; 6] = ["virbr", "docker", "podman", "cni-", "br-", "tailscale"];

/// Runs every requirement for the role. Reads the host through `probe`
/// and changes nothing.
pub fn run(input: &Input, probe: &dyn Probe) -> Report {
    let mut run = Run {
        input,
        probe,
        deps: probe.check_deps(),
        holders: None,
        out: Vec::new(),
    };
    run.all();
    let mut requirements = run.out;
    // The groups print as sections, so each group is contiguous whatever
    // order the checks ran in. The sort is stable.
    requirements.sort_by_key(|requirement| requirement.group as u8);
    Report {
        role: input.role.as_str().to_string(),
        requirements,
    }
}

/// The `nft-permitted` requirement alone. `serve` and `runner` call it at
/// startup through the shared host checks, so
/// a host that cannot load the Bento table stops at the check and not at
/// the first nftables load (SPEC 4.2).
pub fn check_nft_permitted(probe: &dyn Probe) -> Requirement {
    nft_permitted(&probe.check_deps(), Privilege::Actual, probe)
}

struct Run<'a> {
    input: &'a Input<'a>,
    probe: &'a dyn Probe,
    deps: CheckDeps,
    /// Filled on the first busy port. The scan reads every descriptor of
    /// every process, so it runs at most once.
    holders: Option<HashMap<u16, Holder>>,
    out: Vec<Requirement>,
}

impl Run<'_> {
    fn observer(&self) -> bool {
        self.input.privilege == Privilege::Observer
    }

    fn all(&mut self) {
        let controller = self.input.role == Role::Controller;
        self.push(config_loads(self.input));
        self.push(root_privileges(self.input.privilege, self.probe));
        self.push(container(self.probe));

        let hypervisor = bento_hypervisor::check(check_config(self.input), &self.deps);
        let fedora = fedora_like(&self.read_text(OS_RELEASE).unwrap_or_default());
        self.push(from_hypervisor(
            &hypervisor,
            "kvm device",
            "kvm-device",
            Group::Host,
            "KVM device",
            Some(manual(
                "Turn on virtualization in the firmware. Load the kvm module and give Bento access to /dev/kvm.",
            )),
        ));
        let ksm = from_hypervisor(
            &hypervisor,
            "ksm run",
            "ksm",
            Group::Host,
            "KSM",
            Some(ksm_fix(fedora)),
        );
        self.push(ksm_detail(ksm, self.probe));
        if self.input.nested_wanted {
            self.push(from_hypervisor(
                &hypervisor,
                "nested virtualization",
                "nested-virtualization",
                Group::Host,
                "Nested virtualization",
                Some(manual(
                    "Set kvm_intel.nested=1 or kvm_amd.nested=1. Then load the module again.",
                )),
            ));
        }
        for (name, id, title, package) in [
            ("qemu-img binary", "qemu-img", "qemu-img binary", "qemu-img"),
            ("xorriso binary", "xorriso", "xorriso binary", "xorriso"),
        ] {
            self.push(from_hypervisor(
                &hypervisor,
                name,
                id,
                Group::Host,
                title,
                Some(install(package, fedora)),
            ));
        }
        self.push(nft_binary(&self.deps, fedora));
        self.push(from_hypervisor(
            &hypervisor,
            "podman binary",
            "podman",
            Group::Host,
            "podman binary",
            Some(install("podman", fedora)),
        ));

        if let Some(config) = self.input.config {
            self.push(libvirt_socket(config, &hypervisor, &self.deps, self.probe));
        }

        self.push(dedicated_host(self.probe));
        self.push(nft_permitted(&self.deps, self.input.privilege, self.probe));
        self.push(ip_forward(self.probe));

        let Some(config) = self.input.config else {
            return;
        };
        if controller {
            self.controller_ports(config);
        } else {
            let requirement = self.bind_check(
                "port-runner",
                "Runner port",
                &config.runner.listen,
                "runner.listen",
            );
            self.push(requirement);
        }

        for (name, id, title, directory) in [
            (
                "image directory",
                "image-dir",
                "Image directory",
                &config.image_dir,
            ),
            (
                "storage directory",
                "storage-dir",
                "Storage directory",
                &config.storage_dir,
            ),
            (
                "container storage directory",
                "container-storage",
                "Container storage directory",
                &config.bootc.container_storage,
            ),
        ] {
            let mut requirement = from_hypervisor(
                &hypervisor,
                name,
                id,
                Group::Storage,
                title,
                Some(root_fix(
                    &format!("Make the directory {directory}."),
                    &[&["mkdir", "-p", directory]],
                )),
            );
            // The hypervisor check reports a missing directory as the bare
            // OS error. Name the directory, which the operator needs.
            if requirement.status != Status::Met && !requirement.detail.contains(directory.as_str())
            {
                requirement.detail = format!("{directory}: {}", requirement.detail);
            }
            self.push(requirement);
            let writable = format!("{name} writable");
            let requirement = match hypervisor.results.iter().find(|r| r.name == writable) {
                // The directories are root's (DEPLOYING.md section 4). A
                // user that is not root gets EACCES there, which says
                // nothing about the root units.
                Some(result)
                    if !result.ok
                        && self.observer()
                        && EACCES_WORDS.iter().any(|word| result.detail.contains(word)) =>
                {
                    Requirement::new(
                        &format!("{id}-writable"),
                        Group::Storage,
                        &format!("{title} writable"),
                        result.fatal,
                    )
                    .skipped(&format!(
                        "This user cannot write to {directory}. The units run as root. {RUN_AS_ROOT}"
                    ))
                }
                Some(_) => from_hypervisor(
                    &hypervisor,
                    &writable,
                    &format!("{id}-writable"),
                    Group::Storage,
                    &format!("{title} writable"),
                    Some(manual(&format!(
                        "Give the Bento user write access to {directory}. Bento runs as root (DEPLOYING.md section 1)."
                    ))),
                ),
                None => {
                    // The hypervisor check writes only into a directory
                    // that exists. Keep the id in the report anyway.
                    let fatal = hypervisor
                        .results
                        .iter()
                        .find(|r| r.name == name)
                        .is_some_and(|r| r.fatal);
                    Requirement::new(
                        &format!("{id}-writable"),
                        Group::Storage,
                        &format!("{title} writable"),
                        fatal,
                    )
                    .skipped(&format!("{directory} does not exist."))
                }
            };
            self.push(requirement);
        }

        if controller {
            self.push(base_domain(config));
            self.push(front_proxy(config));
            self.push(oidc_settings(config));
            self.push(oidc_discovery(config, self.input.online, self.probe));
            self.push(dns(config, self.input.online, self.probe));
            self.push(image_allowlist(config));
            self.push(image_arch(config, &self.probe.arch()));
            self.push(images_fetched(
                config,
                self.input.config_path.as_deref(),
                self.probe,
            ));
        }
    }

    fn controller_ports(&mut self, config: &Config) {
        for (id, title, listen, field) in [
            (
                "port-control-plane",
                "Control-plane port",
                &config.listen.http,
                "listen.http",
            ),
            (
                "port-https",
                "HTTPS port",
                &config.listen.https,
                "listen.https",
            ),
            ("port-ssh", "SSH port", &config.listen.ssh, "listen.ssh"),
        ] {
            let requirement = self.bind_check(id, title, listen, field);
            self.push(requirement);
        }
        let requirement = self.proxy_high_ports(config);
        self.push(requirement);
        self.push(control_plane_outside_range(config));
        let limits = self.read_text(PROC_LIMITS);
        self.push(open_files_limit(config, limits.as_deref()));
    }

    fn push(&mut self, requirement: Requirement) {
        self.out.push(requirement);
    }

    fn read_text(&self, path: &str) -> Option<String> {
        self.probe
            .read_file(Path::new(path))
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    fn holders(&mut self) -> &HashMap<u16, Holder> {
        let probe = self.probe;
        self.holders.get_or_insert_with(|| probe.port_holders())
    }

    /// Binds one listen address (SPEC 4). A port that `bentod` already
    /// holds is met: the check then runs next to a live deployment.
    fn bind_check(&mut self, id: &str, title: &str, listen: &str, field: &str) -> Requirement {
        let requirement = Requirement::new(id, Group::Ports, title, true);
        if !self.input.binds {
            return requirement.skipped(NO_BINDS);
        }
        let observer = self.observer();
        let address = match resolve_listen_addr(listen) {
            Ok(address) => address,
            Err(error) => {
                return requirement.fail(
                    &format!("{field}: {error}."),
                    Some(manual(&format!(
                        "Set {field} to an address and a port, for example 127.0.0.1:10080."
                    ))),
                );
            }
        };
        match self.probe.bind(address) {
            Ok(()) => requirement.met(&format!("{address} is free.")),
            Err(error) => match error.kind() {
                std::io::ErrorKind::AddrInUse => match self.holders().get(&address.port()) {
                    Some(holder) if holder.comm == "bentod" => requirement.met(&format!(
                        "{address} is in use by bentod (pid {}).",
                        holder.pid
                    )),
                    Some(holder) => requirement.fail(
                        &format!(
                            "{address} is in use by {} (pid {}).",
                            holder.comm, holder.pid
                        ),
                        Some(manual(&format!(
                            "Stop {}, or change {field} (DEPLOYING.md section 2).",
                            holder.comm
                        ))),
                    ),
                    // A root bentod holds its ports, and a user that is
                    // not root cannot see who holds them. That is the
                    // usual state of a live deployment, so an observer
                    // only warns.
                    None if observer => {
                        let mut requirement = requirement;
                        requirement.fatal = false;
                        requirement.fail(
                            &format!(
                                "{address} is in use by a process that this user cannot see. Run the check as root to see the holder."
                            ),
                            Some(manual(&format!(
                                "If bentod does not hold the port, stop the holder, or change {field} (DEPLOYING.md section 2)."
                            ))),
                        )
                    }
                    None => requirement.fail(
                        &format!("{address} is in use by a process that this user cannot see."),
                        Some(manual(&format!(
                            "Find the holder with ss -tlnp as root. Stop it, or change {field} (DEPLOYING.md section 2)."
                        ))),
                    ),
                },
                // The units have CAP_NET_BIND_SERVICE as root.
                std::io::ErrorKind::PermissionDenied if observer => requirement.skipped(&format!(
                    "Port {} needs CAP_NET_BIND_SERVICE, which this user does not have. The units run as root. {RUN_AS_ROOT}",
                    address.port()
                )),
                std::io::ErrorKind::PermissionDenied => requirement.fail(
                    &format!(
                        "Port {} needs CAP_NET_BIND_SERVICE (DEPLOYING.md section 1).",
                        address.port()
                    ),
                    Some(manual(
                        "Run bentod as root. The systemd units in DEPLOYING.md section 6 do this.",
                    )),
                ),
                _ => requirement.fail(&format!("bind {address}: {error}."), None),
            },
        }
    }

    /// Binds each port of the proxy range on the HTTPS address. The proxy
    /// fails when one port of the range is busy (SPEC 9.1, DEPLOYING.md
    /// section 2).
    fn proxy_high_ports(&mut self, config: &Config) -> Requirement {
        let requirement =
            Requirement::new("proxy-high-ports", Group::Ports, "Proxy high ports", true);
        if !self.input.binds {
            return requirement.skipped(NO_BINDS);
        }
        let (min, max) = (config.listen.proxy_port_min, config.listen.proxy_port_max);
        let ip = match resolve_listen_addr(&config.listen.https) {
            Ok(address) => address.ip(),
            Err(error) => return requirement.fail(&format!("listen.https: {error}."), None),
        };
        let mut failures = Vec::new();
        for port in min..=max {
            if let Err(error) = self.probe.bind(SocketAddr::new(ip, port)) {
                failures.push((port, error.kind()));
            }
        }
        let mut busy = Vec::new();
        if !failures.is_empty() {
            let holders = self.holders().clone();
            for (port, kind) in failures {
                let holder = holders.get(&port);
                if kind == std::io::ErrorKind::AddrInUse
                    && holder.is_some_and(|holder| holder.comm == "bentod")
                {
                    continue;
                }
                busy.push((port, kind, holder.cloned()));
            }
        }
        proxy_ports_outcome(requirement, ip, min, max, &busy, self.observer())
    }
}

/// The detail of a port requirement when the run makes no bind.
const NO_BINDS: &str = "Not checked. The units started a moment ago, and a bind at that moment can stop a unit. Press F5 to check again.";

/// The decision of [`Run::proxy_high_ports`] over the busy ports it found.
///
/// An observer cannot bind a port below 1024 and cannot see a root
/// holder. It drops the first kind of port, and for the second it only
/// warns: a root `bentod-proxy` that holds the range looks the same.
fn proxy_ports_outcome(
    requirement: Requirement,
    ip: IpAddr,
    min: u16,
    max: u16,
    busy: &[(u16, std::io::ErrorKind, Option<Holder>)],
    observer: bool,
) -> Requirement {
    let total = usize::from(max - min) + 1;
    let denied = busy
        .iter()
        .filter(|(_, kind, _)| *kind == std::io::ErrorKind::PermissionDenied)
        .count();
    let busy: Vec<(u16, std::io::ErrorKind, Option<Holder>)> = if observer {
        busy.iter()
            .filter(|(_, kind, _)| *kind != std::io::ErrorKind::PermissionDenied)
            .cloned()
            .collect()
    } else {
        busy.to_vec()
    };
    let busy = busy.as_slice();
    if busy.is_empty() && observer && denied > 0 {
        return requirement.skipped(&format!(
            "{denied} ports of {min}-{max} need CAP_NET_BIND_SERVICE, which this user does not have. The units run as root. {RUN_AS_ROOT}"
        ));
    }
    if busy.is_empty() {
        return requirement.met(&format!("All {total} ports {min}-{max} on {ip} are free."));
    }
    let hidden_only = observer && busy.iter().all(|(_, _, holder)| holder.is_none());
    let named: Vec<String> = busy
        .iter()
        .take(BUSY_PORTS_SHOWN)
        .map(|(port, kind, holder)| match (kind, holder) {
            (std::io::ErrorKind::PermissionDenied, _) => {
                format!("{port} (needs CAP_NET_BIND_SERVICE)")
            }
            (_, Some(holder)) => format!("{port} ({} pid {})", holder.comm, holder.pid),
            (_, None) => format!("{port} (holder unknown)"),
        })
        .collect();
    let more = if busy.len() > BUSY_PORTS_SHOWN {
        format!(" and {} more", busy.len() - BUSY_PORTS_SHOWN)
    } else {
        String::new()
    };
    let detail = format!(
        "{} of {total} ports {min}-{max} on {ip} are not free: {}{more}. The proxy stops when one port is busy.",
        busy.len(),
        named.join(", ")
    );
    if hidden_only {
        let mut requirement = requirement;
        requirement.fatal = false;
        return requirement.fail(
            &format!(
                "{detail} This user cannot see the holders. A running bentod-proxy holds these ports. Run the check as root to see the holder."
            ),
            Some(manual(
                "If bentod-proxy does not hold these ports, stop the programs that hold them. Or set proxy_port_min and proxy_port_max to a free range (DEPLOYING.md section 2).",
            )),
        );
    }
    let held_by = |port: u16, comms: &[&str]| {
        busy.iter().any(|(busy_port, _, holder)| {
            *busy_port == port
                && holder
                    .as_ref()
                    .is_some_and(|holder| comms.iter().any(|comm| holder.comm.starts_with(comm)))
        })
    };
    let llmnr = "Put a file in /etc/systemd/resolved.conf.d/ with [Resolve] and LLMNR=no. Then restart systemd-resolved.";
    let fix = if held_by(9090, &["systemd", "cockpit-ws"]) {
        let mut summary = "Stop cockpit.socket, which holds port 9090.".to_string();
        if held_by(5355, &["systemd-resolve"]) {
            summary.push_str(" For port 5355: ");
            summary.push_str(llmnr);
        }
        root_fix(
            &summary,
            &[&["systemctl", "disable", "--now", "cockpit.socket"]],
        )
    } else if held_by(5355, &["systemd-resolve"]) {
        manual(&format!("Port 5355 is LLMNR. {llmnr}"))
    } else {
        manual(
            "Stop the programs that hold these ports. Or set proxy_port_min and proxy_port_max to a free range (DEPLOYING.md section 2).",
        )
    };
    requirement.fail(&detail, Some(fix))
}

impl Requirement {
    fn new(id: &str, group: Group, title: &str, fatal: bool) -> Self {
        Requirement {
            id: id.to_string(),
            group,
            title: title.to_string(),
            status: Status::Skipped,
            fatal,
            detail: String::new(),
            fix: None,
        }
    }

    fn met(mut self, detail: &str) -> Self {
        self.status = Status::Met;
        self.detail = detail.to_string();
        self.fix = None;
        self
    }

    /// `Unmet` for a fatal requirement, `Warn` for the others.
    fn fail(mut self, detail: &str, fix: Option<Fix>) -> Self {
        self.status = if self.fatal {
            Status::Unmet
        } else {
            Status::Warn
        };
        self.detail = detail.to_string();
        self.fix = fix;
        self
    }

    fn skipped(mut self, detail: &str) -> Self {
        self.status = Status::Skipped;
        self.detail = detail.to_string();
        self.fix = None;
        self
    }
}

fn manual(summary: &str) -> Fix {
    Fix {
        summary: summary.to_string(),
        commands: Vec::new(),
        root: false,
    }
}

fn root_fix(summary: &str, commands: &[&[&str]]) -> Fix {
    Fix {
        summary: summary.to_string(),
        commands: commands
            .iter()
            .map(|argv| argv.iter().map(|arg| (*arg).to_string()).collect())
            .collect(),
        root: true,
    }
}

/// `dnf install` on Fedora and RHEL. Elsewhere the package name differs
/// too often to guess a command.
fn install(package: &str, fedora: bool) -> Fix {
    if fedora {
        root_fix(
            &format!("Install the {package} package."),
            &[&["dnf", "install", "-y", package]],
        )
    } else {
        manual(&format!(
            "Install the {package} package with the package manager of this distribution."
        ))
    }
}

/// ksmtuned turns KSM on when memory gets low (SPEC 5.4). Fedora has it as
/// a package. Elsewhere the name of the package can be different.
fn ksm_fix(fedora: bool) -> Fix {
    if fedora {
        root_fix(
            "Install and start ksmtuned to get the memory sharing of SPEC 5.4.",
            &[
                &["dnf", "install", "-y", "ksmtuned"],
                &["systemctl", "enable", "--now", "ksmtuned"],
            ],
        )
    } else {
        manual(
            "Install the ksmtuned package with the package manager of this distribution. Then start the ksmtuned service (SPEC 5.4).",
        )
    }
}

const KSM_RUN: &str = "/sys/kernel/mm/ksm/run";

/// Replaces the detail of a failed KSM check. The hypervisor crate writes
/// a lowercase log phrase, and the report shows full sentences.
fn ksm_detail(mut requirement: Requirement, probe: &dyn Probe) -> Requirement {
    if !matches!(requirement.status, Status::Unmet | Status::Warn) {
        return requirement;
    }
    requirement.detail = match probe.read_file(Path::new(KSM_RUN)) {
        Ok(bytes) => format!(
            "{KSM_RUN} is {}. KSM does not share memory between guests, so the host holds fewer guests (SPEC 5.4).",
            String::from_utf8_lossy(&bytes).trim()
        ),
        Err(error) => format!(
            "Cannot read {KSM_RUN}: {error}. This kernel possibly does not have KSM (SPEC 5.4)."
        ),
    };
    requirement
}

/// Fedora, or a distribution that says it is like Fedora or RHEL.
pub fn fedora_like(os_release: &str) -> bool {
    os_release.lines().any(|line| {
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        match key.trim() {
            "ID" => value == "fedora",
            "ID_LIKE" => value
                .split_whitespace()
                .any(|id| id == "fedora" || id == "rhel"),
            _ => false,
        }
    })
}

/// The SPEC 4.2 check settings for this configuration. With no
/// configuration, the directories and socket are the defaults; only the
/// checks that need no configuration read them.
fn check_config(input: &Input) -> CheckConfig {
    let mut check = CheckConfig {
        nested_wanted: input.nested_wanted,
        ..Default::default()
    };
    if let Some(config) = input.config {
        check.socket_path = socket_path(&config.libvirt_uri);
        check.image_dir = PathBuf::from(&config.image_dir);
        check.storage_dir = PathBuf::from(&config.storage_dir);
        check.container_storage = PathBuf::from(&config.bootc.container_storage);
        check.podman_required = podman_required(config);
    }
    check
}

/// A static OCI entry makes the Podman checks fatal (DEPLOYING.md
/// section 1, "Bootc OCI images").
fn podman_required(config: &Config) -> bool {
    config.images.iter().any(|image| !image.oci.is_empty())
}

/// The Unix socket of a libvirt URI, as `bentod` resolves it: only an
/// explicit `?socket=` overrides the default (`bentod/src/setup.rs`).
fn socket_path(uri: &str) -> PathBuf {
    url::Url::parse(uri)
        .ok()
        .and_then(|url| {
            url.query_pairs()
                .find(|(key, value)| key == "socket" && !value.is_empty())
                .map(|(_, value)| PathBuf::from(value.into_owned()))
        })
        .unwrap_or_else(|| PathBuf::from(bento_hypervisor::DEFAULT_SOCKET_PATH))
}

fn from_hypervisor(
    report: &CheckReport,
    name: &str,
    id: &str,
    group: Group,
    title: &str,
    fix: Option<Fix>,
) -> Requirement {
    match report.results.iter().find(|result| result.name == name) {
        Some(CheckResult {
            ok, fatal, detail, ..
        }) => {
            let requirement = Requirement::new(id, group, title, *fatal);
            if *ok {
                requirement.met(detail)
            } else {
                requirement.fail(&sentence(detail), fix)
            }
        }
        None => Requirement::new(id, group, title, false).skipped("Not checked."),
    }
}

/// Ends a detail with a full stop, for text that other crates wrote.
fn sentence(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() || text.ends_with('.') || text.ends_with(')') {
        text.to_string()
    } else {
        format!("{text}.")
    }
}

fn config_loads(input: &Input) -> Requirement {
    let requirement = Requirement::new("config-loads", Group::Host, "Configuration loads", true);
    let path = input
        .config_path
        .as_ref()
        .map(|path| path.display().to_string());
    match input.config {
        Some(_) => requirement.met(&match path {
            Some(path) => format!("{path} is valid."),
            None => "The configuration is valid.".to_string(),
        }),
        None => requirement.fail(
            &input
                .config_error
                .clone()
                .unwrap_or_else(|| "No configuration was given.".to_string()),
            Some(manual(
                "Correct the configuration file (DEPLOYING.md section 4). Then run the check again.",
            )),
        ),
    }
}

/// The effective uid and the effective capability set, from
/// `/proc/self/status`.
pub fn privileges(status: &str) -> (Option<u32>, Option<u64>) {
    let mut euid = None;
    let mut capeff = None;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("Uid:") {
            euid = value.split_whitespace().nth(1).and_then(|v| v.parse().ok());
        } else if let Some(value) = line.strip_prefix("CapEff:") {
            capeff = u64::from_str_radix(value.trim(), 16).ok();
        }
    }
    (euid, capeff)
}

/// Bento needs root, or the two capabilities that root gives it
/// (DEPLOYING.md section 1, "Root access"). A runner binds no privileged
/// port by default, so there it only warns. An observer is not the
/// process that needs root, so it skips the check.
fn root_privileges(privilege: Privilege, probe: &dyn Probe) -> Requirement {
    // Not fatal: no process refuses to start on its uid alone. The checks
    // that root gates (nft-permitted and the port binds) are fatal and
    // carry the refusal, so the rule "fatal means a Bento process refuses
    // to start" holds (module comment).
    let requirement = Requirement::new("root-privileges", Group::Host, "Root privileges", false);
    let status = probe
        .read_file(Path::new(PROC_STATUS))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let (euid, capeff) = privileges(&status);
    let has = |bit: u32| capeff.is_some_and(|caps| caps & (1 << bit) != 0);
    if euid == Some(0) {
        return requirement.met("The process runs as root.");
    }
    if has(CAP_NET_ADMIN) && has(CAP_NET_BIND_SERVICE) {
        return requirement.met("CAP_NET_ADMIN and CAP_NET_BIND_SERVICE are effective.");
    }
    let who = euid.map_or_else(|| "an unknown uid".to_string(), |uid| format!("uid {uid}"));
    if privilege == Privilege::Observer {
        return requirement.skipped(&format!(
            "The units run as root. This check runs as {who}. {RUN_AS_ROOT}"
        ));
    }
    requirement.fail(
        &format!(
            "The process runs as {who} without CAP_NET_ADMIN and CAP_NET_BIND_SERVICE. \
             nft needs CAP_NET_ADMIN in the host network namespace. \
             Ports 22 and 443 need CAP_NET_BIND_SERVICE. \
             Membership of the libvirt group is not sufficient. \
             Run bentod check as root for a full result."
        ),
        Some(manual(
            "Run the Bento units as root. The systemd units in DEPLOYING.md section 6 do this.",
        )),
    )
}

/// Whether `/proc/self/uid_map` maps every uid to itself, which is true
/// only outside a user namespace.
pub fn identity_uid_map(uid_map: &str) -> bool {
    let rows: Vec<Vec<&str>> = uid_map
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .filter(|fields| !fields.is_empty())
        .collect();
    rows == [vec!["0", "0", "4294967295"]]
}

/// Whether this process runs in a user namespace. Root in such a
/// namespace has no capabilities in the host network namespace.
fn in_user_namespace(probe: &dyn Probe) -> bool {
    probe
        .read_file(Path::new(UID_MAP))
        .is_ok_and(|bytes| !identity_uid_map(&String::from_utf8_lossy(&bytes)))
}

/// A container or a user namespace hides the host firewall and the host
/// paths from Bento (DEPLOYING.md section 9).
fn container(probe: &dyn Probe) -> Requirement {
    let requirement = Requirement::new(
        "container",
        Group::Host,
        "Not in a container or user namespace",
        false,
    );
    let mut evidence: Vec<String> = CONTAINER_MARKERS
        .iter()
        .filter(|marker| probe.exists(Path::new(marker)))
        .map(|marker| format!("{marker} exists"))
        .collect();
    if let Ok(bytes) = probe.read_file(Path::new(UID_MAP))
        && !identity_uid_map(&String::from_utf8_lossy(&bytes))
    {
        evidence.push(format!("{UID_MAP} is not the identity map"));
    }
    if evidence.is_empty() {
        return requirement.met("No container marker. The uid map is the identity map.");
    }
    requirement.fail(
        &format!(
            "This process runs in a container or a user namespace: {}. \
             In a user namespace, nft cannot change the host firewall. \
             The libvirt host must see image_dir and storage_dir at the same path.",
            evidence.join(", ")
        ),
        Some(manual(
            "Run Bento directly on a dedicated host or in a dedicated virtual machine (DEPLOYING.md section 9).",
        )),
    )
}

fn nft_binary(deps: &CheckDeps, fedora: bool) -> Requirement {
    let requirement = Requirement::new("nft-binary", Group::Host, "nft binary", true);
    match (deps.look_path)("nft") {
        Ok(path) => requirement.met(&path.display().to_string()),
        Err(error) => requirement.fail(
            &sentence(&error.to_string()),
            Some(install("nftables", fedora)),
        ),
    }
}

/// A daemon binary on `PATH` or in `/usr/sbin`.
fn daemon_installed(name: &str, deps: &CheckDeps, probe: &dyn Probe) -> bool {
    (deps.look_path)(name).is_ok() || probe.exists(&Path::new(SBIN).join(name))
}

fn libvirt_socket(
    config: &Config,
    report: &CheckReport,
    deps: &CheckDeps,
    probe: &dyn Probe,
) -> Requirement {
    let socket = socket_path(&config.libvirt_uri);
    let requirement = Requirement::new("libvirt-socket", Group::Libvirt, "libvirt socket", true);
    let Some(result) = report
        .results
        .iter()
        .find(|result| result.name == "libvirtd socket")
    else {
        return requirement.skipped("Not checked.");
    };
    if result.ok {
        return requirement.met(&format!("{} answers.", socket.display()));
    }
    let detail = format!("{}: {}", socket.display(), sentence(&result.detail));
    let virtqemud = daemon_installed("virtqemud", deps, probe);
    let libvirtd = daemon_installed("libvirtd", deps, probe);
    let virtqemud_answers = (deps.ping_libvirt)(Path::new(VIRTQEMUD_SOCKET)).is_ok();
    let default_socket = Path::new(bento_hypervisor::DEFAULT_SOCKET_PATH);
    let set_uri = format!(
        "This host has the modular libvirt daemons. Set libvirt_uri = \"qemu:///system?socket={VIRTQEMUD_SOCKET}\" in the configuration."
    );
    let modular_sockets: &[&str] = &[
        "systemctl",
        "enable",
        "--now",
        "virtqemud.socket",
        "virtnetworkd.socket",
    ];
    // Current Fedora runs the modular daemons, which have no
    // libvirt-sock. Only libvirtd supplies the default socket, so no
    // systemctl command makes it appear; the URI is the fix (DEPLOYING.md
    // section 1, "Modular libvirt daemons").
    let fix = if virtqemud && socket == default_socket && !probe.exists(default_socket) {
        if virtqemud_answers {
            manual(&set_uri)
        } else {
            root_fix(
                &format!("{set_uri} Then start the modular libvirt daemons."),
                &[modular_sockets],
            )
        }
    } else if !virtqemud && libvirtd {
        root_fix(
            "Start the libvirtd socket.",
            &[&["systemctl", "enable", "--now", "libvirtd.socket"]],
        )
    } else if socket != Path::new(VIRTQEMUD_SOCKET) && virtqemud_answers {
        manual(&set_uri)
    } else {
        root_fix("Start the modular libvirt daemons.", &[modular_sockets])
    };
    requirement.fail(&detail, Some(fix))
}

/// The names of interfaces whose forwarded traffic the Bento table would
/// drop. Bento's own bridges are `bento*` and never match.
pub fn foreign_bridges(interfaces: &[String]) -> Vec<String> {
    let mut found: Vec<String> = interfaces
        .iter()
        .filter(|name| !name.starts_with("bento"))
        .filter(|name| {
            FOREIGN_BRIDGES
                .iter()
                .any(|prefix| name.starts_with(prefix))
        })
        .cloned()
        .collect();
    found.sort();
    found
}

/// The Bento `forward` chain has policy drop, so it stops the forwarded
/// traffic of every other network on the host (DEPLOYING.md section 2,
/// "The host firewall"). This reads interface names only; it makes no
/// libvirt call.
fn dedicated_host(probe: &dyn Probe) -> Requirement {
    let requirement = Requirement::new("dedicated-host", Group::Network, "Dedicated host", false);
    let interfaces = match probe.list_dir(Path::new(SYS_CLASS_NET)) {
        Ok(interfaces) => interfaces,
        Err(error) => return requirement.skipped(&format!("read {SYS_CLASS_NET}: {error}.")),
    };
    let found = foreign_bridges(&interfaces);
    if found.is_empty() {
        return requirement.met("No other bridge or tunnel forwards through this host.");
    }
    requirement.fail(
        &format!(
            "Other networks forward through this host: {}. \
             The forward chain of the Bento nftables table has policy drop. \
             It stops the forwarded traffic of these networks \
             (DEPLOYING.md section 2, \"The host firewall\").",
            found.join(", ")
        ),
        Some(manual(
            "Use a dedicated host for Bento. Or remove these networks.",
        )),
    )
}

/// Loads a test table in check mode. Check mode needs the same netlink
/// access as the real load, so a refusal here is the refusal `serve`
/// would get before it opens the control-plane port (DEPLOYING.md
/// section 1).
///
/// An observer that is not root gets EPERM from netlink. That says
/// nothing about the root units, so the observer skips the check. Any
/// other refusal is still `Unmet`. Inside a user namespace, root gets the
/// same EPERM, because the namespace does not own the host network
/// namespace (DEPLOYING.md section 9). There the observer does not skip.
fn nft_permitted(deps: &CheckDeps, privilege: Privilege, probe: &dyn Probe) -> Requirement {
    let requirement = Requirement::new(
        "nft-permitted",
        Group::Network,
        "nftables changes permitted",
        true,
    );
    if (deps.look_path)("nft").is_err() {
        return requirement.skipped("nft is not on PATH.");
    }
    match probe.nft_check() {
        NftOutcome::Permitted => requirement.met("nft -c accepts a test table."),
        NftOutcome::Missing => requirement.skipped("nft is not on PATH."),
        NftOutcome::Refused(line)
            if privilege == Privilege::Observer
                && !in_user_namespace(probe)
                && EPERM_WORDS.iter().any(|word| line.contains(word)) =>
        {
            requirement.skipped(&format!(
                "nft -c gives \"Operation not permitted\" to this user. The units run as root. {RUN_AS_ROOT}"
            ))
        }
        NftOutcome::Refused(line) => requirement.fail(
            &format!(
                "nft -c refuses a test table: {}. Bento cannot load its nftables table.",
                line.trim_end_matches('.')
            ),
            Some(manual(
                "Run bentod as root in the host network namespace (DEPLOYING.md section 1, \"Root access\").",
            )),
        ),
    }
}

fn ip_forward(probe: &dyn Probe) -> Requirement {
    let requirement = Requirement::new("ip-forward", Group::Network, "IPv4 forwarding", false);
    let value = match probe.read_file(Path::new(IP_FORWARD)) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).trim().to_string(),
        Err(error) => return requirement.skipped(&format!("read {IP_FORWARD}: {error}.")),
    };
    if value == "1" {
        return requirement.met("net.ipv4.ip_forward is 1.");
    }
    requirement.fail(
        &format!("net.ipv4.ip_forward is {value}. Guests get no egress until it is 1."),
        Some(root_fix(
            "Turn on IPv4 forwarding.",
            &[&["sysctl", "-w", "net.ipv4.ip_forward=1"]],
        )),
    )
}

/// The proxy binds every port of its range, so a control plane inside it
/// takes a port from the proxy (SPEC 9.1).
fn control_plane_outside_range(config: &Config) -> Requirement {
    let requirement = Requirement::new(
        "control-plane-outside-range",
        Group::Ports,
        "Control plane outside proxy range",
        true,
    );
    let (min, max) = (config.listen.proxy_port_min, config.listen.proxy_port_max);
    match resolve_listen_addr(&config.listen.http) {
        Ok(address) if (min..=max).contains(&address.port()) => requirement.fail(
            &format!(
                "listen.http port {} is inside the proxy range {min}-{max}.",
                address.port()
            ),
            Some(manual(
                "Move listen.http out of the proxy range, for example to 127.0.0.1:10080.",
            )),
        ),
        Ok(address) => requirement.met(&format!("Port {} is outside {min}-{max}.", address.port())),
        Err(error) => requirement.fail(&format!("listen.http: {error}."), None),
    }
}

/// The soft `Max open files` limit from `/proc/self/limits`.
pub fn soft_nofile(limits: &str) -> Option<u64> {
    let line = limits
        .lines()
        .find(|line| line.starts_with("Max open files"))?;
    let soft = line
        .trim_start_matches("Max open files")
        .split_whitespace()
        .next()?;
    if soft == "unlimited" {
        return Some(u64::MAX);
    }
    soft.parse().ok()
}

/// The proxy holds one descriptor for each port of its range
/// (DEPLOYING.md section 6).
fn open_files_limit(config: &Config, limits: Option<&str>) -> Requirement {
    let requirement = Requirement::new("open-files-limit", Group::Ports, "Open files limit", false);
    let range = u64::from(config.listen.proxy_port_max - config.listen.proxy_port_min) + 1;
    let needed = range + SPARE_FILES;
    let Some(soft) = limits.and_then(soft_nofile) else {
        return requirement.skipped(&format!("{PROC_LIMITS} has no Max open files line."));
    };
    if soft >= needed {
        return requirement.met(&format!(
            "The soft limit {soft} covers {range} proxy ports."
        ));
    }
    requirement.fail(
        &format!(
            "The soft RLIMIT_NOFILE is {soft}. The proxy needs {needed}: one for each of {range} ports, plus {SPARE_FILES}. \
             The unit file sets LimitNOFILE=65536 (DEPLOYING.md section 6). \
             This limit applies when you start bentod by hand."
        ),
        Some(manual(
            "Before you start bentod by hand, run ulimit -n 65536.",
        )),
    )
}

fn base_domain(config: &Config) -> Requirement {
    let requirement = Requirement::new("base-domain", Group::Config, "Base domain", true);
    let domain = &config.base_domain;
    if domain.is_empty() {
        return requirement.fail(
            "base_domain is empty.",
            Some(manual(
                "Set base_domain to the domain of the control plane (SPEC 1).",
            )),
        );
    }
    if domain.ends_with(".example.org") {
        let mut requirement = requirement;
        requirement.fatal = false;
        return requirement.fail(
            &format!("{domain} is a documentation domain. Nobody can get a certificate or DNS records for it."),
            Some(manual("Set base_domain to a domain that you control (DEPLOYING.md section 4).")),
        );
    }
    requirement.met(domain)
}

/// The proxy serves plain HTTP; a TLS terminator in front of Bento owns
/// the certificate (SPEC 8). A public bind and keys left from the time the
/// proxy terminated TLS only warn: the proxy starts with either.
fn front_proxy(config: &Config) -> Requirement {
    let mut requirement =
        Requirement::new("front-proxy", Group::Config, "Behind a TLS terminator", false);
    let public = resolve_listen_addr(&config.listen.https)
        .map(|address| !address.ip().is_loopback())
        .unwrap_or(false);
    let mut problems = Vec::new();
    let mut fixes = Vec::new();
    if public {
        problems.push(format!(
            "The proxy serves plain HTTP on {}, which is not a loopback address. \
             The internet must not get to these listeners directly.",
            config.listen.https
        ));
        fixes.push(
            "Bind listen.https to 127.0.0.1 behind the TLS terminator (DEPLOYING.md section 5)."
                .to_string(),
        );
    }
    if !config.retired_keys.is_empty() {
        let keys = config.retired_keys.join(" and ");
        problems.push(format!(
            "{keys} no longer have an effect: Bento does not terminate TLS (SPEC 8)."
        ));
        fixes.push(format!("Remove {keys} from the configuration."));
    }
    if problems.is_empty() {
        return requirement.met(&format!(
            "Plain HTTP on {}. The TLS terminator owns the certificate (SPEC 8).",
            config.listen.https
        ));
    }
    requirement.fatal = false;
    requirement.fail(&problems.join(" "), Some(manual(&fixes.join(" "))))
}

fn oidc_settings(config: &Config) -> Requirement {
    // `serve` starts without OIDC and only warns (bentod/src/serve.rs),
    // so this is not fatal.
    let requirement = Requirement::new("oidc-settings", Group::Config, "OIDC settings", false);
    let missing: Vec<&str> = [
        ("oidc.issuer", &config.oidc.issuer),
        ("oidc.client_id", &config.oidc.client_id),
        ("oidc.client_secret", &config.oidc.client_secret),
    ]
    .into_iter()
    .filter(|(_, value)| value.is_empty())
    .map(|(name, _)| name)
    .collect();
    if missing.is_empty() {
        return requirement.met(&format!("Issuer {}.", config.oidc.issuer));
    }
    requirement.fail(
        &format!(
            "{} is empty. Bento starts, but nobody can sign in to the dashboard (SPEC 13).",
            missing.join(", ")
        ),
        Some(manual(&format!(
            "Set {} (DEPLOYING.md section 7, \"OIDC\").",
            missing.join(", ")
        ))),
    )
}

/// The discovery document URL of an issuer.
fn discovery_url(issuer: &str) -> String {
    format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    )
}

fn oidc_discovery(config: &Config, online: bool, probe: &dyn Probe) -> Requirement {
    let requirement = Requirement::new("oidc-discovery", Group::Config, "OIDC discovery", false);
    if !online {
        return requirement.skipped("Run the check with -online to fetch the discovery document.");
    }
    if config.oidc.issuer.is_empty() {
        return requirement.skipped("oidc.issuer is empty.");
    }
    let url = discovery_url(&config.oidc.issuer);
    let fix = Some(manual(
        "Make sure that oidc.issuer is the issuer URL of the provider and that this host can get to it.",
    ));
    match probe.http_get(&url, DISCOVERY_TIMEOUT) {
        Ok((200, body)) => {
            let issuer = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|document| document.get("issuer")?.as_str().map(str::to_string));
            match issuer {
                Some(issuer)
                    if issuer.trim_end_matches('/') == config.oidc.issuer.trim_end_matches('/') =>
                {
                    requirement.met(&format!("{url} answers."))
                }
                Some(issuer) => requirement.fail(
                    &format!(
                        "{url} names the issuer {issuer}, not {}. Token verification fails.",
                        config.oidc.issuer
                    ),
                    fix,
                ),
                None => requirement.fail(&format!("{url} is not a discovery document."), fix),
            }
        }
        Ok((status, _)) => requirement.fail(&format!("GET {url} gave status {status}."), fix),
        Err(error) => requirement.fail(&format!("GET {url}: {error}."), fix),
    }
}

/// Both the base domain and a name under it must resolve (DEPLOYING.md
/// section 3). Any label under the wildcard does for the second.
fn dns(config: &Config, online: bool, probe: &dyn Probe) -> Requirement {
    let requirement = Requirement::new("dns", Group::Config, "DNS records", false);
    if !online {
        return requirement.skipped("Run the check with -online to resolve the base domain.");
    }
    let wildcard = format!("preflight-check.{}", config.base_domain);
    let failed: Vec<String> = [config.base_domain.as_str(), wildcard.as_str()]
        .into_iter()
        .filter_map(|host| {
            probe
                .resolve(host)
                .err()
                .map(|error| format!("{host}: {error}"))
        })
        .collect();
    if failed.is_empty() {
        return requirement.met(&format!("{} and {wildcard} resolve.", config.base_domain));
    }
    requirement.fail(
        &format!("{}.", failed.join("; ")),
        Some(manual(&format!(
            "Make an A record for {0} and for *.{0} (DEPLOYING.md section 3).",
            config.base_domain
        ))),
    )
}

fn image_allowlist(config: &Config) -> Requirement {
    // No process refuses an empty allowlist, so this is not fatal.
    let requirement = Requirement::new("image-allowlist", Group::Images, "Image allowlist", false);
    if config.images.is_empty() {
        return requirement.fail(
            "The configuration has no [[images]] entry. Bento starts, but nobody can make an instance (SPEC 5.1).",
            Some(manual("Add an [[images]] entry (DEPLOYING.md section 4).")),
        );
    }
    let names: Vec<&str> = config
        .images
        .iter()
        .map(|image| image.name.as_str())
        .collect();
    requirement.met(&names.join(", "))
}

/// The architecture words that a URL for the other architecture carries.
fn foreign_arch_words(arch: &str) -> &'static [&'static str] {
    match arch {
        "x86_64" => &["arm64", "aarch64"],
        "aarch64" => &["amd64", "x86_64"],
        _ => &[],
    }
}

/// The guest architecture is the host architecture (DEPLOYING.md
/// section 1, "Guest architecture"). One requirement names every image
/// that does not agree, so the id stays unique.
fn image_arch(config: &Config, arch: &str) -> Requirement {
    let requirement = Requirement::new("image-arch", Group::Images, "Image architecture", false);
    let words = foreign_arch_words(arch);
    let wrong: Vec<&str> = config
        .images
        .iter()
        .filter(|image| {
            let url = image.url.to_ascii_lowercase();
            words.iter().any(|word| url.contains(word))
        })
        .map(|image| image.name.as_str())
        .collect();
    if wrong.is_empty() {
        return requirement.met(&format!(
            "No image URL names an architecture other than {arch}."
        ));
    }
    requirement.fail(
        &format!(
            "The URL of {} names a different architecture. This host is {arch}, and guests use the host architecture.",
            wrong.join(", ")
        ),
        Some(manual(&format!("Use an image URL for {arch}."))),
    )
}

/// Only a file count. The versions that each allowlist entry needs are
/// rows of the database, and the preflight does not open it: opening it
/// can migrate it. The Images step of the monitor reads it read-only and
/// checks each entry.
fn images_fetched(config: &Config, config_path: Option<&Path>, probe: &dyn Probe) -> Requirement {
    let requirement = Requirement::new(
        "images-fetched",
        Group::Images,
        "Image files present",
        false,
    );
    let fetched = probe
        .list_dir(Path::new(&config.image_dir))
        .unwrap_or_default()
        .iter()
        .filter(|name| name.starts_with("sha256-") && name.ends_with(".qcow2"))
        .count();
    if fetched > 0 {
        let files = if fetched == 1 { "file" } else { "files" };
        return requirement.met(&format!(
            "{} holds {fetched} image {files}. This check only finds one or more image files. The Images step of the Install tab checks each allowlist entry.",
            config.image_dir
        ));
    }
    let mut argv: Vec<String> = vec!["bentod".to_string()];
    if let Some(path) = config_path {
        argv.push("-config".to_string());
        argv.push(path.display().to_string());
    }
    argv.push("fetch-images".to_string());
    requirement.fail(
        &format!(
            "{} holds no sha256-*.qcow2 file. Users cannot make an instance before fetch-images runs (DEPLOYING.md section 4).",
            config.image_dir
        ),
        Some(Fix {
            summary: "Download the allowlisted images (SPEC 5.1).".to_string(),
            commands: vec![argv],
            root: true,
        }),
    )
}
