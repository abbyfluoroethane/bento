//! Core domain types shared across the Bento crates.
//!
//! The definitions follow SPEC.md sections 11 and 12. This crate holds
//! types only, no behavior beyond parsing and rendering the string enums
//! that reach SQLite, the JSON API, and the command line interface.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A value that did not name any variant of a string enum.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("types: {value:?} is not a valid {kind}")]
pub struct ParseError {
    /// The name of the enum that rejected the value.
    pub kind: &'static str,
    /// The value that was rejected.
    pub value: String,
}

impl ParseError {
    fn new(kind: &'static str, value: &str) -> Self {
        Self {
            kind,
            value: value.to_string(),
        }
    }
}

/// The observed state of an instance. libvirt is authoritative for this
/// value (SPEC 11.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Running,
    Stopped,
    Starting,
}

impl State {
    /// The wire form, as stored in `instances.state`.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Stopped => "stopped",
            State::Starting => "starting",
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for State {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "running" => Ok(State::Running),
            "stopped" => Ok(State::Stopped),
            "starting" => Ok(State::Starting),
            other => Err(ParseError::new("state", other)),
        }
    }
}

/// The state the last user action asked for. Bento is authoritative for
/// this value (SPEC 11.1). It never holds `starting`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DesiredState {
    Running,
    Stopped,
}

impl DesiredState {
    /// The wire form, as stored in `instances.desired_state`.
    pub fn as_str(self) -> &'static str {
        match self {
            DesiredState::Running => "running",
            DesiredState::Stopped => "stopped",
        }
    }
}

impl fmt::Display for DesiredState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DesiredState {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "running" => Ok(DesiredState::Running),
            "stopped" => Ok(DesiredState::Stopped),
            other => Err(ParseError::new("desired state", other)),
        }
    }
}

/// How the HTTP proxy treats requests for an instance name (SPEC 9.2).
/// The default is [`Visibility::Off`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    #[default]
    Off,
    Private,
    Public,
}

impl Visibility {
    /// The wire form, as stored in `instances.visibility`.
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Off => "off",
            Visibility::Private => "private",
            Visibility::Public => "public",
        }
    }
}

impl fmt::Display for Visibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Visibility {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(Visibility::Off),
            "private" => Ok(Visibility::Private),
            "public" => Ok(Visibility::Public),
            other => Err(ParseError::new("visibility", other)),
        }
    }
}

/// One virtual machine. One instance is one libvirt domain. The UUID is
/// the identifier; the name is a label that can change (SPEC 7.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    pub uuid: String,
    pub name: String,
    pub owner_id: i64,
    pub host_id: i64,
    pub image_name: String,
    pub base_checksum: String,
    pub state: State,
    pub desired_state: DesiredState,
    pub address: String,
    pub mac: String,
    pub vcpu: u32,
    pub memory_mib: i64,
    pub disk_gib: i64,
    pub nested: bool,
    pub ksm: bool,
    pub http_port: u16,
    pub visibility: Visibility,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// The last SSH connection or HTTP request. Bento never acts on this
    /// column; it only helps a user find a forgotten instance (SPEC 12).
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_seen_at: Option<OffsetDateTime>,
    /// The runner slot that supplied `address` (MULTI-NODE 16). `None`
    /// until slot-aware allocation places it (MULTI-NODE 22 step 6).
    pub slot: Option<i64>,
}

/// A person with a Bento account (SPEC 12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub email: String,
    /// Set by the operator by hand; until then the user has no dashboard
    /// login (SPEC 13).
    pub oidc_subject: Option<String>,
    /// The `/24` of the user (SPEC 6.2).
    pub subnet: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// What the host can hold (SPEC 6.1). Bento has no per-user limit, so
/// these two numbers are the only ceiling on a create or a resize. The
/// binary reads them from the host once at startup.
///
/// vCPU has no entry. Processor time is shared, so a host may carry more
/// virtual processors than it has cores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capacity {
    /// Host memory times the operator's overcommit ratio (SPEC 5.3).
    pub memory_mib: i64,
    /// The size of the storage volume, against virtual disk size.
    pub disk_gib: i64,
}

