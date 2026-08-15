use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bento_store::Error as StoreError;
use bento_types::{Instance, SshKey, User};
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};

use crate::session::{Authenticated, process_channel};
use crate::{DEFAULT_DIAL_INTERVAL, DEFAULT_GUEST_USER, DEFAULT_START_TIMEOUT};

/// An error returned by a consumer-side operation.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A bidirectional byte stream returned by [`Dialer`].
pub type BoxedIo = Pin<Box<dyn AsyncReadWrite + Send>>;

/// The object-safe combination required of a dialed guest connection.
pub trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin {}

impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Unpin {}

/// Resolves a presented public key to a user. The SSH frontend looks keys up
/// by fingerprint on every connection (SPEC 12).
#[async_trait]
pub trait KeyStore: Send + Sync {
    async fn ssh_key_by_fingerprint(&self, fingerprint: &str) -> bento_store::Result<SshKey>;
    async fn user_by_id(&self, id: i64) -> bento_store::Result<User>;
}

#[async_trait]
impl KeyStore for bento_store::Store {
    async fn ssh_key_by_fingerprint(&self, fingerprint: &str) -> bento_store::Result<SshKey> {
        self.ssh_key_by_fingerprint(fingerprint).await
    }

    async fn user_by_id(&self, id: i64) -> bento_store::Result<User> {
        self.user_by_id(id).await
    }
}

/// Resolves names and authorization. This interface deliberately has no way
/// to change desired state: SPEC 10 step 7 starts an instance without
/// recording a user intent.
#[async_trait]
pub trait InstanceStore: Send + Sync {
    async fn instance_by_name(&self, name: &str) -> bento_store::Result<Instance>;
    async fn has_access(&self, instance_uuid: &str, user_id: i64) -> bento_store::Result<bool>;
    async fn touch_last_seen(&self, uuid: &str) -> bento_store::Result<()>;
}

#[async_trait]
impl InstanceStore for bento_store::Store {
    async fn instance_by_name(&self, name: &str) -> bento_store::Result<Instance> {
        self.instance_by_name(name).await
    }

    async fn has_access(&self, instance_uuid: &str, user_id: i64) -> bento_store::Result<bool> {
        self.has_access(instance_uuid, user_id).await
    }

    async fn touch_last_seen(&self, uuid: &str) -> bento_store::Result<()> {
        self.touch_last_seen(uuid).await
    }
}

/// Starts a stopped instance on behalf of an SSH connection.
///
/// Implementations must not change desired state (SPEC 10 step 7, 11.2): a
/// later host reboot returns the instance to what the user last asked for.
#[async_trait]
pub trait Starter: Send + Sync {
    async fn start_instance(&self, instance: Instance) -> Result<(), BoxError>;
}

/// Runs one command line session (SPEC 15). The three streams are owned for
/// the duration of the call and correspond to stdin, stdout, and stderr.
#[async_trait]
pub trait CLIRunner: Send + Sync {
    async fn run(
        &self,
        user: User,
        args: Vec<String>,
        stdin: Pin<Box<dyn AsyncRead + Send>>,
        stdout: Pin<Box<dyn AsyncWrite + Send>>,
        stderr: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> i32;
}

/// A new-user request from the SPEC 13 flow.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registration {
    pub name: String,
    pub email: String,
    /// One authorized_keys line, with no trailing newline.
    pub public_key: String,
    /// SHA256 fingerprint of `public_key`.
    pub fingerprint: String,
    pub comment: String,
}

/// Creates an account: user row, key row, subnet, and the libvirt network of
/// the user (SPEC 13).
#[async_trait]
pub trait Registrar: Send + Sync {
    async fn register(&self, registration: Registration) -> Result<User, BoxError>;
}

/// Dials the internal address of an instance. Tests inject a fake; production
/// uses [`TcpStream::connect`].
#[async_trait]
pub trait Dialer: Send + Sync {
    async fn dial(&self, address: &str) -> io::Result<BoxedIo>;
}

