//! The operator configuration for `bentod`, loaded from a TOML file.
//!
//! Every setting named in SPEC.md that the operator controls lives here:
//! base domain, libvirt URI, directories, database path, overcommit
//! ratio, name cooldown, reboot-restore batch size, the private address
//! range for user `/24`s, ACME and OIDC settings, and listen addresses.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

mod duration;
pub use duration::{GoDuration, parse_go_duration};

/// Defaults from SPEC.md.
pub mod defaults {
    use std::time::Duration;

    pub const LIBVIRT_URI: &str = "qemu:///system";
    pub const IMAGE_DIR: &str = "/var/lib/bento/images";
    pub const STORAGE_DIR: &str = "/var/lib/bento/storage";
    pub const DB_PATH: &str = "/var/lib/bento/bento.db";
    /// SPEC 5.3: no overcommit.
    pub const OVERCOMMIT_RATIO: f64 = 1.0;
    /// SPEC 7.2.
    pub const NAME_COOLDOWN: Duration = Duration::from_secs(24 * 3600);
    /// SPEC 11.2.
    pub const RESTORE_BATCH_SIZE: u32 = 4;
    /// Carved into user `/24`s, SPEC 6.2.
    pub const PRIVATE_RANGE: &str = "10.100.0.0/16";
    /// The control plane behind the proxy. It sits outside
    /// [`PROXY_PORT_MIN`]..[`PROXY_PORT_MAX`] on purpose: the proxy binds
    /// every port of that range, on all interfaces by default, so a
    /// control plane inside it would take a port from the proxy and one
    /// of the two would fail to start.
    pub const LISTEN_HTTP: &str = "127.0.0.1:10080";
    /// HTTP proxy, SPEC 4.
    pub const LISTEN_HTTPS: &str = ":443";
    /// SSH frontend, SPEC 4.
    pub const LISTEN_SSH: &str = ":22";
    /// SPEC 9.1.
    pub const PROXY_PORT_MIN: u16 = 3000;
    /// SPEC 9.1.
    pub const PROXY_PORT_MAX: u16 = 9999;
    /// SSH host and frontend keys.
    pub const KEY_DIR: &str = "/var/lib/bento/keys";
    /// `new` without `--cpu`.
    pub const VCPU: u32 = 2;
    /// `new` without `--memory`.
    pub const MEMORY_MIB: i64 = 2048;
    /// `new` without `--disk`.
    pub const DISK_GIB: i64 = 20;
    /// Written into an instance's network configuration when `dns` is
    /// unset (SPEC 6.2).
    pub const DNS: [&str; 2] = ["1.1.1.1", "9.9.9.9"];
    /// Containerized image-builder used to turn bootc images into qcow2.
    pub const BOOTC_BUILDER_IMAGE: &str = "ghcr.io/osbuild/image-builder-cli:latest";
    /// Root filesystem used when a bootc image does not declare one.
    pub const BOOTC_ROOTFS: &str = "ext4";
    /// Rootful Podman storage shared with the image-builder container.
    pub const CONTAINER_STORAGE: &str = "/var/lib/containers/storage";
}

/// Anything that stops `bentod` from reading its configuration.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config: read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("config: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("config: {0}")]
    Invalid(String),
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::Invalid(msg.into())
}

/// How the proxy listeners get their certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// SPEC 8: the proxy obtains the wildcard certificate itself over
    /// DNS-01.
    #[default]
    Acme,
    /// Serve plain HTTP, for a host where something else already owns
    /// port 443 and terminates TLS in front of Bento. Those listeners
    /// must not be reachable from the internet directly.
    Off,
}

impl TlsMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TlsMode::Acme => "acme",
            TlsMode::Off => "off",
        }
    }
}

