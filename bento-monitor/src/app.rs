//! The state behind the screen, and what each key does to it.
//!
//! Nothing here writes to the host. A key that would change something
//! answers with the commands to run, and the caller runs them in the
//! operator's terminal (see [`Outcome::Run`]).

use std::path::PathBuf;
use std::time::Duration;

use bento_config::Config;
use bento_hypervisor::{CheckConfig, CheckResult};
use ratatui::layout::Rect;

use crate::fleet::{self, View};
use crate::host::HostSample;
use crate::install::{self, CONFIG_DOES_NOT_LOAD, HostFacts, Paths, Staging, Step, StepKind};
use crate::libvirt::{self, Census};
use crate::role::Role;
use crate::run::Cmd;
use crate::systemd::{self, UNITS, UnitAction, UnitStatus};
use crate::ui;

/// The Fleet screen sits beside Services because both report what is
/// running rather than what has to be installed (MULTI-NODE 20).
pub const TABS: [&str; 5] = ["Services", "Fleet", "Install", "Config", "Host"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Services,
    Fleet,
    Install,
    Config,
    Host,
}

impl Tab {
    pub fn index(self) -> usize {
        match self {
            Tab::Services => 0,
            Tab::Fleet => 1,
            Tab::Install => 2,
            Tab::Config => 3,
            Tab::Host => 4,
        }
    }

