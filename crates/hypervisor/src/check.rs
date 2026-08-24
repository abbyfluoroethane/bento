use std::fs;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::DEFAULT_SOCKET_PATH;

const DEFAULT_KVM_PATH: &str = "/dev/kvm";
const DEFAULT_KSM_RUN_PATH: &str = "/sys/kernel/mm/ksm/run";
const DEFAULT_NESTED_PATHS: [&str; 2] = [
    "/sys/module/kvm_intel/parameters/nested",
    "/sys/module/kvm_amd/parameters/nested",
];

/// The kind of filesystem entry returned by [`CheckDeps::stat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
}

/// Names the host paths probed by the requirement checks (SPEC 4.2).
/// Empty fields use the host defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckConfig {
    pub kvm_path: PathBuf,
    pub socket_path: PathBuf,
    pub image_dir: PathBuf,
    pub storage_dir: PathBuf,
    /// Rootful Podman storage used by bootc image builds.
    pub container_storage: PathBuf,
    /// Missing bootc dependencies are fatal only when OCI images are configured.
    pub podman_required: bool,
    pub ksm_run_path: PathBuf,
    pub nested_paths: Vec<PathBuf>,
    /// The nested check only warns when at least one instance requests
    /// nested virtualization (SPEC 4.2 item 7).
    pub nested_wanted: bool,
}

impl CheckConfig {
    fn with_defaults(mut self) -> Self {
        if self.kvm_path.as_os_str().is_empty() {
            self.kvm_path = DEFAULT_KVM_PATH.into();
        }
        if self.socket_path.as_os_str().is_empty() {
            self.socket_path = DEFAULT_SOCKET_PATH.into();
        }
        if self.ksm_run_path.as_os_str().is_empty() {
            self.ksm_run_path = DEFAULT_KSM_RUN_PATH.into();
        }
        if self.nested_paths.is_empty() {
            self.nested_paths = DEFAULT_NESTED_PATHS.iter().map(PathBuf::from).collect();
        }
        self
    }
}

pub type StatFn = Arc<dyn Fn(&Path) -> io::Result<FileKind> + Send + Sync>;
pub type ReadFileFn = Arc<dyn Fn(&Path) -> io::Result<Vec<u8>> + Send + Sync>;
pub type LookPathFn = Arc<dyn Fn(&str) -> io::Result<PathBuf> + Send + Sync>;
pub type PathOpFn = Arc<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;

/// Host-touching operations injected so requirement checks unit-test on
/// any machine.
pub struct CheckDeps {
    pub stat: StatFn,
    pub read_file: ReadFileFn,
    pub look_path: LookPathFn,
    pub write_probe: PathOpFn,
    /// Verifies that libvirtd answers on its Unix socket.
    pub ping_libvirt: PathOpFn,
}

/// Returns requirement-check dependencies backed by the real host.
pub fn default_check_deps() -> CheckDeps {
    CheckDeps {
        stat: Arc::new(|path| {
            let metadata = fs::metadata(path)?;
            Ok(if metadata.is_dir() {
                FileKind::Directory
            } else {
                FileKind::File
            })
        }),
        read_file: Arc::new(|path| fs::read(path)),
        look_path: Arc::new(find_on_path),
        write_probe: Arc::new(|directory| {
            let probe = directory.join(".bento-write-probe");
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&probe)?;
            fs::remove_file(probe)
        }),
        ping_libvirt: Arc::new(|path| UnixStream::connect(path).map(drop)),
    }
}

fn find_on_path(name: &str) -> io::Result<PathBuf> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "PATH is not set"))?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if let Ok(metadata) = fs::metadata(&candidate)
            && metadata.is_file()
            && metadata.permissions().mode() & 0o111 != 0
        {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{name} not found in PATH"),
    ))
}

/// One line of the host requirement report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub name: String,
    pub ok: bool,
    /// A failed fatal check refuses startup (SPEC 4.2).
    pub fatal: bool,
    pub detail: String,
}

/// The outcome of every host requirement check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    pub results: Vec<CheckResult>,
}

impl CheckReport {
    /// Reports whether no fatal check failed. Warnings do not block startup.
    pub fn ok(&self) -> bool {
        self.results.iter().all(|result| !result.fatal || result.ok)
    }

    /// Returns the non-fatal checks that failed.
    pub fn warnings(&self) -> Vec<&CheckResult> {
        self.results
            .iter()
            .filter(|result| !result.fatal && !result.ok)
            .collect()
    }
}

fn result(name: &str, fatal: bool, outcome: io::Result<()>, ok_detail: String) -> CheckResult {
    match outcome {
        Ok(()) => CheckResult {
            name: name.to_string(),
            ok: true,
            fatal,
            detail: ok_detail,
        },
        Err(error) => CheckResult {
            name: name.to_string(),
            ok: false,
            fatal,
            detail: error.to_string(),
        },
    }
}

