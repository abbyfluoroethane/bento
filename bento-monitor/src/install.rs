//! Bringing a host from nothing to three running units, in the order of
//! DEPLOYING.md sections 4 and 6.
//!
//! Each step reports whether the host already has it, and answers with the
//! commands that would do it. The monitor never does the work itself, so
//! an operator can read the list, run it here, or copy it into a runbook.

use std::io;
use std::path::{Path, PathBuf};

use crate::run::Cmd;
use crate::systemd::{self, UNITS, UnitStatus};

pub const DEFAULT_BINARY: &str = "/usr/local/bin/bentod";
pub const DEFAULT_CONFIG: &str = "/etc/bento/bento.toml";
pub const UNIT_DIR: &str = "/etc/systemd/system";

/// Every path the monitor installs to or reads from. The state
/// directories come from the configuration when it loads, and from the
/// SPEC defaults when it does not, so a host with no configuration yet
/// still gets a correct directory step.
#[derive(Debug, Clone)]
pub struct Paths {
    pub binary: PathBuf,
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
        self.source
            .as_ref()
            .map(|source| source.join("target/release/bentod"))
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
    pub binary_installed: bool,
    pub config_installed: bool,
    /// The state directories that do not exist yet, in screen order.
    pub missing_directories: Vec<String>,
}