/// The full operator configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// The domain the operator owns, e.g. `bento.foid.space`.
    pub base_domain: String,
    /// The libvirt connection URI.
    pub libvirt_uri: String,
    /// Holds content-addressed image versions (SPEC 5.1).
    pub image_dir: String,
    /// Holds instance overlay disks.
    pub storage_dir: String,
    /// The SQLite database path (SPEC 12.1).
    pub db_path: String,
    /// The memory overcommit ratio (SPEC 5.3).
    pub overcommit_ratio: f64,
    /// How long a released name is held (SPEC 7.2).
    pub name_cooldown: GoDuration,
    /// The reboot-restore batch size (SPEC 11.2).
    pub restore_batch_size: u32,
    /// The private range carved into user `/24`s (SPEC 6.2).
    pub private_range: String,
    /// Holds the SSH frontend host key and the key the frontend uses
    /// toward the guests (SPEC 10). Both are created on first use.
    pub key_dir: String,
    /// The resolvers written into every instance's cloud-init network
    /// configuration (SPEC 6.2). Empty selects [`defaults::DNS`].
    pub dns: Vec<String>,
    /// The user names allowed to use the operator-only dashboard
    /// controls, such as the database download (SPEC 12.1).
    pub operators: Vec<String>,

    pub listen: Listen,
    pub defaults: Defaults,
    pub acme: Acme,
    pub oidc: Oidc,
    pub bootc: Bootc,

    /// The operator image allowlist (SPEC 5.1).
    pub images: Vec<ImageEntry>,
}

/// The listen addresses of the three processes.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Listen {
    /// The control plane listen address (dashboard and API).
    pub http: String,
    /// The HTTP proxy listen address for port 443.
    pub https: String,
    /// The SSH frontend listen address.
    pub ssh: String,
    /// Bounds the extra proxy ports (SPEC 9.1).
    pub proxy_port_min: u16,
    pub proxy_port_max: u16,
    /// How the proxy listeners get their certificate.
    pub tls: TlsMode,
}

/// The shape of a `new` without flags (SPEC 15). An empty `image`
/// selects the first allowlist entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Defaults {
    pub image: String,
    pub vcpu: u32,
    pub memory_mib: i64,
    pub disk_gib: i64,
}

/// Host-side bootc conversion settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Bootc {
    /// Pin this reference by digest in production for reproducible tooling.
    pub builder_image: String,
    /// Passed to image-builder for images that do not declare a rootfs.
    pub rootfs: String,
    /// Rootful Podman storage containing pulled bootc images.
    pub container_storage: String,
}

impl Default for Bootc {
    fn default() -> Self {
        Self {
            builder_image: defaults::BOOTC_BUILDER_IMAGE.to_owned(),
            rootfs: defaults::BOOTC_ROOTFS.to_owned(),
            container_storage: defaults::CONTAINER_STORAGE.to_owned(),
        }
    }
}

/// Certificate settings (SPEC 8). The wildcard certificate needs the
/// DNS-01 challenge, so a DNS provider token is required.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Acme {
    /// The ACME account contact.
    pub email: String,
    /// The DNS API token, ideally limited to the `_acme-challenge`
    /// records.
    pub cloudflare_token: String,
    /// Overrides the ACME directory URL. Empty means the production
    /// Let's Encrypt directory.
    pub directory: String,
}

/// The dashboard identity settings (SPEC 13).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Oidc {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// Whether a verified login for an identity Bento has never seen
    /// creates an account. This is the only way an account is created, so
    /// turning it off freezes the user list at whoever already exists;
    /// everyone else gets a refusal at the end of the login flow.
    pub allow_signup: bool,
}

impl Default for Oidc {
    fn default() -> Self {
        Self {
            issuer: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            // The provider is already the gate on who can authenticate, so
            // deferring to it is the useful default (SPEC 13).
            allow_signup: true,
        }
    }
}

/// One row of the operator image allowlist (SPEC 5.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageEntry {
    pub name: String,
    #[serde(default)]
    pub url: String,
    /// A bootc-compatible OCI operating-system image reference.
    #[serde(default)]
    pub oci: String,
    /// When set, a download whose checksum differs is rejected. Absent
    /// means trust on first use.
    #[serde(default)]
    pub pinned_checksum: Option<String>,
}

