//! A fake libvirtd.
//!
//! It speaks the slice of the libvirt RPC protocol that `bento-hypervisor`
//! calls (SPEC 4.1) over a Unix socket, so the real `bentod` binary
//! connects to it and cannot tell the difference. Domains and networks
//! live in memory. Every procedure it answers is recorded, and the
//! recording is what the end-to-end assertions read.
//!
//! The wire format mirrors `crates/hypervisor/src/rpc.rs` and
//! `crates/hypervisor/src/xdr.rs`. Those modules are private, so the
//! encoding is written out again here. That duplication is deliberate: a
//! fake that shared the client's codec could not catch a codec mistake.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

const PROGRAM: u32 = 0x2000_8086;
const PROTOCOL_VERSION: u32 = 1;
const HEADER_LENGTH: usize = 28;
const MAX_PACKET_LENGTH: usize = 16 * 1024 * 1024;
const PACKET_CALL: u32 = 0;
const PACKET_REPLY: u32 = 1;
const STATUS_OK: u32 = 0;
const STATUS_ERROR: u32 = 1;

const PROC_CONNECT_OPEN: u32 = 1;
const PROC_CONNECT_CLOSE: u32 = 2;
const PROC_CONNECT_GET_CAPABILITIES: u32 = 7;
const PROC_DOMAIN_CREATE: u32 = 9;
const PROC_DOMAIN_DEFINE_XML: u32 = 11;
const PROC_DOMAIN_DESTROY: u32 = 12;
const PROC_DOMAIN_LOOKUP_BY_NAME: u32 = 23;
const PROC_DOMAIN_REBOOT: u32 = 27;
const PROC_DOMAIN_SET_AUTOSTART: u32 = 29;
const PROC_DOMAIN_SHUTDOWN: u32 = 33;
const PROC_NETWORK_CREATE: u32 = 39;
const PROC_NETWORK_DEFINE_XML: u32 = 41;
const PROC_NETWORK_LOOKUP_BY_NAME: u32 = 46;
const PROC_NETWORK_SET_AUTOSTART: u32 = 48;
const PROC_AUTH_LIST: u32 = 66;
const PROC_NETWORK_IS_ACTIVE: u32 = 152;
const PROC_DOMAIN_GET_STATE: u32 = 212;
const PROC_DOMAIN_UNDEFINE_FLAGS: u32 = 231;
const PROC_CONNECT_LIST_ALL_DOMAINS: u32 = 273;

/// The socket advertises `none`, which is what a passing test wants:
/// polkit would need a real daemon behind it.
const AUTH_NONE: i32 = 0;

/// libvirt `virDomainState` values. Only these two ever leave the fake.
const DOMAIN_RUNNING: i32 = 1;
const DOMAIN_SHUTOFF: i32 = 5;

const ERR_NO_DOMAIN: i32 = 42;
const ERR_NO_NETWORK: i32 = 43;

/// The host capabilities XML. Nothing in Bento parses it today; it is
/// answered so a caller that starts to cannot hang.
const CAPABILITIES: &str =
    "<capabilities><host><cpu><arch>unused</arch></cpu></host></capabilities>";