impl HostFacts {
    pub fn probe(paths: &Paths) -> Self {
        HostFacts {
            binary_installed: paths.binary.is_file(),
            config_installed: paths.config.is_file(),
            missing_directories: directories(paths)
                .into_iter()
                .filter(|dir| !dir.is_dir())
                .map(|dir| dir.display().to_string())
                .collect(),
        }
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
/// installed and enabled.
pub fn steps(
    paths: &Paths,
    facts: &HostFacts,
    units: &[UnitStatus],
    config_loaded: Option<bool>,
) -> Vec<Step> {
    let binary_done = facts.binary_installed;
    let missing = &facts.missing_directories;
    let installed_units: Vec<&UnitStatus> = units.iter().filter(|unit| unit.installed()).collect();
    let enabled_units = units.iter().filter(|unit| unit.enabled()).count();

    vec![
        Step {
            kind: StepKind::Binary,
            title: "bentod binary".to_string(),
            detail: if binary_done {
                paths.binary.display().to_string()
            } else {
                format!("{} is missing", paths.binary.display())
            },
            done: binary_done,
            // A step that is already done is never blocked: what blocks a
            // step is what it still needs.
            blocked: (!binary_done && paths.source.is_none()).then(|| {
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
            detail: if installed_units.len() == UNITS.len() {
                format!("{} in {}", UNITS.len(), paths.unit_dir.display())
            } else {
                format!("{} of {} installed", installed_units.len(), UNITS.len())
            },
            done: installed_units.len() == UNITS.len(),
            blocked: (installed_units.len() != UNITS.len() && !binary_done)
                .then(|| "install the binary first".to_string()),
        },
        Step {
            kind: StepKind::Enable,
            title: "enabled at boot".to_string(),
            detail: format!("{enabled_units} of {} enabled", UNITS.len()),
            done: enabled_units == UNITS.len(),
            blocked: (enabled_units != UNITS.len() && installed_units.len() != UNITS.len())
                .then(|| "install the unit files first".to_string()),
        },
    ]
}

/// The commands one step runs, in order. A step stops at the first
/// command that fails.
///
/// Writing a unit file needs privilege, so the text is rendered to a
/// temporary file first and `install` moves it into place. That keeps
/// every privileged act a command the operator saw.
pub fn commands(kind: StepKind, paths: &Paths, euid: u32) -> Result<Vec<Cmd>, String> {
    match kind {
        StepKind::Binary => {
            let source = paths
                .source
                .as_ref()
                .ok_or("no source tree; pass --source")?;
            let built = paths.built_binary().ok_or("no source tree")?;
            Ok(vec![
                // The build runs as the operator. Only the install into
                // /usr/local/bin needs privilege, and a root-owned target
                // directory would leave the tree unbuildable afterwards.
                Cmd::new("cargo", &["build", "--release"]).in_dir(source.display().to_string()),
                Cmd::owned(
                    "install",
                    vec![
                        "-m".into(),
                        "0755".into(),
                        built.display().to_string(),
                        paths.binary.display().to_string(),
                    ],
                )
                .privileged(euid),
            ])
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
            for unit in UNITS {
                let text = systemd::unit_file(
                    &unit,
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
            args.extend(UNITS.iter().map(|unit| unit.name.to_string()));
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

    fn paths(source: Option<PathBuf>) -> Paths {
        Paths {
            binary: PathBuf::from(DEFAULT_BINARY),
            config: PathBuf::from(DEFAULT_CONFIG),
            unit_dir: PathBuf::from(UNIT_DIR),
            image_dir: PathBuf::from("/var/lib/bento/images"),
            storage_dir: PathBuf::from("/var/lib/bento/storage"),
            key_dir: PathBuf::from("/var/lib/bento/keys"),
            source,
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
        let commands = commands(StepKind::Directories, &paths(None), 0).expect("commands");
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
    fn the_binary_step_builds_unprivileged_and_installs_privileged() {
        let commands = commands(
            StepKind::Binary,
            &paths(Some(PathBuf::from("/srv/src"))),
            1000,
        )
        .expect("commands");
        assert_eq!(commands[0].program, "cargo");
        assert_eq!(commands[0].dir.as_deref(), Some("/srv/src"));
        assert_eq!(commands[1].program, "sudo");
        assert!(
            commands[1]
                .display()
                .contains("/srv/src/target/release/bentod")
        );
    }

    #[test]
    fn a_step_without_a_source_tree_says_so_rather_than_running() {
        assert!(commands(StepKind::Binary, &paths(None), 0).is_err());
        assert!(commands(StepKind::Config, &paths(None), 0).is_err());
        let steps = steps(&paths(None), &HostFacts::default(), &[], None);
        assert!(steps[0].blocked.is_some());
    }

    #[test]
    fn the_unit_step_stages_every_unit_and_reloads() {
        let commands = commands(StepKind::Units, &paths(None), 0).expect("commands");
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
        let units: Vec<UnitStatus> = UNITS
            .iter()
            .map(|unit| installed_unit(unit.name, true))
            .collect();
        let facts = HostFacts {
            binary_installed: true,
            config_installed: true,
            missing_directories: Vec::new(),
        };
        let steps = steps(&paths(None), &facts, &units, Some(true));
        assert!(steps.iter().all(|step| step.done), "{steps:?}");
        assert!(steps.iter().all(|step| step.blocked.is_none()));
    }

    #[test]
    fn a_bare_host_blocks_the_steps_that_depend_on_earlier_ones() {
        let facts = HostFacts {
            binary_installed: false,
            config_installed: false,
            missing_directories: vec!["/var/lib/bento/storage".to_string()],
        };
        let steps = steps(
            &paths(Some(PathBuf::from("/srv/bento"))),
            &facts,
            &[],
            Some(false),
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
        let facts = HostFacts {
            binary_installed: true,
            config_installed: true,
            missing_directories: Vec::new(),
        };
        let units = vec![installed_unit(UNITS[0].name, false)];
        let steps = steps(&paths(None), &facts, &units, Some(true));
        assert!(!steps[3].done);
        assert_eq!(steps[3].detail, "1 of 3 installed");
        assert_eq!(steps[4].detail, "0 of 3 enabled");
        assert!(steps[4].blocked.is_some());
    }

    #[test]
    fn a_configuration_that_does_not_load_is_still_installed() {
        let facts = HostFacts {
            config_installed: true,
            ..Default::default()
        };
        let steps = steps(&paths(None), &facts, &[], Some(false));
        assert!(steps[2].done);
        assert!(
            steps[2].detail.contains("does not load"),
            "{}",
            steps[2].detail
        );
    }
}
