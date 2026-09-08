//! Bringing a host from nothing to three running units, in the order of
//! DEPLOYING.md sections 4 and 6.
//!
//! Each step reports whether the host already has it, and answers with the
//! commands that would do it. The monitor never does the work itself, so
//! an operator can read the list, run it here, or copy it into a runbook.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::role::Role;
use crate::run::Cmd;
use crate::systemd::{self, UnitStatus};

pub const DEFAULT_BINARY: &str = "/usr/local/bin/bentod";
/// The monitor installs itself beside `bentod`. Without this step the
/// screen an operator drives the deployment from is the one binary the
/// deployment never updates, and it silently keeps reporting a host it
/// no longer describes.
pub const DEFAULT_MONITOR: &str = "/usr/local/bin/bento-monitor";
pub const DEFAULT_CONFIG: &str = "/etc/bento/bento.toml";
pub const UNIT_DIR: &str = "/etc/systemd/system";

/// Every path the monitor installs to or reads from. The state
/// directories come from the configuration when it loads, and from the
/// SPEC defaults when it does not, so a host with no configuration yet
/// still gets a correct directory step.
#[derive(Debug, Clone)]
pub struct Paths {
    pub binary: PathBuf,
    /// Where this screen's own binary is installed.
    pub monitor: PathBuf,
    pub config: PathBuf,
    pub unit_dir: PathBuf,
    pub image_dir: PathBuf,
    pub storage_dir: PathBuf,
    pub key_dir: PathBuf,
    /// The Bento source tree, when the monitor runs inside one. Without
    /// it there is nothing to build and no example configuration to copy.
    pub source: Option<PathBuf>,
}

impl Paths {
    /// The `-config` value the unit files carry. A configuration at the
    /// path `bentod` already defaults to needs no flag.
    pub fn config_flag(&self) -> Option<&str> {
        if self.config == Path::new(DEFAULT_CONFIG) {
            None
        } else {
            self.config.to_str()
        }
    }

    pub fn built_binary(&self) -> Option<PathBuf> {
        self.built("bentod")
    }

    pub fn built_monitor(&self) -> Option<PathBuf> {
        self.built("bento-monitor")
    }

    fn built(&self, name: &str) -> Option<PathBuf> {
        self.source
            .as_ref()
            .map(|source| source.join("target/release").join(name))
    }

    /// The two binaries a release build produces, as (built, installed)
    /// pairs. Both are installed by the same step, because a deployment
    /// whose halves came from different commits is one an operator cannot
    /// reason about.
    pub fn binaries(&self) -> Vec<(Option<PathBuf>, &Path)> {
        vec![
            (self.built_binary(), self.binary.as_path()),
            (self.built_monitor(), self.monitor.as_path()),
        ]
    }

    pub fn example_config(&self) -> Option<PathBuf> {
        self.source
            .as_ref()
            .map(|source| source.join("bento.example.toml"))
    }
}

/// Walks up from `start` for the Bento source tree. The example
/// configuration is the marker, because it is the file the configuration
/// step copies and the one a source tree always carries.
pub fn find_source(start: &Path) -> Option<PathBuf> {
    let mut directory = Some(start);
    while let Some(current) = directory {
        if current.join("bento.example.toml").is_file() && current.join("bentod").is_dir() {
            return Some(current.to_path_buf());
        }
        directory = current.parent();
    }
    None
}

/// The install steps, in the order they have to happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Binary,
    Directories,
    Config,
    Units,
    Enable,
}

/// One row of the install screen.
#[derive(Debug, Clone)]
pub struct Step {
    pub kind: StepKind,
    pub title: String,
    pub detail: String,
    pub done: bool,
    /// Why the step cannot run yet, when it cannot.
    pub blocked: Option<String>,
}

