use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::client::{Domain, DomainStats, LibvirtApi, Network, NetworkApi};
use crate::error::{ApiError, LibvirtError};
use crate::xdr::{Reader, Writer};

const PROGRAM: u32 = 0x2000_8086;
const PROTOCOL_VERSION: u32 = 1;
const HEADER_LENGTH: usize = 28;
const MAX_PACKET_LENGTH: usize = 16 * 1024 * 1024;

const PACKET_CALL: u32 = 0;
const PACKET_REPLY: u32 = 1;
const PACKET_MESSAGE: u32 = 2;
const STATUS_OK: u32 = 0;
const STATUS_ERROR: u32 = 1;

const PROC_CONNECT_OPEN: u32 = 1;
const PROC_CONNECT_CLOSE: u32 = 2;
const PROC_CONNECT_GET_CAPABILITIES: u32 = 7;
const PROC_DOMAIN_CREATE: u32 = 9;
const PROC_DOMAIN_DEFINE_XML: u32 = 11;
const PROC_DOMAIN_DESTROY: u32 = 12;
const PROC_DOMAIN_GET_INFO: u32 = 16;
const PROC_DOMAIN_LOOKUP_BY_NAME: u32 = 23;
const PROC_DOMAIN_REBOOT: u32 = 27;
const PROC_DOMAIN_SET_AUTOSTART: u32 = 29;
const PROC_DOMAIN_SHUTDOWN: u32 = 33;
const PROC_NETWORK_CREATE: u32 = 39;
const PROC_NETWORK_DEFINE_XML: u32 = 41;
const PROC_NETWORK_LOOKUP_BY_NAME: u32 = 46;
const PROC_NETWORK_SET_AUTOSTART: u32 = 48;
const PROC_AUTH_LIST: u32 = 66;
const PROC_AUTH_POLKIT: u32 = 70;
const PROC_DOMAIN_MEMORY_STATS: u32 = 159;
const PROC_NETWORK_IS_ACTIVE: u32 = 152;
const PROC_DOMAIN_GET_STATE: u32 = 212;
const PROC_DOMAIN_UNDEFINE_FLAGS: u32 = 231;
const PROC_CONNECT_LIST_ALL_DOMAINS: u32 = 273;

/// The longest one libvirt exchange, or one handshake, may take.
///
/// The slowest calls are a domain start (QEMU must spawn) and a destroy
/// (libvirt waits for QEMU to exit before it answers). Both finish in
/// seconds on a healthy host; a busy one can take tens of seconds. Two
/// minutes is far above that, so only a hung daemon reaches it. Without
/// a limit a hung daemon blocks every caller, because all calls share
/// one socket (SPEC 4.1).
const CALL_DEADLINE: Duration = Duration::from_secs(120);