// ---------------------------------------------------------------- XDR

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn i32(&mut self, value: i32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    /// Writes bytes with no length prefix, padded to a four-byte boundary.
    fn fixed_opaque(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
        let padding = (4 - value.len() % 4) % 4;
        self.bytes.resize(self.bytes.len() + padding, 0);
    }

    fn string(&mut self, value: &str) {
        self.u32(value.len() as u32);
        self.fixed_opaque(value.as_bytes());
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

/// Anything malformed is a bug in the client or in this fake. Both want
/// the same treatment: fail the request loudly with the reason.
type XdrResult<T> = Result<T, String>;

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, len: usize) -> XdrResult<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| "XDR offset overflow".to_string())?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| format!("truncated XDR value at byte {}", self.offset))?;
        self.offset = end;
        Ok(value)
    }

    fn u32(&mut self) -> XdrResult<u32> {
        let bytes: [u8; 4] = self.take(4)?.try_into().expect("slice has four bytes");
        Ok(u32::from_be_bytes(bytes))
    }

    fn i32(&mut self) -> XdrResult<i32> {
        let bytes: [u8; 4] = self.take(4)?.try_into().expect("slice has four bytes");
        Ok(i32::from_be_bytes(bytes))
    }

    fn string(&mut self) -> XdrResult<String> {
        let len = self.u32()? as usize;
        if len > MAX_PACKET_LENGTH {
            return Err(format!("XDR string length {len} is too large"));
        }
        let value = self.take(len)?.to_vec();
        let padding = (4 - len % 4) % 4;
        self.take(padding)?;
        String::from_utf8(value).map_err(|error| format!("XDR string is not UTF-8: {error}"))
    }

    fn optional_string(&mut self) -> XdrResult<Option<String>> {
        match self.u32()? {
            0 => Ok(None),
            1 => self.string().map(Some),
            value => Err(format!("invalid XDR optional discriminant {value}")),
        }
    }

    fn uuid(&mut self) -> XdrResult<[u8; 16]> {
        Ok(self.take(16)?.try_into().expect("slice has sixteen bytes"))
    }
}

// ------------------------------------------------------------- Records

/// One domain held by the fake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Domain {
    pub name: String,
    pub uuid: [u8; 16],
    pub xml: String,
    pub running: bool,
    pub autostart: bool,
}

/// One network held by the fake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub name: String,
    pub uuid: [u8; 16],
    pub xml: String,
    pub active: bool,
    pub autostart: bool,
}

#[derive(Default)]
struct Inner {
    domains: HashMap<String, Domain>,
    networks: HashMap<String, Network>,
    calls: Vec<String>,
}

/// A running fake libvirtd. Dropping it stops the listener.
pub struct Libvirtd {
    inner: Arc<Mutex<Inner>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Libvirtd {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Libvirtd {
    /// Binds `socket` and serves it until the returned handle is dropped.
    pub fn start(socket: &Path) -> std::io::Result<Self> {
        let listener = UnixListener::bind(socket)?;
        let inner = Arc::new(Mutex::new(Inner::default()));
        let task = tokio::spawn({
            let inner = inner.clone();
            async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    tokio::spawn(serve_connection(stream, inner.clone()));
                }
            }
        });
        Ok(Self { inner, task })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every procedure answered so far, in order, as `"<name> <argument>"`.
    pub fn calls(&self) -> Vec<String> {
        self.lock().calls.clone()
    }

    /// Reports whether the recording holds this exact call.
    pub fn saw(&self, call: &str) -> bool {
        self.lock().calls.iter().any(|seen| seen == call)
    }

    pub fn domain(&self, name: &str) -> Option<Domain> {
        self.lock().domains.get(name).cloned()
    }

    pub fn domain_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.lock().domains.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn network(&self, name: &str) -> Option<Network> {
        self.lock().networks.get(name).cloned()
    }

    pub fn network_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.lock().networks.keys().cloned().collect();
        names.sort();
        names
    }
}

// ------------------------------------------------------------ Serving

async fn serve_connection(mut stream: UnixStream, inner: Arc<Mutex<Inner>>) {
    loop {
        let Some((procedure, serial, payload)) = read_call(&mut stream).await else {
            return;
        };
        let outcome = dispatch(procedure, &payload, &inner);
        let (status, body) = match outcome {
            Ok(body) => (STATUS_OK, body),
            Err(error) => (STATUS_ERROR, encode_error(&error)),
        };
        if write_reply(&mut stream, procedure, serial, status, &body)
            .await
            .is_err()
        {
            return;
        }
        if procedure == PROC_CONNECT_CLOSE {
            return;
        }
    }
}

