//! The state behind the screen, and what each key does to it.
//!
//! Nothing here writes to the host. A key that would change something
//! answers with the commands to run, and the caller runs them in the
//! operator's terminal (see [`Outcome::Run`]).

use std::path::PathBuf;
use std::time::Duration;

use bento_config::Config;
use bento_hypervisor::{CheckConfig, CheckResult};

use crate::host::HostSample;
use crate::install::{self, HostFacts, Paths, Step, StepKind};
use crate::libvirt::{self, Census};
use crate::run::Cmd;
use crate::systemd::{self, UNITS, UnitAction, UnitStatus};

pub const TABS: [&str; 4] = ["Services", "Install", "Config", "Host"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Services,
    Install,
    Config,
    Host,
}

impl Tab {
    pub fn index(self) -> usize {
        match self {
            Tab::Services => 0,
            Tab::Install => 1,
            Tab::Config => 2,
            Tab::Host => 3,
        }
    }

    fn from_index(index: usize) -> Tab {
        match index % TABS.len() {
            0 => Tab::Services,
            1 => Tab::Install,
            2 => Tab::Config,
            _ => Tab::Host,
        }
    }
}

/// What sits over the screen, if anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    /// The commands an action would run, waiting for a yes.
    Confirm {
        title: String,
        commands: Vec<Cmd>,
    },
    Help,
    /// Something the operator has to read, such as a refused action.
    Message {
        title: String,
        body: String,
    },
}

/// What the caller does after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    None,
    Quit,
    /// Leave the screen, run these in order, and come back.
    Run {
        title: String,
        commands: Vec<Cmd>,
    },
}

pub struct App {
    pub paths: Paths,
    pub euid: u32,
    pub tab: Tab,
    pub units: Vec<UnitStatus>,
    pub systemctl_error: Option<String>,
    pub unit_cursor: usize,
    pub steps: Vec<Step>,
    pub step_cursor: usize,
    pub config: Result<Config, String>,
    pub checks: Vec<CheckResult>,
    pub census: Result<Census, String>,
    pub host: HostSample,
    pub modal: Option<Modal>,
    /// The last thing that happened, shown along the bottom.
    pub status: String,
}

impl App {
    pub fn new(paths: Paths, euid: u32) -> Self {
        App {
            paths,
            euid,
            tab: Tab::Services,
            units: merge_units(&[]),
            systemctl_error: None,
            unit_cursor: 0,
            steps: Vec::new(),
            step_cursor: 0,
            config: Err("not read yet".to_string()),
            checks: Vec::new(),
            census: Err("not read yet".to_string()),
            host: HostSample::default(),
            modal: None,
            status: if euid == 0 {
                "running as root".to_string()
            } else {
                "not root: every change runs through sudo".to_string()
            },
        }
    }

    /// The unit the Services tab has selected.
    pub fn selected_unit(&self) -> &UnitStatus {
        &self.units[self.unit_cursor.min(self.units.len() - 1)]
    }

    pub fn selected_step(&self) -> Option<&Step> {
        self.steps.get(self.step_cursor)
    }

    /// Rereads everything the screen shows. The libvirt census is the only
    /// asynchronous part, so it borrows the caller's runtime.
    pub fn refresh(&mut self, runtime: &tokio::runtime::Runtime) {
        match systemd::show_command().capture() {
            Ok(output) => {
                self.units = merge_units(&systemd::parse_show(&output));
                self.systemctl_error = None;
            }
            Err(error) => self.systemctl_error = Some(error.to_string()),
        }

        self.config = Config::load(&self.paths.config).map_err(|error| error.to_string());
        // A configuration that loads moves the state directories, so the
        // paths follow it rather than staying at the defaults.
        if let Ok(config) = &self.config {
            self.paths.image_dir = PathBuf::from(&config.image_dir);
            self.paths.storage_dir = PathBuf::from(&config.storage_dir);
            self.paths.key_dir = PathBuf::from(&config.key_dir);
        }

        let facts = HostFacts::probe(&self.paths);
        self.steps = install::steps(&self.paths, &facts, &self.units, Some(self.config.is_ok()));
        self.step_cursor = self.step_cursor.min(self.steps.len().saturating_sub(1));
        self.host = HostSample::take(
            Some(&self.host),
            &self.paths.image_dir,
            &self.paths.storage_dir,
        );

        let socket = self
            .config
            .as_ref()
            .map(|config| libvirt::socket_path(&config.libvirt_uri))
            .unwrap_or_default();
        self.checks = self.host_checks(&socket);
        self.census = runtime.block_on(libvirt::census(&socket));
    }