impl Default for Config {
    /// A configuration with every default applied and no
    /// deployment-specific value set.
    fn default() -> Self {
        Config {
            base_domain: String::new(),
            libvirt_uri: defaults::LIBVIRT_URI.to_string(),
            image_dir: defaults::IMAGE_DIR.to_string(),
            storage_dir: defaults::STORAGE_DIR.to_string(),
            db_path: defaults::DB_PATH.to_string(),
            overcommit_ratio: defaults::OVERCOMMIT_RATIO,
            name_cooldown: GoDuration(defaults::NAME_COOLDOWN),
            restore_batch_size: defaults::RESTORE_BATCH_SIZE,
            private_range: defaults::PRIVATE_RANGE.to_string(),
            key_dir: defaults::KEY_DIR.to_string(),
            dns: Vec::new(),
            operators: Vec::new(),
            listen: Listen::default(),
            defaults: Defaults::default(),
            acme: Acme::default(),
            oidc: Oidc::default(),
            bootc: Bootc::default(),
            images: Vec::new(),
        }
    }
}

impl Default for Listen {
    fn default() -> Self {
        Listen {
            http: defaults::LISTEN_HTTP.to_string(),
            https: defaults::LISTEN_HTTPS.to_string(),
            ssh: defaults::LISTEN_SSH.to_string(),
            proxy_port_min: defaults::PROXY_PORT_MIN,
            proxy_port_max: defaults::PROXY_PORT_MAX,
            tls: TlsMode::Acme,
        }
    }
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            image: String::new(),
            vcpu: defaults::VCPU,
            memory_mib: defaults::MEMORY_MIB,
            disk_gib: defaults::DISK_GIB,
        }
    }
}

/// An IPv4 network in CIDR form, as `private_range` carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Prefix {
    pub addr: std::net::Ipv4Addr,
    pub bits: u8,
}

/// Parses `10.100.0.0/16`. Rejects an IPv6 range, which SPEC 7.1 leaves
/// out of version 1, and a prefix with bits set below its length.
pub fn parse_prefix(s: &str) -> Result<Ipv4Prefix, String> {
    let (addr_part, bits_part) = s
        .split_once('/')
        .ok_or_else(|| format!("{s:?} is not a CIDR range"))?;
    let addr: IpAddr = addr_part
        .parse()
        .map_err(|_| format!("{addr_part:?} is not an IP address"))?;
    let bits: u8 = bits_part
        .parse()
        .map_err(|_| format!("{bits_part:?} is not a prefix length"))?;
    let addr = match addr {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(_) => {
            return Err(format!("must be an IPv4 range, got {s:?}"));
        }
    };
    if bits > 32 {
        return Err(format!("prefix length /{bits} exceeds /32"));
    }
    Ok(Ipv4Prefix { addr, bits })
}

/// The port of a listen address, or `None` when it has none to read.
/// Accepts both `:443` and `127.0.0.1:10080`.
fn listen_port(addr: &str) -> Option<u16> {
    let (_, port) = addr.rsplit_once(':')?;
    port.parse().ok()
}