const AUTH_NONE: i32 = 0;
const AUTH_POLKIT: i32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Header {
    program: u32,
    version: u32,
    procedure: u32,
    packet_type: u32,
    serial: i32,
    status: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Packet {
    header: Header,
    payload: Vec<u8>,
}

fn encode_packet(header: &Header, payload: &[u8]) -> Result<Vec<u8>, ApiError> {
    let length = HEADER_LENGTH
        .checked_add(payload.len())
        .ok_or_else(|| ApiError::Protocol("RPC packet length overflow".to_string()))?;
    let length = u32::try_from(length)
        .map_err(|_| ApiError::Protocol("RPC packet is too large".to_string()))?;
    let mut packet = Vec::with_capacity(length as usize);
    packet.extend_from_slice(&length.to_be_bytes());
    packet.extend_from_slice(&header.program.to_be_bytes());
    packet.extend_from_slice(&header.version.to_be_bytes());
    packet.extend_from_slice(&header.procedure.to_be_bytes());
    packet.extend_from_slice(&header.packet_type.to_be_bytes());
    packet.extend_from_slice(&header.serial.to_be_bytes());
    packet.extend_from_slice(&header.status.to_be_bytes());
    packet.extend_from_slice(payload);
    Ok(packet)
}

fn decode_packet(bytes: &[u8]) -> Result<Packet, ApiError> {
    if bytes.len() < HEADER_LENGTH {
        return Err(ApiError::Protocol(format!(
            "truncated RPC packet: got {} bytes, need at least {HEADER_LENGTH}",
            bytes.len()
        )));
    }
    let declared = u32::from_be_bytes(bytes[0..4].try_into().expect("four-byte slice")) as usize;
    if declared != bytes.len() {
        return Err(ApiError::Protocol(format!(
            "RPC packet length says {declared} bytes, received {}",
            bytes.len()
        )));
    }
    if declared > MAX_PACKET_LENGTH {
        return Err(ApiError::Protocol(format!(
            "RPC packet length {declared} exceeds {MAX_PACKET_LENGTH}"
        )));
    }
    let word = |start: usize| {
        u32::from_be_bytes(bytes[start..start + 4].try_into().expect("four-byte slice"))
    };
    Ok(Packet {
        header: Header {
            program: word(4),
            version: word(8),
            procedure: word(12),
            packet_type: word(16),
            serial: i32::from_be_bytes(bytes[20..24].try_into().expect("four-byte slice")),
            status: word(24),
        },
        payload: bytes[HEADER_LENGTH..].to_vec(),
    })
}

async fn read_packet(stream: &mut UnixStream) -> Result<Packet, ApiError> {
    let mut length_bytes = [0_u8; 4];
    stream.read_exact(&mut length_bytes).await?;
    let length = u32::from_be_bytes(length_bytes) as usize;
    if !(HEADER_LENGTH..=MAX_PACKET_LENGTH).contains(&length) {
        return Err(ApiError::Protocol(format!(
            "invalid RPC packet length {length}"
        )));
    }
    let mut bytes = vec![0_u8; length];
    bytes[..4].copy_from_slice(&length_bytes);
    stream.read_exact(&mut bytes[4..]).await?;
    decode_packet(&bytes)
}

fn decode_error(payload: &[u8]) -> Result<LibvirtError, ApiError> {
    // remote_error begins with code, domain id, optional message, and level.
    // Later optional diagnostic fields are irrelevant to operation matching.
    let mut reader = Reader::new(payload);
    let code = reader.i32()?;
    let _domain_id = reader.i32()?;
    let message = reader.optional_string()?.unwrap_or_default();
    let _level = reader.i32()?;
    Ok(LibvirtError { code, message })
}

fn process_reply(packet: Packet, serial: i32, procedure: u32) -> Result<Vec<u8>, ApiError> {
    let header = &packet.header;
    if header.program != PROGRAM || header.version != PROTOCOL_VERSION {
        return Err(ApiError::Protocol(format!(
            "unexpected RPC program/version {:#x}/{}",
            header.program, header.version
        )));
    }
    if header.packet_type != PACKET_REPLY {
        return Err(ApiError::Protocol(format!(
            "unexpected RPC packet type {}",
            header.packet_type
        )));
    }
    if header.serial != serial || header.procedure != procedure {
        return Err(ApiError::Protocol(format!(
            "reply does not match request: serial/procedure {}/{}, expected {serial}/{procedure}",
            header.serial, header.procedure
        )));
    }
    match header.status {
        STATUS_OK => Ok(packet.payload),
        STATUS_ERROR => Err(decode_error(&packet.payload)?.into()),
        status => Err(ApiError::Protocol(format!(
            "unexpected RPC reply status {status}"
        ))),
    }
}

#[derive(Debug)]
struct Transport {
    stream: Mutex<UnixStream>,
    serial: AtomicI32,
    /// True from the moment a request starts to be written until its
    /// reply is read in full. A caller can drop the future in between
    /// (a timeout, a client that left), and `read_exact` is not cancel
    /// safe, so the next reply on the socket could be stale or half
    /// read. A poisoned transport is never used again.
    poisoned: AtomicBool,
}

impl Transport {
    async fn connect(path: &Path) -> Result<Self, ApiError> {
        Ok(Self {
            stream: Mutex::new(UnixStream::connect(path).await?),
            serial: AtomicI32::new(0),
            poisoned: AtomicBool::new(false),
        })
    }

    async fn request(&self, procedure: u32, payload: Vec<u8>) -> Result<Vec<u8>, ApiError> {
        let serial = self.serial.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let header = Header {
            program: PROGRAM,
            version: PROTOCOL_VERSION,
            procedure,
            packet_type: PACKET_CALL,
            serial,
            status: STATUS_OK,
        };
        let bytes = encode_packet(&header, &payload)?;
        let mut stream = self.stream.lock().await;
        if self.poisoned.load(Ordering::SeqCst) {
            return Err(ApiError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "libvirt connection is out of step after an interrupted call",
            )));
        }
        self.poisoned.store(true, Ordering::SeqCst);
        stream.write_all(&bytes).await?;
        stream.flush().await?;
        loop {
            let packet = read_packet(&mut stream).await?;
            // Notifications share the socket with replies. This crate does not
            // subscribe to events, but a daemon may still send a message while
            // a request is outstanding, so consume it before awaiting the reply.
            if packet.header.packet_type == PACKET_MESSAGE {
                continue;
            }
            let result = process_reply(packet, serial, procedure);
            // A full, valid reply leaves the socket in step. That holds
            // for a libvirt error reply too. Any framing or header
            // fault keeps the transport poisoned.
            if matches!(result, Ok(_) | Err(ApiError::Libvirt(_))) {
                self.poisoned.store(false, Ordering::SeqCst);
            }
            return result;
        }
    }
}