impl Capacity {
    /// A capacity that bounds nothing. Zero means "no ceiling", not "no
    /// room": a check whose ceiling is zero is skipped. Tests use this.
    /// `bentod` refuses to start without real host figures, so a running
    /// deployment never holds one.
    pub fn unbounded() -> Self {
        Self::default()
    }
}

/// One public key registered by a user. The SSH frontend looks keys up by
/// fingerprint on every connection (SPEC 12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshKey {
    pub id: i64,
    pub user_id: i64,
    pub public_key: String,
    pub fingerprint: String,
    pub comment: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// A machine that runs libvirtd and holds instances. Version 1 supports
/// one host (SPEC 12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    pub id: i64,
    /// The durable identity of the machine, from `/etc/machine-id`. This
    /// is the key; `name` is a label (MULTI-NODE 16). It is `None` only
    /// for a row a version-1 database carried and no machine has claimed.
    pub machine_id: Option<String>,
    pub name: String,
    pub libvirt_uri: String,
    /// Where the controller reaches this host's runner service
    /// (MULTI-NODE 11.2). `None` for a version-1 host, which the controller
    /// drives through its own libvirt socket.
    pub endpoint: Option<String>,
    /// The next hop other machines use to reach this machine's guest
    /// slots (MULTI-NODE 8.5).
    ///
    /// It is separate from `endpoint` because management traffic and
    /// guest data need not share a path: a deployment can keep management
    /// on the LAN and carry guest traffic over a tunnel. `None` means no
    /// machine can route to this one, so it gets no slot route.
    pub underlay: Option<String>,
    /// A disabled host keeps its guests and its routes. It only stops
    /// taking new placement.
    pub enabled: bool,
    pub placement: Placement,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Whether a host takes new instances (MULTI-NODE 18).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Placement {
    #[default]
    Active,
    /// Keeps its guests, takes nothing new, and waits to be emptied.
    Draining,
    /// Emptied and retired. The row stays for the audit trail.
    Removed,
}

impl Placement {
    pub fn as_str(self) -> &'static str {
        match self {
            Placement::Active => "active",
            Placement::Draining => "draining",
            Placement::Removed => "removed",
        }
    }
}

impl FromStr for Placement {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Placement::Active),
            "draining" => Ok(Placement::Draining),
            "removed" => Ok(Placement::Removed),
            other => Err(ParseError::new("placement", other)),
        }
    }
}

/// What a new instance needs from a machine, so placement can weigh
/// every machine against it (MULTI-NODE 12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placing {
    pub vcpu: i64,
    pub memory_mib: i64,
    pub disk_gib: i64,
    /// The architecture of the image the instance boots. A machine of
    /// another architecture cannot run it.
    pub arch: Option<String>,
}

/// One runner slot: the same subprefix of every user `/24` (MULTI-NODE 7.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    pub slot: i64,
    pub state: SlotState,
    pub owner_host_id: i64,
    /// Increases every time the slot changes hands. A runner refuses an
    /// ownership claim older than the one it holds (MULTI-NODE 11.3).
    pub ownership_epoch: i64,
    pub source_host_id: Option<i64>,
    pub destination_host_id: Option<i64>,
    pub operation_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotState {
    #[default]
    Active,
    Draining,
    Moving,
}

impl SlotState {
    pub fn as_str(self) -> &'static str {
        match self {
            SlotState::Active => "active",
            SlotState::Draining => "draining",
            SlotState::Moving => "moving",
        }
    }
}

impl FromStr for SlotState {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(SlotState::Active),
            "draining" => Ok(SlotState::Draining),
            "moving" => Ok(SlotState::Moving),
            other => Err(ParseError::new("slot state", other)),
        }
    }
}