    /// The SPEC 4.2 requirement checks, the same ones `bentod serve` runs
    /// before it starts.
    fn host_checks(&self, socket: &std::path::Path) -> Vec<CheckResult> {
        let Ok(config) = &self.config else {
            return Vec::new();
        };
        bento_hypervisor::check(
            CheckConfig {
                socket_path: socket.to_path_buf(),
                image_dir: PathBuf::from(&config.image_dir),
                storage_dir: PathBuf::from(&config.storage_dir),
                container_storage: PathBuf::from(&config.bootc.container_storage),
                podman_required: config.images.iter().any(|image| !image.oci.is_empty()),
                ..Default::default()
            },
            &bento_hypervisor::default_check_deps(),
        )
        .results
    }

    /// Applies one key. `char` keys are matched first, then the named
    /// keys, so that the caller only has to classify the key once.
    pub fn on_key(&mut self, key: Key) -> Outcome {
        if let Some(modal) = self.modal.clone() {
            return self.on_modal_key(key, modal);
        }
        match key {
            // Escape closes what is open. It does not quit: an operator
            // who presses it out of habit should not lose the screen.
            Key::Char('q') | Key::Quit => return Outcome::Quit,
            Key::Esc => {}
            Key::Char('?') | Key::Help => self.modal = Some(Modal::Help),
            Key::Right | Key::Tab => self.tab = Tab::from_index(self.tab.index() + 1),
            Key::Left | Key::BackTab => {
                self.tab = Tab::from_index(self.tab.index() + TABS.len() - 1);
            }
            Key::Char(digit @ '1'..='4') => {
                self.tab = Tab::from_index(digit as usize - '1' as usize);
            }
            Key::Up => self.move_cursor(-1),
            Key::Down => self.move_cursor(1),
            Key::Char('k') => self.move_cursor(-1),
            Key::Char('j') => self.move_cursor(1),
            // The caller rereads the host before it passes the key on,
            // so by now it is done.
            Key::Refresh => self.status = "host reread".to_string(),
            other => return self.on_tab_key(other),
        }
        Outcome::None
    }