/// One libvirt connection that outlives the daemon behind it.
///
/// virtqemud restarts on a package update, and an idle modular daemon
/// can exit on its own. Each restart closes the socket. A client that
/// kept the first socket for its whole life then failed every call with
/// "Broken pipe" until the process restarted, and a runner stopped
/// observing its machine (SPEC 4.1).
///
/// An I/O error therefore drops the socket, and the next call opens a
/// new one with the full handshake. The call that saw the error is not
/// sent again: when the reply is lost, libvirt can already have done
/// the work, and a create or a destroy must not run twice.
#[derive(Debug)]
pub(crate) struct RpcApi {
    path: PathBuf,
    flags: u32,
    /// `None` after an I/O error, until the next call opens a new socket.
    current: Mutex<Option<Arc<Transport>>>,
    deadline: Duration,
}

impl RpcApi {
    pub(crate) async fn connect(path: &Path, flags: u32) -> Result<Self, ApiError> {
        Self::connect_with_deadline(path, flags, CALL_DEADLINE).await
    }

    async fn connect_with_deadline(
        path: &Path,
        flags: u32,
        deadline: Duration,
    ) -> Result<Self, ApiError> {
        let transport = within(deadline, open(path, flags)).await?;
        Ok(Self {
            path: path.to_path_buf(),
            flags,
            current: Mutex::new(Some(Arc::new(transport))),
            deadline,
        })
    }

    async fn request(&self, procedure: u32, payload: Vec<u8>) -> Result<Vec<u8>, ApiError> {
        let transport = {
            let mut current = self.current.lock().await;
            match current.as_ref() {
                Some(transport) if !transport.poisoned.load(Ordering::SeqCst) => {
                    Arc::clone(transport)
                }
                _ => {
                    let transport =
                        Arc::new(within(self.deadline, open(&self.path, self.flags)).await?);
                    *current = Some(Arc::clone(&transport));
                    transport
                }
            }
        };
        let result = within(self.deadline, transport.request(procedure, payload)).await;
        if transport.poisoned.load(Ordering::SeqCst) || matches!(&result, Err(ApiError::Io(_))) {
            // The socket is gone, or a call was cut short (an I/O error,
            // a timeout, a bad frame) and the next reply would be read
            // out of step. Either way it is unusable. Another call may
            // already have opened a new socket; keep it.
            let mut current = self.current.lock().await;
            if current
                .as_ref()
                .is_some_and(|open| Arc::ptr_eq(open, &transport))
            {
                *current = None;
            }
        }
        result
    }

