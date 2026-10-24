//! The operator configuration for `bentod`, loaded from a TOML file.
//!
//! Every setting named in SPEC.md that the operator controls lives here:
//! base domain, libvirt URI, directories, database path, overcommit
//! ratio, name cooldown, reboot-restore batch size, the private address
//! range for user `/24`s, ACME and OIDC settings, and listen addresses.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

mod duration;
pub use duration::{GoDuration, parse_go_duration};

/// Defaults from SPEC.md.
pub mod defaults {
    use std::time::Duration;

    pub const LIBVIRT_URI: &str = "qemu:///system";
    pub const IMAGE_DIR: &str = "/var/lib/bento/images";
    pub const STORAGE_DIR: &str = "/var/lib/bento/storage";
    pub const DB_PATH: &str = "/var/lib/bento/bento.db";
    /// The runner listens on loopback until the operator sets an underlay
    /// address (MULTI-NODE 11.1).
    pub const RUNNER_LISTEN: &str = "127.0.0.1:10443";
    /// Durable runner fencing state (MULTI-NODE 11.3).
    pub const RUNNER_FENCE_DB: &str = "/var/lib/bento/runner.db";
    /// The runner fails closed when the clocks differ by more than this
    /// value (MULTI-NODE 11.3).
    pub const RUNNER_MAX_CLOCK_SKEW: Duration = Duration::from_secs(60);
    /// SPEC 5.3: no overcommit.
    pub const OVERCOMMIT_RATIO: f64 = 1.0;
    /// SPEC 7.2. An operator may set `0s` for no cooldown; only an omitted
    /// key selects this value.
    pub const NAME_COOLDOWN: Duration = Duration::from_secs(24 * 3600);
    /// Names an instance may not take (SPEC 7.4). `bento` and `www` are the
    /// names a visitor assumes belong to the deployment itself.
    pub const RESERVED_NAMES: [&str; 2] = ["www", "bento"];
    /// SPEC 11.2.
    pub const RESTORE_BATCH_SIZE: u32 = 4;
    /// Carved into user `/24`s, SPEC 6.2.
    pub const PRIVATE_RANGE: super::Ipv4Prefix = super::Ipv4Prefix {
        addr: std::net::Ipv4Addr::new(10, 100, 0, 0),
        bits: 16,
    };
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
    pub const MEMORY_MIB: u64 = 2048;
    /// `new` without `--disk`.
    pub const DISK_GIB: u64 = 20;
    /// Written into an instance's network configuration when `dns` is
    /// unset (SPEC 6.2).
    pub const DNS: [std::net::IpAddr; 2] = [
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(1, 1, 1, 1)),
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(9, 9, 9, 9)),
    ];
    /// Root filesystem used when a bootc image does not declare one.
    pub const BOOTC_ROOTFS: &str = "ext4";
    /// Rootful Podman storage shared with the image-builder container.
    pub const CONTAINER_STORAGE: &str = "/var/lib/containers/storage";
    /// Maximum duration of one Podman pull, validation, or image build.
    pub const BOOTC_BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
}

/// Anything that stops `bentod` from reading its configuration.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config: read {path}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("config: invalid TOML")]
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

/// The full operator configuration, with every default resolved.
///
/// [`Config::parse`] is the only place that turns an omitted key into a
/// default. A consumer reads a field and applies no fallback of its own.
/// Zero keeps its literal meaning where zero is meaningful (a `0s`
/// `name_cooldown` is no cooldown); `validate` rejects it where it is not.
#[derive(Debug, Clone)]
pub struct Config {
    /// The control plane's domain, e.g. `bento.foid.space`. The dashboard,
    /// the SSH frontend, and the OIDC callback all answer here (SPEC 1).
    pub base_domain: String,
    /// The domain an instance publishes under, e.g. `foid.space`. When the
    /// file omits it, `parse` copies `base_domain` here (SPEC 7.1).
    pub instance_domain: String,
    /// Names an instance may not take (SPEC 7.4). The computed reservation
    /// in `reserved` is applied on top of this list.
    pub reserved_names: Vec<String>,
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
    pub private_range: Ipv4Prefix,
    /// Holds the SSH frontend host key and the key the frontend uses
    /// toward the guests (SPEC 10). Both are created on first use.
    pub key_dir: String,
    /// The resolvers written into every instance's cloud-init network
    /// configuration (SPEC 6.2). Omitting the key selects
    /// [`defaults::DNS`]; an explicit empty list is rejected.
    pub dns: Vec<IpAddr>,
    /// The user names allowed to use the operator-only dashboard
    /// controls, such as the database download (SPEC 12.1).
    pub operators: Vec<String>,

