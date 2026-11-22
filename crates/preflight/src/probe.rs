//! Every host touch of the preflight, behind one trait, with the real
//! host and an in-memory fake.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bento_hypervisor::{CheckDeps, FileKind};

/// The ruleset that `nft -c` parses. Check mode needs the same netlink
/// access as a real load, so it answers the permission question without
/// a change to the ruleset (DEPLOYING.md section 1).
pub const NFT_PROBE_RULESET: &str = "add table inet bento_preflight\n";

/// What `nft -c -f -` said about [`NFT_PROBE_RULESET`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NftOutcome {
    /// Exit status 0.
    #[default]
    Permitted,
    /// `nft` is not on `PATH`.
    Missing,
    /// A non-zero exit, with the first line of standard error.
    Refused(String),
}

/// The process that holds a listening TCP port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    /// `/proc/<pid>/comm`, which the kernel cuts to 15 bytes.
    pub comm: String,
}

/// Every touch of the host that the preflight makes. None changes a host
/// setting: a bind closes at once, `nft` runs in check mode, and the write
/// probe of [`Probe::check_deps`] deletes the file it makes.
pub trait Probe: Send + Sync {
    /// The hooks of the SPEC 4.2 host checks, which this crate reuses
    /// from `bento-hypervisor`. `look_path` also answers for `nft`.
    fn check_deps(&self) -> CheckDeps;
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn exists(&self, path: &Path) -> bool;
    /// The entry names of a directory.
    fn list_dir(&self, path: &Path) -> io::Result<Vec<String>>;
    /// Runs `nft -c -f -` with [`NFT_PROBE_RULESET`] on standard input.
    fn nft_check(&self) -> NftOutcome;
    /// Binds a TCP listener and closes it at once.
    fn bind(&self, addr: SocketAddr) -> io::Result<()>;
    /// The holder of each listening TCP port that this process can see.
    /// Best effort: the preflight calls it at most once per run, because
    /// it reads every file descriptor of every process.
    fn port_holders(&self) -> HashMap<u16, Holder>;
    /// A GET with a timeout. Returns the status and the body.
    fn http_get(&self, url: &str, timeout: Duration) -> Result<(u16, Vec<u8>), String>;
    /// Resolves a host name through the system resolver.
    fn resolve(&self, host: &str) -> Result<(), String>;
    /// The CPU architecture, as `std::env::consts::ARCH` names it.
    fn arch(&self) -> String;
}

/// The real host.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostProbe;

impl Probe for HostProbe {
    fn check_deps(&self) -> CheckDeps {
        bento_hypervisor::default_check_deps()
    }

    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn exists(&self, path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok()
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<String>> {
        fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect()
    }

    fn nft_check(&self) -> NftOutcome {
        let child = Command::new("nft")
            .args(["-c", "-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return NftOutcome::Missing,
            Err(error) => return NftOutcome::Refused(format!("start nft: {error}")),
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(NFT_PROBE_RULESET.as_bytes());
        }
        match child.wait_with_output() {
            Ok(output) if output.status.success() => NftOutcome::Permitted,
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let line = stderr
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map_or_else(|| output.status.to_string(), str::to_string);
                NftOutcome::Refused(line)
            }
            Err(error) => NftOutcome::Refused(format!("wait for nft: {error}")),
        }
    }

    fn bind(&self, addr: SocketAddr) -> io::Result<()> {
        TcpListener::bind(addr).map(drop)
    }

    fn port_holders(&self) -> HashMap<u16, Holder> {
        let mut ports_by_inode = HashMap::new();
        for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
            if let Ok(text) = fs::read_to_string(table) {
                for (port, inode) in parse_listeners(&text) {
                    ports_by_inode.insert(inode, port);
                }
            }
        }
        let mut holders = HashMap::new();
        if ports_by_inode.is_empty() {
            return holders;
        }
        let Ok(processes) = fs::read_dir("/proc") else {
            return holders;
        };
        for process in processes.flatten() {
            let Some(pid) = process.file_name().to_str().and_then(|s| s.parse().ok()) else {
                continue;
            };
            // Another user's descriptors are unreadable without root.
            // Skip them: a holder the scan cannot name is still busy.
            let Ok(descriptors) = fs::read_dir(process.path().join("fd")) else {
                continue;
            };
            for descriptor in descriptors.flatten() {
                let Some(inode) = fs::read_link(descriptor.path())
                    .ok()
                    .and_then(|target| parse_socket_link(&target.to_string_lossy()))
                else {
                    continue;
                };
                if let Some(port) = ports_by_inode.get(&inode) {
                    let comm = fs::read_to_string(process.path().join("comm"))
                        .map(|comm| comm.trim().to_string())
                        .unwrap_or_default();
                    holders.entry(*port).or_insert(Holder { pid, comm });
                }
            }
        }
        holders
    }