#[derive(Debug)]
struct TcpDialer;

#[async_trait]
impl Dialer for TcpDialer {
    async fn dial(&self, address: &str) -> io::Result<BoxedIo> {
        let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(address))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connection timed out"))??;
        Ok(Box::pin(stream))
    }
}

/// The SSH frontend. Construct it with [`Server::new`], adjust the public
/// runtime seams if needed, then call [`Server::listen_and_serve`] or
/// [`Server::serve`].
#[derive(Clone)]
pub struct Server {
    pub keys: Arc<dyn KeyStore>,
    pub instances: Arc<dyn InstanceStore>,
    pub starter: Arc<dyn Starter>,
    pub cli: Arc<dyn CLIRunner>,
    /// `None` disables registration and rejects unknown keys.
    pub registrar: Option<Arc<dyn Registrar>>,

    /// The one host key every frontend connection sees (SPEC 10).
    pub host_key: Arc<PrivateKey>,
    /// The frontend key installed in every guest by cloud-init (SPEC 10 step 9).
    pub guest_key: Arc<PrivateKey>,
    pub guest_user: String,

    pub dialer: Arc<dyn Dialer>,
    pub start_timeout: Duration,
    pub dial_interval: Duration,
}

impl Server {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        keys: Arc<dyn KeyStore>,
        instances: Arc<dyn InstanceStore>,
        starter: Arc<dyn Starter>,
        cli: Arc<dyn CLIRunner>,
        host_key: Arc<PrivateKey>,
        guest_key: Arc<PrivateKey>,
    ) -> Self {
        Self {
            keys,
            instances,
            starter,
            cli,
            registrar: None,
            host_key,
            guest_key,
            guest_user: DEFAULT_GUEST_USER.to_owned(),
            dialer: Arc::new(TcpDialer),
            start_timeout: DEFAULT_START_TIMEOUT,
            dial_interval: DEFAULT_DIAL_INTERVAL,
        }
    }

    fn ssh_config(&self) -> Arc<server::Config> {
        // One host key is presented for every instance, so a rename or a name
        // reuse produces no known_hosts warning. That is a consequence of the
        // design, not a defect: the authorization check protects the instance.
        Arc::new(server::Config {
            keys: vec![self.host_key.as_ref().clone()],
            ..server::Config::default()
        })
    }

    /// Listens on `address` until the listener fails.
    pub async fn listen_and_serve<A>(&self, address: A) -> io::Result<()>
    where
        A: ToSocketAddrs,
    {
        let listener = TcpListener::bind(address).await?;
        self.serve(listener).await
    }

    /// Serves on an existing listener.
    pub async fn serve(&self, listener: TcpListener) -> io::Result<()> {
        use russh::server::Server as _;

        let mut factory = self.clone();
        factory.run_on_socket(self.ssh_config(), &listener).await
    }

    /// Implements SPEC 10 steps 1-3 and the authentication half of SPEC 13.
    async fn authenticate_key(&self, public_key: &PublicKey) -> Option<Authenticated> {
        let fingerprint = public_key.fingerprint(HashAlg::Sha256).to_string();
        match self.keys.ssh_key_by_fingerprint(&fingerprint).await {
            Ok(key) => self
                .keys
                .user_by_id(key.user_id)
                .await
                .ok()
                .map(Authenticated::User),
            Err(StoreError::NotFound) if self.registrar.is_some() => {
                let public_key = public_key.to_openssh().ok()?.trim().to_owned();
                Some(Authenticated::Registration(Registration {
                    // A serialized public key is an authorized_keys line. It
                    // must be stored without its trailing newline because the
                    // cloud-init seed rejects a control character (SPEC 4.2).
                    public_key,
                    fingerprint,
                    ..Registration::default()
                }))
            }
            // A data-layer failure is not an unknown key. Reject; never fall
            // through to registration.
            Err(_) => None,
        }
    }

    pub(crate) async fn guest_auth_key(
        &self,
        client: &russh::client::Handle<GuestClient>,
    ) -> Result<PrivateKeyWithHashAlg, russh::Error> {
        let hash = if matches!(
            self.guest_key.algorithm(),
            russh::keys::Algorithm::Rsa { .. }
        ) {
            client.best_supported_rsa_hash().await?.flatten()
        } else {
            None
        };
        Ok(PrivateKeyWithHashAlg::new(
            Arc::clone(&self.guest_key),
            hash,
        ))
    }
}