    /// Sends `ConnectClose`. A connection the daemon already closed is
    /// closed, so that is not an error: a runner stopping after a
    /// libvirt restart must exit cleanly.
    pub(crate) async fn close(&self) -> Result<(), ApiError> {
        let Some(transport) = self.current.lock().await.take() else {
            return Ok(());
        };
        match transport.request(PROC_CONNECT_CLOSE, Vec::new()).await {
            Ok(_) | Err(ApiError::Io(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn capabilities(&self) -> Result<String, ApiError> {
        let payload = self
            .request(PROC_CONNECT_GET_CAPABILITIES, Vec::new())
            .await?;
        let mut reader = Reader::new(&payload);
        let capabilities = reader.string()?;
        reader.finish()?;
        Ok(capabilities)
    }
}

/// Runs `future` with a time limit. The caller of a call that timed out
/// sees an I/O error; the transport stays poisoned, so it is replaced.
async fn within<T>(
    deadline: Duration,
    future: impl std::future::Future<Output = Result<T, ApiError>>,
) -> Result<T, ApiError> {
    match tokio::time::timeout(deadline, future).await {
        Ok(result) => result,
        Err(_) => Err(ApiError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "libvirt did not answer in time",
        ))),
    }
}

/// Opens a socket and runs the handshake: authentication, then
/// `ConnectOpen`.
async fn open(path: &Path, flags: u32) -> Result<Transport, ApiError> {
    let transport = Transport::connect(path).await?;
    authenticate(&transport).await?;
    let mut writer = Writer::new();
    writer.optional_string(Some("qemu:///system"))?;
    writer.u32(flags);
    transport
        .request(PROC_CONNECT_OPEN, writer.into_inner())
        .await?;
    Ok(transport)
}

/// Settles authentication before the connection opens. libvirtd
/// requires the auth-list call even when the socket needs no
/// authentication at all.
///
/// The first type in the advertised list that we can satisfy wins.
/// `none` needs no exchange. `polkit` needs one more call, which is
/// what the read-write socket of the modular daemons asks for: it
/// authorizes the peer by its socket credentials, so membership of
/// the `libvirt` group is what actually decides the answer and
/// nothing is sent. A list with neither is a socket Bento cannot
/// use, and saying so plainly beats failing later inside
/// ConnectOpen.
async fn authenticate(transport: &Transport) -> Result<(), ApiError> {
    let advertised = auth_list(transport).await?;
    for auth in &advertised {
        match *auth {
            AUTH_NONE => return Ok(()),
            AUTH_POLKIT => {
                auth_polkit(transport).await?;
                return Ok(());
            }
            _ => continue,
        }
    }
    if advertised.is_empty() {
        return Ok(());
    }
    Err(ApiError::Protocol(format!(
        "libvirt socket requires an unsupported authentication type: {advertised:?}"
    )))
}

/// Runs the polkit exchange. The reply carries a completion flag;
/// a refusal arrives as an error reply, not as a false flag.
async fn auth_polkit(transport: &Transport) -> Result<(), ApiError> {
    let payload = transport.request(PROC_AUTH_POLKIT, Vec::new()).await?;
    let mut reader = Reader::new(&payload);
    let complete = reader.i32()?;
    reader.finish()?;
    if complete == 0 {
        return Err(ApiError::Protocol(
            "libvirt refused the polkit authentication".to_string(),
        ));
    }
    Ok(())
}

async fn auth_list(transport: &Transport) -> Result<Vec<i32>, ApiError> {
    let payload = transport.request(PROC_AUTH_LIST, Vec::new()).await?;
    let mut reader = Reader::new(&payload);
    let len = reader.array_len()?;
    let mut auth = Vec::with_capacity(len);
    for _ in 0..len {
        auth.push(reader.i32()?);
    }
    reader.finish()?;
    Ok(auth)
}

fn encode_domain(writer: &mut Writer, domain: &Domain) -> Result<(), ApiError> {
    writer.string(&domain.name)?;
    writer.fixed_opaque(&domain.uuid);
    writer.i32(domain.id);
    Ok(())
}

fn decode_domain_stats(reader: &mut Reader<'_>) -> Result<DomainStats, ApiError> {
    Ok(DomainStats {
        state: reader.u32()?,
        max_mem_kib: reader.u64()?,
        memory_kib: reader.u64()?,
        vcpus: reader.u32()?,
        cpu_time_ns: reader.u64()?,
    })
}

fn decode_memory_stats(reader: &mut Reader<'_>) -> Result<Vec<(i32, u64)>, ApiError> {
    let count = reader.array_len()?;
    let mut stats = Vec::with_capacity(count);
    for _ in 0..count {
        stats.push((reader.i32()?, reader.u64()?));
    }
    Ok(stats)
}

fn decode_domain(reader: &mut Reader<'_>) -> Result<Domain, ApiError> {
    Ok(Domain {
        name: reader.string()?,
        uuid: reader.fixed_opaque::<16>()?,
        id: reader.i32()?,
    })
}

fn encode_network(writer: &mut Writer, network: &Network) -> Result<(), ApiError> {
    writer.string(&network.name)?;
    writer.fixed_opaque(&network.uuid);
    Ok(())
}

fn decode_network(reader: &mut Reader<'_>) -> Result<Network, ApiError> {
    Ok(Network {
        name: reader.string()?,
        uuid: reader.fixed_opaque::<16>()?,
    })
}

#[async_trait]
impl LibvirtApi for RpcApi {
    async fn domain_define_xml(&self, xml: &str) -> Result<Domain, ApiError> {
        let mut writer = Writer::new();
        writer.string(xml)?;
        let payload = self
            .request(PROC_DOMAIN_DEFINE_XML, writer.into_inner())
            .await?;
        let mut reader = Reader::new(&payload);
        let domain = decode_domain(&mut reader)?;
        reader.finish()?;
        Ok(domain)
    }

    async fn domain_create(&self, domain: &Domain) -> Result<(), ApiError> {
        let mut writer = Writer::new();
        encode_domain(&mut writer, domain)?;
        self.request(PROC_DOMAIN_CREATE, writer.into_inner())
            .await?;
        Ok(())
    }

    async fn domain_shutdown(&self, domain: &Domain) -> Result<(), ApiError> {
        let mut writer = Writer::new();
        encode_domain(&mut writer, domain)?;
        self.request(PROC_DOMAIN_SHUTDOWN, writer.into_inner())
            .await?;
        Ok(())
    }

    async fn domain_reboot(&self, domain: &Domain, flags: u32) -> Result<(), ApiError> {
        let mut writer = Writer::new();
        encode_domain(&mut writer, domain)?;
        writer.u32(flags);
        self.request(PROC_DOMAIN_REBOOT, writer.into_inner())
            .await?;
        Ok(())
    }

    async fn domain_destroy(&self, domain: &Domain) -> Result<(), ApiError> {
        let mut writer = Writer::new();
        encode_domain(&mut writer, domain)?;
        self.request(PROC_DOMAIN_DESTROY, writer.into_inner())
            .await?;
        Ok(())
    }

    async fn domain_undefine_flags(&self, domain: &Domain, flags: u32) -> Result<(), ApiError> {
        let mut writer = Writer::new();
        encode_domain(&mut writer, domain)?;
        writer.u32(flags);
        self.request(PROC_DOMAIN_UNDEFINE_FLAGS, writer.into_inner())
            .await?;
        Ok(())
    }

    async fn domain_set_autostart(&self, domain: &Domain, value: i32) -> Result<(), ApiError> {
        let mut writer = Writer::new();
        encode_domain(&mut writer, domain)?;
        writer.i32(value);
        self.request(PROC_DOMAIN_SET_AUTOSTART, writer.into_inner())
            .await?;
        Ok(())
    }

    async fn domain_lookup_by_name(&self, name: &str) -> Result<Domain, ApiError> {
        let mut writer = Writer::new();
        writer.string(name)?;
        let payload = self
            .request(PROC_DOMAIN_LOOKUP_BY_NAME, writer.into_inner())
            .await?;
        let mut reader = Reader::new(&payload);
        let domain = decode_domain(&mut reader)?;
        reader.finish()?;
        Ok(domain)
    }

    async fn domain_get_state(&self, domain: &Domain, flags: u32) -> Result<(i32, i32), ApiError> {
        let mut writer = Writer::new();
        encode_domain(&mut writer, domain)?;
        writer.u32(flags);
        let payload = self
            .request(PROC_DOMAIN_GET_STATE, writer.into_inner())
            .await?;
        let mut reader = Reader::new(&payload);
        let result = (reader.i32()?, reader.i32()?);
        reader.finish()?;
        Ok(result)
    }

    /// `remote_domain_get_info_ret` is five fixed fields. XDR has no
    /// type narrower than four bytes, so `state` and `nr_virt_cpu` each
    /// occupy a whole word even though libvirt declares them as a char
    /// and a short.
    async fn domain_get_info(&self, domain: &Domain) -> Result<DomainStats, ApiError> {
        let mut writer = Writer::new();
        encode_domain(&mut writer, domain)?;
        let payload = self
            .request(PROC_DOMAIN_GET_INFO, writer.into_inner())
            .await?;
        let mut reader = Reader::new(&payload);
        let stats = decode_domain_stats(&mut reader)?;
        reader.finish()?;
        Ok(stats)
    }

    /// `remote_domain_memory_stats_ret` is a counted array of
    /// `{ int tag; unsigned hyper val; }`. The tags are the
    /// `VIR_DOMAIN_MEMORY_STAT_*` values; the caller picks the ones it
    /// wants and ignores the rest, because which tags a domain answers
    /// with depends on the guest.
    async fn domain_memory_stats(
        &self,
        domain: &Domain,
        max_stats: u32,
        flags: u32,
    ) -> Result<Vec<(i32, u64)>, ApiError> {
        let mut writer = Writer::new();
        encode_domain(&mut writer, domain)?;
        writer.u32(max_stats);
        writer.u32(flags);
        let payload = self
            .request(PROC_DOMAIN_MEMORY_STATS, writer.into_inner())
            .await?;
        let mut reader = Reader::new(&payload);
        let stats = decode_memory_stats(&mut reader)?;
        reader.finish()?;
        Ok(stats)
    }

    async fn connect_list_all_domains(
        &self,
        need_results: i32,
        flags: u32,
    ) -> Result<(Vec<Domain>, u32), ApiError> {
        let mut writer = Writer::new();
        writer.i32(need_results);
        writer.u32(flags);
        let payload = self
            .request(PROC_CONNECT_LIST_ALL_DOMAINS, writer.into_inner())
            .await?;
        let mut reader = Reader::new(&payload);
        let len = reader.array_len()?;
        let mut domains = Vec::with_capacity(len);
        for _ in 0..len {
            domains.push(decode_domain(&mut reader)?);
        }
        let count = reader.u32()?;
        reader.finish()?;
        Ok((domains, count))
    }
}

#[async_trait]
impl NetworkApi for RpcApi {
    async fn network_lookup_by_name(&self, name: &str) -> Result<Network, ApiError> {
        let mut writer = Writer::new();
        writer.string(name)?;
        let payload = self
            .request(PROC_NETWORK_LOOKUP_BY_NAME, writer.into_inner())
            .await?;
        let mut reader = Reader::new(&payload);
        let network = decode_network(&mut reader)?;
        reader.finish()?;
        Ok(network)
    }

    async fn network_define_xml(&self, xml: &str) -> Result<Network, ApiError> {
        let mut writer = Writer::new();
        writer.string(xml)?;
        let payload = self
            .request(PROC_NETWORK_DEFINE_XML, writer.into_inner())
            .await?;
        let mut reader = Reader::new(&payload);
        let network = decode_network(&mut reader)?;
        reader.finish()?;
        Ok(network)
    }

    async fn network_create(&self, network: &Network) -> Result<(), ApiError> {
        let mut writer = Writer::new();
        encode_network(&mut writer, network)?;
        self.request(PROC_NETWORK_CREATE, writer.into_inner())
            .await?;
        Ok(())
    }

    async fn network_is_active(&self, network: &Network) -> Result<i32, ApiError> {
        let mut writer = Writer::new();
        encode_network(&mut writer, network)?;
        let payload = self
            .request(PROC_NETWORK_IS_ACTIVE, writer.into_inner())
            .await?;
        let mut reader = Reader::new(&payload);
        let active = reader.i32()?;
        reader.finish()?;
        Ok(active)
    }

    async fn network_set_autostart(&self, network: &Network, value: i32) -> Result<(), ApiError> {
        let mut writer = Writer::new();
        encode_network(&mut writer, network)?;
        writer.i32(value);
        self.request(PROC_NETWORK_SET_AUTOSTART, writer.into_inner())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::error::ERR_NO_DOMAIN;

    fn reply(serial: i32, procedure: u32, status: u32, payload: Vec<u8>) -> Packet {
        Packet {
            header: Header {
                program: PROGRAM,
                version: PROTOCOL_VERSION,
                procedure,
                packet_type: PACKET_REPLY,
                serial,
                status,
            },
            payload,
        }
    }

    /// Builds the bytes libvirt would send, by hand, from the layout in
    /// `remote_protocol.x`. XDR has no type narrower than four bytes, so
    /// the `unsigned char` state and the `unsigned short` vcpu count each
    /// take a whole word.
    #[test]
    fn domain_info_is_decoded_from_the_wire_layout() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1_u32.to_be_bytes()); // state: running
        bytes.extend_from_slice(&2_097_152_u64.to_be_bytes()); // maxMem KiB
        bytes.extend_from_slice(&1_048_576_u64.to_be_bytes()); // memory KiB
        bytes.extend_from_slice(&4_u32.to_be_bytes()); // nrVirtCpu
        bytes.extend_from_slice(&123_456_789_u64.to_be_bytes()); // cpuTime ns
        assert_eq!(bytes.len(), 32);

        let mut reader = Reader::new(&bytes);
        let stats = decode_domain_stats(&mut reader).unwrap();
        reader.finish().unwrap();
        assert_eq!(
            stats,
            DomainStats {
                state: 1,
                max_mem_kib: 2_097_152,
                memory_kib: 1_048_576,
                vcpus: 4,
                cpu_time_ns: 123_456_789,
            }
        );
    }

    #[test]
    fn memory_stats_decode_as_a_counted_array_of_tag_and_value() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&3_u32.to_be_bytes()); // array length
        for (tag, value) in [(4_i32, 900_u64), (7, 180_464), (9, 1_787_457_032)] {
            bytes.extend_from_slice(&tag.to_be_bytes());
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        let mut reader = Reader::new(&bytes);
        let stats = decode_memory_stats(&mut reader).unwrap();
        reader.finish().unwrap();
        assert_eq!(stats, [(4, 900), (7, 180_464), (9, 1_787_457_032)]);

        // An empty array is a normal answer from a guest that has not
        // reported, not a protocol error.
        let empty = 0_u32.to_be_bytes();
        let mut reader = Reader::new(&empty);
        assert!(decode_memory_stats(&mut reader).unwrap().is_empty());
    }