    pub listen: Listen,
    pub defaults: Defaults,
    pub acme: Acme,
    pub oidc: Oidc,
    pub bootc: Bootc,
    pub runner: Runner,

    /// The runner endpoints the controller may call (MULTI-NODE 11.2).
    pub runners: Vec<RunnerEntry>,

    /// The operator image allowlist (SPEC 5.1).
    pub images: Vec<ImageEntry>,
}

/// Settings for the runner service on this machine.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Runner {
    /// The underlay address for the runner management listener.
    pub listen: String,
    /// The SQLite file that holds durable fencing state.
    pub fence_db: String,
    /// The allowed difference between the controller and runner clocks.
    pub max_clock_skew: GoDuration,
}

/// One runner endpoint known to the controller.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RunnerEntry {
    pub name: String,
    /// Where the controller sends management requests (MULTI-NODE 11.2).
    pub endpoint: String,
    /// The next hop other machines use to reach this machine's guest
    /// slots (MULTI-NODE 8.5).
    ///
    /// When absent it is the host part of `endpoint`, which on a LAN is the
    /// machine's LAN address and is what a two-machine deployment wants.
    /// Setting it separately moves guest traffic onto a tunnel, such as
    /// a WireGuard or Tailscale address, while management traffic stays
    /// where it is. Management and guest data do not have to share a
    /// path, so they are two settings.
    pub underlay: Option<std::net::Ipv4Addr>,
}

impl RunnerEntry {
    /// The next hop for this machine's slot routes.
    ///
    /// Returns an error rather than a guess when neither `underlay` nor a
    /// usable `endpoint` host is present. A wrong next hop produces a
    /// black hole that looks exactly like a healthy runner
    /// (MULTI-NODE 8.3), so it must fail at startup instead.
    pub fn underlay_address(&self) -> Result<std::net::Ipv4Addr, String> {
        if let Some(underlay) = self.underlay {
            return Ok(underlay);
        }
        let host = endpoint_host(&self.endpoint)
            .ok_or_else(|| format!("endpoint {:?} has no host part", self.endpoint))?;
        host.parse().map_err(|_| {
            format!(
                "endpoint host {host:?} is not an IPv4 address; set `underlay` for runner {:?}",
                self.name
            )
        })
    }
}

/// Extracts the host part of `http://10.0.0.97:10443`. It does not parse
/// a full URL: the endpoint is operator-written and only ever names a
/// scheme, a host, and a port.
pub fn endpoint_host(endpoint: &str) -> Option<&str> {
    let rest = endpoint
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(endpoint);
    let rest = rest.split('/').next()?;
    let host = match rest.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => rest,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (!host.is_empty()).then_some(host)
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

/// The shape of a `new` without flags (SPEC 15). An absent `image`
/// selects the first allowlist entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Defaults {
    pub image: Option<String>,
    pub vcpu: u32,
    pub memory_mib: u64,
    pub disk_gib: u64,
}

/// Host-side bootc conversion settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Bootc {
    /// Required for OCI support and always pinned by digest. Absent means
    /// OCI images are off.
    pub builder_image: Option<String>,
    /// Passed to image-builder for images that do not declare a rootfs.
    pub rootfs: String,
    /// Rootful Podman storage containing pulled bootc images.
    pub container_storage: String,
    /// Bounds each Podman operation, including image-builder.
    pub build_timeout: GoDuration,
}

impl Default for Bootc {
    fn default() -> Self {
        Self {
            builder_image: None,
            rootfs: defaults::BOOTC_ROOTFS.to_owned(),
            container_storage: defaults::CONTAINER_STORAGE.to_owned(),
            build_timeout: GoDuration(defaults::BOOTC_BUILD_TIMEOUT),
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
    /// Absent means dashboard login is off.
    pub issuer: Option<String>,
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
            issuer: None,
            client_id: String::new(),
            client_secret: String::new(),
            // The provider is already the gate on who can authenticate, so
            // deferring to it is the useful default (SPEC 13).
            allow_signup: true,
        }
    }
}

/// Where an allowlisted image comes from (SPEC 5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    /// A qcow2 download URL.
    Url(String),
    /// A bootc-compatible OCI operating-system image reference.
    Oci(String),
}

impl ImageSource {
    /// The URL or the OCI reference, whichever the entry names.
    pub fn reference(&self) -> &str {
        match self {
            ImageSource::Url(value) | ImageSource::Oci(value) => value,
        }
    }

    pub fn is_oci(&self) -> bool {
        matches!(self, ImageSource::Oci(_))
    }
}