impl server::Server for Server {
    type Handler = ConnectionHandler;

    fn new_client(&mut self, _peer_addr: Option<std::net::SocketAddr>) -> Self::Handler {
        ConnectionHandler {
            server: self.clone(),
            authenticated: None,
            user_name: String::new(),
        }
    }
}

#[doc(hidden)]
pub struct ConnectionHandler {
    server: Server,
    authenticated: Option<Authenticated>,
    user_name: String,
}

#[derive(Debug, thiserror::Error)]
#[doc(hidden)]
pub enum HandlerError {
    #[error(transparent)]
    Ssh(#[from] russh::Error),
}

impl server::Handler for ConnectionHandler {
    type Error = HandlerError;

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        if let Some(authenticated) = self.server.authenticate_key(public_key).await {
            self.authenticated = Some(authenticated);
            self.user_name = user.to_owned();
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(authenticated) = self.authenticated.clone() else {
            drop(reply);
            return Ok(());
        };
        let server = self.server.clone();
        let user_name = self.user_name.clone();
        let handle = session.handle();
        reply.accept().await;
        tokio::spawn(async move {
            process_channel(server, authenticated, user_name, channel, handle).await;
        });
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        Ok(())
    }
}

pub(crate) struct GuestClient;

impl russh::client::Handler for GuestClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        // The frontend reached this guest over the Bento-managed bridge at a
        // Bento-assigned address. Guest host keys are generated on first boot
        // and are not recorded, so checking one would add no authentication.
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use bento_store::Error as StoreError;
    use bento_types::{DesiredState, Instance, SshKey, State, User, Visibility};
    use bytes::Bytes;
    use russh::client;
    use russh::keys::{Algorithm, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey};
    use russh::server::{self, Auth, Msg, Session};
    use russh::{Channel, ChannelId, ChannelMsg};
    use time::OffsetDateTime;
    use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::{
        BoxError, BoxedIo, CLIRunner, Dialer, InstanceStore, KeyStore, Registrar, Registration,
        Server, Starter,
    };

    struct FakeKeys {
        by_fingerprint: Mutex<HashMap<String, SshKey>>,
        users: Mutex<HashMap<i64, User>>,
        fail: Mutex<bool>,
    }

    #[async_trait]
    impl KeyStore for FakeKeys {
        async fn ssh_key_by_fingerprint(&self, fingerprint: &str) -> bento_store::Result<SshKey> {
            if *self.fail.lock().unwrap() {
                return Err(StoreError::MutexPoisoned);
            }
            self.by_fingerprint
                .lock()
                .unwrap()
                .get(fingerprint)
                .cloned()
                .ok_or(StoreError::NotFound)
        }

        async fn user_by_id(&self, id: i64) -> bento_store::Result<User> {
            self.users
                .lock()
                .unwrap()
                .get(&id)
                .cloned()
                .ok_or(StoreError::NotFound)
        }
    }