    #[test]
    fn framing_round_trip() {
        let header = Header {
            program: PROGRAM,
            version: PROTOCOL_VERSION,
            procedure: PROC_DOMAIN_GET_STATE,
            packet_type: PACKET_CALL,
            serial: 17,
            status: STATUS_OK,
        };
        let bytes = encode_packet(&header, &[1, 2, 3, 4]).unwrap();
        assert_eq!(u32::from_be_bytes(bytes[..4].try_into().unwrap()), 32);
        assert_eq!(
            decode_packet(&bytes).unwrap(),
            Packet {
                header,
                payload: vec![1, 2, 3, 4]
            }
        );
    }

    #[test]
    fn truncated_packet_is_rejected() {
        let header = Header {
            program: PROGRAM,
            version: PROTOCOL_VERSION,
            procedure: PROC_CONNECT_CLOSE,
            packet_type: PACKET_REPLY,
            serial: 1,
            status: STATUS_OK,
        };
        let mut bytes = encode_packet(&header, &[1, 2, 3, 4]).unwrap();
        bytes.pop();
        assert!(decode_packet(&bytes).is_err());
    }

    #[test]
    fn error_status_reply_decodes_typed_error() {
        let mut payload = Writer::new();
        payload.i32(ERR_NO_DOMAIN);
        payload.i32(10);
        payload.optional_string(Some("domain not found")).unwrap();
        payload.i32(2);
        let error = process_reply(
            reply(
                9,
                PROC_DOMAIN_LOOKUP_BY_NAME,
                STATUS_ERROR,
                payload.into_inner(),
            ),
            9,
            PROC_DOMAIN_LOOKUP_BY_NAME,
        )
        .unwrap_err();
        match error {
            ApiError::Libvirt(error) => {
                assert_eq!(error.code, ERR_NO_DOMAIN);
                assert_eq!(error.message, "domain not found");
            }
            other => panic!("expected libvirt error, got {other:?}"),
        }
    }

