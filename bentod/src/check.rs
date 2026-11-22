//! `bentod check`: the deployment preflight (SPEC 4.2, DEPLOYING.md
//! sections 1, 2, and 6). It reads the host and prints every requirement
//! with its fix. It changes no host setting; the directory write probe
//! makes and deletes one file in each checked directory.
//!
//! The result is the truth for this process. The units run as root, so
//! run `bentod check` as root for the full result: without root, the
//! root-privileges warning shows, and the nftables, port, and directory
//! checks fail for this user only.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use bento_preflight::{Group, HostProbe, Input, Privilege, Probe, Report, Status};

#[derive(Debug, Default, PartialEq, Eq)]
struct Flags {
    json: bool,
    online: bool,
}

fn parse_flags(args: &[OsString]) -> Result<Flags, String> {
    let mut flags = Flags::default();
    for arg in args {
        match arg.to_string_lossy().as_ref() {
            "-json" | "--json" => flags.json = true,
            "-online" | "--online" => flags.online = true,
            other => return Err(format!("check: flag provided but not defined: {other}")),
        }
    }
    Ok(flags)
}

/// Returns the exit status: 0 when no fatal requirement is unmet, 1 when
/// one is, 2 for a usage error. A configuration that does not load is a
/// requirement of the report, not an early exit, so the operator sees
/// the host requirements too.
pub(crate) async fn run_check(config_path: &Path, args: &[OsString]) -> i32 {
    let config_path = config_path.to_path_buf();
    let args = args.to_vec();
    // The running binary, so that a fix command runs this bentod and not
    // the first one on the PATH of whoever copies it.
    let bentod = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "bentod".to_string());
    // Binds, a /proc scan, and a child process: blocking work that stays
    // off the async workers.
    let status = tokio::task::spawn_blocking(move || {
        run_check_with(
            &config_path,
            &args,
            &HostProbe,
            &bentod,
            &mut std::io::stdout().lock(),
        )
    })
    .await;
    status.unwrap_or_else(|error| {
        eprintln!("bentod check: {error}");
        1
    })
}

/// [`run_check`] against `probe`, writing the report to `out`. `bentod`
/// replaces the bare program name `bentod` in each fix command.
fn run_check_with(
    config_path: &Path,
    args: &[OsString],
    probe: &dyn Probe,
    bentod: &str,
    out: &mut dyn Write,
) -> i32 {
    let flags = match parse_flags(args) {
        Ok(flags) => flags,
        Err(error) => {
            eprintln!("bentod {error}\nUsage: bentod [-config path] check [-json] [-online]");
            return 2;
        }
    };
    let (config, config_error) = match bento_config::Config::load(config_path) {
        Ok(config) => (Some(config), None),
        Err(error) => (None, Some(error.to_string())),
    };
    let input = Input {
        config: config.as_ref(),
        config_error,
        config_path: fix_config_path(config_path),
        role: bento_preflight::detect_role(config.as_ref()),
        online: flags.online,
        // The instances that request nested virtualization live in the
        // database. Opening it can migrate it, so the nested check stays
        // out.
        nested_wanted: false,
        // The truth for this process. An operator who runs the check
        // without root sees what this user can do; the root-privileges
        // line says to run it as root for the full result.
        privilege: Privilege::Actual,
        binds: true,
    };
    let mut report = bento_preflight::run(&input, probe);
    name_binary(&mut report, bentod);
    let written = if flags.json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => writeln!(out, "{text}"),
            Err(error) => {
                eprintln!("bentod check: {error}");
                return 1;
            }
        }
    } else {
        write!(out, "{}", render(&report))
    };
    if let Err(error) = written {
        eprintln!("bentod check: {error}");
        return 1;
    }
    if report.ok() { 0 } else { 1 }
}

/// The `-config` value for fix commands. The default path is what
/// `bentod` reads with no flag, so a fix command that omits it runs
/// against the same file. A relative path is made absolute: the operator
/// can run the fix from a different directory, or under sudo.
fn fix_config_path(config_path: &Path) -> Option<PathBuf> {
    if config_path == Path::new(crate::DEFAULT_CONFIG_PATH) {
        return None;
    }
    if config_path.is_absolute() {
        return Some(config_path.to_path_buf());
    }
    Some(
        std::fs::canonicalize(config_path)
            .or_else(|_| std::path::absolute(config_path))
            .unwrap_or_else(|_| config_path.to_path_buf()),
    )
}

/// Replaces the bare program name `bentod` in each fix command. Under
/// sudo, a bare name is looked up in the `secure_path` of sudo, which can
/// hold a different bentod or none.
fn name_binary(report: &mut Report, bentod: &str) {
    for requirement in &mut report.requirements {
        if let Some(fix) = &mut requirement.fix {
            for command in &mut fix.commands {
                if let Some(program) = command.first_mut()
                    && program == "bentod"
                {
                    *program = bentod.to_string();
                }
            }
        }
    }
}

fn group_title(group: Group) -> &'static str {
    match group {
        Group::Host => "Host",
        Group::Libvirt => "libvirt",
        Group::Network => "Network",
        Group::Ports => "Ports",
        Group::Storage => "Storage",
        Group::Config => "Configuration",
        Group::Images => "Images",
    }
}

fn marker(status: Status) -> &'static str {
    match status {
        Status::Met => "ok  ",
        Status::Unmet => "FAIL",
        Status::Warn => "warn",
        Status::Skipped => "skip",
    }
}