    #[derive(Default)]
    struct FakeInstances {
        by_name: Mutex<HashMap<String, Instance>>,
        access: Mutex<HashMap<String, Vec<i64>>>,
        touched: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl InstanceStore for FakeInstances {
        async fn instance_by_name(&self, name: &str) -> bento_store::Result<Instance> {
            self.by_name
                .lock()
                .unwrap()
                .get(name)
                .cloned()
                .ok_or(StoreError::NotFound)
        }

        async fn has_access(&self, instance_uuid: &str, user_id: i64) -> bento_store::Result<bool> {
            Ok(self
                .access
                .lock()
                .unwrap()
                .get(instance_uuid)
                .is_some_and(|users| users.contains(&user_id)))
        }

        async fn touch_last_seen(&self, uuid: &str) -> bento_store::Result<()> {
            self.touched.lock().unwrap().push(uuid.to_owned());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeStarter;

    #[async_trait]
    impl Starter for FakeStarter {
        async fn start_instance(&self, _instance: Instance) -> Result<(), BoxError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeCli {
        user: Mutex<Option<User>>,
        args: Mutex<Option<Vec<String>>>,
    }

    #[async_trait]
    impl CLIRunner for FakeCli {
        async fn run(
            &self,
            user: User,
            args: Vec<String>,
            _stdin: Pin<Box<dyn AsyncRead + Send>>,
            mut stdout: Pin<Box<dyn AsyncWrite + Send>>,
            _stderr: Pin<Box<dyn AsyncWrite + Send>>,
        ) -> i32 {
            *self.user.lock().unwrap() = Some(user);
            *self.args.lock().unwrap() = Some(args);
            stdout.write_all(b"cli ran\n").await.unwrap();
            0
        }
    }

    struct FakeRegistrar;

    #[async_trait]
    impl Registrar for FakeRegistrar {
        async fn register(&self, _registration: Registration) -> Result<User, BoxError> {
            unreachable!()
        }
    }

    struct RewriteDialer(String);

    #[async_trait]
    impl Dialer for RewriteDialer {
        async fn dial(&self, _address: &str) -> io::Result<BoxedIo> {
            Ok(Box::pin(TcpStream::connect(&self.0).await?))
        }
    }

    fn private_key() -> Arc<PrivateKey> {
        Arc::new(PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap())
    }

    fn user() -> User {
        User {
            id: 1,
            name: "frank".to_owned(),
            email: "frank@example.com".to_owned(),
            oidc_subject: None,
            subnet: "10.100.0.0/24".to_owned(),
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn ssh_key(user_id: i64, fingerprint: &str) -> SshKey {
        SshKey {
            id: 1,
            user_id,
            public_key: String::new(),
            fingerprint: fingerprint.to_owned(),
            comment: String::new(),
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn instance() -> Instance {
        Instance {
            uuid: "uuid-web".to_owned(),
            name: "web".to_owned(),
            owner_id: 1,
            host_id: 1,
            image_name: "debian".to_owned(),
            base_checksum: "abc".to_owned(),
            state: State::Running,
            desired_state: DesiredState::Running,
            address: "10.100.0.2".to_owned(),
            mac: "52:54:00:00:00:02".to_owned(),
            vcpu: 2,
            memory_mib: 2048,
            disk_gib: 20,
            nested: false,
            ksm: true,
            http_port: 80,
            visibility: Visibility::Off,
            created_at: OffsetDateTime::UNIX_EPOCH,
            last_seen_at: None,
        }
    }

    fn keys_for(key: &PrivateKey) -> Arc<FakeKeys> {
        let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
        Arc::new(FakeKeys {
            by_fingerprint: Mutex::new(HashMap::from([(
                fingerprint.clone(),
                ssh_key(1, &fingerprint),
            )])),
            users: Mutex::new(HashMap::from([(1, user())])),
            fail: Mutex::new(false),
        })
    }

    fn frontend(
        client_key: &PrivateKey,
        host_key: Arc<PrivateKey>,
        guest_key: Arc<PrivateKey>,
        instances: Arc<FakeInstances>,
        cli: Arc<FakeCli>,
    ) -> Server {
        Server::new(
            keys_for(client_key),
            instances,
            Arc::new(FakeStarter),
            cli,
            host_key,
            guest_key,
        )
    }

    #[tokio::test]
    async fn public_key_authentication_handles_known_unknown_and_store_failures() {
        let known = private_key();
        let unknown = private_key();
        let host = private_key();
        let guest = private_key();
        let keys = keys_for(&known);
        let mut server = Server::new(
            keys.clone(),
            Arc::new(FakeInstances::default()),
            Arc::new(FakeStarter),
            Arc::new(FakeCli::default()),
            host,
            guest,
        );

        for username in ["web", ""] {
            let auth = server.authenticate_key(known.public_key()).await;
            assert!(
                matches!(auth, Some(super::Authenticated::User(_))),
                "{username}"
            );
        }
        for username in ["web", ""] {
            assert!(
                server
                    .authenticate_key(unknown.public_key())
                    .await
                    .is_none(),
                "{username}"
            );
        }

        server.registrar = Some(Arc::new(FakeRegistrar));
        for username in ["web", ""] {
            let auth = server.authenticate_key(unknown.public_key()).await;
            let Some(super::Authenticated::Registration(registration)) = auth else {
                panic!("unknown key did not enter registration as {username:?}");
            };
            assert_eq!(
                registration.fingerprint,
                unknown
                    .public_key()
                    .fingerprint(HashAlg::Sha256)
                    .to_string()
            );
            assert!(!registration.public_key.is_empty());
            assert_eq!(registration.public_key, registration.public_key.trim());
            assert!(!registration.public_key.contains(['\r', '\n']));
        }

        *keys.fail.lock().unwrap() = true;
        assert!(
            server
                .authenticate_key(unknown.public_key())
                .await
                .is_none()
        );
    }

    #[derive(Clone)]
    struct GuestFactory;

    impl server::Server for GuestFactory {
        type Handler = GuestHandler;

        fn new_client(&mut self, _peer_addr: Option<std::net::SocketAddr>) -> Self::Handler {
            GuestHandler
        }
    }

    struct GuestHandler;

    impl server::Handler for GuestHandler {
        type Error = russh::Error;

        async fn auth_publickey(
            &mut self,
            _user: &str,
            _public_key: &PublicKey,
        ) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn channel_open_session(
            &mut self,
            _channel: Channel<Msg>,
            reply: server::ChannelOpenHandle,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            reply.accept().await;
            Ok(())
        }

        async fn exec_request(
            &mut self,
            channel: ChannelId,
            data: &[u8],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            session.channel_success(channel)?;
            session.data(
                channel,
                Bytes::from(format!("guest ran: {}\n", String::from_utf8_lossy(data))),
            )?;
            session.exit_status_request(channel, 7)?;
            Ok(())
        }
    }

    struct ClientHandler {
        expected: Option<PublicKey>,
    }

    impl client::Handler for ClientHandler {
        type Error = russh::Error;

        async fn check_server_key(
            &mut self,
            server_public_key: &PublicKey,
        ) -> Result<bool, Self::Error> {
            Ok(self
                .expected
                .as_ref()
                .is_none_or(|key| key == server_public_key))
        }
    }

    async fn start_guest(host_key: Arc<PrivateKey>) -> (String, tokio::task::JoinHandle<()>) {
        use russh::server::Server as _;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let config = Arc::new(server::Config {
            keys: vec![host_key.as_ref().clone()],
            ..server::Config::default()
        });
        let task = tokio::spawn(async move {
            let mut guest = GuestFactory;
            let _ = guest.run_on_socket(config, &listener).await;
        });
        (address, task)
    }

    async fn start_frontend(server: Server) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let task = tokio::spawn(async move {
            let _ = server.serve(listener).await;
        });
        (address, task)
    }

    async fn connect_client(
        address: &str,
        username: &str,
        key: Arc<PrivateKey>,
        expected_host_key: Option<PublicKey>,
    ) -> (client::Handle<ClientHandler>, bool) {
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_secs(5)),
            ..client::Config::default()
        });
        let mut client = client::connect(
            config,
            address,
            ClientHandler {
                expected: expected_host_key,
            },
        )
        .await
        .unwrap();
        let hash = if matches!(key.algorithm(), Algorithm::Rsa { .. }) {
            client.best_supported_rsa_hash().await.unwrap().flatten()
        } else {
            None
        };
        let auth = client
            .authenticate_publickey(username, PrivateKeyWithHashAlg::new(key, hash))
            .await
            .unwrap();
        (client, auth.success())
    }

    async fn run_command(client: &client::Handle<ClientHandler>, command: &str) -> (Vec<u8>, u32) {
        let mut channel = client.channel_open_session().await.unwrap();
        channel.exec(true, command.as_bytes()).await.unwrap();
        let mut output = Vec::new();
        let mut status = None;
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } => output.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => {
                    status = Some(exit_status);
                    break;
                }
                _ => {}
            }
        }
        (output, status.unwrap())
    }