    #[test]
    fn domain_and_network_handles_round_trip() {
        let domain = Domain {
            name: "web".to_string(),
            uuid: [7; 16],
            id: -1,
        };
        let network = Network {
            name: "bento-user-1".to_string(),
            uuid: [9; 16],
        };
        let mut writer = Writer::new();
        encode_domain(&mut writer, &domain).unwrap();
        encode_network(&mut writer, &network).unwrap();
        let bytes = writer.into_inner();
        let mut reader = Reader::new(&bytes);
        assert_eq!(decode_domain(&mut reader).unwrap(), domain);
        assert_eq!(decode_network(&mut reader).unwrap(), network);
        reader.finish().unwrap();
    }

    /// Read-only validation against Fedora's modular qemu daemon. No domain
    /// mutation is permitted here; explicit opt-in is required to run it.
    #[tokio::test]
    #[ignore = "requires a local virtqemud socket"]
    async fn real_daemon_read_only_codec_validation() {
        let api = match RpcApi::connect(Path::new("/run/libvirt/virtqemud-sock"), 1).await {
            Ok(api) => Arc::new(api),
            Err(error) => panic!("read-only libvirt connection failed: {error}"),
        };
        let capabilities = api.capabilities().await.unwrap();
        assert!(capabilities.contains("<capabilities>"));
        let (domains, count) = api.connect_list_all_domains(1, 0).await.unwrap();
        assert_eq!(domains.len(), count as usize);

        // Procedures 35 and 159 carry 64-bit fields, so a wrong field
        // width desynchronizes the reply and nothing but a real daemon
        // shows it. Both are read-only.
        for domain in &domains {
            let info = api.domain_get_info(domain).await.unwrap();
            assert!(info.vcpus >= 1, "{:?} reports no vcpus", domain.name);
            assert!(
                info.max_mem_kib >= info.memory_kib,
                "{:?} reports more memory than its maximum",
                domain.name
            );
            // A running domain has spent processor time. State 1 is
            // running; a stopped domain has none, and asking it for
            // memory statistics is an error rather than an empty answer.
            if info.state != 1 {
                continue;
            }
            assert!(info.cpu_time_ns > 0, "{:?} has no cpu time", domain.name);
            let stats = api.domain_memory_stats(domain, 32, 0).await.unwrap();
            // Every tag libvirt defines today is below this. A decode
            // that lost alignment produces enormous or negative tags.
            for (tag, _) in &stats {
                assert!((0..64).contains(tag), "{:?} decoded tag {tag}", domain.name);
            }
        }
        api.close().await.unwrap();
    }