    fn from_index(index: usize) -> Tab {
        match index % TABS.len() {
            0 => Tab::Services,
            1 => Tab::Fleet,
            2 => Tab::Install,
            3 => Tab::Config,
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
    /// This machine's own identity (MULTI-NODE 16). The hostname drifts;
    /// this is what a host row is keyed on, so it is what says which row
    /// of the fleet is this machine.
    pub machine_id: Option<String>,
    /// What this machine is meant to run. It decides which units the
    /// Services and Install tabs expect, and which fleet view the Fleet
    /// tab can show.
    pub role: Role,
    /// The role that the last configuration that loaded decided. A
    /// configuration that stops loading keeps it (see
    /// [`Role::next_role`]).
    settled_role: Option<Role>,
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
    pub fleet: View,
    pub runner_cursor: usize,
    pub modal: Option<Modal>,
    /// How many lines the open modal is scrolled down.
    pub modal_scroll: u16,
    /// The size of the terminal, set by the caller before each key. A
    /// modal that is taller than it has to scroll, and what counts as
    /// "read to the end" depends on it.
    pub screen: Rect,
    /// The unit files that the open prompt shows, rendered for `install`
    /// to copy from. They live as long as the prompt does.
    pub staging: Staging,
    /// The last thing that happened, shown along the bottom.
    pub status: String,
}

impl App {
    pub fn new(paths: Paths, euid: u32) -> Self {
        App {
            paths,
            euid,
            machine_id: fleet::machine_id(),
            role: Role::Controller,
            settled_role: None,
            tab: Tab::Services,
            units: merge_units(&[], Role::Controller),
            systemctl_error: None,
            unit_cursor: 0,
            steps: Vec::new(),
            step_cursor: 0,
            config: Err("not read yet".to_string()),
            checks: Vec::new(),
            census: Err("not read yet".to_string()),
            host: HostSample::default(),
            fleet: View::Controller(Err("not read yet".to_string())),
            runner_cursor: 0,
            modal: None,
            modal_scroll: 0,
            screen: Rect::new(0, 0, 80, 24),
            staging: Staging::default(),
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

    /// The runner the Fleet tab has selected, when the fleet read.
    pub fn selected_runner(&self) -> Option<&fleet::Runner> {
        match &self.fleet {
            View::Controller(Ok(fleet)) => fleet.selected(self.runner_cursor),
            _ => None,
        }
    }

    fn runner_count(&self) -> usize {
        match &self.fleet {
            View::Controller(Ok(fleet)) => fleet.runners.len(),
            _ => 0,
        }
    }

    /// Rereads everything the screen shows. The libvirt census is the only
    /// asynchronous part, so it borrows the caller's runtime.
    pub fn refresh(&mut self, runtime: &tokio::runtime::Runtime) {
        // A prompt holds commands that were built from the state it was
        // opened on: the paths, the role, the staged files. A reread
        // under it would let the operator confirm commands that no
        // longer match the screen behind them.
        if matches!(self.modal, Some(Modal::Confirm { .. })) {
            return;
        }
        let parsed = match systemd::show_command().capture() {
            Ok(output) => {
                self.systemctl_error = None;
                systemd::parse_show(&output)
            }
            Err(error) => {
                self.systemctl_error = Some(error.to_string());
                Vec::new()
            }
        };

        self.config = Config::load(&self.paths.config).map_err(|error| error.to_string());
        // A configuration that loads moves the state directories, so the
        // paths follow it rather than staying at the defaults.
        if let Ok(config) = &self.config {
            self.paths.image_dir = PathBuf::from(&config.image_dir);
            self.paths.storage_dir = PathBuf::from(&config.storage_dir);
            self.paths.key_dir = PathBuf::from(&config.key_dir);
        }

        // The role is decided from the configuration and the unit files
        // this machine already carries, so it is read after both and
        // before anything that counts units.
        (self.role, self.settled_role) =
            Role::next_role(self.settled_role, self.config.as_ref().ok(), &parsed);
        self.units = merge_units(&parsed, self.role);

        let facts = HostFacts::probe(&self.paths);
        self.steps = install::steps(
            &self.paths,
            &facts,
            &self.units,
            Some(self.config.is_ok()),
            self.role,
        );
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
        self.fleet = View::read(
            runtime,
            self.role,
            self.config.as_ref().ok(),
            self.machine_id.as_deref(),
        );
        self.runner_cursor = self
            .runner_cursor
            .min(self.runner_count().saturating_sub(1));
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
                podman_required: config.images.iter().any(|image| image.source.is_oci()),
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
            Key::Char(digit @ '1'..='5') => {
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
        // Ctrl-C quits from anywhere, as it does with no modal open.
        if key == Key::Quit {
            self.close_modal();
            return Outcome::Quit;
        }
        let (max, page) = ui::modal_scroll_limit(&modal, self.screen);
        self.modal_scroll = self.modal_scroll.min(max);
        if max > 0 {
            let scrolled = match key {
                Key::Up | Key::Char('k') => Some(self.modal_scroll.saturating_sub(1)),
                Key::Down | Key::Char('j') => Some(self.modal_scroll.saturating_add(1)),
                Key::PageUp => Some(self.modal_scroll.saturating_sub(page)),
                Key::PageDown | Key::Char(' ') => Some(self.modal_scroll.saturating_add(page)),
                _ => None,
            };
            if let Some(scroll) = scrolled {
                self.modal_scroll = scroll.min(max);
                return Outcome::None;
            }
        }
        match (key, modal) {
            // Only `y` confirms. Enter opens the prompt, so a held or
            // repeated Enter would otherwise run a privileged step that
            // nobody read. A prompt that does not fit on the screen
            // confirms only once its last line has been shown.
            (Key::Char('y'), Modal::Confirm { title, commands }) => {
                if self.modal_scroll < max {
                    self.status = "scroll down to the end of the commands first".to_string();
                    return Outcome::None;
                }
                self.modal = None;
                self.modal_scroll = 0;
                Outcome::Run { title, commands }
            }
            // `q` closes the help rather than the program: an operator
            // who opened help by accident should not lose the screen.
            _ => {
                self.close_modal();
                Outcome::None
            }
        }
    }

    /// Closes the modal. A cancelled prompt takes its staged files with
    /// it. A confirmed one does not come through here: the commands still
    /// need them (see [`App::finish_run`]).
    fn close_modal(&mut self) {
        self.modal = None;
        self.modal_scroll = 0;
        self.staging.clear();
    }

    /// Removes what the last run staged. The caller calls it when the
    /// commands have ended.
    pub fn finish_run(&mut self) {
        self.staging.clear();
    }

    fn move_cursor(&mut self, delta: isize) {
        let runners = self.runner_count();
        let (cursor, len) = match self.tab {
            Tab::Services => (&mut self.unit_cursor, self.units.len()),
            Tab::Install => (&mut self.step_cursor, self.steps.len()),
            Tab::Fleet => (&mut self.runner_cursor, runners),
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
            Tab::Fleet => self.on_fleet_key(key),
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
        let selected = self.selected_unit();
        let unit = selected.name.clone();
        // A left-over unit file for a unit the role does not run can be
        // stopped, disabled, and read, but never started: a proxy on a
        // runner is what DEPLOYING.md 6 rules out (MULTI-NODE 19).
        if !selected.wanted
            && matches!(
                action,
                UnitAction::Start | UnitAction::Enable | UnitAction::Restart
            )
        {
            self.modal = Some(Modal::Message {
                title: format!("not a {} unit", self.role.label()),
                body: self.role.not_here(&unit),
            });
            return Outcome::None;
        }
        if !selected.installed() && !matches!(action, UnitAction::Logs) {
            let (title, body) = if selected.wanted {
                (
                    "no unit file".to_string(),
                    format!(
                        "{unit} is not installed. Run the unit-file step on the Install tab first."
                    ),
                )
            } else {
                (
                    format!("not a {} unit", self.role.label()),
                    self.role.not_here(&unit),
                )
            };
            self.modal = Some(Modal::Message { title, body });
            return Outcome::None;
        }
        self.confirm(
            format!("{} {unit}", action.label()),
            vec![action.command(&unit, self.euid)],
        );
        Outcome::None
    }

    /// The Fleet tab reads; it does not dispatch. Every change to a
    /// runner goes through the controller, which holds the lease
    /// (MULTI-NODE 11.3), so what the keys here offer is the operator
    /// command that reports more than one screen can hold.
    fn on_fleet_key(&mut self, key: Key) -> Outcome {
        let Key::Char(c) = key else {
            return Outcome::None;
        };
        if self.role == Role::Runner {
            self.modal = Some(Modal::Message {
                title: "no fleet here".to_string(),
                body: "This machine holds guests for a controller elsewhere. Slots and\n                       placement are the controller's to report; run bentod there."
                    .to_string(),
            });
            return Outcome::None;
        }
        match c {
            's' => self.confirm(
                "the slot table and who owns each slot".to_string(),
                vec![self.bentod("slots", Vec::new())],
            ),
            'p' => {
                let machine = self
                    .selected_runner()
                    .map(|runner| runner.name.clone())
                    .unwrap_or_default();
                let args = if machine.is_empty() {
                    Vec::new()
                } else {
                    vec![machine.clone()]
                };
                self.confirm(
                    if machine.is_empty() {
                        "the slot plan".to_string()
                    } else {
                        format!("the slot plan for {machine}")
                    },
                    vec![self.bentod("slots", {
                        let mut rest = vec!["plan".to_string()];
                        rest.extend(args);
                        rest
                    })],
                );
            }
            'c' => self.confirm(
                "report libvirt and database disagreements".to_string(),
                vec![self.bentod("reconcile", Vec::new())],
            ),
            _ => {}
        }
        Outcome::None
    }

    fn on_install_key(&mut self, key: Key) -> Outcome {
        match key {
            Key::Enter => {
                let Some(step) = self.selected_step().cloned() else {
                    return Outcome::None;
                };
                self.run_step(&step);
            }
            Key::Char('a') => self.run_remaining_steps(),
            _ => {}
        }
        Outcome::None
    }

    fn run_step(&mut self, step: &Step) {
        let title = &step.title;
        if let Some(reason) = &step.blocked {
            self.modal = Some(Modal::Message {
                title: format!("cannot run: {title}"),
                body: reason.clone(),
            });
            return;
        }
        // A step that is done does not run again. Each one can replace
        // what the host has now: the configuration step would put the
        // example over the live file and its secrets. What a done step
        // lacks the operator changes on purpose, by hand or from the
        // Config tab.
        if step.done {
            self.modal = Some(Modal::Message {
                title: format!("already done: {title}"),
                body: format!(
                    "{}\n\nThis step does not run again, because it could replace what \
                     the host has now.",
                    step.detail
                ),
            });
            return;
        }
        self.staging = Staging::default();
        match install::commands(
            step.kind,
            &self.paths,
            self.euid,
            self.role,
            &mut self.staging,
        ) {
            Ok(commands) => self.confirm(title.clone(), commands),
            Err(error) => {
                self.staging.clear();
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
        // A blocked step normally waits for an earlier one in this list.
        // One that waits for the configuration to load does not: the role
        // is not known, so the units are not either.
        if let Some(step) = self
            .steps
            .iter()
            .find(|step| step.blocked.as_deref() == Some(CONFIG_DOES_NOT_LOAD))
        {
            self.modal = Some(Modal::Message {
                title: "cannot install everything".to_string(),
                body: format!(
                    "{}: {CONFIG_DOES_NOT_LOAD}. Fix it on the Config tab first.",
                    step.title
                ),
            });
            return;
        }
        self.staging = Staging::default();
        let mut commands = Vec::new();
        for kind in pending {
            match install::commands(kind, &self.paths, self.euid, self.role, &mut self.staging) {
                Ok(mut list) => commands.append(&mut list),
                Err(error) => {
                    self.staging.clear();
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

    /// One `bentod` operator command, with the same configuration the
    /// units run with.
    fn bentod(&self, subcommand: &str, extra: Vec<String>) -> Cmd {
        let mut args = Vec::new();
        if let Some(path) = self.paths.config_flag() {
            args.push("-config".to_string());
            args.push(path.to_string());
        }
        args.push(subcommand.to_string());
        args.extend(extra);
        Cmd::owned(self.paths.binary.display().to_string(), args).privileged(self.euid)
    }

    fn on_config_key(&mut self, key: Key) -> Outcome {
        let Key::Char(c) = key else {
            return Outcome::None;
        };
        let config = self.paths.config.display().to_string();
        let bentod_with = |subcommand: &str, extra: Vec<String>| self.bentod(subcommand, extra);
        let bentod = |subcommand: &str| bentod_with(subcommand, Vec::new());
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
            'b' => match self.database_path() {
                Some(db) => {
                    let stale = self.units_older_than_binary();
                    if !stale.is_empty() {
                        self.modal = Some(Modal::Message {
                            title: "Backup".to_string(),
                            body: format!(
                                "The installed bentod is newer than the running {}.\n\n\
                                 A backup runs the installed bentod, and opening the database \
                                 applies its migrations under the process that still runs. \
                                 Restart those units first, or use the copy that the install \
                                 script made in /var/lib/bento/rollback/ (DEPLOYING.md 6).",
                                stale.join(", ")
                            ),
                        });
                        return Outcome::None;
                    }
                    let destination = format!("{db}.backup-{}", timestamp());
                    self.confirm(
                        format!("copy the database to {destination}"),
                        vec![bentod_with("dump-db", vec![destination])],
                    );
                }
                None => self.modal = Some(no_configuration("Backup")),
            },
            // A restore needs a file to restore from, and the screen has
            // nowhere to type one. It offers the newest copy beside the
            // database, and the confirmation shows the whole command, so
            // an operator who wants a different file can read the shape
            // and run it themselves.
            'r' => match self.database_path() {
                Some(_) if !self.running_units().is_empty() => {
                    // A restore does not coordinate with a writer
                    // (DEPLOYING.md 8): the units stop first.
                    self.modal = Some(Modal::Message {
                        title: "Restore".to_string(),
                        body: format!(
                            "Stop these units first: {}.\n\n\
                             A restore replaces the database under any process that has it \
                             open. Start them again after it.",
                            self.running_units().join(", ")
                        ),
                    });
                }
                Some(db) => match newest_backup(&db) {
                    Some(source) => self.confirm(
                        format!("replace {db} with {source}"),
                        vec![bentod_with("restore-db", vec![source])],
                    ),
                    None => {
                        self.modal = Some(Modal::Message {
                            title: "Restore".to_string(),
                            body: format!(
                                "No copy to restore from sits beside {db}.\n\n\
                                 Press b to make one, or run restore-db yourself with\n\
                                 the path of the copy you want."
                            ),
                        });
                    }
                },
                None => self.modal = Some(no_configuration("Restore")),
            },
            _ => {}
        }
        Outcome::None
    }

    /// The units that run now, by name. A unit the role does not want
    /// counts: a left-over one holds the database open all the same.
    fn running_units(&self) -> Vec<String> {
        self.units
            .iter()
            .filter(|unit| unit.running())
            .map(|unit| unit.name.clone())
            .collect()
    }

    /// The running units that started before the installed `bentod` was
    /// written (DEPLOYING.md 6, Upgrading).
    fn units_older_than_binary(&self) -> Vec<String> {
        let age = std::fs::metadata(&self.paths.binary)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.elapsed().ok());
        units_older_than(&self.units, self.host.uptime, age)
    }

    /// The database the loaded configuration names, if one loaded.
    fn database_path(&self) -> Option<String> {
        self.config.as_ref().ok().map(|c| c.db_path.clone())
    }

    fn confirm(&mut self, title: String, commands: Vec<Cmd>) {
        self.modal_scroll = 0;
        self.modal = Some(Modal::Confirm { title, commands });
    }
}

/// What an action says when it needs the database and no configuration
/// loaded to name one.
fn no_configuration(title: &str) -> Modal {
    Modal::Message {
        title: title.to_string(),
        body: "The configuration has not loaded, so nothing here knows where the\n\
               database is. The Config tab says why."
            .to_string(),
    }
}

/// A stamp for a backup file name: sortable, and readable by an operator
/// listing the directory.
fn timestamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::macros::format_description!(
            "[year][month][day]-[hour][minute][second]"
        ))
        .expect("fixed UTC timestamp format")
}

/// The newest copy of `db` that the Backup key wrote beside it.
///
/// `restore-db` keeps what it replaced as `.before-restore-<stamp>`. That
/// file is the way back from a restore, not a source for one: it is the
/// newest file of the directory right after a restore, and offering it
/// would let a second `r` undo the first without a word. It is read by
/// hand, never picked here.
pub fn newest_backup(db: &str) -> Option<String> {
    let path = std::path::Path::new(db);
    let directory = path.parent()?;
    let prefix = format!("{}.backup-", path.file_name()?.to_str()?);
    let mut newest: Option<String> = None;
    for entry in std::fs::read_dir(directory).ok()? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        // The stamp sorts as text, so the largest name is the newest.
        if name.starts_with(&prefix) && newest.as_ref().is_none_or(|best| name > *best) {
            newest = Some(name);
        }
    }
    newest.map(|name| directory.join(name).display().to_string())
}

/// The running, wanted units that went active before the binary was
/// written, which is to say that still run the old build.
///
/// `binary_age` is how long ago the installed binary was written, and a
/// unit's uptime is how long it has been active. A unit that has been
/// active for longer than the binary has existed started before it.
/// `None` for either says nothing is known, and then nothing is claimed.
pub fn units_older_than(
    units: &[UnitStatus],
    host_uptime: Option<Duration>,
    binary_age: Option<Duration>,
) -> Vec<String> {
    let (Some(host_uptime), Some(binary_age)) = (host_uptime, binary_age) else {
        return Vec::new();
    };
    units
        .iter()
        .filter(|unit| unit.wanted && unit.running())
        .filter(|unit| unit.uptime(host_uptime).is_some_and(|up| up > binary_age))
        .map(|unit| unit.name.clone())
        .collect()
}

/// One entry per unit of [`UNITS`], in that order, whatever `systemctl`
/// answered. A unit the host has never heard of still needs a row, because
/// that row is what says it has to be installed.
///
/// `role` marks which of them this machine is meant to run. Every unit
/// keeps its row either way: a controller unit left running on a runner
/// is exactly what an operator has to see (MULTI-NODE 19).
pub fn merge_units(parsed: &[UnitStatus], role: Role) -> Vec<UnitStatus> {
    UNITS
        .iter()
        .map(|unit| {
            let mut status = parsed
                .iter()
                .find(|status| status.name == unit.name)
                .cloned()
                .unwrap_or_else(|| UnitStatus {
                    name: unit.name.to_string(),
                    description: unit.description.to_string(),
                    ..Default::default()
                });
            // systemd answers for a unit it has no file for by repeating
            // the name as the description. The built-in one says what the
            // unit would do, which is what a machine that has not
            // installed it needs to read.
            if status.description.is_empty() || status.description == status.name {
                unit.description.clone_into(&mut status.description);
            }
            status.wanted = role.wants(unit.name);
            status
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
    PageUp,
    PageDown,
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
    use crate::install::{DEFAULT_BINARY, DEFAULT_CONFIG, DEFAULT_MONITOR, UNIT_DIR};

    /// A host with the binaries in place and nothing else. The facts are
    /// stated rather than probed, so that these tests do not depend on
    /// what the machine running them has installed.
    fn facts() -> HostFacts {
        HostFacts {
            missing_binaries: Vec::new(),
            stale_binaries: Vec::new(),
            config_installed: false,
            missing_directories: vec!["/var/lib/bento/storage".to_string()],
        }
    }

    fn app() -> App {
        app_as(Role::Controller)
    }

    fn app_as(role: Role) -> App {
        let paths = Paths {
            binary: PathBuf::from(DEFAULT_BINARY),
            monitor: PathBuf::from(DEFAULT_MONITOR),
            config: PathBuf::from(DEFAULT_CONFIG),
            unit_dir: PathBuf::from(UNIT_DIR),
            image_dir: PathBuf::from("/var/lib/bento/images"),
            storage_dir: PathBuf::from("/var/lib/bento/storage"),
            key_dir: PathBuf::from("/var/lib/bento/keys"),
            source: Some(PathBuf::from("/srv/bento")),
        };
        let mut app = App::new(paths, 1000);
        app.role = role;
        app.units = merge_units(
            &[UnitStatus {
                name: systemd::SERVE.to_string(),
                load_state: "loaded".to_string(),
                fragment_path: format!("{UNIT_DIR}/{}", systemd::SERVE),
                active_state: "active".to_string(),
                ..Default::default()
            }],
            role,
        );
        app.steps = install::steps(&app.paths, &facts(), &app.units, Some(true), role);
        app
    }

    fn stop_every_unit(app: &mut App) {
        for unit in &mut app.units {
            unit.active_state = "inactive".to_string();
        }
    }

    fn message(app: &App) -> (String, String) {
        let Some(Modal::Message { title, body }) = app.modal.clone() else {
            panic!("expected a message, got {:?}", app.modal);
        };
        (title, body)
    }

    #[test]
    fn a_unit_with_no_file_still_says_what_it_would_do() {
        let parsed = vec![UnitStatus {
            name: systemd::SERVE.to_string(),
            // What systemd answers for a unit it has no file for.
            description: systemd::SERVE.to_string(),
            load_state: "not-found".to_string(),
            ..Default::default()
        }];
        let units = merge_units(&parsed, Role::Runner);
        assert_eq!(units[0].description, "Bento control plane");
    }

    #[test]
    fn every_unit_gets_a_row_even_when_systemd_never_heard_of_it() {
        let units = merge_units(&[], Role::Controller);
        assert_eq!(units.len(), UNITS.len());
        assert_eq!(units[0].name, systemd::SERVE);
        assert!(!units[0].installed());
        assert!(units.iter().all(|unit| unit.wanted));

        // A runner keeps the same rows and wants one of them.
        let units = merge_units(&[], Role::Runner);
        assert_eq!(units.len(), UNITS.len());
        assert_eq!(units.iter().filter(|unit| unit.wanted).count(), 1);
    }

    #[test]
    fn the_tabs_wrap_in_both_directions() {
        let mut app = app();
        app.on_key(Key::Left);
        assert_eq!(app.tab, Tab::Host);
        app.on_key(Key::Right);
        assert_eq!(app.tab, Tab::Services);
        app.on_key(Key::Char('2'));
        assert_eq!(app.tab, Tab::Fleet);
        app.on_key(Key::Char('5'));
        assert_eq!(app.tab, Tab::Host);
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
        let outcome = app.on_key(Key::Char('y'));
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
        // The directory step reads the host, so point it at a tree that
        // has none of them.
        let root = tempfile::tempdir().unwrap();
        app.paths.config = root.path().join("etc/bento.toml");
        app.paths.image_dir = root.path().join("images");
        app.paths.storage_dir = root.path().join("storage");
        app.paths.key_dir = root.path().join("keys");
        app.tab = Tab::Install;
        app.step_cursor = 1; // the directories, which nothing blocks
        app.on_key(Key::Enter);
        let Some(Modal::Confirm { commands, .. }) = app.modal.clone() else {
            panic!("expected a confirmation, got {:?}", app.modal);
        };
        // The shared mode 0755 and the private key directory.
        assert_eq!(commands.len(), 2);

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
        let bare = HostFacts {
            missing_binaries: vec!["bentod".to_string()],
            ..HostFacts::default()
        };
        app.steps = install::steps(&app.paths, &bare, &app.units, Some(true), app.role);
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
    fn the_config_tab_backs_the_database_up_and_restores_the_newest_copy() {
        let directory = tempfile::tempdir().unwrap();
        let db = directory.path().join("bento.db");
        let mut app = app();
        app.tab = Tab::Config;
        app.config = Ok(Config {
            db_path: db.display().to_string(),
            ..Default::default()
        });
        stop_every_unit(&mut app);

        // With no copy beside it, restore says so rather than offering a
        // command with nothing to run.
        app.on_key(Key::Char('r'));
        let Some(Modal::Message { title, body }) = app.modal.clone() else {
            panic!("expected a message, got {:?}", app.modal);
        };
        assert_eq!(title, "Restore");
        assert!(body.contains("No copy to restore from"), "{body}");
        app.modal = None;

        // Backup names its destination beside the database.
        app.on_key(Key::Char('b'));
        let Some(Modal::Confirm { commands, .. }) = app.modal.clone() else {
            panic!("expected a confirmation, got {:?}", app.modal);
        };
        let line = commands[0].display();
        assert!(line.contains("dump-db"), "{line}");
        assert!(
            line.contains(&format!("{}.backup-", db.display())),
            "{line}"
        );
        app.modal = None;

        // Restore offers the newest of the copies that are there.
        for name in [
            "bento.db.backup-20260101-000000",
            "bento.db.backup-20260905-120000",
        ] {
            std::fs::write(directory.path().join(name), b"").unwrap();
        }
        std::fs::write(
            directory
                .path()
                .join("bento.db.before-restore-20260301-000000"),
            b"",
        )
        .unwrap();
        app.on_key(Key::Char('r'));
        let Some(Modal::Confirm { commands, .. }) = app.modal.clone() else {
            panic!("expected a confirmation, got {:?}", app.modal);
        };
        let line = commands[0].display();
        assert!(line.contains("restore-db"), "{line}");
        assert!(line.ends_with("bento.db.backup-20260905-120000"), "{line}");
    }

    #[test]
    fn an_action_that_needs_the_database_says_when_no_configuration_loaded() {
        let mut app = app();
        app.tab = Tab::Config;
        app.config = Err("no such file".to_string());
        for key in ['b', 'r'] {
            app.on_key(Key::Char(key));
            assert!(
                matches!(app.modal, Some(Modal::Message { .. })),
                "{key}: {:?}",
                app.modal
            );
            app.modal = None;
        }
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

    fn with_config_installed(app: &mut App) {
        let facts = HostFacts {
            config_installed: true,
            ..facts()
        };
        app.steps = install::steps(&app.paths, &facts, &app.units, Some(true), app.role);
    }

    #[test]
    fn a_step_that_is_done_answers_with_a_message_and_not_a_prompt() {
        let mut app = app();
        with_config_installed(&mut app);
        app.tab = Tab::Install;
        app.step_cursor = 2; // the configuration file, which the host has
        assert!(app.steps[2].done);
        assert_eq!(app.on_key(Key::Enter), Outcome::None);
        let (title, _) = message(&app);
        assert!(title.starts_with("already done"), "{title}");
    }

    #[test]
    fn enter_opens_a_prompt_and_never_confirms_it() {
        let mut app = app();
        app.tab = Tab::Install;
        app.step_cursor = 1;
        assert_eq!(app.on_key(Key::Enter), Outcome::None);
        assert!(matches!(app.modal, Some(Modal::Confirm { .. })));
        // A repeated Enter must not run the step.
        assert_eq!(app.on_key(Key::Enter), Outcome::None);
        assert!(app.modal.is_none());
    }

    #[test]
    fn control_c_quits_from_inside_a_prompt() {
        let mut app = app();
        app.on_key(Key::Char('r'));
        assert!(matches!(app.modal, Some(Modal::Confirm { .. })));
        assert_eq!(app.on_key(Key::Quit), Outcome::Quit);
        assert!(app.modal.is_none());
    }

    #[test]
    fn a_prompt_that_does_not_fit_confirms_only_after_its_end_was_shown() {
        let mut app = app();
        let commands: Vec<Cmd> = (0..11)
            .map(|index| Cmd::owned("sudo", vec!["install".into(), format!("file-{index}")]))
            .collect();
        // Make each command long enough to wrap on 80 columns.
        let commands: Vec<Cmd> = commands
            .into_iter()
            .map(|mut command| {
                command.args.push("x".repeat(90));
                command
            })
            .collect();
        app.screen = Rect::new(0, 0, 80, 24);
        app.confirm("install everything".to_string(), commands);
        let (max, _) = ui::modal_scroll_limit(app.modal.as_ref().expect("modal"), app.screen);
        assert!(max > 0);

        // `y` before the end does nothing and keeps the prompt.
        assert_eq!(app.on_key(Key::Char('y')), Outcome::None);
        assert!(matches!(app.modal, Some(Modal::Confirm { .. })));
        for _ in 0..max {
            assert_eq!(app.on_key(Key::Char('j')), Outcome::None);
        }
        assert_eq!(app.modal_scroll, max);
        assert!(matches!(app.on_key(Key::Char('y')), Outcome::Run { .. }));
        assert!(app.modal.is_none());
    }

    #[test]
    fn a_reread_leaves_an_open_prompt_and_its_state_alone() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let mut app = app();
        app.on_key(Key::Char('r'));
        let before = app.modal.clone();
        app.role = Role::Runner;
        app.refresh(&runtime);
        assert_eq!(app.modal, before);
        // Nothing was reread: the configuration is still unread.
        assert_eq!(
            app.config.as_ref().err().map(String::as_str),
            Some("not read yet")
        );
        assert_eq!(app.role, Role::Runner);
    }

    #[test]
    fn a_unit_the_role_does_not_run_is_not_started_even_with_a_unit_file() {
        let mut app = app_as(Role::Runner);
        app.units = merge_units(
            &[UnitStatus {
                name: systemd::PROXY.to_string(),
                load_state: "loaded".to_string(),
                fragment_path: format!("{UNIT_DIR}/{}", systemd::PROXY),
                active_state: "inactive".to_string(),
                ..Default::default()
            }],
            Role::Runner,
        );
        app.unit_cursor = 1;
        assert!(app.selected_unit().installed());
        for key in ['s', 'e', 'r'] {
            app.on_key(Key::Char(key));
            let (title, _) = message(&app);
            assert_eq!(title, "not a runner unit", "{key}");
            app.modal = None;
        }
        // Stopping and reading stay possible.
        for key in ['t', 'd', 'l'] {
            app.on_key(Key::Char(key));
            assert!(matches!(app.modal, Some(Modal::Confirm { .. })), "{key}");
            app.modal = None;
        }
    }

    #[test]
    fn a_restore_waits_for_the_units_to_stop() {
        let directory = tempfile::tempdir().unwrap();
        let db = directory.path().join("bento.db");
        std::fs::write(
            directory.path().join("bento.db.backup-20260101-000000"),
            b"",
        )
        .unwrap();
        let mut app = app();
        app.tab = Tab::Config;
        app.config = Ok(Config {
            db_path: db.display().to_string(),
            ..Default::default()
        });
        // The fixture's serve unit is active.
        app.on_key(Key::Char('r'));
        let (title, body) = message(&app);
        assert_eq!(title, "Restore");
        assert!(body.contains(systemd::SERVE), "{body}");
    }

    #[test]
    fn a_restore_never_offers_the_safety_copy_that_a_restore_left() {
        let directory = tempfile::tempdir().unwrap();
        let db = directory.path().join("bento.db");
        for name in [
            "bento.db.backup-20260101-000000",
            "bento.db.before-restore-20260905-120000",
        ] {
            std::fs::write(directory.path().join(name), b"").unwrap();
        }
        assert_eq!(
            newest_backup(&db.display().to_string()),
            Some(
                directory
                    .path()
                    .join("bento.db.backup-20260101-000000")
                    .display()
                    .to_string()
            )
        );
    }

    fn running(name: &str, since_seconds: u64) -> UnitStatus {
        UnitStatus {
            name: name.to_string(),
            wanted: true,
            active_state: "active".to_string(),
            active_since_monotonic: Some(since_seconds * 1_000_000),
            ..Default::default()
        }
    }

    #[test]
    fn a_unit_that_started_before_the_binary_was_written_still_runs_the_old_build() {
        let host = Some(Duration::from_secs(1000));
        // Active for 900 s. The binary is 60 s old, so it is newer.
        let units = vec![running(systemd::SERVE, 100)];
        assert_eq!(
            units_older_than(&units, host, Some(Duration::from_secs(60))),
            [systemd::SERVE]
        );
        // Restarted 10 s ago, after the binary: current.
        let units = vec![running(systemd::SERVE, 990)];
        assert!(units_older_than(&units, host, Some(Duration::from_secs(60))).is_empty());
        // Unknown ages claim nothing.
        assert!(units_older_than(&units, None, Some(Duration::ZERO)).is_empty());
        assert!(units_older_than(&units, host, None).is_empty());
    }

    #[test]
    fn a_backup_after_an_install_and_before_a_restart_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("bentod");
        std::fs::write(&binary, b"new build").unwrap();
        let mut app = app();
        app.tab = Tab::Config;
        app.paths.binary = binary;
        app.config = Ok(Config {
            db_path: directory.path().join("bento.db").display().to_string(),
            ..Default::default()
        });
        // serve went active 900 s ago; the binary was written just now.
        app.host.uptime = Some(Duration::from_secs(1000));
        app.units[0] = UnitStatus {
            fragment_path: format!("{UNIT_DIR}/{}", systemd::SERVE),
            load_state: "loaded".to_string(),
            ..running(systemd::SERVE, 100)
        };
        app.on_key(Key::Char('b'));
        let (title, body) = message(&app);
        assert_eq!(title, "Backup");
        assert!(body.contains(systemd::SERVE), "{body}");
        app.modal = None;

        // After the restart the unit is newer than the binary.
        app.units[0].active_since_monotonic = Some(1_000_000_000);
        app.on_key(Key::Char('b'));
        assert!(matches!(app.modal, Some(Modal::Confirm { .. })));
    }
}