    #[tokio::test]
    async fn end_to_end_exec_splices_output_exit_status_and_last_seen() {
        let client_key = private_key();
        let frontend_key = private_key();
        let guest_auth_key = private_key();
        let guest_host_key = private_key();
        let (guest_address, guest_task) = start_guest(guest_host_key).await;

        let instances = Arc::new(FakeInstances::default());
        instances
            .by_name
            .lock()
            .unwrap()
            .insert("web".to_owned(), instance());
        instances
            .access
            .lock()
            .unwrap()
            .insert("uuid-web".to_owned(), vec![1]);
        let mut frontend = frontend(
            &client_key,
            frontend_key.clone(),
            guest_auth_key,
            instances.clone(),
            Arc::new(FakeCli::default()),
        );
        frontend.dialer = Arc::new(RewriteDialer(guest_address));
        let (frontend_address, frontend_task) = start_frontend(frontend).await;

        let (client, authenticated) = connect_client(
            &frontend_address,
            "web",
            client_key,
            Some(frontend_key.public_key().clone()),
        )
        .await;
        assert!(authenticated);
        let (output, status) = run_command(&client, "uname -a").await;
        assert_eq!(status, 7);
        assert_eq!(output, b"guest ran: uname -a\n");
        assert_eq!(instances.touched.lock().unwrap().as_slice(), &["uuid-web"]);

        frontend_task.abort();
        guest_task.abort();
    }

