//! The three systemd units of a Bento host (DEPLOYING.md section 6):
//! their unit-file text, the `systemctl show` reader, and the commands
//! that act on them.

use std::time::Duration;

use crate::run::Cmd;

pub const SERVE: &str = "bentod-serve.service";
pub const PROXY: &str = "bentod-proxy.service";
pub const SSHD: &str = "bentod-sshd.service";

/// One unit of the deployment. The binary holds every process as a
/// subcommand (SPEC 4), so the units differ only in that subcommand, in
/// what they wait for, and in the descriptor limit the proxy needs.
#[derive(Debug, Clone, Copy)]
pub struct Unit {
    pub name: &'static str,
    pub subcommand: &'static str,
    pub description: &'static str,
    /// Extra `After=` entries, beyond the network and libvirt sockets.
    pub after: &'static [&'static str],
    /// Extra `[Service]` lines.
    pub service_lines: &'static [&'static str],
}

/// `bentod-serve` owns the database, so it is first in this order and the
/// other two order themselves after it.
pub const UNITS: [Unit; 3] = [
    Unit {
        name: SERVE,
        subcommand: "serve",
        description: "Bento control plane",
        after: &[],
        service_lines: &[],
    },
    Unit {
        name: PROXY,
        subcommand: "proxy",
        description: "Bento HTTP proxy",
        after: &[SERVE],
        // The proxy binds one listening descriptor per port of the
        // 3000-9999 range. systemd gives a service a soft RLIMIT_NOFILE of
        // 1024 whatever the hard limit is, and without this line the bind
        // stops partway with "Too many open files" (DEPLOYING.md 6).
        service_lines: &["LimitNOFILE=65536"],
    },
    Unit {
        name: SSHD,
        subcommand: "sshd",
        description: "Bento SSH frontend",
        after: &[SERVE],
        service_lines: &[],
    },
];

/// The properties the screen reads. `Id` comes first because it is what
/// pairs a block of output with the unit that it describes.
const PROPERTIES: &str = "Id,Description,LoadState,ActiveState,SubState,UnitFileState,\
MainPID,MemoryCurrent,TasksCurrent,NRestarts,ActiveEnterTimestampMonotonic,FragmentPath,Result";

/// Writes the unit file of DEPLOYING.md section 6. A configuration path
/// other than the built-in default becomes an explicit `-config` flag, so
/// that a monitor started with `--config` installs units that agree with
/// it.
pub fn unit_file(unit: &Unit, binary: &str, config: Option<&str>) -> String {
    let mut after = vec![
        "network-online.target".to_string(),
        "virtqemud.socket".to_string(),
        "virtnetworkd.socket".to_string(),
    ];
    after.extend(unit.after.iter().map(|name| (*name).to_string()));
    let exec = match config {
        Some(path) => format!("{binary} -config {path} {}", unit.subcommand),
        None => format!("{binary} {}", unit.subcommand),
    };
    let mut text = format!(
        "[Unit]\n\
         Description={}\n\
         After={}\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={exec}\n\
         Restart=on-failure\n\
         RestartSec=5s\n",
        unit.description,
        after.join(" "),
    );
    for line in unit.service_lines {
        text.push_str(line);
        text.push('\n');
    }
    text.push_str("\n[Install]\nWantedBy=multi-user.target\n");
    text
}

/// What `systemctl show` reports about one unit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitStatus {
    pub name: String,
    pub description: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub file_state: String,
    pub main_pid: u32,
    pub memory: Option<u64>,
    pub tasks: Option<u64>,
    pub restarts: u32,
    pub fragment_path: String,
    pub result: String,
    /// Microseconds since boot at which the unit went active.
    pub active_since_monotonic: Option<u64>,
}

impl UnitStatus {
    /// A unit systemd has no file for. `systemctl show` answers for such a
    /// unit as well, which is why the install screen can report it.
    pub fn installed(&self) -> bool {
        self.load_state != "not-found" && !self.fragment_path.is_empty()
    }

    pub fn running(&self) -> bool {
        self.active_state == "active"
    }

    pub fn failed(&self) -> bool {
        self.active_state == "failed" || (self.result != "success" && !self.result.is_empty())
    }