/// Reads one call packet. Returns `None` at end of stream or on a packet
/// this fake cannot parse, which closes the connection.
async fn read_call(stream: &mut UnixStream) -> Option<(u32, i32, Vec<u8>)> {
    let mut length_bytes = [0_u8; 4];
    stream.read_exact(&mut length_bytes).await.ok()?;
    let length = u32::from_be_bytes(length_bytes) as usize;
    if !(HEADER_LENGTH..=MAX_PACKET_LENGTH).contains(&length) {
        return None;
    }
    let mut bytes = vec![0_u8; length];
    bytes[..4].copy_from_slice(&length_bytes);
    stream.read_exact(&mut bytes[4..]).await.ok()?;

    let word = |start: usize| {
        u32::from_be_bytes(bytes[start..start + 4].try_into().expect("four-byte slice"))
    };
    if word(4) != PROGRAM || word(8) != PROTOCOL_VERSION || word(16) != PACKET_CALL {
        return None;
    }
    let procedure = word(12);
    let serial = i32::from_be_bytes(bytes[20..24].try_into().expect("four-byte slice"));
    Some((procedure, serial, bytes[HEADER_LENGTH..].to_vec()))
}

async fn write_reply(
    stream: &mut UnixStream,
    procedure: u32,
    serial: i32,
    status: u32,
    payload: &[u8],
) -> std::io::Result<()> {
    let length = (HEADER_LENGTH + payload.len()) as u32;
    let mut packet = Vec::with_capacity(length as usize);
    packet.extend_from_slice(&length.to_be_bytes());
    packet.extend_from_slice(&PROGRAM.to_be_bytes());
    packet.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    packet.extend_from_slice(&procedure.to_be_bytes());
    packet.extend_from_slice(&PACKET_REPLY.to_be_bytes());
    packet.extend_from_slice(&serial.to_be_bytes());
    packet.extend_from_slice(&status.to_be_bytes());
    packet.extend_from_slice(payload);
    stream.write_all(&packet).await?;
    stream.flush().await
}

/// A libvirt `remote_error`. The client reads the code, the domain id,
/// the message, and the level, in that order.
fn encode_error(error: &Failure) -> Vec<u8> {
    let mut writer = Writer::default();
    writer.i32(error.code);
    writer.i32(0);
    writer.u32(1);
    writer.string(&error.message);
    writer.i32(2);
    writer.into_inner()
}

struct Failure {
    code: i32,
    message: String,
}

impl Failure {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// A protocol slip in the fake or the client. libvirt's
    /// `VIR_ERR_INTERNAL_ERROR` is 1.
    fn internal(message: impl Into<String>) -> Self {
        Self::new(1, message)
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::internal(message)
    }
}

type Answer = Result<Vec<u8>, Failure>;

