//! `bento-monitor`: a terminal screen over the systemd units that run
//! Bento on one host (DEPLOYING.md sections 4 and 6).
//!
//! It is a shim, on purpose. It installs the binary, the directories, the
//! configuration, and the three units; it starts, stops, restarts,
//! enables, and disables them; and it reports what systemd, libvirt, and
//! the host say. It holds no state of its own and it changes nothing
//! behind the operator: every action is shown as the command it is, and
//! that command runs in this terminal, where `sudo` can still ask for a
//! password and the operator can read what happened.

mod app;
mod host;
mod install;
mod libvirt;
mod run;
mod systemd;
mod ui;

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Instant;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use app::{App, Key, Outcome, REFRESH_INTERVAL};
use install::{DEFAULT_BINARY, DEFAULT_CONFIG, Paths, UNIT_DIR};
use run::Cmd;

fn main() {
    let options = match parse_args(std::env::args_os().skip(1).collect()) {
        Ok(Some(options)) => options,
        Ok(None) => {
            usage(&mut io::stdout());
            return;
        }
        Err(error) => {
            eprintln!("bento-monitor: {error}\n");
            usage(&mut io::stderr());
            std::process::exit(2);
        }
    };

    // One thread is enough: the only asynchronous work is the libvirt
    // census, and the screen waits for it.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("bento-monitor: {error}");
            std::process::exit(1);
        }
    };

    let mut app = App::new(options.paths, run::euid());
    app.refresh(&runtime);

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &runtime, &mut app);
    ratatui::restore();
    if let Err(error) = result {
        eprintln!("bento-monitor: {error}");
        std::process::exit(1);
    }
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    runtime: &tokio::runtime::Runtime,
    app: &mut App,
) -> io::Result<()> {
    let mut last_refresh = Instant::now();
    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;

        let waited = REFRESH_INTERVAL.saturating_sub(last_refresh.elapsed());
        if event::poll(waited)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let key = classify(key);
                    if key == Key::Refresh {
                        app.refresh(runtime);
                        last_refresh = Instant::now();
                    }
                    match app.on_key(key) {
                        Outcome::Quit => return Ok(()),
                        Outcome::Run { title, commands } => {
                            run_commands(terminal, app, &title, &commands)?;
                            app.refresh(runtime);
                            last_refresh = Instant::now();
                        }
                        Outcome::None => {}
                    }
                }
                _ => {}
            }
        }
        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            app.refresh(runtime);
            last_refresh = Instant::now();
        }
    }
}

/// Leaves the screen, runs the commands in order, and comes back. The
/// terminal is the operator's own again while they run, which is what
/// lets `sudo` ask for a password, `journalctl -f` scroll, and `$EDITOR`
/// take over. The first command that fails stops the list, because a
/// later command usually depends on it.
fn run_commands(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    title: &str,
    commands: &[Cmd],
) -> io::Result<()> {
    ratatui::restore();
    println!("\n=== {title}\n");
    let mut failure = None;
    for command in commands {
        println!("$ {}", command.display());
        io::stdout().flush()?;
        match command.spawn_interactive() {
            Ok(status) if status.success() => {}
            Ok(status) => {
                failure = Some(match status.code() {
                    Some(code) => format!("{} exited with {code}", command.program),
                    None => format!("{} was killed by a signal", command.program),
                });
                break;
            }
            Err(error) => {
                failure = Some(format!("{}: {error}", command.program));
                break;
            }
        }
    }
    let summary = match &failure {
        Some(reason) => format!("{title}: {reason}"),
        None => format!("{title}: done"),
    };
    println!("\n{summary}");
    print!("press enter to go back to bento-monitor ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;

    // A new terminal starts with an empty previous buffer, so the next
    // draw writes the whole screen. `Terminal::clear` would do the same,
    // but it first asks the terminal where the cursor is, and a terminal
    // that never answers that query would leave the screen stuck here.
    *terminal = ratatui::init();
    app.status = summary;
    Ok(())
}

/// Turns a terminal key into the one the screen knows.
fn classify(key: KeyEvent) -> Key {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Key::Quit,
            KeyCode::Char('r') => Key::Refresh,
            _ => Key::Other,
        };
    }
    match key.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::F(1) => Key::Help,
        KeyCode::F(5) => Key::Refresh,
        _ => Key::Other,
    }
}

struct Options {
    paths: Paths,
}