    pub fn enabled(&self) -> bool {
        self.file_state == "enabled" || self.file_state == "enabled-runtime"
    }

    /// How long the unit has been active, from the host uptime and the
    /// monotonic stamp. The monotonic stamp is used rather than the
    /// wall-clock one because it needs no date parsing and no time zone.
    pub fn uptime(&self, host_uptime: Duration) -> Option<Duration> {
        let since = self.active_since_monotonic.filter(|value| *value > 0)?;
        let since = Duration::from_micros(since);
        host_uptime.checked_sub(since)
    }
}

/// Reads the output of [`show_command`]. `systemctl` separates the blocks
/// of several units with an empty line and keeps the argument order, but
/// the blocks are paired by their `Id` value rather than by position, so
/// that a rename or an alias cannot shift the whole screen.
pub fn parse_show(output: &str) -> Vec<UnitStatus> {
    let mut units = Vec::new();
    for block in output.split("\n\n") {
        let mut status = UnitStatus::default();
        let mut seen = false;
        for line in block.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            seen = true;
            let value = value.trim();
            match key {
                "Id" => status.name = value.to_string(),
                "Description" => status.description = value.to_string(),
                "LoadState" => status.load_state = value.to_string(),
                "ActiveState" => status.active_state = value.to_string(),
                "SubState" => status.sub_state = value.to_string(),
                "UnitFileState" => status.file_state = value.to_string(),
                "MainPID" => status.main_pid = value.parse().unwrap_or_default(),
                // An unset limit or counter reads as the u64 maximum, which
                // would print as 16 exabytes of memory.
                "MemoryCurrent" => status.memory = value.parse().ok().filter(|v| *v != u64::MAX),
                "TasksCurrent" => status.tasks = value.parse().ok().filter(|v| *v != u64::MAX),
                "NRestarts" => status.restarts = value.parse().unwrap_or_default(),
                "FragmentPath" => status.fragment_path = value.to_string(),
                "Result" => status.result = value.to_string(),
                "ActiveEnterTimestampMonotonic" => {
                    status.active_since_monotonic = value.parse().ok();
                }
                _ => {}
            }
        }
        if seen && !status.name.is_empty() {
            units.push(status);
        }
    }
    units
}

/// Asks for every unit in one call.
pub fn show_command() -> Cmd {
    let mut args: Vec<String> = vec!["show".to_string(), format!("--property={PROPERTIES}")];
    args.extend(UNITS.iter().map(|unit| unit.name.to_string()));
    Cmd::owned("systemctl", args)
}

/// What an operator can ask of one unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitAction {
    Start,
    Stop,
    Restart,
    Enable,
    Disable,
    Logs,
    FollowLogs,
}

impl UnitAction {
    pub fn label(self) -> &'static str {
        match self {
            UnitAction::Start => "start",
            UnitAction::Stop => "stop",
            UnitAction::Restart => "restart",
            UnitAction::Enable => "enable at boot",
            UnitAction::Disable => "disable at boot",
            UnitAction::Logs => "last 200 log lines",
            UnitAction::FollowLogs => "follow the log",
        }
    }

    /// The command the action runs. Reading a log needs no privilege of
    /// its own on a host where the operator is in `systemd-journal`, but
    /// it is wrapped like the rest so that a host without that group
    /// still answers.
    pub fn command(self, unit: &str, euid: u32) -> Cmd {
        match self {
            UnitAction::Start => Cmd::new("systemctl", &["start", unit]).privileged(euid),
            UnitAction::Stop => Cmd::new("systemctl", &["stop", unit]).privileged(euid),
            UnitAction::Restart => Cmd::new("systemctl", &["restart", unit]).privileged(euid),
            UnitAction::Enable => Cmd::new("systemctl", &["enable", unit]).privileged(euid),
            UnitAction::Disable => Cmd::new("systemctl", &["disable", unit]).privileged(euid),
            UnitAction::Logs => {
                Cmd::new("journalctl", &["-u", unit, "-n", "200", "--no-pager"]).privileged(euid)
            }
            UnitAction::FollowLogs => Cmd::new("journalctl", &["-u", unit, "-f"]).privileged(euid),
        }
    }
}