    fn http_get(&self, url: &str, timeout: Duration) -> Result<(u16, Vec<u8>), String> {
        // A caller may already run inside a Tokio runtime, where a second
        // runtime cannot block. A scoped thread owns a small runtime of its
        // own, so this stays a plain blocking call for every caller.
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _ = rustls::crypto::ring::default_provider().install_default();
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| error.to_string())?;
                    runtime.block_on(async {
                        let client = reqwest::Client::builder()
                            .timeout(timeout)
                            .build()
                            .map_err(|error| error.to_string())?;
                        let response = client
                            .get(url)
                            .send()
                            .await
                            .map_err(|error| error_chain(&error))?;
                        let status = response.status().as_u16();
                        let body = response
                            .bytes()
                            .await
                            .map_err(|error| error_chain(&error))?;
                        Ok((status, body.to_vec()))
                    })
                })
                .join()
                .unwrap_or_else(|_| Err("the HTTP probe thread panicked".to_string()))
        })
    }

    fn resolve(&self, host: &str) -> Result<(), String> {
        let mut addresses = (host, 443).to_socket_addrs().map_err(|e| e.to_string())?;
        match addresses.next() {
            Some(_) => Ok(()),
            None => Err("no address".to_string()),
        }
    }

    fn arch(&self) -> String {
        std::env::consts::ARCH.to_string()
    }
}

/// An error and each of its sources, joined with ": ". The top of a
/// reqwest error says only "error sending request"; the sources say
/// whether DNS, the connection, or TLS failed.
pub fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        // Some layers repeat the text of their source.
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// The local port and socket inode of each LISTEN row (state `0A`) of
/// `/proc/net/tcp` or `/proc/net/tcp6`. The port is hexadecimal, most
/// significant byte first, as the kernel prints it.
pub fn parse_listeners(text: &str) -> Vec<(u16, u64)> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 || fields[3] != "0A" {
                return None;
            }
            let (_, port) = fields[1].rsplit_once(':')?;
            let port = u16::from_str_radix(port, 16).ok()?;
            let inode = fields[9].parse().ok()?;
            Some((port, inode))
        })
        .collect()
}

