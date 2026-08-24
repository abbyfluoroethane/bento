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

/// The four per-user limits: instance count, total vCPU count, total
/// memory, and total virtual disk size (SPEC 6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Quota {
    pub user_id: i64,
    pub max_instances: i64,
    pub max_vcpu: i64,
    pub max_memory_mib: i64,
    pub max_disk_gib: i64,
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
    pub name: String,
    pub libvirt_uri: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
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
