//! The commands the monitor runs on the host.
//!
//! The monitor changes nothing by itself. Every action becomes a command
//! that the operator reads before it runs, and that runs in the operator's
//! own terminal with the operator's own stdio. That keeps two properties
//! the tool depends on: `sudo` can still ask for a password, and an
//! operator can repeat by hand what the monitor did.

use std::io;
use std::process::{Command, ExitStatus};

/// One command line, kept as program and arguments so that nothing has to
/// go through a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    /// The working directory, for the commands that need the source tree.
    pub dir: Option<String>,
}

impl Cmd {
    pub fn new<S: Into<String>>(program: S, args: &[&str]) -> Self {
        Cmd {
            program: program.into(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            dir: None,
        }
    }

    pub fn owned(program: impl Into<String>, args: Vec<String>) -> Self {
        Cmd {
            program: program.into(),
            args,
            dir: None,
        }
    }

    pub fn in_dir(mut self, dir: impl Into<String>) -> Self {
        self.dir = Some(dir.into());
        self
    }

    /// Prefixes `sudo` when the monitor does not already run as root.
    /// `sudo` is not added twice, so an action built from other actions
    /// stays one `sudo` deep.
    pub fn privileged(mut self, euid: u32) -> Self {
        if euid == 0 || self.program == "sudo" {
            return self;
        }
        let mut args = vec![self.program];
        args.append(&mut self.args);
        Cmd {
            program: "sudo".to_string(),
            args,
            dir: self.dir,
        }
    }

    /// The command as an operator would type it. Only what needs quoting
    /// is quoted, so the usual command reads as itself.
    pub fn display(&self) -> String {
        let mut line = String::new();
        for word in std::iter::once(&self.program).chain(self.args.iter()) {
            if !line.is_empty() {
                line.push(' ');
            }
            if word.is_empty() || word.chars().any(|c| " \t\"'\\$`&|;<>()".contains(c)) {
                line.push('\'');
                line.push_str(&word.replace('\'', "'\\''"));
                line.push('\'');
            } else {
                line.push_str(word);
            }
        }
        line
    }

    /// Runs the command with the terminal's own stdio. The caller leaves
    /// the alternate screen first (see [`crate::main`]).
    pub fn spawn_interactive(&self) -> io::Result<ExitStatus> {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if let Some(dir) = &self.dir {
            command.current_dir(dir);
        }
        command.status()
    }

    /// Runs the command and captures its standard output. Used for the
    /// read-only polls that fill the screen, never for an action.
    pub fn capture(&self) -> io::Result<String> {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if let Some(dir) = &self.dir {
            command.current_dir(dir);
        }
        let output = command.output()?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(io::Error::other(if detail.is_empty() {
                format!("{} exited with {}", self.program, output.status)
            } else {
                detail
            }));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// The effective user id of this process. Root skips `sudo` everywhere.
pub fn euid() -> u32 {
    // Safety: `geteuid` reads one field of the calling process and cannot
    // fail.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_root_command_gets_sudo_once() {
        let cmd = Cmd::new("systemctl", &["restart", "bentod-serve.service"]);
        let wrapped = cmd.clone().privileged(1000);
        assert_eq!(wrapped.program, "sudo");
        assert_eq!(wrapped.args[0], "systemctl");
        assert_eq!(wrapped.clone().privileged(1000), wrapped);
        assert_eq!(cmd.clone().privileged(0), cmd);
    }

    #[test]
    fn the_working_directory_survives_the_sudo_wrap() {
        let cmd = Cmd::new("cargo", &["build"]).in_dir("/srv/bento");
        assert_eq!(cmd.privileged(1000).dir.as_deref(), Some("/srv/bento"));
    }

    #[test]
    fn display_quotes_only_what_needs_it() {
        let cmd = Cmd::new("install", &["-m", "0644", "/tmp/a b", "/etc/x"]);
        assert_eq!(cmd.display(), "install -m 0644 '/tmp/a b' /etc/x");
    }
}
