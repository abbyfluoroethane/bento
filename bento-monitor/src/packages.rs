//! The host programs `bentod serve` needs before it starts
//! (DEPLOYING.md section 1, SPEC 4.2), and the one command that installs
//! them on Fedora.
//!
//! Only Fedora and its relatives get a command. The package names below
//! are Fedora's, and a command with the wrong names for another
//! distribution fails halfway through an install the operator trusted.
//! Every other host gets the list of what is missing instead.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::run::Cmd;

/// The programs every Bento host needs, by the name they have on `PATH`,
/// with the Fedora package that supplies each one. `qemu-img` and
/// `xorriso` are SPEC 4.2 startup checks. `nft` is not checked at
/// startup, but `serve` stops when it cannot load its table
/// (DEPLOYING.md 1).
pub const TOOLS: [(&str, &str); 3] = [
    ("qemu-img", "qemu-img"),
    ("xorriso", "xorriso"),
    ("nft", "nftables"),
];

/// What the screen calls the libvirt daemon when neither is installed.
pub const DAEMON_NAME: &str = "virtqemud or libvirtd";
/// The Fedora package of the modular daemons (DEPLOYING.md 1).
pub const DAEMON_PACKAGE: &str = "libvirt-daemon-kvm";
/// Rootful Podman is needed only for a bootc OCI entry (DEPLOYING.md 1).
pub const PODMAN: &str = "podman";

/// Directories searched after `PATH`. `sudo` and a login shell of a
/// normal user often leave the sbin directories out of `PATH`, and that
/// is where Fedora puts the libvirt daemons and, before Fedora 42, `nft`.
const SBIN: [&str; 2] = ["/usr/sbin", "/sbin"];

/// Which libvirt daemon the host has. The two need different socket
/// units, and only the monolithic one supplies the socket that Bento
/// uses by default (DEPLOYING.md 1, "Modular libvirt daemons").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibvirtDaemon {
    /// The modular daemons of current Fedora.
    Virtqemud,
    /// The monolithic daemon.
    Libvirtd,
}

/// What `/etc/os-release` says about the host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OsRelease {
    /// The `ID` value, or empty when the file does not say.
    pub id: String,
    /// `ID` is `fedora`, or `ID_LIKE` names `fedora` or `rhel`. Only such
    /// a host has the package names of [`TOOLS`].
    pub fedora: bool,
}

impl OsRelease {
    /// Reads the text of an os-release file. Values can be quoted with
    /// single or double quotes (os-release(5)). The Fedora test is the
    /// one of the preflight, so the Install and Requirements tabs agree
    /// on which hosts get a `dnf` command.
    pub fn parse(text: &str) -> OsRelease {
        // The last ID line wins, as when a shell sources the file.
        let id = text
            .lines()
            .rev()
            .filter_map(|line| line.trim().split_once('='))
            .find(|(key, _)| *key == "ID")
            .map(|(_, value)| {
                value
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_string()
            })
            .unwrap_or_default();
        OsRelease {
            id,
            fedora: bento_preflight::fedora_like(text),
        }
    }

    /// Reads this host's file. `/usr/lib/os-release` is the fallback the
    /// specification names for a host without the `/etc` copy.
    pub fn read() -> OsRelease {
        ["/etc/os-release", "/usr/lib/os-release"]
            .iter()
            .find_map(|path| std::fs::read_to_string(path).ok())
            .map(|text| OsRelease::parse(&text))
            .unwrap_or_default()
    }
}