/// One row of the operator image allowlist (SPEC 5.1).
///
/// The TOML form is unchanged: a table with `name`, exactly one of `url`
/// or `oci`, and an optional `pinned_checksum`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawImageEntry")]
pub struct ImageEntry {
    pub name: String,
    pub source: ImageSource,
    /// When set, a download whose checksum differs is rejected. Absent
    /// means trust on first use. An OCI entry never sets it.
    pub pinned_checksum: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawImageEntry {
    name: String,
    url: Option<String>,
    oci: Option<String>,
    pinned_checksum: Option<String>,
}

impl TryFrom<RawImageEntry> for ImageEntry {
    type Error = String;

    fn try_from(raw: RawImageEntry) -> Result<Self, String> {
        if raw.name.is_empty() {
            return Err("images: entry with empty name".into());
        }
        let source = match (raw.url, raw.oci) {
            (Some(url), None) if !url.is_empty() => ImageSource::Url(url),
            (None, Some(oci)) if !oci.is_empty() => ImageSource::Oci(oci),
            _ => {
                return Err(format!(
                    "images: entry {:?} must set exactly one of url or oci, non-empty",
                    raw.name
                ));
            }
        };
        if source.is_oci() && raw.pinned_checksum.is_some() {
            return Err(format!(
                "images: OCI entry {:?} cannot set pinned_checksum; pin the OCI reference digest instead",
                raw.name
            ));
        }
        Ok(ImageEntry {
            name: raw.name,
            source,
            pinned_checksum: raw.pinned_checksum,
        })
    }
}

impl Default for Config {
    /// A configuration with every default applied and no
    /// deployment-specific value set.
    fn default() -> Self {
        Config {
            base_domain: String::new(),
            instance_domain: String::new(),
            reserved_names: defaults::RESERVED_NAMES
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            libvirt_uri: defaults::LIBVIRT_URI.to_string(),
            image_dir: defaults::IMAGE_DIR.to_string(),
            storage_dir: defaults::STORAGE_DIR.to_string(),
            db_path: defaults::DB_PATH.to_string(),
            overcommit_ratio: defaults::OVERCOMMIT_RATIO,
            name_cooldown: GoDuration(defaults::NAME_COOLDOWN),
            restore_batch_size: defaults::RESTORE_BATCH_SIZE,
            private_range: defaults::PRIVATE_RANGE,
            key_dir: defaults::KEY_DIR.to_string(),
            dns: defaults::DNS.to_vec(),
            operators: Vec::new(),
            listen: Listen::default(),
            defaults: Defaults::default(),
            acme: Acme::default(),
            oidc: Oidc::default(),
            bootc: Bootc::default(),
            runner: Runner::default(),
            runners: Vec::new(),
            images: Vec::new(),
        }
    }
}

impl Default for Runner {
    fn default() -> Self {
        Self {
            listen: defaults::RUNNER_LISTEN.to_owned(),
            fence_db: defaults::RUNNER_FENCE_DB.to_owned(),
            max_clock_skew: GoDuration(defaults::RUNNER_MAX_CLOCK_SKEW),
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
            image: None,
            vcpu: defaults::VCPU,
            memory_mib: defaults::MEMORY_MIB,
            disk_gib: defaults::DISK_GIB,
        }
    }
}

/// An IPv4 network in CIDR form, as `private_range` and a user's subnet
/// carry it. This is the one CIDR parser in the workspace (SPEC 6.2).
/// An address with host bits set is allowed, because a guest address is
/// written as `10.100.0.5/24`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ipv4Prefix {
    pub addr: std::net::Ipv4Addr,
    pub bits: u8,
}

impl Ipv4Prefix {
    /// Whether the prefix covers `address`.
    pub fn contains(self, address: std::net::Ipv4Addr) -> bool {
        let mask: u32 = match self.bits {
            0 => 0,
            bits @ 1..=32 => u32::MAX << (32 - bits),
            _ => return false,
        };
        (u32::from(address) & mask) == (u32::from(self.addr) & mask)
    }
}

impl std::fmt::Display for Ipv4Prefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.bits)
    }
}

/// Parses `10.100.0.0/16`. Rejects an IPv6 range, which SPEC 7.1 leaves
/// out of version 1, and a prefix length above 32.
impl std::str::FromStr for Ipv4Prefix {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
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
}