/// Runs the host requirement checks in SPEC 4.2. Items 1 through 5 are
/// fatal; KSM and nested-virtualization failures warn.
pub fn check(config: CheckConfig, deps: &CheckDeps) -> CheckReport {
    let config = config.with_defaults();
    let mut report = CheckReport::default();

    report.results.push(result(
        "kvm device",
        true,
        (deps.stat)(&config.kvm_path).map(drop),
        config.kvm_path.display().to_string(),
    ));
    report.results.push(result(
        "libvirtd socket",
        true,
        (deps.ping_libvirt)(&config.socket_path),
        config.socket_path.display().to_string(),
    ));
    for binary in ["qemu-img", "xorriso"] {
        let found = (deps.look_path)(binary);
        let detail = found
            .as_ref()
            .map_or_else(|_| String::new(), |path| path.display().to_string());
        report.results.push(result(
            &format!("{binary} binary"),
            true,
            found.map(drop),
            detail,
        ));
    }

    let podman = (deps.look_path)("podman");
    let podman_detail = podman
        .as_ref()
        .map_or_else(|_| String::new(), |path| path.display().to_string());
    report.results.push(result(
        "podman binary",
        config.podman_required,
        podman.map(drop),
        podman_detail,
    ));

    for (name, directory) in [
        ("image directory", &config.image_dir),
        ("storage directory", &config.storage_dir),
    ] {
        let stat = (deps.stat)(directory).and_then(|kind| {
            if kind == FileKind::Directory {
                Ok(())
            } else {
                Err(io::Error::other(format!(
                    "{} is not a directory",
                    directory.display()
                )))
            }
        });
        let exists = stat.is_ok();
        report
            .results
            .push(result(name, true, stat, directory.display().to_string()));
        if exists {
            report.results.push(result(
                &format!("{name} writable"),
                true,
                (deps.write_probe)(directory),
                directory.display().to_string(),
            ));
        }
    }

    if !config.container_storage.as_os_str().is_empty() {
        let directory = &config.container_storage;
        let stat = (deps.stat)(directory).and_then(|kind| {
            if kind == FileKind::Directory {
                Ok(())
            } else {
                Err(io::Error::other(format!(
                    "{} is not a directory",
                    directory.display()
                )))
            }
        });
        let exists = stat.is_ok();
        report.results.push(result(
            "container storage directory",
            config.podman_required,
            stat,
            directory.display().to_string(),
        ));
        if exists {
            report.results.push(result(
                "container storage directory writable",
                config.podman_required,
                (deps.write_probe)(directory),
                directory.display().to_string(),
            ));
        }
    }

    report.results.push(check_ksm(&config, deps));
    if config.nested_wanted {
        report.results.push(check_nested(&config, deps));
    }
    report
}

fn check_ksm(config: &CheckConfig, deps: &CheckDeps) -> CheckResult {
    let raw = match (deps.read_file)(&config.ksm_run_path) {
        Ok(raw) => raw,
        Err(error) => {
            return CheckResult {
                name: "ksm run".to_string(),
                ok: false,
                fatal: false,
                detail: format!("read {}: {error}", config.ksm_run_path.display()),
            };
        }
    };
    let value = String::from_utf8_lossy(&raw);
    let value = value.trim();
    if value == "0" {
        return CheckResult {
            name: "ksm run".to_string(),
            ok: false,
            fatal: false,
            detail: format!(
                "{} is 0; set it to 1 or run ksmtuned (SPEC 5.4)",
                config.ksm_run_path.display()
            ),
        };
    }
    CheckResult {
        name: "ksm run".to_string(),
        ok: true,
        fatal: false,
        detail: format!("{} = {value}", config.ksm_run_path.display()),
    }
}

/// Reports whether the first existing KVM nested parameter is enabled.
/// The lifecycle layer uses this to reject `nested=true` when the host
/// cannot provide it (SPEC 5.5).
pub fn nested_enabled(config: CheckConfig, deps: &CheckDeps) -> (bool, Option<PathBuf>) {
    let config = config.with_defaults();
    for path in config.nested_paths {
        if let Ok(raw) = (deps.read_file)(&path) {
            let value = String::from_utf8_lossy(&raw);
            let value = value.trim();
            return (value == "1" || value.eq_ignore_ascii_case("Y"), Some(path));
        }
    }
    (false, None)
}