    #[derive(Clone)]
    struct PtyGuestFactory {
        events: Arc<Mutex<Vec<String>>>,
    }

    impl server::Server for PtyGuestFactory {
        type Handler = PtyGuestHandler;

        fn new_client(&mut self, _peer_addr: Option<std::net::SocketAddr>) -> Self::Handler {
            PtyGuestHandler {
                events: self.events.clone(),
                active_channel: None,
            }
        }
    }

    struct PtyGuestHandler {
        events: Arc<Mutex<Vec<String>>>,
        active_channel: Option<ChannelId>,
    }

    impl server::Handler for PtyGuestHandler {
        type Error = russh::Error;

        async fn auth_publickey(
            &mut self,
            _user: &str,
            _public_key: &PublicKey,
        ) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn channel_open_session(
            &mut self,
            _channel: Channel<Msg>,
            reply: server::ChannelOpenHandle,
            _session: &mut Session,
        ) -> Result<(), Self::Error> {
            reply.accept().await;
            Ok(())
        }

        async fn pty_request(
            &mut self,
            channel: ChannelId,
            term: &str,
            col_width: u32,
            row_height: u32,
            _pix_width: u32,
            _pix_height: u32,
            modes: &[(russh::Pty, u32)],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            self.events.lock().unwrap().push(format!(
                "pty:{term}:{col_width}x{row_height}:echo={}",
                modes.contains(&(russh::Pty::ECHO, 1))
            ));
            session.channel_success(channel)?;
            Ok(())
        }

        async fn exec_request(
            &mut self,
            channel: ChannelId,
            _data: &[u8],
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            self.active_channel = Some(channel);
            session.channel_success(channel)?;
            Ok(())
        }