    /// A fake daemon. Socket number `n` (from 1) answers `limits[n - 1]`
    /// calls and then hangs up, the way a restarting virtqemud does; a
    /// socket past the list never hangs up. The capabilities reply names
    /// the socket, so a test can tell a new socket from the old one.
    fn fake_daemon(
        limits: Vec<usize>,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("virtqemud-sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let sockets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&sockets);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let number = counter.fetch_add(1, Ordering::SeqCst) + 1;
                let limit = limits.get(number - 1).copied().unwrap_or(usize::MAX);
                tokio::spawn(async move {
                    for _ in 0..limit {
                        let Ok(call) = read_packet(&mut stream).await else {
                            return;
                        };
                        let mut writer = Writer::new();
                        match call.header.procedure {
                            PROC_AUTH_LIST => writer.u32(0),
                            PROC_CONNECT_GET_CAPABILITIES => {
                                writer.string(&format!("socket {number}")).unwrap();
                            }
                            _ => {}
                        }
                        let header = Header {
                            packet_type: PACKET_REPLY,
                            status: STATUS_OK,
                            ..call.header
                        };
                        let bytes = encode_packet(&header, &writer.into_inner()).unwrap();
                        if stream.write_all(&bytes).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (directory, path, sockets)
    }

    #[tokio::test]
    async fn a_daemon_restart_costs_one_call_and_the_next_one_reconnects() {
        // The handshake is two calls, auth-list and open, then one more.
        let (_directory, path, sockets) = fake_daemon(vec![3]);
        let api = RpcApi::connect(&path, 0).await.unwrap();
        assert_eq!(api.capabilities().await.unwrap(), "socket 1");

        // The daemon has gone. This call fails, and it is not sent again.
        let error = api.capabilities().await.unwrap_err();
        assert!(matches!(error, ApiError::Io(_)), "{error:?}");
        assert_eq!(sockets.load(Ordering::SeqCst), 1);

        // The next call opens a new socket with the full handshake.
        assert_eq!(api.capabilities().await.unwrap(), "socket 2");
        assert_eq!(api.capabilities().await.unwrap(), "socket 2");
        assert_eq!(sockets.load(Ordering::SeqCst), 2);
    }

    /// How a scripted socket treats its first capabilities call.
    #[derive(Clone)]
    enum Fault {
        /// Wait for the notification, then answer in full.
        Stall(Arc<tokio::sync::Notify>),
        /// Write half of the reply, wait, then write the rest.
        Partial(Arc<tokio::sync::Notify>),
        /// Answer with a packet length of zero.
        ZeroLength,
    }

    /// A fake daemon like `fake_daemon`, but socket `n` applies
    /// `faults[n - 1]` to its first capabilities call. The handshake is
    /// always answered, and a socket past the list answers every call.
    fn faulty_daemon(
        faults: Vec<Fault>,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("virtqemud-sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let sockets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&sockets);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let number = counter.fetch_add(1, Ordering::SeqCst) + 1;
                let mut fault = faults.get(number - 1).cloned();
                tokio::spawn(async move {
                    loop {
                        let Ok(call) = read_packet(&mut stream).await else {
                            return;
                        };
                        let mut writer = Writer::new();
                        let mut applied = None;
                        match call.header.procedure {
                            PROC_AUTH_LIST => writer.u32(0),
                            PROC_CONNECT_GET_CAPABILITIES => {
                                writer.string(&format!("socket {number}")).unwrap();
                                applied = fault.take();
                            }
                            _ => {}
                        }
                        let header = Header {
                            packet_type: PACKET_REPLY,
                            status: STATUS_OK,
                            ..call.header
                        };
                        let bytes = encode_packet(&header, &writer.into_inner()).unwrap();
                        let result = match applied {
                            None => stream.write_all(&bytes).await,
                            Some(Fault::Stall(release)) => {
                                release.notified().await;
                                stream.write_all(&bytes).await
                            }
                            Some(Fault::Partial(release)) => {
                                let half = bytes.len() / 2;
                                let _ = stream.write_all(&bytes[..half]).await;
                                release.notified().await;
                                stream.write_all(&bytes[half..]).await
                            }
                            Some(Fault::ZeroLength) => stream.write_all(&[0, 0, 0, 0]).await,
                        };
                        if result.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (directory, path, sockets)
    }

    const SHORT: Duration = Duration::from_millis(100);

    #[tokio::test]
    async fn a_cancelled_call_does_not_leave_a_stale_reply_for_the_next_call() {
        let release = Arc::new(tokio::sync::Notify::new());
        let (_directory, path, sockets) = faulty_daemon(vec![Fault::Stall(Arc::clone(&release))]);
        let api = RpcApi::connect(&path, 0).await.unwrap();
        // The caller gives up while the daemon stalls.
        tokio::time::timeout(SHORT, api.capabilities())
            .await
            .unwrap_err();
        // The daemon now sends the reply nobody waits for.
        release.notify_one();
        assert_eq!(api.capabilities().await.unwrap(), "socket 2");
        assert_eq!(sockets.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_call_cancelled_mid_reply_does_not_desynchronise_the_next_call() {
        let release = Arc::new(tokio::sync::Notify::new());
        let (_directory, path, sockets) = faulty_daemon(vec![Fault::Partial(Arc::clone(&release))]);
        let api = RpcApi::connect(&path, 0).await.unwrap();
        tokio::time::timeout(SHORT, api.capabilities())
            .await
            .unwrap_err();
        release.notify_one();
        assert_eq!(api.capabilities().await.unwrap(), "socket 2");
        assert_eq!(sockets.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_framing_error_makes_the_next_call_reconnect() {
        let (_directory, path, sockets) = faulty_daemon(vec![Fault::ZeroLength]);
        let api = RpcApi::connect(&path, 0).await.unwrap();
        let error = api.capabilities().await.unwrap_err();
        assert!(matches!(error, ApiError::Protocol(_)), "{error:?}");
        assert_eq!(api.capabilities().await.unwrap(), "socket 2");
        assert_eq!(sockets.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_hung_daemon_ends_the_call_with_a_timeout_and_the_next_call_reconnects() {
        let release = Arc::new(tokio::sync::Notify::new());
        let (_directory, path, sockets) = faulty_daemon(vec![Fault::Stall(Arc::clone(&release))]);
        let api = RpcApi::connect_with_deadline(&path, 0, SHORT)
            .await
            .unwrap();
        let error = api.capabilities().await.unwrap_err();
        match error {
            ApiError::Io(error) => assert_eq!(error.kind(), std::io::ErrorKind::TimedOut),
            other => panic!("{other:?}"),
        }
        assert_eq!(api.capabilities().await.unwrap(), "socket 2");
        assert_eq!(sockets.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn closing_a_connection_the_daemon_already_closed_is_not_an_error() {
        let (_directory, path, _sockets) = fake_daemon(vec![2]);
        let api = RpcApi::connect(&path, 0).await.unwrap();
        api.close().await.unwrap();
        // A second close has nothing left to close.
        api.close().await.unwrap();
    }
}