fn dispatch(procedure: u32, payload: &[u8], inner: &Arc<Mutex<Inner>>) -> Answer {
    let mut reader = Reader::new(payload);
    let mut state = inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // A macro, not a closure: a closure would hold `state` borrowed for
    // the whole match.
    macro_rules! record {
        ($name:expr) => {
            state.calls.push($name.to_string())
        };
        ($name:expr, $argument:expr) => {
            state.calls.push(format!("{} {}", $name, $argument))
        };
    }

    match procedure {
        PROC_AUTH_LIST => {
            record!("auth-list");
            let mut writer = Writer::default();
            writer.u32(1);
            writer.i32(AUTH_NONE);
            Ok(writer.into_inner())
        }
        PROC_CONNECT_OPEN => {
            let uri = reader.optional_string()?.unwrap_or_default();
            record!("connect-open", uri);
            Ok(Vec::new())
        }
        PROC_CONNECT_CLOSE => {
            record!("connect-close");
            Ok(Vec::new())
        }
        PROC_CONNECT_GET_CAPABILITIES => {
            record!("capabilities");
            let mut writer = Writer::default();
            writer.string(CAPABILITIES);
            Ok(writer.into_inner())
        }

        PROC_DOMAIN_DEFINE_XML => {
            let xml = reader.string()?;
            let name = element(&xml, "name")
                .ok_or_else(|| Failure::internal("domain XML has no <name>"))?;
            let uuid = element(&xml, "uuid")
                .as_deref()
                .and_then(parse_uuid)
                .ok_or_else(|| Failure::internal("domain XML has no usable <uuid>"))?;
            record!("define-domain", name);
            let existing = state.domains.get(&name);
            let running = existing.is_some_and(|domain| domain.running);
            let autostart = existing.is_some_and(|domain| domain.autostart);
            state.domains.insert(
                name.clone(),
                Domain {
                    name: name.clone(),
                    uuid,
                    xml,
                    running,
                    autostart,
                },
            );
            Ok(encode_domain(&name, uuid, running))
        }
        PROC_DOMAIN_LOOKUP_BY_NAME => {
            let name = reader.string()?;
            record!("lookup-domain", name);
            let domain = state
                .domains
                .get(&name)
                .ok_or_else(|| Failure::new(ERR_NO_DOMAIN, format!("no domain named {name}")))?;
            Ok(encode_domain(&domain.name, domain.uuid, domain.running))
        }
        PROC_DOMAIN_CREATE => {
            let (name, _) = decode_domain(&mut reader)?;
            record!("start-domain", name);
            let domain = require_domain(&mut state, &name)?;
            domain.running = true;
            Ok(Vec::new())
        }
        PROC_DOMAIN_SHUTDOWN => {
            let (name, _) = decode_domain(&mut reader)?;
            record!("shutdown-domain", name);
            // A guest that always honours the request keeps the graceful
            // path in `Client::stop` free of a timeout wait.
            let domain = require_domain(&mut state, &name)?;
            domain.running = false;
            Ok(Vec::new())
        }
        PROC_DOMAIN_DESTROY => {
            let (name, _) = decode_domain(&mut reader)?;
            record!("destroy-domain", name);
            let domain = require_domain(&mut state, &name)?;
            domain.running = false;
            Ok(Vec::new())
        }
        PROC_DOMAIN_REBOOT => {
            let (name, _) = decode_domain(&mut reader)?;
            let _flags = reader.u32()?;
            record!("reboot-domain", name);
            let domain = require_domain(&mut state, &name)?;
            domain.running = true;
            Ok(Vec::new())
        }
        PROC_DOMAIN_UNDEFINE_FLAGS => {
            let (name, _) = decode_domain(&mut reader)?;
            let _flags = reader.u32()?;
            record!("undefine-domain", name);
            state
                .domains
                .remove(&name)
                .ok_or_else(|| Failure::new(ERR_NO_DOMAIN, format!("no domain named {name}")))?;
            Ok(Vec::new())
        }
        PROC_DOMAIN_SET_AUTOSTART => {
            let (name, _) = decode_domain(&mut reader)?;
            let value = reader.i32()?;
            record!("set-domain-autostart", format!("{name} {value}"));
            let domain = require_domain(&mut state, &name)?;
            domain.autostart = value != 0;
            Ok(Vec::new())
        }
        PROC_DOMAIN_GET_STATE => {
            let (name, _) = decode_domain(&mut reader)?;
            let _flags = reader.u32()?;
            record!("domain-state", name);
            let domain = require_domain(&mut state, &name)?;
            let running = domain.running;
            let mut writer = Writer::default();
            writer.i32(if running {
                DOMAIN_RUNNING
            } else {
                DOMAIN_SHUTOFF
            });
            writer.i32(0);
            Ok(writer.into_inner())
        }
        PROC_CONNECT_LIST_ALL_DOMAINS => {
            let _need_results = reader.i32()?;
            let _flags = reader.u32()?;
            record!("list-domains");
            let mut domains: Vec<Domain> = state.domains.values().cloned().collect();
            domains.sort_by(|left, right| left.name.cmp(&right.name));
            let mut writer = Writer::default();
            writer.u32(domains.len() as u32);
            for domain in &domains {
                writer.string(&domain.name);
                writer.fixed_opaque(&domain.uuid);
                writer.i32(if domain.running { 1 } else { -1 });
            }
            writer.u32(domains.len() as u32);
            Ok(writer.into_inner())
        }

        PROC_NETWORK_LOOKUP_BY_NAME => {
            let name = reader.string()?;
            record!("lookup-network", name);
            let network = state
                .networks
                .get(&name)
                .ok_or_else(|| Failure::new(ERR_NO_NETWORK, format!("no network named {name}")))?;
            Ok(encode_network(&network.name, network.uuid))
        }
        PROC_NETWORK_DEFINE_XML => {
            let xml = reader.string()?;
            let name = element(&xml, "name")
                .ok_or_else(|| Failure::internal("network XML has no <name>"))?;
            record!("define-network", name);
            // libvirt assigns a network UUID when the XML omits one, so
            // the fake derives a stable one from the name.
            let uuid = element(&xml, "uuid")
                .as_deref()
                .and_then(parse_uuid)
                .unwrap_or_else(|| derive_uuid(&name));
            state.networks.insert(
                name.clone(),
                Network {
                    name: name.clone(),
                    uuid,
                    xml,
                    active: false,
                    autostart: false,
                },
            );
            Ok(encode_network(&name, uuid))
        }
        PROC_NETWORK_CREATE => {
            let (name, _) = decode_network(&mut reader)?;
            record!("start-network", name);
            let network = require_network(&mut state, &name)?;
            network.active = true;
            Ok(Vec::new())
        }
        PROC_NETWORK_IS_ACTIVE => {
            let (name, _) = decode_network(&mut reader)?;
            record!("network-active", name);
            let network = require_network(&mut state, &name)?;
            let active = i32::from(network.active);
            let mut writer = Writer::default();
            writer.i32(active);
            Ok(writer.into_inner())
        }
        PROC_NETWORK_SET_AUTOSTART => {
            let (name, _) = decode_network(&mut reader)?;
            let value = reader.i32()?;
            record!("set-network-autostart", format!("{name} {value}"));
            let network = require_network(&mut state, &name)?;
            network.autostart = value != 0;
            Ok(Vec::new())
        }

        other => Err(Failure::internal(format!(
            "fake libvirtd does not implement procedure {other}"
        ))),
    }
}