fn check_nested(config: &CheckConfig, deps: &CheckDeps) -> CheckResult {
    let (enabled, path) = nested_enabled(config.clone(), deps);
    match (enabled, path) {
        (true, Some(path)) => CheckResult {
            name: "nested virtualization".to_string(),
            ok: true,
            fatal: false,
            detail: format!("{} is on", path.display()),
        },
        (false, Some(path)) => CheckResult {
            name: "nested virtualization".to_string(),
            ok: false,
            fatal: false,
            detail: format!(
                "{} is off; instances request nested virtualization (set kvm_intel.nested=1 or kvm_amd.nested=1)",
                path.display()
            ),
        },
        (_, None) => CheckResult {
            name: "nested virtualization".to_string(),
            ok: false,
            fatal: false,
            detail: "no kvm nested parameter found; load kvm_intel.nested=1 or kvm_amd.nested=1"
                .to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn healthy_deps() -> CheckDeps {
        CheckDeps {
            stat: Arc::new(|path| match path.to_str().unwrap() {
                "/dev/kvm" => Ok(FileKind::File),
                "/img" | "/store" | "/containers" => Ok(FileKind::Directory),
                _ => Err(io::Error::from(io::ErrorKind::NotFound)),
            }),
            read_file: Arc::new(|path| match path.to_str().unwrap() {
                "/sys/kernel/mm/ksm/run" => Ok(b"1\n".to_vec()),
                "/sys/module/kvm_intel/parameters/nested" => Ok(b"Y\n".to_vec()),
                _ => Err(io::Error::from(io::ErrorKind::NotFound)),
            }),
            look_path: Arc::new(|name| Ok(PathBuf::from(format!("/usr/bin/{name}")))),
            write_probe: Arc::new(|_| Ok(())),
            ping_libvirt: Arc::new(|_| Ok(())),
        }
    }

    fn healthy_config() -> CheckConfig {
        CheckConfig {
            image_dir: "/img".into(),
            storage_dir: "/store".into(),
            container_storage: "/containers".into(),
            ..Default::default()
        }
    }

    fn named<'a>(report: &'a CheckReport, name: &str) -> &'a CheckResult {
        report
            .results
            .iter()
            .find(|result| result.name == name)
            .unwrap()
    }

    #[test]
    fn healthy_host_passes_every_check() {
        let report = check(healthy_config(), &healthy_deps());
        assert!(report.ok(), "{:?}", report.results);
        assert!(report.warnings().is_empty());
        for name in [
            "kvm device",
            "libvirtd socket",
            "qemu-img binary",
            "xorriso binary",
            "podman binary",
            "image directory",
            "image directory writable",
            "storage directory",
            "storage directory writable",
            "container storage directory",
            "container storage directory writable",
            "ksm run",
        ] {
            assert!(named(&report, name).ok, "{name} failed");
        }
    }

    #[test]
    fn fatal_host_failures_refuse_startup() {
        for case in [
            "missing kvm",
            "libvirt down",
            "xorriso missing",
            "storage missing",
            "image unwritable",
        ] {
            let mut deps = healthy_deps();
            let failed = match case {
                "missing kvm" => {
                    let stat = deps.stat.clone();
                    deps.stat = Arc::new(move |path| {
                        if path == Path::new("/dev/kvm") {
                            Err(io::Error::from(io::ErrorKind::NotFound))
                        } else {
                            stat(path)
                        }
                    });
                    "kvm device"
                }
                "libvirt down" => {
                    deps.ping_libvirt = Arc::new(|_| Err(io::Error::other("connection refused")));
                    "libvirtd socket"
                }
                "xorriso missing" => {
                    deps.look_path = Arc::new(|name| {
                        if name == "xorriso" {
                            Err(io::Error::from(io::ErrorKind::NotFound))
                        } else {
                            Ok(PathBuf::from(format!("/usr/bin/{name}")))
                        }
                    });
                    "xorriso binary"
                }
                "storage missing" => {
                    let stat = deps.stat.clone();
                    deps.stat = Arc::new(move |path| {
                        if path == Path::new("/store") {
                            Err(io::Error::from(io::ErrorKind::NotFound))
                        } else {
                            stat(path)
                        }
                    });
                    "storage directory"
                }
                "image unwritable" => {
                    deps.write_probe = Arc::new(|path| {
                        if path == Path::new("/img") {
                            Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "permission denied",
                            ))
                        } else {
                            Ok(())
                        }
                    });
                    "image directory writable"
                }
                _ => unreachable!(),
            };
            let report = check(healthy_config(), &deps);
            assert!(!report.ok(), "{case} must refuse startup");
            let result = named(&report, failed);
            assert!(!result.ok && result.fatal, "{case}: {result:?}");
        }
    }

    #[test]
    fn missing_directory_skips_write_probe() {
        let mut deps = healthy_deps();
        let stat = deps.stat.clone();
        deps.stat = Arc::new(move |path| {
            if path == Path::new("/img") {
                Err(io::Error::from(io::ErrorKind::NotFound))
            } else {
                stat(path)
            }
        });
        let probed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = probed.clone();
        deps.write_probe = Arc::new(move |path| {
            if path == Path::new("/img") {
                observed.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(())
        });
        check(healthy_config(), &deps);
        assert!(!probed.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn bootc_dependencies_warn_until_an_oci_image_requires_them() {
        let mut deps = healthy_deps();
        let stat = deps.stat.clone();
        deps.stat = Arc::new(move |path| {
            if path == Path::new("/containers") {
                Err(io::Error::from(io::ErrorKind::NotFound))
            } else {
                stat(path)
            }
        });
        deps.look_path = Arc::new(|name| {
            if name == "podman" {
                Err(io::Error::from(io::ErrorKind::NotFound))
            } else {
                Ok(PathBuf::from(format!("/usr/bin/{name}")))
            }
        });

        let report = check(healthy_config(), &deps);
        assert!(report.ok());
        assert_eq!(report.warnings().len(), 2);

        let mut required = healthy_config();
        required.podman_required = true;
        let report = check(required, &deps);
        assert!(!report.ok());
        assert!(named(&report, "podman binary").fatal);
        assert!(named(&report, "container storage directory").fatal);
    }

    #[test]
    fn ksm_off_is_only_a_warning() {
        let mut deps = healthy_deps();
        deps.read_file = Arc::new(|path| {
            if path == Path::new(DEFAULT_KSM_RUN_PATH) {
                Ok(b"0\n".to_vec())
            } else {
                Err(io::Error::from(io::ErrorKind::NotFound))
            }
        });
        let report = check(healthy_config(), &deps);
        assert!(report.ok());
        let warnings = report.warnings();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].name, "ksm run");
    }

    #[test]
    fn nested_check_runs_only_when_wanted_and_warns_when_off() {
        for (wanted, value, read_error, checked, warns) in [
            (false, "0", false, false, false),
            (true, "Y", false, true, false),
            (true, "1", false, true, false),
            (true, "0", false, true, true),
            (true, "", true, true, true),
        ] {
            let mut deps = healthy_deps();
            let value = value.to_string();
            deps.read_file = Arc::new(move |path| {
                let path = path.to_string_lossy();
                if path == DEFAULT_KSM_RUN_PATH {
                    return Ok(b"1".to_vec());
                }
                if path.contains("nested") && !read_error && path.contains("kvm_intel") {
                    return Ok(format!("{value}\n").into_bytes());
                }
                Err(io::Error::from(io::ErrorKind::NotFound))
            });
            let mut config = healthy_config();
            config.nested_wanted = wanted;
            let report = check(config, &deps);
            let nested = report
                .results
                .iter()
                .find(|result| result.name == "nested virtualization");
            assert_eq!(nested.is_some(), checked);
            if let Some(nested) = nested {
                assert!(!nested.fatal);
                assert_eq!(!nested.ok, warns);
                if warns {
                    assert!(nested.detail.contains("nested=1"));
                }
            }
        }
    }

    #[test]
    fn nested_enabled_reads_the_first_existing_parameter() {
        let cases = [
            (
                HashMap::from([(DEFAULT_NESTED_PATHS[0], "Y\n")]),
                true,
                Some(DEFAULT_NESTED_PATHS[0]),
            ),
            (
                HashMap::from([(DEFAULT_NESTED_PATHS[1], "1")]),
                true,
                Some(DEFAULT_NESTED_PATHS[1]),
            ),
            (
                HashMap::from([(DEFAULT_NESTED_PATHS[0], "0\n")]),
                false,
                Some(DEFAULT_NESTED_PATHS[0]),
            ),
            (
                HashMap::from([(DEFAULT_NESTED_PATHS[1], "N")]),
                false,
                Some(DEFAULT_NESTED_PATHS[1]),
            ),
            (HashMap::new(), false, None),
        ];
        for (files, expected_enabled, expected_path) in cases {
            let deps = CheckDeps {
                stat: Arc::new(|_| unreachable!()),
                read_file: Arc::new(move |path| {
                    files
                        .get(path.to_str().unwrap())
                        .map(|value| value.as_bytes().to_vec())
                        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
                }),
                look_path: Arc::new(|_| unreachable!()),
                write_probe: Arc::new(|_| unreachable!()),
                ping_libvirt: Arc::new(|_| unreachable!()),
            };
            let (enabled, path) = nested_enabled(CheckConfig::default(), &deps);
            assert_eq!(enabled, expected_enabled);
            assert_eq!(path.as_deref(), expected_path.map(Path::new));
        }
    }
}