/// Reads the flags. They follow `bentod`: one leading dash or two, and a
/// value either after a space or after `=`.
fn parse_args(args: Vec<OsString>) -> Result<Option<Options>, String> {
    let mut config = PathBuf::from(DEFAULT_CONFIG);
    let mut binary = PathBuf::from(DEFAULT_BINARY);
    let mut source: Option<PathBuf> = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let text = arg.to_string_lossy().into_owned();
        if text == "-h" || text == "--help" {
            return Ok(None);
        }
        let (name, inline) = match text.split_once('=') {
            Some((name, value)) => (name.to_string(), Some(value.to_string())),
            None => (text.clone(), None),
        };
        let mut value = || -> Result<String, String> {
            match inline.clone() {
                Some(value) => Ok(value),
                None => iter
                    .next()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .ok_or_else(|| format!("flag needs an argument: {name}")),
            }
        };
        match name.as_str() {
            "-config" | "--config" => config = PathBuf::from(value()?),
            "-binary" | "--binary" => binary = PathBuf::from(value()?),
            "-source" | "--source" => source = Some(PathBuf::from(value()?)),
            other => return Err(format!("flag provided but not defined: {other}")),
        }
    }

    let source = match source {
        Some(source) => Some(source),
        // Without a flag, the source tree is the one the monitor was
        // started inside, if any. It is what the build and the example
        // configuration come from.
        None => std::env::current_dir()
            .ok()
            .and_then(|cwd| install::find_source(&cwd)),
    };
    Ok(Some(Options {
        paths: Paths {
            binary,
            config,
            unit_dir: PathBuf::from(UNIT_DIR),
            // The configuration moves these when it loads (`App::refresh`).
            image_dir: PathBuf::from(bento_config::defaults::IMAGE_DIR),
            storage_dir: PathBuf::from(bento_config::defaults::STORAGE_DIR),
            key_dir: PathBuf::from(bento_config::defaults::KEY_DIR),
            source,
        },
    }))
}

fn usage(writer: &mut dyn Write) {
    let _ = writeln!(writer, "Usage: bento-monitor [flags]");
    let _ = writeln!(
        writer,
        "\nA terminal screen over the bentod units, the configuration, and the host."
    );
    let _ = writeln!(
        writer,
        "Run it as root, or as a user who can sudo: it adds the sudo itself, per action."
    );
    let _ = writeln!(writer, "\nFlags:");
    let _ = writeln!(
        writer,
        "  -config string\n    \tpath to the bento configuration file (default \"{DEFAULT_CONFIG}\")"
    );
    let _ = writeln!(
        writer,
        "  -binary string\n    \tpath the bentod binary is installed at (default \"{DEFAULT_BINARY}\")"
    );
    let _ = writeln!(
        writer,
        "  -source string\n    \tthe Bento source tree to build and copy from\n    \t(default: the tree the working directory is in)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Options>, String> {
        parse_args(args.iter().map(OsString::from).collect())
    }

    #[test]
    fn the_defaults_match_the_runbook() {
        let options = parse(&[]).expect("parse").expect("options");
        assert_eq!(options.paths.config, PathBuf::from("/etc/bento/bento.toml"));
        assert_eq!(options.paths.binary, PathBuf::from("/usr/local/bin/bentod"));
        assert_eq!(options.paths.unit_dir, PathBuf::from("/etc/systemd/system"));
    }

    #[test]
    fn a_flag_takes_its_value_after_a_space_or_an_equals_sign() {
        let options = parse(&["--config", "/srv/a.toml", "-binary=/opt/bentod"])
            .expect("parse")
            .expect("options");
        assert_eq!(options.paths.config, PathBuf::from("/srv/a.toml"));
        assert_eq!(options.paths.binary, PathBuf::from("/opt/bentod"));
    }

    #[test]
    fn help_asks_for_the_usage_and_an_unknown_flag_is_refused() {
        assert!(parse(&["-h"]).expect("parse").is_none());
        assert!(parse(&["-nope"]).is_err());
        assert!(parse(&["--config"]).is_err(), "a flag without its value");
    }

    #[test]
    fn the_source_tree_defaults_to_the_one_the_monitor_runs_in() {
        // The test runs inside the Bento tree.
        let options = parse(&[]).expect("parse").expect("options");
        let source = options.paths.source.expect("source tree");
        assert!(source.join("bento.example.toml").is_file());

        let options = parse(&["--source", "/srv/elsewhere"])
            .expect("parse")
            .expect("options");
        assert_eq!(options.paths.source, Some(PathBuf::from("/srv/elsewhere")));
    }

    #[test]
    fn control_c_quits_and_the_function_keys_do_their_jobs() {
        let key = |code, modifiers| classify(KeyEvent::new(code, modifiers));
        assert_eq!(key(KeyCode::Char('c'), KeyModifiers::CONTROL), Key::Quit);
        assert_eq!(key(KeyCode::Char('r'), KeyModifiers::CONTROL), Key::Refresh);
        // A plain r is the restart key, not a refresh.
        assert_eq!(key(KeyCode::Char('r'), KeyModifiers::NONE), Key::Char('r'));
        assert_eq!(key(KeyCode::F(5), KeyModifiers::NONE), Key::Refresh);
        assert_eq!(key(KeyCode::F(1), KeyModifiers::NONE), Key::Help);
        assert_eq!(key(KeyCode::BackTab, KeyModifiers::SHIFT), Key::BackTab);
    }

    #[test]
    fn the_usage_names_every_flag() {
        let mut text = Vec::new();
        usage(&mut text);
        let text = String::from_utf8(text).expect("utf-8");
        for flag in ["-config", "-binary", "-source"] {
            assert!(text.contains(flag), "{text}");
        }
    }
}