/// Rereads the unit files after one is written or removed.
pub fn daemon_reload(euid: u32) -> Cmd {
    Cmd::new("systemctl", &["daemon-reload"]).privileged(euid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_serve_unit_matches_the_runbook() {
        let text = unit_file(&UNITS[0], "/usr/local/bin/bentod", None);
        assert_eq!(
            text,
            "[Unit]\n\
             Description=Bento control plane\n\
             After=network-online.target virtqemud.socket virtnetworkd.socket\n\
             Wants=network-online.target\n\
             \n\
             [Service]\n\
             ExecStart=/usr/local/bin/bentod serve\n\
             Restart=on-failure\n\
             RestartSec=5s\n\
             \n\
             [Install]\n\
             WantedBy=multi-user.target\n"
        );
    }

    #[test]
    fn the_proxy_unit_carries_the_descriptor_limit_and_waits_for_serve() {
        let text = unit_file(&UNITS[1], "/usr/local/bin/bentod", None);
        assert!(text.contains("LimitNOFILE=65536"), "{text}");
        assert!(
            text.contains(&format!(
                "After=network-online.target virtqemud.socket virtnetworkd.socket {SERVE}"
            )),
            "{text}"
        );
    }

    #[test]
    fn a_configuration_path_reaches_the_exec_line() {
        let text = unit_file(&UNITS[2], "/usr/local/bin/bentod", Some("/srv/bento.toml"));
        assert!(
            text.contains("ExecStart=/usr/local/bin/bentod -config /srv/bento.toml sshd"),
            "{text}"
        );
    }

    #[test]
    fn show_output_of_two_units_reads_as_two_units() {
        let output = "Id=bentod-serve.service\n\
             Description=Bento control plane\n\
             LoadState=loaded\n\
             ActiveState=active\n\
             SubState=running\n\
             UnitFileState=enabled\n\
             MainPID=1234\n\
             MemoryCurrent=52428800\n\
             TasksCurrent=18\n\
             NRestarts=0\n\
             ActiveEnterTimestampMonotonic=60000000\n\
             FragmentPath=/etc/systemd/system/bentod-serve.service\n\
             Result=success\n\
             \n\
             Id=bentod-proxy.service\n\
             LoadState=not-found\n\
             ActiveState=inactive\n\
             SubState=dead\n\
             UnitFileState=\n\
             MainPID=0\n\
             MemoryCurrent=18446744073709551615\n\
             FragmentPath=\n\
             Result=success\n";
        let units = parse_show(output);
        assert_eq!(units.len(), 2);

        let serve = &units[0];
        assert_eq!(serve.name, SERVE);
        assert!(serve.installed() && serve.running() && serve.enabled() && !serve.failed());
        assert_eq!(serve.memory, Some(52_428_800));
        assert_eq!(
            serve.uptime(Duration::from_secs(300)),
            Some(Duration::from_secs(240))
        );

        let proxy = &units[1];
        assert!(!proxy.installed() && !proxy.running() && !proxy.enabled());
        // An unset MemoryCurrent must not print as 16 exabytes.
        assert_eq!(proxy.memory, None);
        assert_eq!(proxy.uptime(Duration::from_secs(300)), None);
    }

    #[test]
    fn a_failed_unit_reports_its_failure() {
        let units = parse_show(
            "Id=bentod-proxy.service\nActiveState=failed\nResult=exit-code\nLoadState=loaded\nFragmentPath=/etc/systemd/system/bentod-proxy.service\n",
        );
        assert!(units[0].failed());
    }

    #[test]
    fn the_show_call_names_every_unit() {
        let cmd = show_command();
        for unit in UNITS {
            assert!(cmd.args.contains(&unit.name.to_string()));
        }
    }

    #[test]
    fn unit_actions_run_systemctl_and_journalctl() {
        assert_eq!(
            UnitAction::Restart.command(SERVE, 0).display(),
            "systemctl restart bentod-serve.service"
        );
        assert_eq!(
            UnitAction::Logs.command(PROXY, 0).display(),
            "journalctl -u bentod-proxy.service -n 200 --no-pager"
        );
    }
}