/// One argument as an operator types it into a POSIX shell.
fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=@%+,".contains(&b));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// The human report: one section for each group, one line for each
/// requirement, a fix under each failure, and a count line.
fn render(report: &Report) -> String {
    let mut out = format!("Bento preflight for a {} machine\n", report.role);
    let mut current = None;
    for requirement in &report.requirements {
        if current != Some(requirement.group) {
            current = Some(requirement.group);
            out.push_str(&format!("\n{}\n", group_title(requirement.group)));
        }
        out.push_str(&format!(
            "  {}  {:<38} {}\n",
            marker(requirement.status),
            requirement.title,
            requirement.detail
        ));
        if !matches!(requirement.status, Status::Unmet | Status::Warn) {
            continue;
        }
        if let Some(fix) = &requirement.fix {
            let who = if fix.root && !fix.commands.is_empty() {
                " (as root)"
            } else {
                ""
            };
            out.push_str(&format!("        fix{who}: {}\n", fix.summary));
            for command in &fix.commands {
                let words: Vec<String> = command.iter().map(|word| shell_word(word)).collect();
                out.push_str(&format!("          {}\n", words.join(" ")));
            }
        }
    }
    let (met, unmet, warn) = report.counts();
    let skipped = report
        .requirements
        .iter()
        .filter(|requirement| requirement.status == Status::Skipped)
        .count();
    out.push_str(&format!(
        "\n{met} met, {unmet} not met, {warn} warnings, {skipped} skipped. {}\n",
        if report.ok() {
            "Bento can start."
        } else {
            "Bento cannot start until each FAIL line is corrected."
        }
    ));
    out
}

#[cfg(test)]
mod tests {
    use bento_preflight::{Fix, Requirement};

    use super::*;

    #[test]
    fn flags_accept_json_and_online_and_refuse_the_rest() {
        let args = |words: &[&str]| words.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(parse_flags(&args(&[])).unwrap(), Flags::default());
        assert_eq!(
            parse_flags(&args(&["-json", "--online"])).unwrap(),
            Flags {
                json: true,
                online: true
            }
        );
        assert!(parse_flags(&args(&["-fix"])).is_err());
    }

    #[test]
    fn the_human_report_groups_lines_and_shows_fixes_for_failures() {
        let requirement = |id: &str, group, status, fix: Option<Fix>| Requirement {
            id: id.into(),
            group,
            title: id.into(),
            status,
            fatal: true,
            detail: format!("{id} detail"),
            fix,
        };
        let fix = Fix {
            summary: "Install the xorriso package.".into(),
            commands: vec![vec![
                "dnf".into(),
                "install".into(),
                "-y".into(),
                "xorriso".into(),
            ]],
            root: true,
        };
        let report = Report {
            role: "controller".into(),
            requirements: vec![
                requirement("kvm-device", Group::Host, Status::Met, None),
                requirement("xorriso", Group::Host, Status::Unmet, Some(fix.clone())),
                requirement("dns", Group::Config, Status::Skipped, None),
            ],
        };
        let text = render(&report);
        assert!(text.contains("\nHost\n  ok    kvm-device"), "{text}");
        assert!(text.contains("  FAIL  xorriso"), "{text}");
        assert!(
            text.contains("        fix (as root): Install the xorriso package.\n          dnf install -y xorriso\n"),
            "{text}"
        );
        assert!(text.contains("\nConfiguration\n  skip  dns"), "{text}");
        assert!(text.ends_with("1 met, 1 not met, 0 warnings, 1 skipped. Bento cannot start until each FAIL line is corrected.\n"), "{text}");
        assert_eq!(text.matches("fix").count(), 1);
    }

    #[test]
    fn shell_words_quote_only_what_needs_it() {
        assert_eq!(shell_word("net.ipv4.ip_forward=1"), "net.ipv4.ip_forward=1");
        assert_eq!(
            shell_word("/etc/bento/my file.toml"),
            "'/etc/bento/my file.toml'"
        );
        assert_eq!(shell_word("it's"), r"'it'\''s'");
    }

    #[test]
    fn a_configuration_that_does_not_load_fails_the_check() {
        let probe = bento_preflight::FakeProbe::healthy();
        let mut out = Vec::new();
        let status = run_check_with(
            Path::new("/nonexistent/bento.toml"),
            &[],
            &probe,
            "bentod",
            &mut out,
        );
        assert_eq!(status, 1);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("FAIL  Configuration loads"), "{text}");

        let mut out = Vec::new();
        let status = run_check_with(
            Path::new("/nonexistent/bento.toml"),
            &[OsString::from("-x")],
            &probe,
            "bentod",
            &mut out,
        );
        assert_eq!(status, 2);
        assert!(out.is_empty());
    }

    #[test]
    fn fix_commands_name_the_running_binary_and_an_absolute_config() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("bento.toml");
        std::fs::write(
            &config,
            "base_domain = \"example.org\"\n",
        )
        .unwrap();
        // No image file yet, so images-fetched offers fetch-images.
        let mut probe = bento_preflight::FakeProbe::healthy();
        probe
            .dirs
            .insert(bento_config::defaults::IMAGE_DIR.into(), Vec::new());
        let mut out = Vec::new();
        run_check_with(
            &config,
            &[OsString::from("-json")],
            &probe,
            "/opt/bento/bentod",
            &mut out,
        );
        let report: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let fetch = report["requirements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|requirement| requirement["id"] == "images-fetched")
            .unwrap();
        assert_eq!(
            fetch["fix"]["commands"][0],
            serde_json::json!([
                "/opt/bento/bentod",
                "-config",
                config.display().to_string(),
                "fetch-images"
            ])
        );

        assert_eq!(fix_config_path(Path::new(crate::DEFAULT_CONFIG_PATH)), None);
        let relative = fix_config_path(Path::new("some/bento.toml")).unwrap();
        assert!(relative.is_absolute(), "{}", relative.display());
        assert!(relative.ends_with("some/bento.toml"));
    }
}