/// The inode of a `/proc/<pid>/fd` link target such as `socket:[1234]`.
pub fn parse_socket_link(target: &str) -> Option<u64> {
    target
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// An in-memory host for tests, here and in `bento-monitor`.
///
/// Start from [`FakeProbe::healthy`], which meets every requirement for
/// a configuration with the default directories and socket, then change
/// the public fields. [`FakeProbe::default`] is an empty host on which
/// almost every requirement fails.
#[derive(Debug, Default)]
pub struct FakeProbe {
    /// File contents by path. A path here also exists.
    pub files: BTreeMap<PathBuf, Vec<u8>>,
    /// Directory entries by path. A path here exists and is a directory.
    pub dirs: BTreeMap<PathBuf, Vec<String>>,
    /// Directories whose write probe fails.
    pub unwritable: BTreeSet<PathBuf>,
    /// Binary names on `PATH`.
    pub binaries: BTreeSet<String>,
    /// Unix sockets that answer. A path here also exists.
    pub sockets: BTreeSet<PathBuf>,
    pub nft: NftOutcome,
    /// Bind errors by port. A port not here binds.
    pub bind_errors: BTreeMap<u16, io::ErrorKind>,
    pub holders: BTreeMap<u16, Holder>,
    /// Responses by URL. A URL not here fails to connect.
    pub http: BTreeMap<String, Result<(u16, String), String>>,
    /// Host names that resolve.
    pub resolvable: BTreeSet<String>,
    pub arch: String,
    holder_scans: AtomicUsize,
}

impl FakeProbe {
    /// A host that meets every requirement for a configuration with the
    /// default `image_dir`, `storage_dir`, container storage, and libvirt
    /// socket, run as root outside a container.
    pub fn healthy() -> Self {
        let mut probe = FakeProbe {
            arch: "x86_64".to_string(),
            ..Default::default()
        };
        for (path, content) in [
            ("/dev/kvm", ""),
            ("/sys/kernel/mm/ksm/run", "1\n"),
            ("/sys/module/kvm_intel/parameters/nested", "Y\n"),
            ("/etc/os-release", "NAME=\"Fedora Linux\"\nID=fedora\n"),
            (
                "/proc/self/status",
                "Name:\tbentod\nUid:\t0\t0\t0\t0\nCapEff:\t000001ffffffffff\n",
            ),
            ("/proc/self/uid_map", "         0          0 4294967295\n"),
            ("/proc/sys/net/ipv4/ip_forward", "1\n"),
            (
                "/proc/self/limits",
                "Limit                     Soft Limit           Hard Limit           Units     \n\
                 Max open files            65536                524288               files     \n",
            ),
        ] {
            probe.files.insert(path.into(), content.as_bytes().to_vec());
        }
        probe.dirs.insert(
            bento_config::defaults::IMAGE_DIR.into(),
            vec![format!("sha256-{}.qcow2", "0".repeat(64))],
        );
        probe
            .dirs
            .insert(bento_config::defaults::STORAGE_DIR.into(), Vec::new());
        probe
            .dirs
            .insert(bento_config::defaults::CONTAINER_STORAGE.into(), Vec::new());
        probe
            .dirs
            .insert("/sys/class/net".into(), vec!["lo".into(), "eth0".into()]);
        for binary in ["qemu-img", "xorriso", "nft", "podman"] {
            probe.binaries.insert(binary.to_string());
        }
        probe
            .sockets
            .insert(bento_hypervisor::DEFAULT_SOCKET_PATH.into());
        probe
    }

    /// Sets a file, for chained setup.
    pub fn with_file(mut self, path: impl Into<PathBuf>, content: impl AsRef<[u8]>) -> Self {
        self.files.insert(path.into(), content.as_ref().to_vec());
        self
    }

    /// Removes a file, directory, socket, or binary, for chained setup.
    pub fn without(mut self, path_or_binary: &str) -> Self {
        self.files.remove(Path::new(path_or_binary));
        self.dirs.remove(Path::new(path_or_binary));
        self.sockets.remove(Path::new(path_or_binary));
        self.binaries.remove(path_or_binary);
        self
    }

    /// How many times [`Probe::port_holders`] ran.
    pub fn holder_scans(&self) -> usize {
        self.holder_scans.load(Ordering::Relaxed)
    }
}

impl Probe for FakeProbe {
    fn check_deps(&self) -> CheckDeps {
        let files: HashMap<PathBuf, Vec<u8>> = self.files.clone().into_iter().collect();
        let dirs: HashSet<PathBuf> = self.dirs.keys().cloned().collect();
        let sockets: HashSet<PathBuf> = self.sockets.iter().cloned().collect();
        let unwritable: HashSet<PathBuf> = self.unwritable.iter().cloned().collect();
        let binaries: HashSet<String> = self.binaries.iter().cloned().collect();
        let files = Arc::new(files);
        let stat_files = files.clone();
        CheckDeps {
            stat: Arc::new(move |path| {
                if dirs.contains(path) {
                    Ok(FileKind::Directory)
                } else if stat_files.contains_key(path) {
                    Ok(FileKind::File)
                } else {
                    Err(io::Error::from(io::ErrorKind::NotFound))
                }
            }),
            read_file: Arc::new(move |path| {
                files
                    .get(path)
                    .cloned()
                    .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
            }),
            look_path: Arc::new(move |name| {
                if binaries.contains(name) {
                    Ok(PathBuf::from(format!("/usr/bin/{name}")))
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("{name} not found in PATH"),
                    ))
                }
            }),
            write_probe: Arc::new(move |path| {
                if unwritable.contains(path) {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Permission denied (os error 13)",
                    ))
                } else {
                    Ok(())
                }
            }),
            ping_libvirt: Arc::new(move |path| {
                if sockets.contains(path) {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "No such file or directory (os error 2)",
                    ))
                }
            }),
        }
    }

    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    fn exists(&self, path: &Path) -> bool {
        self.files.contains_key(path) || self.dirs.contains_key(path) || self.sockets.contains(path)
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<String>> {
        self.dirs
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    fn nft_check(&self) -> NftOutcome {
        self.nft.clone()
    }

    fn bind(&self, addr: SocketAddr) -> io::Result<()> {
        match self.bind_errors.get(&addr.port()) {
            Some(kind) => Err(io::Error::from(*kind)),
            None => Ok(()),
        }
    }

    fn port_holders(&self) -> HashMap<u16, Holder> {
        self.holder_scans.fetch_add(1, Ordering::Relaxed);
        self.holders.clone().into_iter().collect()
    }

    fn http_get(&self, url: &str, _timeout: Duration) -> Result<(u16, Vec<u8>), String> {
        match self.http.get(url) {
            Some(Ok((status, body))) => Ok((*status, body.clone().into_bytes())),
            Some(Err(error)) => Err(error.clone()),
            None => Err("connection refused".to_string()),
        }
    }

    fn resolve(&self, host: &str) -> Result<(), String> {
        if self.resolvable.contains(host) {
            Ok(())
        } else {
            Err("failed to lookup address information: Name or service not known".to_string())
        }
    }

    fn arch(&self) -> String {
        self.arch.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listeners_come_from_listen_rows_only() {
        let text = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
           0: 0100007F:2760 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41234 1 0000000000000000 100 0 0 10 0\n\
           1: 0100007F:2760 0100007F:C350 01 00000000:00000000 00:00000000 00000000  1000        0 41299 1 0000000000000000 20 4 30 10 -1\n";
        assert_eq!(parse_listeners(text), vec![(0x2760, 41234)]);
    }

    #[test]
    fn tcp6_rows_carry_the_port_after_the_last_colon() {
        let text = "header\n\
           0: 00000000000000000000000000000000:2382 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 777 1 0000000000000000 100 0 0 10 0\n";
        assert_eq!(parse_listeners(text), vec![(9090, 777)]);
    }

    #[test]
    fn an_error_chain_names_every_source_once() {
        #[derive(Debug)]
        struct Layer(&'static str, Option<Box<Layer>>);
        impl std::fmt::Display for Layer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for Layer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.1.as_deref().map(|layer| layer as _)
            }
        }
        let error = Layer(
            "error sending request",
            Some(Box::new(Layer(
                "client error (Connect)",
                Some(Box::new(Layer(
                    "dns error",
                    Some(Box::new(Layer("dns error", None))),
                ))),
            ))),
        );
        assert_eq!(
            error_chain(&error),
            "error sending request: client error (Connect): dns error"
        );
    }

    #[test]
    fn socket_links_give_their_inode() {
        assert_eq!(parse_socket_link("socket:[41234]"), Some(41234));
        assert_eq!(parse_socket_link("pipe:[41234]"), None);
        assert_eq!(parse_socket_link("/dev/null"), None);
    }

    #[test]
    fn host_probe_bind_closes_the_port_at_once() {
        let probe = HostProbe;
        let address: SocketAddr = "127.0.0.1:0".parse().unwrap();
        probe.bind(address).unwrap();
        let held = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        let error = probe
            .bind(SocketAddr::from(([127, 0, 0, 1], port)))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    }
}