        async fn window_change_request(
            &mut self,
            _channel: ChannelId,
            col_width: u32,
            row_height: u32,
            _pix_width: u32,
            _pix_height: u32,
            session: &mut Session,
        ) -> Result<(), Self::Error> {
            self.events
                .lock()
                .unwrap()
                .push(format!("window:{col_width}x{row_height}"));
            if let Some(channel) = self.active_channel {
                session.data(channel, Bytes::from_static(b"resized\n"))?;
                session.exit_status_request(channel, 0)?;
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn end_to_end_forwards_pty_and_window_changes() {
        use russh::server::Server as _;

        let events = Arc::new(Mutex::new(Vec::new()));
        let guest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let guest_address = guest_listener.local_addr().unwrap().to_string();
        let guest_config = Arc::new(server::Config {
            keys: vec![private_key().as_ref().clone()],
            ..server::Config::default()
        });
        let guest_events = events.clone();
        let guest_task = tokio::spawn(async move {
            let mut guest = PtyGuestFactory {
                events: guest_events,
            };
            let _ = guest.run_on_socket(guest_config, &guest_listener).await;
        });

        let client_key = private_key();
        let frontend_key = private_key();
        let instances = Arc::new(FakeInstances::default());
        instances
            .by_name
            .lock()
            .unwrap()
            .insert("web".to_owned(), instance());
        instances
            .access
            .lock()
            .unwrap()
            .insert("uuid-web".to_owned(), vec![1]);
        let mut frontend = frontend(
            &client_key,
            frontend_key.clone(),
            private_key(),
            instances,
            Arc::new(FakeCli::default()),
        );
        frontend.dialer = Arc::new(RewriteDialer(guest_address));
        let (frontend_address, frontend_task) = start_frontend(frontend).await;
        let (client, authenticated) = connect_client(
            &frontend_address,
            "web",
            client_key,
            Some(frontend_key.public_key().clone()),
        )
        .await;
        assert!(authenticated);

        let mut channel = client.channel_open_session().await.unwrap();
        channel
            .request_pty(true, "xterm-256color", 80, 24, 0, 0, &[])
            .await
            .unwrap();
        channel.exec(true, "watch date").await.unwrap();
        channel.window_change(132, 43, 0, 0).await.unwrap();
        let mut output = Vec::new();
        let mut status = None;
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } => output.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => {
                    status = Some(exit_status);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(status, Some(0));
        assert_eq!(output, b"resized\n");
        assert_eq!(
            events.lock().unwrap().as_slice(),
            ["pty:xterm-256color:80x24:echo=true", "window:132x43"]
        );

        frontend_task.abort();
        guest_task.abort();
    }

    #[tokio::test]
    async fn end_to_end_unknown_key_is_rejected_for_every_username() {
        let known_key = private_key();
        let unknown_key = private_key();
        let frontend_key = private_key();
        let frontend = frontend(
            &known_key,
            frontend_key,
            private_key(),
            Arc::new(FakeInstances::default()),
            Arc::new(FakeCli::default()),
        );
        let (address, task) = start_frontend(frontend).await;
        for username in ["web", ""] {
            let (_client, authenticated) =
                connect_client(&address, username, unknown_key.clone(), None).await;
            assert!(!authenticated, "unknown key authenticated as {username:?}");
        }
        task.abort();
    }

    #[tokio::test]
    async fn end_to_end_cli_accepts_a_stock_clients_username() {
        let client_key = private_key();
        let frontend_key = private_key();
        let cli = Arc::new(FakeCli::default());
        let frontend = frontend(
            &client_key,
            frontend_key.clone(),
            private_key(),
            Arc::new(FakeInstances::default()),
            cli.clone(),
        );
        let (address, task) = start_frontend(frontend).await;
        let (client, authenticated) = connect_client(
            &address,
            "shaun",
            client_key,
            Some(frontend_key.public_key().clone()),
        )
        .await;
        assert!(authenticated);
        let (output, status) = run_command(&client, "ls").await;
        assert_eq!(status, 0);
        assert_eq!(output, b"cli ran\n");
        assert_eq!(cli.user.lock().unwrap().as_ref().unwrap().name, "frank");
        assert_eq!(cli.args.lock().unwrap().as_ref().unwrap(), &["ls"]);
        task.abort();
    }
}