fn require_domain<'a>(state: &'a mut Inner, name: &str) -> Result<&'a mut Domain, Failure> {
    state
        .domains
        .get_mut(name)
        .ok_or_else(|| Failure::new(ERR_NO_DOMAIN, format!("no domain named {name}")))
}

fn require_network<'a>(state: &'a mut Inner, name: &str) -> Result<&'a mut Network, Failure> {
    state
        .networks
        .get_mut(name)
        .ok_or_else(|| Failure::new(ERR_NO_NETWORK, format!("no network named {name}")))
}

fn encode_domain(name: &str, uuid: [u8; 16], running: bool) -> Vec<u8> {
    let mut writer = Writer::default();
    writer.string(name);
    writer.fixed_opaque(&uuid);
    writer.i32(if running { 1 } else { -1 });
    writer.into_inner()
}

fn decode_domain(reader: &mut Reader<'_>) -> Result<(String, [u8; 16]), Failure> {
    let name = reader.string()?;
    let uuid = reader.uuid()?;
    let _id = reader.i32()?;
    Ok((name, uuid))
}

fn encode_network(name: &str, uuid: [u8; 16]) -> Vec<u8> {
    let mut writer = Writer::default();
    writer.string(name);
    writer.fixed_opaque(&uuid);
    writer.into_inner()
}

fn decode_network(reader: &mut Reader<'_>) -> Result<(String, [u8; 16]), Failure> {
    let name = reader.string()?;
    let uuid = reader.uuid()?;
    Ok((name, uuid))
}

/// Returns the text of the first `<tag>` element. Bento's generated XML
/// is flat and machine written, so a scan is enough and pulling in an XML
/// parser is not.
fn element(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

/// Parses a hyphenated UUID into its sixteen bytes.
fn parse_uuid(text: &str) -> Option<[u8; 16]> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| *byte != b'-')
        .map(|byte| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        })
        .collect::<Option<Vec<u8>>>()?;
    if digits.len() != 32 {
        return None;
    }
    let mut uuid = [0_u8; 16];
    for (index, pair) in digits.as_chunks::<2>().0.iter().enumerate() {
        uuid[index] = (pair[0] << 4) | pair[1];
    }
    Some(uuid)
}

/// Derives a stable UUID from a name, for records that carry none.
fn derive_uuid(name: &str) -> [u8; 16] {
    let mut uuid = [0_u8; 16];
    for (index, byte) in name.bytes().enumerate() {
        uuid[index % 16] ^= byte.rotate_left((index % 8) as u32);
    }
    uuid
}