impl Config {
    /// Reads the TOML file at `path`, applies defaults for unset values,
    /// and validates the result.
    pub fn load(path: impl AsRef<Path>) -> Result<Config, Error> {
        let path = path.as_ref();
        let data = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.display().to_string(),
            source,
        })?;
        Config::parse(&data)
    }

    /// Decodes TOML text, applies defaults for unset values, and
    /// validates the result.
    pub fn parse(data: &str) -> Result<Config, Error> {
        let cfg: Config = toml::from_str(data)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// The resolvers to write into an instance's network configuration,
    /// falling back to the built-in pair (SPEC 6.2).
    pub fn resolvers(&self) -> Vec<String> {
        if self.dns.is_empty() {
            defaults::DNS.iter().map(|s| s.to_string()).collect()
        } else {
            self.dns.clone()
        }
    }

    /// Whether `name` may use the operator-only controls (SPEC 12.1).
    pub fn is_operator(&self, name: &str) -> bool {
        self.operators.iter().any(|op| op == name)
    }

    /// The name cooldown as a [`std::time::Duration`] (SPEC 7.2).
    pub fn cooldown(&self) -> Duration {
        self.name_cooldown.std()
    }

    /// The image an unflagged `new` uses: the configured default, or the
    /// first allowlist entry (SPEC 15).
    pub fn default_image(&self) -> Option<&str> {
        if !self.defaults.image.is_empty() {
            return Some(&self.defaults.image);
        }
        self.images.first().map(|i| i.name.as_str())
    }

    fn validate(&self) -> Result<(), Error> {
        if self.base_domain.is_empty() {
            return Err(invalid("base_domain is required"));
        }
        if self.overcommit_ratio < 1.0 {
            return Err(invalid(format!(
                "overcommit_ratio must be at least 1.0, got {}",
                self.overcommit_ratio
            )));
        }
        if self.restore_batch_size < 1 {
            return Err(invalid(format!(
                "restore_batch_size must be at least 1, got {}",
                self.restore_batch_size
            )));
        }
        let prefix = parse_prefix(&self.private_range)
            .map_err(|e| invalid(format!("private_range: {e}")))?;
        if prefix.bits > 24 {
            return Err(invalid(format!(
                "private_range must be /24 or wider to hold user /24s, got /{}",
                prefix.bits
            )));
        }
        if self.listen.proxy_port_min > self.listen.proxy_port_max {
            return Err(invalid(format!(
                "proxy_port_min {} exceeds proxy_port_max {}",
                self.listen.proxy_port_min, self.listen.proxy_port_max
            )));
        }
        if let Some(port) = listen_port(&self.listen.http)
            && port >= self.listen.proxy_port_min
            && port <= self.listen.proxy_port_max
        {
            return Err(invalid(format!(
                "listen http port {port} falls inside the proxy port range {}-{}; \
                 the proxy binds every port of that range, so one of the two \
                 processes could not start",
                self.listen.proxy_port_min, self.listen.proxy_port_max
            )));
        }
        for d in &self.dns {
            if d.parse::<IpAddr>().is_err() {
                return Err(invalid(format!("dns: {d:?} is not an IP address")));
            }
        }
        if self.defaults.vcpu < 1 || self.defaults.memory_mib < 1 || self.defaults.disk_gib < 1 {
            return Err(invalid(
                "defaults: vcpu, memory_mib, and disk_gib must be positive",
            ));
        }
        if self.bootc.builder_image.is_empty() {
            return Err(invalid("bootc: builder_image is required"));
        }
        if !matches!(self.bootc.rootfs.as_str(), "ext4" | "xfs" | "btrfs") {
            return Err(invalid("bootc: rootfs must be one of ext4, xfs, or btrfs"));
        }
        if self.bootc.container_storage.is_empty() {
            return Err(invalid("bootc: container_storage is required"));
        }
        let mut seen = std::collections::HashSet::new();
        for img in &self.images {
            if img.name.is_empty() {
                return Err(invalid("images: entry with empty name"));
            }
            if img.url.is_empty() == img.oci.is_empty() {
                return Err(invalid(format!(
                    "images: entry {:?} must set exactly one of url or oci",
                    img.name
                )));
            }
            if !seen.insert(img.name.as_str()) {
                return Err(invalid(format!("images: duplicate entry {:?}", img.name)));
            }
        }
        Ok(())
    }
}