impl<'de> Deserialize<'de> for Ipv4Prefix {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

impl Serialize for Ipv4Prefix {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// The port of a listen address, or `None` when it has none to read.
/// Accepts both `:443` and `127.0.0.1:10080`.
fn listen_port(addr: &str) -> Option<u16> {
    let (_, port) = addr.rsplit_once(':')?;
    port.parse().ok()
}

/// The file as written. Every top-level key is optional; `resolve` is the
/// one place an omitted key becomes its default (SPEC 15).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    base_domain: Option<String>,
    instance_domain: Option<String>,
    reserved_names: Option<Vec<String>>,
    libvirt_uri: Option<String>,
    image_dir: Option<String>,
    storage_dir: Option<String>,
    db_path: Option<String>,
    overcommit_ratio: Option<f64>,
    name_cooldown: Option<GoDuration>,
    restore_batch_size: Option<u32>,
    private_range: Option<Ipv4Prefix>,
    key_dir: Option<String>,
    dns: Option<Vec<IpAddr>>,
    operators: Option<Vec<String>>,
    listen: Option<Listen>,
    defaults: Option<Defaults>,
    acme: Option<Acme>,
    oidc: Option<Oidc>,
    bootc: Option<Bootc>,
    runner: Option<Runner>,
    runners: Option<Vec<RunnerEntry>>,
    images: Option<Vec<ImageEntry>>,
}

impl RawConfig {
    fn resolve(self) -> Config {
        let mut cfg = Config::default();
        macro_rules! set {
            ($($field:ident),* $(,)?) => {
                $(if let Some(value) = self.$field { cfg.$field = value; })*
            };
        }
        set!(
            base_domain,
            reserved_names,
            libvirt_uri,
            image_dir,
            storage_dir,
            db_path,
            overcommit_ratio,
            name_cooldown,
            restore_batch_size,
            private_range,
            key_dir,
            dns,
            operators,
            listen,
            defaults,
            acme,
            oidc,
            bootc,
            runner,
            runners,
            images,
        );
        // An omitted instance domain means the deployment publishes
        // instances under the control plane's own domain, which is what
        // every single-domain deployment does (SPEC 7.1).
        cfg.instance_domain = self
            .instance_domain
            .unwrap_or_else(|| cfg.base_domain.clone());
        cfg
    }
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
        let cfg = toml::from_str::<RawConfig>(data)?.resolve();
        cfg.validate()?;
        Ok(cfg)
    }

    /// The names an instance may not take (SPEC 7.4).
    ///
    /// The operator list is taken as given. On top of it, a base domain that
    /// sits one label under the instance domain reserves that label: the
    /// proxy answers it from the control plane, so an instance of that name
    /// could never be reached.
    pub fn reserved(&self) -> Vec<String> {
        let mut names: Vec<String> = self.reserved_names.clone();
        if let Some(label) = self
            .base_domain
            .strip_suffix(&format!(".{}", self.instance_domain))
            && !label.is_empty()
            && !label.contains('.')
            && !names.iter().any(|name| name == label)
        {
            names.push(label.to_string());
        }
        names
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
        if let Some(image) = &self.defaults.image {
            return Some(image);
        }
        self.images.first().map(|i| i.name.as_str())
    }