    fn on_modal_key(&mut self, key: Key, modal: Modal) -> Outcome {
        match (key, modal) {
            (Key::Char('y') | Key::Enter, Modal::Confirm { title, commands }) => {
                self.modal = None;
                Outcome::Run { title, commands }
            }
            (Key::Char('q'), Modal::Help) => {
                // `q` closes the help rather than the program: an operator
                // who opened help by accident should not lose the screen.
                self.modal = None;
                Outcome::None
            }
            _ => {
                self.modal = None;
                Outcome::None
            }
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let (cursor, len) = match self.tab {
            Tab::Services => (&mut self.unit_cursor, self.units.len()),
            Tab::Install => (&mut self.step_cursor, self.steps.len()),
            _ => return,
        };
        if len == 0 {
            return;
        }
        let next = (*cursor as isize + delta).rem_euclid(len as isize);
        *cursor = next as usize;
    }

    fn on_tab_key(&mut self, key: Key) -> Outcome {
        match self.tab {
            Tab::Services => self.on_services_key(key),
            Tab::Install => self.on_install_key(key),
            Tab::Config => self.on_config_key(key),
            Tab::Host => Outcome::None,
        }
    }

    fn on_services_key(&mut self, key: Key) -> Outcome {
        let Key::Char(c) = key else {
            return Outcome::None;
        };
        let action = match c {
            's' => UnitAction::Start,
            't' => UnitAction::Stop,
            'r' => UnitAction::Restart,
            'e' => UnitAction::Enable,
            'd' => UnitAction::Disable,
            'l' => UnitAction::Logs,
            'f' => UnitAction::FollowLogs,
            'D' => {
                self.confirm(
                    "reload the unit files".to_string(),
                    vec![systemd::daemon_reload(self.euid)],
                );
                return Outcome::None;
            }
            _ => return Outcome::None,
        };
        let unit = self.selected_unit().name.clone();
        if !self.selected_unit().installed() && !matches!(action, UnitAction::Logs) {
            self.modal = Some(Modal::Message {
                title: "no unit file".to_string(),
                body: format!(
                    "{unit} is not installed. Run the unit-file step on the Install tab first."
                ),
            });
            return Outcome::None;
        }
        self.confirm(
            format!("{} {unit}", action.label()),
            vec![action.command(&unit, self.euid)],
        );
        Outcome::None
    }

    fn on_install_key(&mut self, key: Key) -> Outcome {
        match key {
            Key::Enter => {
                let Some(step) = self.selected_step().cloned() else {
                    return Outcome::None;
                };
                self.run_step(step.kind, &step.title, step.blocked.as_deref());
            }
            Key::Char('a') => self.run_remaining_steps(),
            _ => {}
        }
        Outcome::None
    }

    fn run_step(&mut self, kind: StepKind, title: &str, blocked: Option<&str>) {
        if let Some(reason) = blocked {
            self.modal = Some(Modal::Message {
                title: format!("cannot run: {title}"),
                body: reason.to_string(),
            });
            return;
        }
        match install::commands(kind, &self.paths, self.euid) {
            Ok(commands) => self.confirm(title.to_string(), commands),
            Err(error) => {
                self.modal = Some(Modal::Message {
                    title: format!("cannot run: {title}"),
                    body: error,
                });
            }
        }
    }

    /// Every step the host still needs, in order, as one list. A step that
    /// is blocked by an earlier step is included: the earlier step runs
    /// first and clears the block.
    fn run_remaining_steps(&mut self) {
        let pending: Vec<StepKind> = self
            .steps
            .iter()
            .filter(|step| !step.done)
            .map(|step| step.kind)
            .collect();
        if pending.is_empty() {
            self.modal = Some(Modal::Message {
                title: "nothing to install".to_string(),
                body: "Every step is done.".to_string(),
            });
            return;
        }
        let mut commands = Vec::new();
        for kind in pending {
            match install::commands(kind, &self.paths, self.euid) {
                Ok(mut list) => commands.append(&mut list),
                Err(error) => {
                    self.modal = Some(Modal::Message {
                        title: "cannot install everything".to_string(),
                        body: error,
                    });
                    return;
                }
            }
        }
        self.confirm("install everything that is missing".to_string(), commands);
    }

    fn on_config_key(&mut self, key: Key) -> Outcome {
        let Key::Char(c) = key else {
            return Outcome::None;
        };
        let config = self.paths.config.display().to_string();
        let binary = self.paths.binary.display().to_string();
        // The operator commands need the same configuration the units use.
        let bentod = |subcommand: &str| {
            let mut args = Vec::new();
            if let Some(path) = self.paths.config_flag() {
                args.push("-config".to_string());
                args.push(path.to_string());
            }
            args.push(subcommand.to_string());
            Cmd::owned(binary.clone(), args).privileged(self.euid)
        };
        match c {
            'e' => {
                let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
                self.confirm(
                    format!("edit {config}"),
                    vec![Cmd::owned(editor, vec![config]).privileged(self.euid)],
                );
            }
            'f' => self.confirm(
                "fetch the allowlisted images".to_string(),
                vec![bentod("fetch-images")],
            ),
            'c' => self.confirm(
                "report libvirt and database disagreements".to_string(),
                vec![bentod("reconcile")],
            ),
            'i' => self.confirm("list the stored images".to_string(), vec![bentod("images")]),
            _ => {}
        }
        Outcome::None
    }

    fn confirm(&mut self, title: String, commands: Vec<Cmd>) {
        self.modal = Some(Modal::Confirm { title, commands });
    }
}

/// One entry per unit of [`UNITS`], in that order, whatever `systemctl`
/// answered. A unit the host has never heard of still needs a row, because
/// that row is what says it has to be installed.
pub fn merge_units(parsed: &[UnitStatus]) -> Vec<UnitStatus> {
    UNITS
        .iter()
        .map(|unit| {
            parsed
                .iter()
                .find(|status| status.name == unit.name)
                .cloned()
                .unwrap_or_else(|| UnitStatus {
                    name: unit.name.to_string(),
                    description: unit.description.to_string(),
                    ..Default::default()
                })
        })
        .collect()
}

/// The keys the screen knows, apart from the terminal that sent them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Tab,
    BackTab,
    Enter,
    Esc,
    Help,
    Refresh,
    Quit,
    Other,
}