/// The controller lease (MULTI-NODE 11.3). Only its holder may dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// Random, one for each controller process.
    pub holder_id: String,
    /// Durable and strictly increasing across acquisitions.
    pub epoch: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// Settings that belong to the deployment (MULTI-NODE 19).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deployment {
    /// 24, 25, 26, or 27.
    pub runner_prefix: u8,
}

impl Deployment {
    /// How many slots the prefix divides a user `/24` into: 1, 2, 4, or 8.
    pub fn slot_count(&self) -> i64 {
        1 << (self.runner_prefix.saturating_sub(24)).min(3)
    }
}

/// A named entry in the operator allowlist (SPEC 5.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageKind {
    #[default]
    Qcow2,
    Oci,
}

impl ImageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Qcow2 => "qcow2",
            Self::Oci => "oci",
        }
    }
}

impl std::str::FromStr for ImageKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "qcow2" => Ok(Self::Qcow2),
            "oci" => Ok(Self::Oci),
            other => Err(format!("unknown image kind {other:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub name: String,
    /// Download URL for qcow2 entries, or an OCI image reference for bootc.
    pub url: String,
    #[serde(default)]
    pub kind: ImageKind,
    /// When set, a download whose checksum differs is rejected. `None`
    /// means trust on first use.
    pub pinned_checksum: Option<String>,
    pub current_checksum: Option<String>,
}

/// One downloaded file for an image, identified by its checksum and
/// stored at a content-addressed path (SPEC 5.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageVersion {
    pub checksum: String,
    pub image_name: String,
    pub path: String,
    pub size: i64,
    /// How this immutable disk version was produced.
    #[serde(default)]
    pub kind: ImageKind,
    /// Digest of the OCI image used to build this disk, when applicable.
    #[serde(default)]
    pub source_digest: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub fetched_at: OffsetDateTime,
}

/// Grants a second user access to an instance. Shares key on the instance
/// UUID, never on the name (SPEC 7.2, 12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub instance_uuid: String,
    pub user_id: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// A name released by a delete or a rename, for the cooldown in SPEC 7.2.
/// Rows are kept after the cooldown expires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleasedName {
    pub name: String,
    pub previous_owner_id: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub released_at: OffsetDateTime,
}

/// A pending request to link one SSH public key to an account (SPEC 13).
///
/// The SSH frontend creates one of these when it meets a key it does not
/// know, and creates nothing else: an unknown key allocates no account,
/// no subnet, and no network until a browser session confirms it. Only
/// the hash of the link token is stored, as for [`Token::hash`]; the
/// token itself exists once, in the URL handed to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pairing {
    pub id: i64,
    pub token_hash: String,
    pub public_key: String,
    pub fingerprint: String,
    pub comment: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    /// Set once the key has been linked. A pairing is single-use.
    pub linked_user_id: Option<i64>,
}

/// Programmatic access. Only the hash of the token is stored (SPEC 13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub id: i64,
    pub user_id: i64,
    pub hash: String,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_enums_round_trip() {
        for s in [State::Running, State::Stopped, State::Starting] {
            assert_eq!(s.as_str().parse::<State>().unwrap(), s);
        }
        for s in [DesiredState::Running, DesiredState::Stopped] {
            assert_eq!(s.as_str().parse::<DesiredState>().unwrap(), s);
        }
        for v in [Visibility::Off, Visibility::Private, Visibility::Public] {
            assert_eq!(v.as_str().parse::<Visibility>().unwrap(), v);
        }
    }

    #[test]
    fn desired_state_rejects_starting() {
        // The desired state comes from a user action and never holds the
        // transitional value (SPEC 11.1).
        assert!("starting".parse::<DesiredState>().is_err());
    }

    #[test]
    fn unknown_values_are_rejected() {
        assert!("gone".parse::<State>().is_err());
        assert!("".parse::<Visibility>().is_err());
    }

    #[test]
    fn visibility_defaults_to_off() {
        assert_eq!(Visibility::default(), Visibility::Off);
    }
}