    fn validate(&self) -> Result<(), Error> {
        if self.base_domain.is_empty() {
            return Err(invalid("base_domain is required"));
        }
        if self.instance_domain.is_empty() {
            return Err(invalid("instance_domain must not be empty"));
        }
        if self.dns.is_empty() {
            return Err(invalid(
                "dns must list at least one resolver; omit the key for the default pair",
            ));
        }
        for (field, value) in [
            ("base_domain", &self.base_domain),
            ("instance_domain", &self.instance_domain),
        ] {
            if value.contains(['*', '/', ' ']) {
                return Err(invalid(format!("{field} must be a domain, got {value:?}")));
            }
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
        validate_runner_listen(&self.runner.listen)?;
        if self.runner.max_clock_skew.std().is_zero() {
            return Err(invalid("runner: max_clock_skew must be positive"));
        }
        let prefix = self.private_range;
        if prefix.bits > 24 {
            return Err(invalid(format!(
                "private_range must be /24 or wider to hold user /24s, got /{}",
                prefix.bits
            )));
        }
        // Every runner endpoint and every route next hop must sit outside
        // the whole private range, including /24s no user holds yet
        // (MULTI-NODE 8.5). An underlay address inside the range would
        // make a slot route recursive: the route to reach the runner
        // would point through the runner.
        for entry in &self.runners {
            let underlay = entry
                .underlay_address()
                .map_err(|e| invalid(format!("runner {:?}: {e}", entry.name)))?;
            let endpoint = endpoint_host(&entry.endpoint)
                .and_then(|host| host.parse::<std::net::Ipv4Addr>().ok());
            for (what, address) in [("underlay", Some(underlay)), ("endpoint", endpoint)] {
                let Some(address) = address else { continue };
                if prefix.contains(address) {
                    return Err(invalid(format!(
                        "runner {:?} {what} {address} is inside private_range {}; \
                         management and next-hop addresses must sit outside the guest \
                         range or their own routes become recursive",
                        entry.name, self.private_range
                    )));
                }
            }
        }
        if self.listen.proxy_port_min == 0 {
            return Err(invalid("proxy_port_min must be at least 1"));
        }
        for (field, value) in [("http", &self.listen.http), ("ssh", &self.listen.ssh)] {
            resolve_listen_addr(value).map_err(|e| invalid(format!("listen {field}: {e}")))?;
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
        if self.defaults.vcpu < 1 || self.defaults.memory_mib < 1 || self.defaults.disk_gib < 1 {
            return Err(invalid(
                "defaults: vcpu, memory_mib, and disk_gib must be positive",
            ));
        }
        if !matches!(self.bootc.rootfs.as_str(), "ext4" | "xfs" | "btrfs") {
            return Err(invalid("bootc: rootfs must be one of ext4, xfs, or btrfs"));
        }
        if self.bootc.container_storage.is_empty() {
            return Err(invalid("bootc: container_storage is required"));
        }
        if self.bootc.build_timeout.std().is_zero() {
            return Err(invalid("bootc: build_timeout must be positive"));
        }
        let mut seen = std::collections::HashSet::new();
        for img in &self.images {
            if !seen.insert(img.name.as_str()) {
                return Err(invalid(format!("images: duplicate entry {:?}", img.name)));
            }
        }
        let has_oci = self.images.iter().any(|image| image.source.is_oci());
        match &self.bootc.builder_image {
            None if has_oci => {
                return Err(invalid(
                    "bootc: builder_image pinned by sha256 digest is required for OCI images",
                ));
            }
            Some(builder) if !pinned_oci_reference(builder) => {
                return Err(invalid(
                    "bootc: builder_image must use an immutable @sha256:<64 hex> digest",
                ));
            }
            _ => {}
        }
        Ok(())
    }
}

fn validate_runner_listen(listen: &str) -> Result<(), Error> {
    let empty_host = listen.is_empty() || listen.starts_with(':');
    let bare_wildcard = matches!(listen, "0.0.0.0" | "::");
    if empty_host || bare_wildcard {
        return Err(invalid(
            "runner listen must bind one underlay address, not a wildcard; a wildcard exposes the management port on user bridges (MULTI-NODE 11.1)",
        ));
    }

    let address = match listen.parse::<SocketAddr>() {
        Ok(address) => address,
        Err(_) if listen.parse::<IpAddr>().is_ok() => {
            return Err(invalid("runner listen must include a port"));
        }
        Err(_) => {
            return Err(invalid(format!(
                "runner listen {listen:?} is not an IP address with a port"
            )));
        }
    };
    if address.ip().is_unspecified() {
        return Err(invalid(
            "runner listen must bind one underlay address, not a wildcard; a wildcard exposes the management port on user bridges (MULTI-NODE 11.1)",
        ));
    }
    Ok(())
}

fn pinned_oci_reference(reference: &str) -> bool {
    reference
        .rsplit_once("@sha256:")
        .is_some_and(|(name, digest)| {
            !name.is_empty()
                && digest.len() == 64
                && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
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
        assert_eq!(cfg.private_range.to_string(), "10.100.0.0/16");
        assert_eq!(cfg.runner.listen, "127.0.0.1:10443");
        assert_eq!(cfg.runner.fence_db, "/var/lib/bento/runner.db");
        assert_eq!(cfg.runner.max_clock_skew.std(), Duration::from_secs(60));
        assert!(cfg.runners.is_empty());
        assert_eq!(cfg.listen.http, "127.0.0.1:10080");
        assert_eq!(cfg.listen.https, ":443");
        assert_eq!(cfg.listen.ssh, ":22");
        assert_eq!(cfg.listen.proxy_port_min, 3000);
        assert_eq!(cfg.listen.proxy_port_max, 9999);
        // SPEC 8 is the default: the proxy owns its certificate.
        assert_eq!(cfg.listen.tls, TlsMode::Acme);
        assert_eq!(cfg.bootc.rootfs, "ext4");
        assert_eq!(cfg.bootc.container_storage, "/var/lib/containers/storage");
        assert_eq!(cfg.bootc.builder_image, None);
        assert_eq!(cfg.defaults.image, None);
        assert_eq!(cfg.dns, defaults::DNS.to_vec());
        assert_eq!(cfg.oidc.issuer, None);
        assert_eq!(cfg.bootc.build_timeout.std(), Duration::from_secs(30 * 60));
    }

    #[test]
    fn a_negative_size_is_a_parse_error() {
        for key in ["memory_mib", "disk_gib"] {
            let src = format!("base_domain = \"bento.example.org\"\n[defaults]\n{key} = -1\n");
            assert!(Config::parse(&src).is_err(), "{key} = -1 must not parse");
        }
        let ok = "base_domain = \"bento.example.org\"\n[defaults]\nmemory_mib = 512\n";
        assert_eq!(Config::parse(ok).unwrap().defaults.memory_mib, 512);
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
        assert_eq!(cfg.oidc.issuer.as_deref(), Some("https://id.foid.space"));
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
        let error = Config::parse(src).expect_err("parse succeeded");
        // Display names one layer. The TOML detail is in the source chain.
        let mut err = error.to_string();
        let mut cause = std::error::Error::source(&error);
        while let Some(layer) = cause {
            err.push_str(": ");
            err.push_str(&layer.to_string());
            cause = layer.source();
        }
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
            "base_domain = \"b.example\"\n[bootc]\nbuild_timeout = \"0s\"",
            "build_timeout",
        );
        parse_err(
            "base_domain = \"b.example\"\n[[images]]\nname = \"oci\"\noci = \"quay.io/x/os:latest\"",
            "builder_image",
        );
        parse_err(
            "base_domain = \"b.example\"\n[bootc]\nbuilder_image = \"quay.io/centos-bootc/bootc-image-builder:latest\"",
            "immutable",
        );
        parse_err(
            &format!(
                "base_domain = \"b.example\"\n[bootc]\nbuilder_image = \"quay.io/builder@sha256:{}\"\n[[images]]\nname = \"oci\"\noci = \"quay.io/x/os:latest\"\npinned_checksum = \"abc\"",
                "12".repeat(32)
            ),
            "cannot set pinned_checksum",
        );
        parse_err(
            "base_domain = \"b.example\"\n[[images]]\nname = \"a\"\nurl = \"https://x/a\"\n\
             [[images]]\nname = \"a\"\nurl = \"https://x/b\"",
            "duplicate",
        );
    }

    #[test]
    fn runner_listener_rejects_wildcards() {
        for listen in ["0.0.0.0:10443", "[::]:10443", ":10443", "::", ""] {
            parse_err(
                &format!("base_domain = \"bento.example.org\"\n[runner]\nlisten = {listen:?}"),
                "wildcard exposes the management port on user bridges",
            );
        }
    }

    #[test]
    fn runner_listener_requires_a_port() {
        parse_err(
            "base_domain = \"bento.example.org\"\n[runner]\nlisten = \"10.0.0.97\"",
            "runner listen must include a port",
        );
    }

    #[test]
    fn runner_listener_accepts_an_underlay_address() {
        let cfg = Config::parse(
            "base_domain = \"bento.example.org\"\n[runner]\nlisten = \"10.0.0.97:10443\"\nfence_db = \"/srv/bento/runner.db\"\nmax_clock_skew = \"45s\"",
        )
        .unwrap();
        assert_eq!(cfg.runner.listen, "10.0.0.97:10443");
        assert_eq!(cfg.runner.fence_db, "/srv/bento/runner.db");
        assert_eq!(cfg.runner.max_clock_skew.std(), Duration::from_secs(45));
    }

    #[test]
    fn underlay_defaults_to_the_endpoint_host() {
        let cfg = Config::parse(
            r#"
base_domain = "b.example"
[[runners]]
name = "tsukasa"
endpoint = "http://10.0.0.97:10443"

[[runners]]
name = "on-a-tunnel"
endpoint = "http://10.0.0.98:10443"
underlay = "100.110.234.123"
"#,
        )
        .unwrap();
        // Management and guest data need not share a path.
        assert_eq!(
            cfg.runners[0].underlay_address().unwrap(),
            std::net::Ipv4Addr::new(10, 0, 0, 97)
        );
        assert_eq!(
            cfg.runners[1].underlay_address().unwrap(),
            std::net::Ipv4Addr::new(100, 110, 234, 123)
        );
    }

    #[test]
    fn underlay_inside_the_private_range_is_refused() {
        // MULTI-NODE 8.5: a next hop inside the guest range makes the
        // route to reach it point through itself.
        let error = Config::parse(
            r#"
base_domain = "b.example"
private_range = "10.100.0.0/16"
[[runners]]
name = "wrong"
endpoint = "http://10.100.4.9:10443"
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("recursive"), "{error}");

        // The endpoint is checked as well as the underlay. A management
        // address inside the guest range is reachable only through the
        // routes Bento is about to install, which is the same trap.
        let error = Config::parse(
            r#"
base_domain = "b.example"
private_range = "10.100.0.0/16"
[[runners]]
name = "wrong"
endpoint = "http://10.100.4.9:10443"
underlay = "10.0.0.97"
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("endpoint"), "{error}");

        // Both outside the range is the accepted shape.
        assert!(
            Config::parse(
                r#"
base_domain = "b.example"
private_range = "10.100.0.0/16"
[[runners]]
name = "ok"
endpoint = "http://10.0.0.97:10443"
underlay = "100.110.234.123"
"#,
            )
            .is_ok()
        );
    }

    #[test]
    fn a_runner_with_no_usable_address_is_refused_at_startup() {
        for body in [
            "name = \"a\"\nendpoint = \"http://runner.example.org:10443\"",
            "name = \"a\"\nendpoint = \"\"",
            "name = \"a\"\nendpoint = \"http://10.0.0.9:1\"\nunderlay = \"not-an-address\"",
        ] {
            let text = format!("base_domain = \"b.example\"\n[[runners]]\n{body}\n");
            assert!(Config::parse(&text).is_err(), "accepted:\n{text}");
        }
    }

    #[test]
    fn runner_endpoints_parse() {
        let cfg = Config::parse(
            r#"
base_domain = "bento.example.org"

[[runners]]
name = "runner-a.example.org"
endpoint = "http://10.0.0.10:10443"

[[runners]]
name = "runner-b.example.org"
endpoint = "http://10.0.0.97:10443"
"#,
        )
        .unwrap();
        assert_eq!(cfg.runners.len(), 2);
        assert_eq!(cfg.runners[0].name, "runner-a.example.org");
        assert_eq!(cfg.runners[1].endpoint, "http://10.0.0.97:10443");
    }

    #[test]
    fn parse_oci_with_pinned_builder() {
        let cfg = Config::parse(&format!(
            "base_domain = \"b.example\"\n[bootc]\nbuilder_image = \"quay.io/builder@sha256:{}\"\nbuild_timeout = \"45m\"\n[[images]]\nname = \"oci\"\noci = \"quay.io/x/os@sha256:{}\"",
            "12".repeat(32),
            "34".repeat(32)
        ))
        .unwrap();
        assert_eq!(cfg.bootc.build_timeout.std(), Duration::from_secs(45 * 60));
        assert_eq!(
            cfg.images[0].source,
            ImageSource::Oci(format!("quay.io/x/os@sha256:{}", "34".repeat(32)))
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
    /// An unset instance domain follows the base domain, so a single-domain
    /// deployment is unchanged (SPEC 7.1).
    #[test]
    fn instance_domain_defaults_to_the_base_domain() {
        let cfg = Config::parse(r#"base_domain = "bento.example.org""#).unwrap();
        assert_eq!(cfg.instance_domain, "bento.example.org");
        let split =
            Config::parse("base_domain = \"bento.example.org\"\ninstance_domain = \"example.org\"")
                .unwrap();
        assert_eq!(split.instance_domain, "example.org");
        parse_err(
            "base_domain = \"b.example\"\ninstance_domain = \"*.example\"",
            "instance_domain must be a domain",
        );
    }

    /// The operator list is taken as given; a base domain one label under the
    /// instance domain reserves that label too (SPEC 7.4).
    #[test]
    fn reserved_names_add_the_control_plane_label() {
        let single = Config::parse(r#"base_domain = "bento.example.org""#).unwrap();
        assert_eq!(single.reserved(), vec!["www", "bento"]);

        let split =
            Config::parse("base_domain = \"bento.example.org\"\ninstance_domain = \"example.org\"")
                .unwrap();
        // `bento` is already in the default list, so it is not repeated.
        assert_eq!(split.reserved(), vec!["www", "bento"]);

        let other = Config::parse(
            "base_domain = \"panel.example.org\"\ninstance_domain = \"example.org\"\nreserved_names = []",
        )
        .unwrap();
        assert_eq!(other.reserved(), vec!["panel"]);

        // Two labels down is not the control plane's own label.
        let deep = Config::parse(
            "base_domain = \"a.b.example.org\"\ninstance_domain = \"example.org\"\nreserved_names = []",
        )
        .unwrap();
        assert!(deep.reserved().is_empty());

        // An operator can clear the list entirely.
        let none =
            Config::parse("base_domain = \"bento.example.org\"\nreserved_names = []").unwrap();
        assert!(none.reserved().is_empty());
    }

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
        assert_eq!(cfg.dns, defaults::DNS.to_vec());
        let cfg = Config::parse("base_domain = \"b.example\"\ndns = [\"10.0.0.1\"]").unwrap();
        assert_eq!(cfg.dns, vec!["10.0.0.1".parse::<IpAddr>().unwrap()]);
        // An explicit empty list is not "use the default".
        parse_err("base_domain = \"b.example\"\ndns = []", "dns must list");
        parse_err("base_domain = \"b.example\"\ndns = [\"nope\"]", "nope");
    }

    /// Zero means zero where zero is meaningful, and an omitted key means
    /// the default. The two must not be confused.
    #[test]
    fn zero_cooldown_is_no_cooldown() {
        let cfg = Config::parse("base_domain = \"b.example\"\nname_cooldown = \"0s\"").unwrap();
        assert_eq!(cfg.cooldown(), Duration::ZERO);
        let cfg = Config::parse("base_domain = \"b.example\"").unwrap();
        assert_eq!(cfg.cooldown(), defaults::NAME_COOLDOWN);
    }

    #[test]
    fn meaningless_zeros_are_rejected() {
        for (body, want) in [
            ("restore_batch_size = 0", "restore_batch_size"),
            ("overcommit_ratio = 0.0", "overcommit_ratio"),
            ("[listen]\nproxy_port_min = 0", "proxy_port_min"),
            ("[runner]\nmax_clock_skew = \"0s\"", "max_clock_skew"),
            ("[bootc]\nbuild_timeout = \"0s\"", "build_timeout"),
            ("[defaults]\nvcpu = 0", "positive"),
            ("[defaults]\nmemory_mib = 0", "positive"),
            ("[defaults]\ndisk_gib = 0", "positive"),
        ] {
            parse_err(&format!("base_domain = \"b.example\"\n{body}"), want);
        }
    }

    #[test]
    fn omitted_keys_in_a_table_keep_their_defaults() {
        let cfg = Config::parse("base_domain = \"b.example\"\n[defaults]\nvcpu = 4").unwrap();
        assert_eq!(cfg.defaults.vcpu, 4);
        assert_eq!(cfg.defaults.memory_mib, defaults::MEMORY_MIB);
        assert_eq!(cfg.defaults.disk_gib, defaults::DISK_GIB);
    }

    #[test]
    fn image_source_is_exactly_one_of_url_or_oci() {
        let cfg = Config::parse(
            "base_domain = \"b.example\"\n[[images]]\nname = \"a\"\nurl = \"https://x/a\"",
        )
        .unwrap();
        assert_eq!(cfg.images[0].source, ImageSource::Url("https://x/a".into()));
        for body in [
            "name = \"a\"",
            "name = \"a\"\nurl = \"\"",
            "name = \"a\"\nurl = \"u\"\noci = \"o\"",
            "name = \"\"\nurl = \"u\"",
        ] {
            parse_err(
                &format!("base_domain = \"b.example\"\n[[images]]\n{body}"),
                "images:",
            );
        }
    }

    #[test]
    fn prefix_round_trips() {
        let prefix: Ipv4Prefix = "10.100.0.0/16".parse().unwrap();
        assert_eq!(prefix.to_string(), "10.100.0.0/16");
        assert!(prefix.contains("10.100.4.9".parse().unwrap()));
        assert!(!prefix.contains("10.101.0.1".parse().unwrap()));
        for bad in ["10.0.0.0", "10.0.0.0/33", "x/8", "10.0.0.0/x", "fd00::/48"] {
            assert!(bad.parse::<Ipv4Prefix>().is_err(), "{bad}");
        }
    }

    /// Every config in the repository, including the TOML blocks in the
    /// docs, must keep parsing.
    #[test]
    fn repository_configs_parse() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let example = std::fs::read_to_string(root.join("bento.example.toml")).unwrap();
        Config::parse(&example).expect("bento.example.toml");
        for doc in ["DEPLOYING.md", "README.md", "TESTING.md"] {
            let Ok(text) = std::fs::read_to_string(root.join(doc)) else {
                continue;
            };
            let mut blocks = Vec::new();
            let mut current: Option<String> = None;
            for line in text.lines() {
                match (&mut current, line.trim_end()) {
                    (None, "```toml") => current = Some(String::new()),
                    (Some(block), "```") => {
                        blocks.push(std::mem::take(block));
                        current = None;
                    }
                    (Some(block), line) => {
                        block.push_str(line);
                        block.push('\n');
                    }
                    _ => {}
                }
            }
            for block in blocks {
                // A snippet that names no base_domain is a fragment of a
                // larger file; give it the one required key.
                let text = if block.contains("base_domain") {
                    block
                } else {
                    format!("base_domain = \"b.example\"\n{block}")
                };
                // The docs show a placeholder where a real digest goes.
                let text = text.replace("<digest>", &"0".repeat(64));
                if let Err(error) = Config::parse(&text) {
                    panic!("{doc} snippet does not parse: {error}\n{text}");
                }
            }
        }
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
