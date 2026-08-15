use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::client::{Domain, LibvirtApi, Network, NetworkApi};
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
const PROC_NETWORK_IS_ACTIVE: u32 = 152;
const PROC_DOMAIN_GET_STATE: u32 = 212;
const PROC_DOMAIN_UNDEFINE_FLAGS: u32 = 231;
const PROC_CONNECT_LIST_ALL_DOMAINS: u32 = 273;

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
}

impl Transport {
    async fn connect(path: &Path) -> Result<Self, ApiError> {
        Ok(Self {
            stream: Mutex::new(UnixStream::connect(path).await?),
            serial: AtomicI32::new(0),
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
            return process_reply(packet, serial, procedure);
        }
    }
}

#[derive(Debug)]
pub(crate) struct RpcApi {
    transport: Transport,
}

impl RpcApi {
    pub(crate) async fn connect(path: &Path, flags: u32) -> Result<Self, ApiError> {
        let api = Self {
            transport: Transport::connect(path).await?,
        };
        api.authenticate().await?;
        let mut writer = Writer::new();
        writer.optional_string(Some("qemu:///system"))?;
        writer.u32(flags);
        api.transport
            .request(PROC_CONNECT_OPEN, writer.into_inner())
            .await?;
        Ok(api)
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
    async fn authenticate(&self) -> Result<(), ApiError> {
        let advertised = self.auth_list().await?;
        for auth in &advertised {
            match *auth {
                AUTH_NONE => return Ok(()),
                AUTH_POLKIT => {
                    self.auth_polkit().await?;
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
    async fn auth_polkit(&self) -> Result<(), ApiError> {
        let payload = self.transport.request(PROC_AUTH_POLKIT, Vec::new()).await?;
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

    async fn auth_list(&self) -> Result<Vec<i32>, ApiError> {
        let payload = self.transport.request(PROC_AUTH_LIST, Vec::new()).await?;
        let mut reader = Reader::new(&payload);
        let len = reader.array_len()?;
        let mut auth = Vec::with_capacity(len);
        for _ in 0..len {
            auth.push(reader.i32()?);
        }
        reader.finish()?;
        Ok(auth)
    }

    pub(crate) async fn close(&self) -> Result<(), ApiError> {
        self.transport
            .request(PROC_CONNECT_CLOSE, Vec::new())
            .await?;
        Ok(())
    }

    pub(crate) async fn capabilities(&self) -> Result<String, ApiError> {
        let payload = self
            .transport
            .request(PROC_CONNECT_GET_CAPABILITIES, Vec::new())
            .await?;
        let mut reader = Reader::new(&payload);
        let capabilities = reader.string()?;
        reader.finish()?;
        Ok(capabilities)
    }
}

fn encode_domain(writer: &mut Writer, domain: &Domain) -> Result<(), ApiError> {
    writer.string(&domain.name)?;
    writer.fixed_opaque(&domain.uuid);
    writer.i32(domain.id);
    Ok(())
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
            .transport
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
        self.transport
            .request(PROC_DOMAIN_CREATE, writer.into_inner())
            .await?;
        Ok(())
    }

    async fn domain_shutdown(&self, domain: &Domain) -> Result<(), ApiError> {
        let mut writer = Writer::new();
        encode_domain(&mut writer, domain)?;
        self.transport
            .request(PROC_DOMAIN_SHUTDOWN, writer.into_inner())
            .await?;
        Ok(())
    }

    async fn domain_reboot(&self, domain: &Domain, flags: u32) -> Result<(), ApiError> {
        let mut writer = Writer::new();
        encode_domain(&mut writer, domain)?;
        writer.u32(flags);
        self.transport
            .request(PROC_DOMAIN_REBOOT, writer.into_inner())
            .await?;
        Ok(())
    }

    async fn domain_destroy(&self, domain: &Domain) -> Result<(), ApiError> {
        let mut writer = Writer::new();
        encode_domain(&mut writer, domain)?;
        self.transport
            .request(PROC_DOMAIN_DESTROY, writer.into_inner())
            .await?;
        Ok(())
    }

    async fn domain_undefine_flags(&self, domain: &Domain, flags: u32) -> Result<(), ApiError> {
        let mut writer = Writer::new();
        encode_domain(&mut writer, domain)?;
        writer.u32(flags);
        self.transport
            .request(PROC_DOMAIN_UNDEFINE_FLAGS, writer.into_inner())
            .await?;
        Ok(())
    }

    async fn domain_set_autostart(&self, domain: &Domain, value: i32) -> Result<(), ApiError> {
        let mut writer = Writer::new();
        encode_domain(&mut writer, domain)?;
        writer.i32(value);
        self.transport
            .request(PROC_DOMAIN_SET_AUTOSTART, writer.into_inner())
            .await?;
        Ok(())
    }

    async fn domain_lookup_by_name(&self, name: &str) -> Result<Domain, ApiError> {
        let mut writer = Writer::new();
        writer.string(name)?;
        let payload = self
            .transport
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
            .transport
            .request(PROC_DOMAIN_GET_STATE, writer.into_inner())
            .await?;
        let mut reader = Reader::new(&payload);
        let result = (reader.i32()?, reader.i32()?);
        reader.finish()?;
        Ok(result)
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
            .transport
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
            .transport
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
            .transport
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
        self.transport
            .request(PROC_NETWORK_CREATE, writer.into_inner())
            .await?;
        Ok(())
    }

    async fn network_is_active(&self, network: &Network) -> Result<i32, ApiError> {
        let mut writer = Writer::new();
        encode_network(&mut writer, network)?;
        let payload = self
            .transport
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
        self.transport
            .request(PROC_NETWORK_SET_AUTOSTART, writer.into_inner())
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
        api.close().await.unwrap();
    }
}