/// How long the screen waits before it rereads the host.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::{DEFAULT_BINARY, DEFAULT_CONFIG, UNIT_DIR};

    /// A host with the binary in place and nothing else. The facts are
    /// stated rather than probed, so that these tests do not depend on
    /// what the machine running them has installed.
    fn facts() -> HostFacts {
        HostFacts {
            binary_installed: true,
            config_installed: false,
            missing_directories: vec!["/var/lib/bento/storage".to_string()],
        }
    }

    fn app() -> App {
        let paths = Paths {
            binary: PathBuf::from(DEFAULT_BINARY),
            config: PathBuf::from(DEFAULT_CONFIG),
            unit_dir: PathBuf::from(UNIT_DIR),
            image_dir: PathBuf::from("/var/lib/bento/images"),
            storage_dir: PathBuf::from("/var/lib/bento/storage"),
            key_dir: PathBuf::from("/var/lib/bento/keys"),
            source: Some(PathBuf::from("/srv/bento")),
        };
        let mut app = App::new(paths, 1000);
        app.units = merge_units(&[UnitStatus {
            name: systemd::SERVE.to_string(),
            load_state: "loaded".to_string(),
            fragment_path: format!("{UNIT_DIR}/{}", systemd::SERVE),
            active_state: "active".to_string(),
            ..Default::default()
        }]);
        app.steps = install::steps(&app.paths, &facts(), &app.units, Some(true));
        app
    }

    #[test]
    fn every_unit_gets_a_row_even_when_systemd_never_heard_of_it() {
        let units = merge_units(&[]);
        assert_eq!(units.len(), UNITS.len());
        assert_eq!(units[0].name, systemd::SERVE);
        assert!(!units[0].installed());
    }

    #[test]
    fn the_tabs_wrap_in_both_directions() {
        let mut app = app();
        app.on_key(Key::Left);
        assert_eq!(app.tab, Tab::Host);
        app.on_key(Key::Right);
        assert_eq!(app.tab, Tab::Services);
        app.on_key(Key::Char('3'));
        assert_eq!(app.tab, Tab::Config);
    }

    #[test]
    fn the_cursor_wraps_inside_the_selected_tab() {
        let mut app = app();
        app.on_key(Key::Up);
        assert_eq!(app.unit_cursor, UNITS.len() - 1);
        app.on_key(Key::Down);
        assert_eq!(app.unit_cursor, 0);
    }

    #[test]
    fn a_unit_action_asks_before_it_runs_and_then_returns_the_command() {
        let mut app = app();
        assert_eq!(app.on_key(Key::Char('r')), Outcome::None);
        let Some(Modal::Confirm { title, commands }) = app.modal.clone() else {
            panic!("expected a confirmation, got {:?}", app.modal);
        };
        assert!(title.contains("restart"));
        assert_eq!(
            commands[0].display(),
            format!("sudo systemctl restart {}", systemd::SERVE)
        );
        let outcome = app.on_key(Key::Enter);
        assert_eq!(outcome, Outcome::Run { title, commands });
        assert!(app.modal.is_none());
    }

    #[test]
    fn any_other_key_cancels_the_confirmation() {
        let mut app = app();
        app.on_key(Key::Char('r'));
        assert_eq!(app.on_key(Key::Esc), Outcome::None);
        assert!(app.modal.is_none(), "the action must not have run");
    }

    #[test]
    fn an_action_on_a_unit_with_no_unit_file_says_what_to_do_first() {
        let mut app = app();
        app.unit_cursor = 1; // the proxy, which this host has not installed
        app.on_key(Key::Char('s'));
        let Some(Modal::Message { title, .. }) = &app.modal else {
            panic!("expected a message, got {:?}", app.modal);
        };
        assert_eq!(title, "no unit file");
    }

    #[test]
    fn reading_a_log_is_allowed_before_the_unit_exists() {
        // The journal keeps what a unit wrote before it was removed, and
        // that is often the reason it is gone.
        let mut app = app();
        app.unit_cursor = 1;
        app.on_key(Key::Char('l'));
        assert!(matches!(app.modal, Some(Modal::Confirm { .. })));
    }

    #[test]
    fn the_install_tab_runs_one_step_or_every_missing_step() {
        let mut app = app();
        app.tab = Tab::Install;
        app.step_cursor = 1; // the directories, which nothing blocks
        app.on_key(Key::Enter);
        let Some(Modal::Confirm { commands, .. }) = app.modal.clone() else {
            panic!("expected a confirmation, got {:?}", app.modal);
        };
        assert_eq!(commands.len(), 1);

        app.modal = None;
        app.on_key(Key::Char('a'));
        let Some(Modal::Confirm { commands, .. }) = app.modal.clone() else {
            panic!("expected a confirmation");
        };
        // Every step is missing on this host except none, so the list is
        // longer than one step's worth.
        assert!(commands.len() > 1, "{commands:?}");
    }

    #[test]
    fn a_blocked_step_explains_itself_rather_than_offering_commands() {
        let mut app = app();
        app.tab = Tab::Install;
        app.paths.source = None;
        let bare = HostFacts::default();
        app.steps = install::steps(&app.paths, &bare, &app.units, Some(true));
        app.step_cursor = 0; // the binary, with no source tree to build
        app.on_key(Key::Enter);
        assert!(matches!(app.modal, Some(Modal::Message { .. })));
    }

    #[test]
    fn the_config_tab_offers_the_operator_commands_with_the_right_path() {
        let mut app = app();
        app.tab = Tab::Config;
        app.paths.config = PathBuf::from("/srv/bento.toml");
        app.on_key(Key::Char('f'));
        let Some(Modal::Confirm { commands, .. }) = app.modal.clone() else {
            panic!("expected a confirmation");
        };
        assert_eq!(
            commands[0].display(),
            "sudo /usr/local/bin/bentod -config /srv/bento.toml fetch-images"
        );
    }

    #[test]
    fn escape_closes_what_is_open_but_never_the_program() {
        let mut app = app();
        assert_eq!(app.on_key(Key::Esc), Outcome::None);
        app.on_key(Key::Char('?'));
        assert_eq!(app.on_key(Key::Esc), Outcome::None);
        assert!(app.modal.is_none());
    }

    #[test]
    fn help_closes_without_closing_the_program() {
        let mut app = app();
        app.on_key(Key::Char('?'));
        assert_eq!(app.modal, Some(Modal::Help));
        assert_eq!(app.on_key(Key::Char('q')), Outcome::None);
        assert!(app.modal.is_none());
        assert_eq!(app.on_key(Key::Char('q')), Outcome::Quit);
    }
}