/// Where `name` is, if it is an executable file on `path` or in an sbin
/// directory. `is_executable` is the filesystem test, so a test can
/// answer it without the host.
pub fn find_program(
    name: &str,
    path: Option<&OsStr>,
    is_executable: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let mut directories: Vec<PathBuf> = path
        .map(|value| std::env::split_paths(value).collect())
        .unwrap_or_default();
    directories.extend(SBIN.iter().map(PathBuf::from));
    directories
        .into_iter()
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

/// The filesystem test of [`find_program`] on this host.
pub fn is_executable(path: &Path) -> bool {
    path.metadata()
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// What the packages step found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    /// The programs that are not there, by the names of [`TOOLS`],
    /// [`DAEMON_NAME`], and [`PODMAN`].
    pub missing: Vec<String>,
    /// The libvirt daemon, preferring the modular one when both are
    /// installed, because that is what current Fedora runs.
    pub daemon: Option<LibvirtDaemon>,
}

/// Looks for every program the host needs. `podman` is looked for only
/// when the configuration has an OCI entry.
pub fn find(podman: bool, lookup: &dyn Fn(&str) -> bool) -> Found {
    let mut missing: Vec<String> = TOOLS
        .iter()
        .filter(|(program, _)| !lookup(program))
        .map(|(program, _)| (*program).to_string())
        .collect();
    let daemon = if lookup("virtqemud") {
        Some(LibvirtDaemon::Virtqemud)
    } else if lookup("libvirtd") {
        Some(LibvirtDaemon::Libvirtd)
    } else {
        None
    };
    if daemon.is_none() {
        missing.push(DAEMON_NAME.to_string());
    }
    if podman && !lookup(PODMAN) {
        missing.push(PODMAN.to_string());
    }
    Found { missing, daemon }
}

/// The Fedora packages of a host, in install order.
pub fn fedora_packages(podman: bool) -> Vec<String> {
    let mut packages: Vec<String> = TOOLS
        .iter()
        .map(|(_, package)| (*package).to_string())
        .collect();
    packages.push(DAEMON_PACKAGE.to_string());
    if podman {
        packages.push(PODMAN.to_string());
    }
    packages
}

/// One `dnf install` for every package. Packages that are already
/// installed cost nothing, and one transaction means one password
/// prompt and one resolver run.
pub fn dnf_install(podman: bool, euid: u32) -> Cmd {
    let mut args = vec!["install".to_string(), "-y".to_string()];
    args.extend(fedora_packages(podman));
    Cmd::owned("dnf", args).privileged(euid)
}

/// What an operator on another distribution must install for the
/// programs in `missing`, in words that do not assume Fedora's package
/// names.
pub fn other_packages(missing: &[String]) -> String {
    missing
        .iter()
        .map(|program| match program.as_str() {
            "nft" => "nftables".to_string(),
            DAEMON_NAME => format!("libvirt ({DAEMON_NAME})"),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every program a host needs, as [`find`] names them when none is
/// installed.
pub fn all_programs(podman: bool) -> Vec<String> {
    find(podman, &|_| false).missing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fedora_and_its_relatives_read_as_fedora() {
        let fedora = OsRelease::parse("NAME=\"Fedora Linux\"\nID=fedora\nVERSION_ID=44\n");
        assert_eq!(fedora.id, "fedora");
        assert!(fedora.fedora);
        let alma = OsRelease::parse("ID=\"almalinux\"\nID_LIKE=\"rhel centos fedora\"\n");
        assert!(alma.fedora);
        let rocky = OsRelease::parse("ID='rocky'\nID_LIKE='rhel'\n");
        assert!(rocky.fedora);
        let debian = OsRelease::parse("ID=debian\n");
        assert_eq!(debian.id, "debian");
        assert!(!debian.fedora);
        assert!(!OsRelease::parse("").fedora);
    }

    #[test]
    fn a_program_is_found_on_the_path_or_in_sbin() {
        let present = |path: &Path| {
            path == Path::new("/opt/bin/qemu-img") || path == Path::new("/usr/sbin/virtqemud")
        };
        let path = OsStr::new("/usr/bin:/opt/bin");
        assert_eq!(
            find_program("qemu-img", Some(path), &present),
            Some(PathBuf::from("/opt/bin/qemu-img"))
        );
        // Not on PATH, but where Fedora puts the daemons.
        assert_eq!(
            find_program("virtqemud", Some(path), &present),
            Some(PathBuf::from("/usr/sbin/virtqemud"))
        );
        assert_eq!(find_program("xorriso", None, &present), None);
    }

    #[test]
    fn a_missing_daemon_and_podman_are_named() {
        let found = find(true, &|name| name == "qemu-img" || name == "nft");
        assert_eq!(found.missing, vec!["xorriso", DAEMON_NAME, PODMAN]);
        assert_eq!(found.daemon, None);

        // Podman is not looked for without an OCI entry.
        let found = find(false, &|name| name != "podman" && name != "virtqemud");
        assert!(found.missing.is_empty(), "{found:?}");
        assert_eq!(found.daemon, Some(LibvirtDaemon::Libvirtd));
    }

    #[test]
    fn the_modular_daemon_wins_when_both_are_installed() {
        let found = find(false, &|_| true);
        assert_eq!(found.daemon, Some(LibvirtDaemon::Virtqemud));
    }

    #[test]
    fn another_distribution_is_told_the_packages_in_neutral_words() {
        assert_eq!(
            other_packages(&all_programs(true)),
            "qemu-img, xorriso, nftables, libvirt (virtqemud or libvirtd), podman"
        );
        assert_eq!(other_packages(&["nft".to_string()]), "nftables");
    }

    #[test]
    fn one_dnf_transaction_installs_every_package() {
        assert_eq!(
            dnf_install(false, 1000).display(),
            "sudo dnf install -y qemu-img xorriso nftables libvirt-daemon-kvm"
        );
        assert_eq!(
            dnf_install(true, 0).display(),
            "dnf install -y qemu-img xorriso nftables libvirt-daemon-kvm podman"
        );
    }
}