/// Resolves a listen address for binding. A leading `:` means every
/// interface, as it does in Go.
pub fn resolve_listen_addr(addr: &str) -> Result<SocketAddr, String> {
    let with_host = if let Some(port) = addr.strip_prefix(':') {
        format!("0.0.0.0:{port}")
    } else {
        addr.to_string()
    };
    with_host
        .parse()
        .map_err(|_| format!("{addr:?} is not a listen address"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_defaults() {
        let cfg = Config::parse(r#"base_domain = "bento.example.org""#).unwrap();
        assert_eq!(cfg.base_domain, "bento.example.org");
        assert_eq!(cfg.libvirt_uri, "qemu:///system");
        assert_eq!(cfg.image_dir, "/var/lib/bento/images");
        assert_eq!(cfg.storage_dir, "/var/lib/bento/storage");
        assert_eq!(cfg.db_path, "/var/lib/bento/bento.db");
        assert_eq!(cfg.overcommit_ratio, 1.0);
        assert_eq!(cfg.cooldown(), Duration::from_secs(24 * 3600));
        assert_eq!(cfg.restore_batch_size, 4);
        assert_eq!(cfg.private_range, "10.100.0.0/16");
        assert_eq!(cfg.listen.http, "127.0.0.1:10080");
        assert_eq!(cfg.listen.https, ":443");
        assert_eq!(cfg.listen.ssh, ":22");
        assert_eq!(cfg.listen.proxy_port_min, 3000);
        assert_eq!(cfg.listen.proxy_port_max, 9999);
        // SPEC 8 is the default: the proxy owns its certificate.
        assert_eq!(cfg.listen.tls, TlsMode::Acme);
        assert_eq!(cfg.bootc.rootfs, "ext4");
        assert_eq!(cfg.bootc.container_storage, "/var/lib/containers/storage");
    }

    #[test]
    fn parse_full() {
        let src = r#"
base_domain = "bento.foid.space"
libvirt_uri = "qemu+ssh://vmhost/system"
image_dir = "/srv/bento/images"
storage_dir = "/srv/bento/storage"
db_path = "/srv/bento/bento.db"
overcommit_ratio = 1.5
name_cooldown = "48h"
restore_batch_size = 8
private_range = "172.28.0.0/15"

[listen]
http = "127.0.0.1:9090"
https = ":8443"
ssh = ":2222"
proxy_port_min = 4000
proxy_port_max = 5000

[acme]
email = "op@foid.space"
cloudflare_token = "cf-token"
directory = "https://acme-staging-v02.api.letsencrypt.org/directory"

[oidc]
issuer = "https://id.foid.space"
client_id = "bento"
client_secret = "hunter2"

[[images]]
name = "debian-13"
url = "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2"

[[images]]
name = "fedora-42"
url = "https://example.org/fedora-42.qcow2"
pinned_checksum = "sha256-deadbeef"
"#;
        let cfg = Config::parse(src).unwrap();
        assert_eq!(cfg.libvirt_uri, "qemu+ssh://vmhost/system");
        assert_eq!(cfg.cooldown(), Duration::from_secs(48 * 3600));
        assert_eq!(cfg.overcommit_ratio, 1.5);
        assert_eq!(cfg.restore_batch_size, 8);
        assert_eq!(cfg.listen.ssh, ":2222");
        assert_eq!(cfg.acme.cloudflare_token, "cf-token");
        assert_eq!(cfg.oidc.issuer, "https://id.foid.space");
        assert_eq!(cfg.images.len(), 2);
        assert_eq!(
            cfg.images[1].pinned_checksum.as_deref(),
            Some("sha256-deadbeef")
        );
        // Absent means trust on first use (SPEC 5.1).
        assert_eq!(cfg.images[0].pinned_checksum, None);
    }

    #[track_caller]
    fn parse_err(src: &str, want: &str) {
        let err = Config::parse(src).expect_err("parse succeeded").to_string();
        assert!(err.contains(want), "error {err:?} should contain {want:?}");
    }

    #[test]
    fn parse_errors() {
        parse_err("", "base_domain is required");
        parse_err(
            "base_domain = \"b.example\"\novercommit_ratio = 0.5",
            "overcommit_ratio",
        );
        parse_err(
            "base_domain = \"b.example\"\nrestore_batch_size = 0",
            "restore_batch_size",
        );
        parse_err(
            "base_domain = \"b.example\"\n[listen]\ntls = \"selfsigned\"",
            "tls",
        );
        parse_err(
            "base_domain = \"b.example\"\nprivate_range = \"not-a-cidr\"",
            "private_range",
        );
        parse_err(
            "base_domain = \"b.example\"\nprivate_range = \"fd00::/48\"",
            "IPv4",
        );
        parse_err(
            "base_domain = \"b.example\"\nprivate_range = \"10.0.0.0/28\"",
            "/24 or wider",
        );
        parse_err(
            "base_domain = \"b.example\"\n[listen]\nproxy_port_min = 9000\nproxy_port_max = 4000",
            "proxy_port_min",
        );
        parse_err(
            "base_domain = \"b.example\"\nname_cooldown = \"soon\"",
            "duration",
        );
        parse_err(
            "base_domain = \"b.example\"\nbase_domian = \"typo\"",
            "unknown field",
        );
        parse_err(
            "base_domain = \"b.example\"\n[[images]]\nname = \"debian-13\"",
            "url",
        );
        parse_err(
            "base_domain = \"b.example\"\n[[images]]\nname = \"both\"\nurl = \"https://x/a\"\noci = \"quay.io/x/a\"",
            "exactly one",
        );
        parse_err(
            "base_domain = \"b.example\"\n[bootc]\nrootfs = \"zfs\"",
            "rootfs",
        );
        parse_err(
            "base_domain = \"b.example\"\n[[images]]\nname = \"a\"\nurl = \"https://x/a\"\n\
             [[images]]\nname = \"a\"\nurl = \"https://x/b\"",
            "duplicate",
        );
    }

    #[test]
    fn load_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bento.toml");
        std::fs::write(&path, "base_domain = \"bento.example.org\"\n").unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.base_domain, "bento.example.org");
    }

    #[test]
    fn load_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Config::load(dir.path().join("absent.toml")).is_err());
    }

    #[test]
    fn example_config_parses() {
        let data = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../bento.example.toml"
        ))
        .expect("read example config");
        Config::parse(&data).expect("bento.example.toml does not parse");
    }

    /// Signups follow the identity provider unless the operator says
    /// otherwise, so the flag's absence must not read as `false`.
    #[test]
    fn signups_are_open_unless_turned_off() {
        let cfg =
            Config::parse("base_domain = \"b.example\"\n[oidc]\nissuer = \"https://id\"").unwrap();
        assert!(cfg.oidc.allow_signup);
        assert!(
            Config::parse("base_domain = \"b.example\"")
                .unwrap()
                .oidc
                .allow_signup
        );
        let closed = Config::parse(
            "base_domain = \"b.example\"\n[oidc]\nissuer = \"https://id\"\nallow_signup = false",
        )
        .unwrap();
        assert!(!closed.oidc.allow_signup);
    }

    /// Covers the deployment where another proxy already owns port 443
    /// of the host and forwards to Bento.
    #[test]
    fn parse_tls_off() {
        let cfg = Config::parse(
            "base_domain = \"b.example\"\n[listen]\ntls = \"off\"\nhttps = \"127.0.0.1:10443\"",
        )
        .unwrap();
        assert_eq!(cfg.listen.tls, TlsMode::Off);
        assert_eq!(cfg.listen.https, "127.0.0.1:10443");
    }

    /// Guards the default pair: the proxy binds every port of its range,
    /// so a control plane inside that range means one of the two
    /// processes never starts.
    #[test]
    fn parse_control_plane_inside_proxy_range() {
        parse_err(
            "base_domain = \"b.example\"\n[listen]\nhttp = \"127.0.0.1:8080\"",
            "falls inside the proxy port range",
        );
    }

    #[test]
    fn resolvers_fall_back_to_the_built_in_pair() {
        let cfg = Config::parse("base_domain = \"b.example\"").unwrap();
        assert_eq!(cfg.resolvers(), vec!["1.1.1.1", "9.9.9.9"]);
        let cfg = Config::parse("base_domain = \"b.example\"\ndns = [\"10.0.0.1\"]").unwrap();
        assert_eq!(cfg.resolvers(), vec!["10.0.0.1"]);
    }

    #[test]
    fn default_image_falls_back_to_the_first_entry() {
        let cfg = Config::parse(
            "base_domain = \"b.example\"\n[[images]]\nname = \"debian-13\"\nurl = \"https://x/a\"",
        )
        .unwrap();
        assert_eq!(cfg.default_image(), Some("debian-13"));
    }

    #[test]
    fn listen_addresses_resolve() {
        assert_eq!(
            resolve_listen_addr(":443").unwrap().to_string(),
            "0.0.0.0:443"
        );
        assert_eq!(
            resolve_listen_addr("127.0.0.1:10080").unwrap().to_string(),
            "127.0.0.1:10080"
        );
        assert!(resolve_listen_addr("nonsense").is_err());
    }
}