/// What the host already has. The screen reads the filesystem through
/// this one type, so that the step table can be tested without the host
/// it describes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostFacts {
    /// The binaries of [`Paths::binaries`] that are not installed yet, by
    /// file name.
    pub missing_binaries: Vec<String>,
    /// The installed binaries that are older than the copy in the source
    /// tree's `target/release`. This is the one fact that says a host is
    /// running code the operator has already replaced.
    pub stale_binaries: Vec<String>,
    pub config_installed: bool,
    /// The state directories that do not exist yet, in screen order.
    pub missing_directories: Vec<String>,
}

impl HostFacts {
    pub fn probe(paths: &Paths) -> Self {
        let mut missing = Vec::new();
        let mut stale = Vec::new();
        for (built, installed) in paths.binaries() {
            let name = file_name(installed);
            if !installed.is_file() {
                missing.push(name);
                continue;
            }
            if let Some(built) = built
                && is_newer(&built, installed)
            {
                stale.push(name);
            }
        }
        HostFacts {
            missing_binaries: missing,
            stale_binaries: stale,
            config_installed: paths.config.is_file(),
            missing_directories: directories(paths)
                .into_iter()
                .filter(|dir| !dir.is_dir())
                .map(|dir| dir.display().to_string())
                .collect(),
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Whether `built` was produced after `installed` was put in place.
///
/// Modification time is the comparison rather than a version string,
/// because every commit of one release carries the same version: an
/// operator who rebuilds and forgets to install would read two identical
/// version numbers and conclude the host was current.
fn is_newer(built: &Path, installed: &Path) -> bool {
    let time = |path: &Path| -> Option<SystemTime> { path.metadata().ok()?.modified().ok() };
    match (time(built), time(installed)) {
        (Some(built), Some(installed)) => built > installed,
        _ => false,
    }
}

/// The state directories, in the order the screen lists them. The
/// configuration directory is included: `bentod` reads its configuration
/// from it, and the example copy needs it to exist first.
fn directories(paths: &Paths) -> Vec<PathBuf> {
    let mut list = Vec::new();
    if let Some(parent) = paths.config.parent() {
        list.push(parent.to_path_buf());
    }
    list.push(paths.image_dir.clone());
    list.push(paths.storage_dir.clone());
    list.push(paths.key_dir.clone());
    list
}

/// Reports each step from the host facts. `units` is the last
/// `systemctl show` reading, which is what says whether a unit file is
/// installed and enabled. `role` says which units this machine is meant
/// to have, so a runner is not reported as a half-installed controller
/// (MULTI-NODE 19).
pub fn steps(
    paths: &Paths,
    facts: &HostFacts,
    units: &[UnitStatus],
    config_loaded: Option<bool>,
    role: Role,
) -> Vec<Step> {
    let wanted = role.units();
    let missing = &facts.missing_directories;
    let binaries_done = facts.missing_binaries.is_empty() && facts.stale_binaries.is_empty();
    let installed_units = units
        .iter()
        .filter(|unit| unit.wanted && unit.installed())
        .count();
    let enabled_units = units
        .iter()
        .filter(|unit| unit.wanted && unit.enabled())
        .count();

    vec![
        Step {
            kind: StepKind::Binary,
            title: "bentod and bento-monitor".to_string(),
            detail: binary_detail(paths, facts),
            done: binaries_done,
            // A step that is already done is never blocked: what blocks a
            // step is what it still needs.
            blocked: (!binaries_done && paths.source.is_none()).then(|| {
                "no source tree found above the working directory; pass --source".to_string()
            }),
        },
        Step {
            kind: StepKind::Directories,
            title: "state directories".to_string(),
            // Bento checks that the image and storage directories exist
            // but creates neither, so a missing one refuses startup
            // (DEPLOYING.md 4).
            detail: if missing.is_empty() {
                "configuration, image, storage, and key directories exist".to_string()
            } else {
                format!("missing: {}", missing.join(", "))
            },
            done: missing.is_empty(),
            blocked: None,
        },
        Step {
            kind: StepKind::Config,
            title: "configuration file".to_string(),
            detail: match (facts.config_installed, config_loaded) {
                (false, _) => format!("{} is missing", paths.config.display()),
                (true, Some(true)) => format!("{} loads", paths.config.display()),
                (true, Some(false)) => format!(
                    "{} does not load; see the Config tab",
                    paths.config.display()
                ),
                (true, None) => paths.config.display().to_string(),
            },
            done: facts.config_installed,
            blocked: (!facts.config_installed && paths.source.is_none())
                .then(|| "no bento.example.toml to copy; pass --source".to_string()),
        },
        Step {
            kind: StepKind::Units,
            title: "systemd unit files".to_string(),
            detail: if installed_units == wanted.len() {
                format!(
                    "{} for a {} in {}",
                    wanted.len(),
                    role.label(),
                    paths.unit_dir.display()
                )
            } else {
                format!("{installed_units} of {} installed", wanted.len())
            },
            done: installed_units == wanted.len(),
            blocked: (installed_units != wanted.len() && !facts.missing_binaries.is_empty())
                .then(|| "install the binaries first".to_string()),
        },
        Step {
            kind: StepKind::Enable,
            title: "enabled at boot".to_string(),
            detail: format!("{enabled_units} of {} enabled", wanted.len()),
            done: enabled_units == wanted.len(),
            blocked: (enabled_units != wanted.len() && installed_units != wanted.len())
                .then(|| "install the unit files first".to_string()),
        },
    ]
}

/// What the binary step says about itself. A stale copy is called out by
/// name: "done" for a file that merely exists is how a host ends up
/// running last week's build (DEPLOYING.md 4).
fn binary_detail(paths: &Paths, facts: &HostFacts) -> String {
    let mut parts = Vec::new();
    if !facts.missing_binaries.is_empty() {
        parts.push(format!("missing: {}", facts.missing_binaries.join(", ")));
    }
    if !facts.stale_binaries.is_empty() {
        parts.push(format!(
            "older than the built copy: {}",
            facts.stale_binaries.join(", ")
        ));
    }
    if parts.is_empty() {
        let directory = paths
            .binary
            .parent()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        return format!("bentod and bento-monitor in {directory}");
    }
    parts.join("; ")
}

/// The commands one step runs, in order. A step stops at the first
/// command that fails.
///
/// Writing a unit file needs privilege, so the text is rendered to a
/// temporary file first and `install` moves it into place. That keeps
/// every privileged act a command the operator saw.
pub fn commands(kind: StepKind, paths: &Paths, euid: u32, role: Role) -> Result<Vec<Cmd>, String> {
    match kind {
        StepKind::Binary => {
            let source = paths
                .source
                .as_ref()
                .ok_or("no source tree; pass --source")?;
            // The build runs as the operator. Only the install into
            // /usr/local/bin needs privilege, and a root-owned target
            // directory would leave the tree unbuildable afterwards.
            let mut commands = vec![
                Cmd::new("cargo", &["build", "--release"]).in_dir(source.display().to_string()),
            ];
            for (built, installed) in paths.binaries() {
                let built = built.ok_or("no source tree")?;
                commands.push(
                    Cmd::owned(
                        "install",
                        vec![
                            "-m".into(),
                            "0755".into(),
                            built.display().to_string(),
                            installed.display().to_string(),
                        ],
                    )
                    .privileged(euid),
                );
            }
            Ok(commands)
        }
        StepKind::Directories => {
            let mut args = vec!["-d".to_string(), "-m".to_string(), "0755".to_string()];
            args.extend(
                directories(paths)
                    .iter()
                    .map(|dir| dir.display().to_string()),
            );
            Ok(vec![Cmd::owned("install", args).privileged(euid)])
        }
        StepKind::Config => {
            let example = paths
                .example_config()
                .ok_or("no bento.example.toml; pass --source")?;
            Ok(vec![
                // 0600: the file carries the ACME and OIDC secrets
                // (DEPLOYING.md 4).
                Cmd::owned(
                    "install",
                    vec![
                        "-m".into(),
                        "0600".into(),
                        example.display().to_string(),
                        paths.config.display().to_string(),
                    ],
                )
                .privileged(euid),
            ])
        }
        StepKind::Units => {
            let mut commands = Vec::new();
            for unit in role.units() {
                let text = systemd::unit_file(
                    unit,
                    &paths.binary.display().to_string(),
                    paths.config_flag(),
                );
                let staged = stage(&format!("{}.staged", unit.name), &text)
                    .map_err(|error| format!("staging {}: {error}", unit.name))?;
                commands.push(
                    Cmd::owned(
                        "install",
                        vec![
                            "-m".into(),
                            "0644".into(),
                            staged.display().to_string(),
                            paths.unit_dir.join(unit.name).display().to_string(),
                        ],
                    )
                    .privileged(euid),
                );
            }
            commands.push(systemd::daemon_reload(euid));
            Ok(commands)
        }
        StepKind::Enable => {
            let mut args = vec!["enable".to_string()];
            args.extend(role.units().iter().map(|unit| unit.name.to_string()));
            Ok(vec![Cmd::owned("systemctl", args).privileged(euid)])
        }
    }
}

/// Writes one rendered file into a private directory under the temporary
/// directory, for `install` to copy from.
fn stage(name: &str, text: &str) -> io::Result<PathBuf> {
    let directory = std::env::temp_dir().join(format!("bento-monitor-{}", std::process::id()));
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(name);
    std::fs::write(&path, text)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::merge_units;
    use crate::systemd::{RUNNER, SERVE, UNITS};

    fn paths(source: Option<PathBuf>) -> Paths {
        Paths {
            binary: PathBuf::from(DEFAULT_BINARY),
            monitor: PathBuf::from(DEFAULT_MONITOR),
            config: PathBuf::from(DEFAULT_CONFIG),
            unit_dir: PathBuf::from(UNIT_DIR),
            image_dir: PathBuf::from("/var/lib/bento/images"),
            storage_dir: PathBuf::from("/var/lib/bento/storage"),
            key_dir: PathBuf::from("/var/lib/bento/keys"),
            source,
        }
    }

    fn ready() -> HostFacts {
        HostFacts {
            missing_binaries: Vec::new(),
            stale_binaries: Vec::new(),
            config_installed: true,
            missing_directories: Vec::new(),
        }
    }

    #[test]
    fn the_default_configuration_path_needs_no_flag() {
        assert_eq!(paths(None).config_flag(), None);
        let mut other = paths(None);
        other.config = PathBuf::from("/srv/bento.toml");
        assert_eq!(other.config_flag(), Some("/srv/bento.toml"));
    }

    #[test]
    fn the_source_tree_is_found_by_walking_up() {
        let root = std::env::current_dir().expect("cwd");
        let source = find_source(&root.join("crates/config/src"));
        // The test itself runs inside the source tree.
        assert_eq!(source, find_source(&root));
        assert!(source.is_some());
        assert_eq!(find_source(Path::new("/")), None);
    }

    #[test]
    fn the_directory_step_creates_the_configuration_directory_as_well() {
        let commands =
            commands(StepKind::Directories, &paths(None), 0, Role::Controller).expect("commands");
        let line = commands[0].display();
        assert!(line.starts_with("install -d -m 0755 "), "{line}");
        for expected in [
            "/etc/bento",
            "/var/lib/bento/images",
            "/var/lib/bento/storage",
            "/var/lib/bento/keys",
        ] {
            assert!(line.contains(expected), "{line} misses {expected}");
        }
    }

    #[test]
    fn the_binary_step_builds_unprivileged_and_installs_both_binaries() {
        let commands = commands(
            StepKind::Binary,
            &paths(Some(PathBuf::from("/srv/src"))),
            1000,
            Role::Controller,
        )
        .expect("commands");
        assert_eq!(commands[0].program, "cargo");
        assert_eq!(commands[0].dir.as_deref(), Some("/srv/src"));
        assert_eq!(commands.len(), 3);
        for (command, name) in commands[1..].iter().zip(["bentod", "bento-monitor"]) {
            let line = command.display();
            assert_eq!(command.program, "sudo");
            assert!(
                line.contains(&format!("/srv/src/target/release/{name}")),
                "{line}"
            );
            assert!(line.ends_with(&format!("/usr/local/bin/{name}")), "{line}");
        }
    }

    #[test]
    fn an_installed_binary_older_than_the_built_one_is_not_done() {
        let directory = tempfile::tempdir().expect("tempdir");
        let source = directory.path().join("src");
        let release = source.join("target/release");
        std::fs::create_dir_all(&release).expect("release");
        let installed = directory.path().join("bin");
        std::fs::create_dir_all(&installed).expect("bin");

        let mut paths = paths(Some(source));
        paths.binary = installed.join("bentod");
        paths.monitor = installed.join("bento-monitor");

        // Nothing installed: both binaries are missing and the step names
        // them.
        for name in ["bentod", "bento-monitor"] {
            std::fs::write(release.join(name), b"built").expect("write");
        }
        let facts = HostFacts::probe(&paths);
        assert_eq!(facts.missing_binaries, vec!["bentod", "bento-monitor"]);
        assert!(facts.stale_binaries.is_empty());

        // Installed after the build: current.
        for name in ["bentod", "bento-monitor"] {
            std::fs::write(installed.join(name), b"installed").expect("write");
            let now = std::fs::File::open(installed.join(name)).expect("open");
            now.set_modified(SystemTime::now() + std::time::Duration::from_secs(60))
                .expect("touch");
        }
        let facts = HostFacts::probe(&paths);
        assert!(facts.missing_binaries.is_empty(), "{facts:?}");
        assert!(facts.stale_binaries.is_empty(), "{facts:?}");

        // Rebuilt and not installed: stale, and the step says which one.
        let built = std::fs::File::options()
            .write(true)
            .open(release.join("bento-monitor"))
            .expect("open");
        built
            .set_modified(SystemTime::now() + std::time::Duration::from_secs(600))
            .expect("touch");
        let facts = HostFacts::probe(&paths);
        assert_eq!(facts.stale_binaries, vec!["bento-monitor"]);
        let steps = steps(&paths, &facts, &[], Some(true), Role::Controller);
        assert!(!steps[0].done);
        assert!(
            steps[0].detail.contains("older than the built copy"),
            "{:?}",
            steps[0]
        );
        assert!(steps[0].detail.contains("bento-monitor"), "{:?}", steps[0]);
        // A stale binary is not a blocked step: the source tree is right
        // there and the step is what fixes it.
        assert!(steps[0].blocked.is_none());
    }

    #[test]
    fn a_step_without_a_source_tree_says_so_rather_than_running() {
        assert!(commands(StepKind::Binary, &paths(None), 0, Role::Controller).is_err());
        assert!(commands(StepKind::Config, &paths(None), 0, Role::Controller).is_err());
        let steps = steps(
            &paths(None),
            &HostFacts {
                missing_binaries: vec!["bentod".to_string()],
                ..ready()
            },
            &[],
            None,
            Role::Controller,
        );
        assert!(steps[0].blocked.is_some());
    }

    #[test]
    fn the_unit_step_stages_every_unit_and_reloads() {
        let commands =
            commands(StepKind::Units, &paths(None), 0, Role::Controller).expect("commands");
        assert_eq!(commands.len(), UNITS.len() + 1);
        for (command, unit) in commands.iter().zip(UNITS) {
            let line = command.display();
            assert!(
                line.contains(&format!("/etc/systemd/system/{}", unit.name)),
                "{line}"
            );
            let staged = &command.args[command.args.len() - 2];
            let text = std::fs::read_to_string(staged).expect("staged unit");
            assert!(
                text.contains(&format!("ExecStart={DEFAULT_BINARY} {}", unit.subcommand)),
                "{text}"
            );
        }
        assert_eq!(commands[UNITS.len()].display(), "systemctl daemon-reload");
    }

    #[test]
    fn a_runner_machine_installs_and_enables_only_the_runner_unit() {
        let units = commands(StepKind::Units, &paths(None), 0, Role::Runner).expect("commands");
        // One unit file, then the reload.
        assert_eq!(units.len(), 2);
        assert!(units[0].display().contains(RUNNER), "{:?}", units[0]);

        let enable = commands(StepKind::Enable, &paths(None), 0, Role::Runner).expect("commands");
        assert_eq!(enable[0].display(), format!("systemctl enable {RUNNER}"));
    }

    fn installed_unit(name: &str, enabled: bool) -> UnitStatus {
        UnitStatus {
            name: name.to_string(),
            load_state: "loaded".to_string(),
            fragment_path: format!("{UNIT_DIR}/{name}"),
            file_state: if enabled { "enabled" } else { "disabled" }.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn a_host_with_everything_reports_every_step_done() {
        let parsed: Vec<UnitStatus> = UNITS
            .iter()
            .map(|unit| installed_unit(unit.name, true))
            .collect();
        let units = merge_units(&parsed, Role::Controller);
        let steps = steps(&paths(None), &ready(), &units, Some(true), Role::Controller);
        assert!(steps.iter().all(|step| step.done), "{steps:?}");
        assert!(steps.iter().all(|step| step.blocked.is_none()));
    }

    #[test]
    fn a_runner_with_its_one_unit_reports_every_step_done() {
        // The same machine read as a controller is three units short.
        // This is the whole reason the role exists (MULTI-NODE 19).
        let parsed = vec![installed_unit(RUNNER, true)];
        let runner = merge_units(&parsed, Role::Runner);
        let steps = steps(&paths(None), &ready(), &runner, Some(true), Role::Runner);
        assert!(steps.iter().all(|step| step.done), "{steps:?}");
        assert_eq!(steps[3].detail, "1 for a runner in /etc/systemd/system");

        let controller = merge_units(&parsed, Role::Controller);
        let as_controller = super::steps(
            &paths(None),
            &ready(),
            &controller,
            Some(true),
            Role::Controller,
        );
        assert!(!as_controller[3].done);
        assert_eq!(as_controller[3].detail, "1 of 4 installed");
    }

    #[test]
    fn a_bare_host_blocks_the_steps_that_depend_on_earlier_ones() {
        let facts = HostFacts {
            missing_binaries: vec!["bentod".to_string(), "bento-monitor".to_string()],
            stale_binaries: Vec::new(),
            config_installed: false,
            missing_directories: vec!["/var/lib/bento/storage".to_string()],
        };
        let steps = steps(
            &paths(Some(PathBuf::from("/srv/bento"))),
            &facts,
            &[],
            Some(false),
            Role::Controller,
        );
        assert!(steps.iter().all(|step| !step.done));
        assert_eq!(steps[1].detail, "missing: /var/lib/bento/storage");
        assert!(
            steps[3].blocked.is_some(),
            "no binary yet, so the units wait"
        );
        assert!(
            steps[4].blocked.is_some(),
            "no unit files yet, so enabling waits"
        );
    }

    #[test]
    fn a_half_installed_host_counts_what_it_has() {
        let units = merge_units(&[installed_unit(SERVE, false)], Role::Controller);
        let steps = steps(&paths(None), &ready(), &units, Some(true), Role::Controller);
        assert!(!steps[3].done);
        assert_eq!(steps[3].detail, "1 of 4 installed");
        assert_eq!(steps[4].detail, "0 of 4 enabled");
        assert!(steps[4].blocked.is_some());
    }

    #[test]
    fn a_configuration_that_does_not_load_is_still_installed() {
        let facts = HostFacts {
            config_installed: true,
            missing_binaries: vec!["bentod".to_string()],
            ..ready()
        };
        let steps = steps(&paths(None), &facts, &[], Some(false), Role::Controller);
        assert!(steps[2].done);
        assert!(
            steps[2].detail.contains("does not load"),
            "{}",
            steps[2].detail
        );
    }
}
