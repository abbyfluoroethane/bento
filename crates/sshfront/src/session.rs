use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bento_types::{Instance, State, User};
use bytes::Bytes;
use russh::keys::PrivateKeyWithHashAlg;
use russh::server::{Handle, Msg};
use russh::{Channel, ChannelId, ChannelMsg, Disconnect, Pty};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::server::{GuestClient, Registration, Server};
use crate::{DEFAULT_DIAL_INTERVAL, DEFAULT_START_TIMEOUT};

type SessionInput = Pin<Box<dyn AsyncRead + Send>>;
type SessionOutput = Pin<Box<dyn AsyncWrite + Send>>;

#[derive(Clone, Debug)]
pub(crate) enum Authenticated {
    User(User),
    Registration(Registration),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Window {
    col_width: u32,
    row_height: u32,
    pix_width: u32,
    pix_height: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PtyRequest {
    term: String,
    window: Window,
}

#[async_trait]
trait SessionExit: Send + Sync {
    async fn exit(&self, code: u32);
}

struct SessionParts {
    user: String,
    raw_command: Vec<u8>,
    command: Vec<String>,
    pty: Option<PtyRequest>,
    windows: mpsc::Receiver<Window>,
    stdin: SessionInput,
    stdout: SessionOutput,
    stderr: SessionOutput,
    exit: Arc<dyn SessionExit>,
}

/// The narrow terminal-session seam used by dispatch. Tests provide an
/// in-memory implementation, while the real implementation owns one russh
/// channel and its three byte streams.
trait TermSession: Send {
    fn into_parts(self: Box<Self>) -> SessionParts;
}

struct RusshTermSession(SessionParts);

impl TermSession for RusshTermSession {
    fn into_parts(self: Box<Self>) -> SessionParts {
        self.0
    }
}

struct RusshExit {
    handle: Handle,
    channel: ChannelId,
}

#[async_trait]
impl SessionExit for RusshExit {
    async fn exit(&self, code: u32) {
        let _ = self.handle.exit_status_request(self.channel, code).await;
        let _ = self.handle.eof(self.channel).await;
        let _ = self.handle.close(self.channel).await;
    }
}

pub(crate) async fn process_channel(
    server: Server,
    authenticated: Authenticated,
    user_name: String,
    mut channel: Channel<Msg>,
    handle: Handle,
) {
    let mut pty = None;
    let mut pending_data = Vec::new();

    let raw_command = loop {
        match channel.wait().await {
            Some(ChannelMsg::RequestPty {
                term,
                col_width,
                row_height,
                pix_width,
                pix_height,
                ..
            }) => {
                pty = Some(PtyRequest {
                    term,
                    window: Window {
                        col_width,
                        row_height,
                        pix_width,
                        pix_height,
                    },
                });
            }
            Some(ChannelMsg::RequestShell { .. }) => break Vec::new(),
            Some(ChannelMsg::Exec { command, .. }) => break command,
            Some(ChannelMsg::Data { data }) => pending_data.push(data),
            Some(ChannelMsg::Eof | ChannelMsg::Close) | None => return,
            _ => {}
        }
    };

    let channel_id = channel.id();
    let stdout: SessionOutput = Box::pin(channel.make_writer());
    let stderr: SessionOutput = Box::pin(channel.make_writer_ext(Some(1)));
    let (mut input_writer, input_reader) = tokio::io::duplex(64 * 1024);
    let (window_sender, window_receiver) = mpsc::channel(16);

    tokio::spawn(async move {
        for data in pending_data {
            if input_writer.write_all(&data).await.is_err() {
                return;
            }
        }
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } => {
                    if input_writer.write_all(&data).await.is_err() {
                        return;
                    }
                }
                ChannelMsg::WindowChange {
                    col_width,
                    row_height,
                    pix_width,
                    pix_height,
                } => {
                    let _ = window_sender
                        .send(Window {
                            col_width,
                            row_height,
                            pix_width,
                            pix_height,
                        })
                        .await;
                }
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
        let _ = input_writer.shutdown().await;
    });

    let term = RusshTermSession(SessionParts {
        user: user_name,
        command: split_command(&String::from_utf8_lossy(&raw_command)),
        raw_command,
        pty,
        windows: window_receiver,
        stdin: Box::pin(input_reader),
        stdout,
        stderr,
        exit: Arc::new(RusshExit {
            handle,
            channel: channel_id,
        }),
    });
    dispatch(&server, authenticated, Box::new(term)).await;
}

/// Routes one session. An accessible instance name is forwarded; every other
/// known-user session runs the CLI. This uniform fallback both accommodates a
/// stock client's unavoidable local login name and reveals nothing about
/// which instance names exist. Unknown keys always run registration (SPEC 13).
async fn dispatch(server: &Server, authenticated: Authenticated, session: Box<dyn TermSession>) {
    let parts = session.into_parts();
    match authenticated {
        Authenticated::User(user) => {
            if let Some(instance) = resolve_instance(server, &parts.user, user.id).await {
                proxy(server, instance, parts).await;
            } else {
                let SessionParts {
                    command,
                    stdin,
                    stdout,
                    stderr,
                    exit,
                    ..
                } = parts;
                let code = server.cli.run(user, command, stdin, stdout, stderr).await;
                exit.exit(exit_code(code)).await;
            }
        }
        Authenticated::Registration(registration) => {
            register(server, registration, parts).await;
        }
    }
}

/// Implements SPEC 10 steps 4-6: the SSH user name is the instance name,
/// resolved to a UUID, and the connecting user must own it or hold a share on
/// that UUID.
async fn resolve_instance(server: &Server, name: &str, user_id: i64) -> Option<Instance> {
    if name.is_empty() {
        return None;
    }
    let instance = server.instances.instance_by_name(name).await.ok()?;
    match server.instances.has_access(&instance.uuid, user_id).await {
        Ok(true) => Some(instance),
        Ok(false) | Err(_) => None,
    }
}

/// Implements SPEC 10 steps 7-10 for one resolved connection.
async fn proxy(server: &Server, instance: Instance, mut session: SessionParts) {
    // Step 7 starts a stopped instance without changing desired state. The
    // Starter interface has no operation that could change it (SPEC 11.2).
    if instance.state == State::Stopped {
        let _ = session
            .stdout
            .write_all(format!("bento: starting {}\r\n", instance.name).as_bytes())
            .await;
        if let Err(error) = server.starter.start_instance(instance.clone()).await {
            let _ = session
                .stderr
                .write_all(
                    format!("bento: starting {} failed: {error}\r\n", instance.name).as_bytes(),
                )
                .await;
            session.exit.exit(1).await;
            return;
        }
    }

    // Step 8 waits for sshd in the guest, for 120 seconds by default.
    let address = guest_address(&instance.address);
    let timeout = effective_timeout(server.start_timeout);
    let interval = effective_interval(server.dial_interval);
    let connection = match wait_ssh(server, &address, timeout, interval).await {
        Ok(connection) => connection,
        Err(error) => {
            let _ = session
                .stderr
                .write_all(
                    format!(
                        "bento: {} did not accept an SSH connection within {}: {error}\r\n",
                        instance.name,
                        display_duration(timeout)
                    )
                    .as_bytes(),
                )
                .await;
            session.exit.exit(1).await;
            return;
        }
    };

    // SPEC 12: last_seen_at records the last SSH connection.
    let _ = server.instances.touch_last_seen(&instance.uuid).await;

    // Steps 9 and 10 open the guest session and join both directions.
    join(server, &instance, connection, &address, session).await;
}

fn guest_address(address: &str) -> String {
    match address.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(_)) => format!("[{address}]:22"),
        Ok(std::net::IpAddr::V4(_)) | Err(_) => format!("{address}:22"),
    }
}

fn effective_timeout(value: Duration) -> Duration {
    if value.is_zero() {
        DEFAULT_START_TIMEOUT
    } else {
        value
    }
}

fn effective_interval(value: Duration) -> Duration {
    if value.is_zero() {
        DEFAULT_DIAL_INTERVAL
    } else {
        value
    }
}

/// Dials until the address accepts a connection or the timeout budget is
/// exhausted. The successful stream is reused for the SSH handshake.
async fn wait_ssh(
    server: &Server,
    address: &str,
    timeout: Duration,
    interval: Duration,
) -> io::Result<crate::BoxedIo> {
    let mut elapsed = Duration::ZERO;
    loop {
        let remaining = timeout.saturating_sub(elapsed);
        let started = tokio::time::Instant::now();
        let attempt = tokio::time::timeout(remaining, server.dialer.dial(address)).await;
        elapsed = elapsed.saturating_add(started.elapsed());
        match attempt {
            Ok(Ok(connection)) => return Ok(connection),
            Ok(Err(error)) => {
                if elapsed.saturating_add(interval) >= timeout {
                    return Err(error);
                }
            }
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "connection timed out",
                ));
            }
        }
        tokio::time::sleep(interval).await;
        elapsed = elapsed.saturating_add(interval);
    }
}

async fn join(
    server: &Server,
    instance: &Instance,
    connection: crate::BoxedIo,
    _address: &str,
    mut session: SessionParts,
) {
    let config = Arc::new(russh::client::Config::default());
    let mut client = match tokio::time::timeout(
        Duration::from_secs(10),
        russh::client::connect_stream(config, connection, GuestClient),
    )
    .await
    {
        Ok(Ok(client)) => client,
        Ok(Err(error)) => {
            join_error(&mut session, instance, "connecting to", error).await;
            return;
        }
        Err(_) => {
            join_error(
                &mut session,
                instance,
                "connecting to",
                io::Error::new(io::ErrorKind::TimedOut, "SSH handshake timed out"),
            )
            .await;
            return;
        }
    };

    let key: PrivateKeyWithHashAlg = match server.guest_auth_key(&client).await {
        Ok(key) => key,
        Err(error) => {
            join_error(&mut session, instance, "connecting to", error).await;
            return;
        }
    };
    match client
        .authenticate_publickey(server.guest_user.clone(), key)
        .await
    {
        Ok(result) if result.success() => {}
        Ok(_) => {
            join_error(
                &mut session,
                instance,
                "connecting to",
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "public key authentication failed",
                ),
            )
            .await;
            return;
        }
        Err(error) => {
            join_error(&mut session, instance, "connecting to", error).await;
            return;
        }
    }

    let mut channel = match client.channel_open_session().await {
        Ok(channel) => channel,
        Err(error) => {
            join_error(&mut session, instance, "opening a session on", error).await;
            return;
        }
    };

    if let Some(pty) = &session.pty
        && let Err(error) = channel
            .request_pty(
                true,
                &pty.term,
                pty.window.col_width,
                pty.window.row_height,
                pty.window.pix_width,
                pty.window.pix_height,
                &[(Pty::ECHO, 1)],
            )
            .await
    {
        join_error(&mut session, instance, "pty request on", error).await;
        return;
    }
    if session.pty.is_some() && wait_request_reply(&mut channel).await.is_err() {
        join_error(
            &mut session,
            instance,
            "pty request on",
            io::Error::new(io::ErrorKind::PermissionDenied, "request rejected"),
        )
        .await;
        return;
    }

    let request = if session.raw_command.is_empty() {
        channel.request_shell(true).await
    } else {
        channel.exec(true, session.raw_command.clone()).await
    };
    if let Err(error) = request {
        join_error(&mut session, instance, "running the command on", error).await;
        return;
    }
    if wait_request_reply(&mut channel).await.is_err() {
        join_error(
            &mut session,
            instance,
            "running the command on",
            io::Error::new(io::ErrorKind::PermissionDenied, "request rejected"),
        )
        .await;
        return;
    }

    let (mut guest_read, guest_write) = channel.split();
    let mut stdin_open = true;
    let mut windows_open = session.pty.is_some();
    let mut input_buffer = vec![0_u8; 32 * 1024];
    let mut code = 0;

    loop {
        tokio::select! {
            read = session.stdin.read(&mut input_buffer), if stdin_open => {
                match read {
                    Ok(0) => {
                        stdin_open = false;
                        let _ = guest_write.eof().await;
                    }
                    Ok(count) => {
                        if guest_write.data_bytes(Bytes::copy_from_slice(&input_buffer[..count])).await.is_err() {
                            code = 1;
                            break;
                        }
                    }
                    Err(_) => {
                        stdin_open = false;
                        let _ = guest_write.eof().await;
                    }
                }
            }
            window = session.windows.recv(), if windows_open => {
                if let Some(window) = window {
                    let _ = guest_write.window_change(
                        window.col_width,
                        window.row_height,
                        window.pix_width,
                        window.pix_height,
                    ).await;
                } else {
                    windows_open = false;
                }
            }
            message = guest_read.wait() => {
                match message {
                    Some(ChannelMsg::Data { data }) => {
                        let _ = session.stdout.write_all(&data).await;
                    }
                    Some(ChannelMsg::ExtendedData { ext: 1, data }) => {
                        let _ = session.stderr.write_all(&data).await;
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        code = exit_status;
                        break;
                    }
                    Some(ChannelMsg::ExitSignal { .. }) => {
                        code = 1;
                        break;
                    }
                    Some(ChannelMsg::Close) | None => break,
                    _ => {}
                }
            }
        }
    }

    let _ = client
        .disconnect(Disconnect::ByApplication, "", "English")
        .await;
    session.exit.exit(code).await;
}

async fn wait_request_reply(channel: &mut Channel<russh::client::Msg>) -> Result<(), ()> {
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => return Ok(()),
            Some(ChannelMsg::Failure | ChannelMsg::Close) | None => return Err(()),
            _ => {}
        }
    }
}

async fn join_error(
    session: &mut SessionParts,
    instance: &Instance,
    operation: &str,
    error: impl std::fmt::Display,
) {
    let _ = session
        .stderr
        .write_all(format!("bento: {operation} {} failed: {error}\r\n", instance.name).as_bytes())
        .await;
    session.exit.exit(1).await;
}

/// Runs the SPEC 13 flow for an unknown key: record the key, then ask for a
/// name and email address. The Registrar allocates the subnet and network.
async fn register(server: &Server, mut registration: Registration, mut session: SessionParts) {
    let echo = session.pty.is_some();
    let _ = session
        .stdout
        .write_all(
            format!(
                "bento: this key is not registered ({})\r\n",
                registration.fingerprint
            )
            .as_bytes(),
        )
        .await;
    let _ = session
        .stdout
        .write_all(b"bento: answer two questions to create an account\r\n")
        .await;

    let name = match prompt_valid(
        &mut session.stdin,
        &mut session.stdout,
        "account name: ",
        echo,
        validate_account_name,
    )
    .await
    {
        Ok(name) => name,
        Err(_) => {
            session.exit.exit(1).await;
            return;
        }
    };
    let email = match prompt_valid(
        &mut session.stdin,
        &mut session.stdout,
        "email: ",
        echo,
        validate_email,
    )
    .await
    {
        Ok(email) => email,
        Err(_) => {
            session.exit.exit(1).await;
            return;
        }
    };

    registration.name = name;
    registration.email = email;
    let Some(registrar) = &server.registrar else {
        session.exit.exit(1).await;
        return;
    };
    match registrar.register(registration).await {
        Ok(user) => {
            let _ = session
                .stdout
                .write_all(
                    format!(
                        "bento: registered {}, subnet {}\r\n",
                        user.name, user.subnet
                    )
                    .as_bytes(),
                )
                .await;
            let _ = session
                .stdout
                .write_all(
                    b"bento: reconnect for the command line; run \"help\" for the command list\r\n",
                )
                .await;
            session.exit.exit(0).await;
        }
        Err(error) => {
            let _ = session
                .stderr
                .write_all(format!("bento: registration failed: {error}\r\n").as_bytes())
                .await;
            session.exit.exit(1).await;
        }
    }
}

const REGISTRATION_ATTEMPTS: usize = 5;

async fn prompt_valid(
    input: &mut SessionInput,
    output: &mut SessionOutput,
    prompt: &str,
    echo: bool,
    validate: fn(&str) -> Result<(), &'static str>,
) -> io::Result<String> {
    for _ in 0..REGISTRATION_ATTEMPTS {
        output.write_all(prompt.as_bytes()).await?;
        let line = read_line(input, output, echo).await?;
        let line = line.trim().to_owned();
        if let Err(message) = validate(&line) {
            output
                .write_all(format!("bento: {message}\r\n").as_bytes())
                .await?;
            continue;
        }
        return Ok(line);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "too many attempts",
    ))
}

fn validate_account_name(value: &str) -> Result<(), &'static str> {
    let valid = (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric);
    if valid {
        Ok(())
    } else {
        Err("an account name uses lowercase letters, digits, and inner hyphens")
    }
}

fn validate_email(value: &str) -> Result<(), &'static str> {
    if value
        .split_once('@')
        .is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty())
    {
        Ok(())
    } else {
        Err("that does not look like an email address")
    }
}

/// Reads one line byte by byte, handling PTY echo, backspace, and cancellation
/// with control-C or control-D.
async fn read_line(
    input: &mut SessionInput,
    output: &mut SessionOutput,
    echo: bool,
) -> io::Result<String> {
    let mut buffer = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match input.read(&mut byte).await {
            Ok(0) if !buffer.is_empty() => return Ok(String::from_utf8_lossy(&buffer).into_owned()),
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(_) => match byte[0] {
                b'\r' | b'\n' => {
                    if echo {
                        output.write_all(b"\r\n").await?;
                    }
                    return Ok(String::from_utf8_lossy(&buffer).into_owned());
                }
                0x7f | 0x08 => {
                    if buffer.pop().is_some() && echo {
                        output.write_all(b"\x08 \x08").await?;
                    }
                }
                0x03 | 0x04 => {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                }
                value => {
                    buffer.push(value);
                    if echo {
                        output.write_all(&byte).await?;
                    }
                }
            },
            Err(error) => return Err(error),
        }
    }
}

fn exit_code(code: i32) -> u32 {
    u32::try_from(code).unwrap_or(1)
}

fn display_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if duration.subsec_nanos() == 0 && seconds >= 60 {
        format!("{}m{}s", seconds / 60, seconds % 60)
    } else if duration.subsec_nanos() == 0 {
        format!("{seconds}s")
    } else {
        format!("{}ms", duration.as_millis())
    }
}

fn split_command(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;

    for character in command.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            started = true;
            continue;
        }
        match (quote, character) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (None, '\'' | '"') => {
                quote = Some(character);
                started = true;
            }
            (Some('\''), _) => {
                current.push(character);
                started = true;
            }
            (_, '\\') => escaped = true,
            (None, value) if value.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (_, value) => {
                current.push(value);
                started = true;
            }
        }
    }
    if escaped {
        current.push('\\');
    }
    if started {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use std::time::Duration;

    use async_trait::async_trait;
    use bento_store::Error as StoreError;
    use bento_types::{DesiredState, Instance, SshKey, State, User, Visibility};
    use russh::keys::{Algorithm, PrivateKey};
    use time::OffsetDateTime;
    use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
    use tokio::sync::mpsc;

    use super::{
        Authenticated, PtyRequest, SessionExit, SessionParts, TermSession, Window, dispatch,
        read_line, wait_ssh,
    };
    use crate::server::{
        BoxError, BoxedIo, CLIRunner, Dialer, InstanceStore, KeyStore, Registrar, Registration,
        Server, Starter,
    };

    #[derive(Default)]
    struct FakeKeys;

    #[async_trait]
    impl KeyStore for FakeKeys {
        async fn ssh_key_by_fingerprint(&self, _fingerprint: &str) -> bento_store::Result<SshKey> {
            Err(StoreError::NotFound)
        }

        async fn user_by_id(&self, _id: i64) -> bento_store::Result<User> {
            Err(StoreError::NotFound)
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
    struct FakeStarter {
        started: Mutex<Vec<String>>,
        error: Mutex<Option<String>>,
    }

    #[async_trait]
    impl Starter for FakeStarter {
        async fn start_instance(&self, instance: Instance) -> Result<(), BoxError> {
            if let Some(error) = self.error.lock().unwrap().clone() {
                return Err(error.into());
            }
            self.started.lock().unwrap().push(instance.name);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeCli {
        user: Mutex<Option<User>>,
        args: Mutex<Option<Vec<String>>>,
        code: i32,
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
            self.code
        }
    }

    #[derive(Default)]
    struct FakeRegistrar {
        got: Mutex<Option<Registration>>,
        user: Mutex<Option<User>>,
        error: Mutex<Option<String>>,
    }

    #[async_trait]
    impl Registrar for FakeRegistrar {
        async fn register(&self, registration: Registration) -> Result<User, BoxError> {
            if let Some(error) = self.error.lock().unwrap().clone() {
                return Err(error.into());
            }
            *self.got.lock().unwrap() = Some(registration);
            Ok(self.user.lock().unwrap().clone().unwrap())
        }
    }

    struct RefusingDialer {
        attempts: AtomicUsize,
        succeed_on: usize,
        stream: Mutex<Option<BoxedIo>>,
    }

    #[async_trait]
    impl Dialer for RefusingDialer {
        async fn dial(&self, _address: &str) -> io::Result<BoxedIo> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt == self.succeed_on {
                return self.stream.lock().unwrap().take().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused")
                });
            }
            Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "connection refused",
            ))
        }
    }

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl AsyncWrite for SharedWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.0.lock().unwrap().extend_from_slice(data);
            Poll::Ready(Ok(data.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Default)]
    struct FakeExit(Mutex<Option<u32>>);

    #[async_trait]
    impl SessionExit for FakeExit {
        async fn exit(&self, code: u32) {
            *self.0.lock().unwrap() = Some(code);
        }
    }

    struct FakeTerm(Option<SessionParts>);

    impl TermSession for FakeTerm {
        fn into_parts(mut self: Box<Self>) -> SessionParts {
            self.0.take().unwrap()
        }
    }

    struct FakeSession {
        term: Box<dyn TermSession>,
        stdout: SharedWriter,
        stderr: SharedWriter,
        exit: Arc<FakeExit>,
    }

    async fn fake_session(user: &str, input: &str, pty: bool) -> FakeSession {
        let (mut input_writer, input_reader) = tokio::io::duplex(4096);
        input_writer.write_all(input.as_bytes()).await.unwrap();
        input_writer.shutdown().await.unwrap();
        let stdout = SharedWriter::default();
        let stderr = SharedWriter::default();
        let exit = Arc::new(FakeExit::default());
        let (_window_tx, window_rx) = mpsc::channel(1);
        let parts = SessionParts {
            user: user.to_owned(),
            raw_command: Vec::new(),
            command: Vec::new(),
            pty: pty.then(|| PtyRequest {
                term: "xterm".to_owned(),
                window: Window {
                    col_width: 80,
                    row_height: 24,
                    pix_width: 0,
                    pix_height: 0,
                },
            }),
            windows: window_rx,
            stdin: Box::pin(input_reader),
            stdout: Box::pin(stdout.clone()),
            stderr: Box::pin(stderr.clone()),
            exit: exit.clone(),
        };
        FakeSession {
            term: Box::new(FakeTerm(Some(parts))),
            stdout,
            stderr,
            exit,
        }
    }

    fn user(name: &str) -> User {
        User {
            id: 1,
            name: name.to_owned(),
            email: format!("{name}@example.com"),
            oidc_subject: None,
            subnet: "10.100.0.0/24".to_owned(),
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn web_instance(state: State) -> Instance {
        Instance {
            uuid: "uuid-web".to_owned(),
            name: "web".to_owned(),
            owner_id: 1,
            host_id: 1,
            image_name: "debian".to_owned(),
            base_checksum: "abc".to_owned(),
            state,
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

    fn test_key() -> Arc<PrivateKey> {
        Arc::new(PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap())
    }

    fn test_server(
        instances: Arc<FakeInstances>,
        starter: Arc<FakeStarter>,
        cli: Arc<FakeCli>,
    ) -> Server {
        let mut server = Server::new(
            Arc::new(FakeKeys),
            instances,
            starter,
            cli,
            test_key(),
            test_key(),
        );
        server.start_timeout = Duration::from_secs(120);
        server.dial_interval = Duration::from_secs(120);
        server.dialer = Arc::new(RefusingDialer {
            attempts: AtomicUsize::new(0),
            succeed_on: usize::MAX,
            stream: Mutex::new(None),
        });
        server
    }

    #[tokio::test]
    async fn dispatch_runs_cli_for_every_inaccessible_name() {
        for (name, no_access) in [("", false), ("shaun", false), ("web", true)] {
            let instances = Arc::new(FakeInstances::default());
            instances
                .by_name
                .lock()
                .unwrap()
                .insert("web".to_owned(), web_instance(State::Running));
            if !no_access {
                instances
                    .access
                    .lock()
                    .unwrap()
                    .insert("uuid-web".to_owned(), vec![1]);
            }
            let starter = Arc::new(FakeStarter::default());
            let cli = Arc::new(FakeCli {
                code: 3,
                ..FakeCli::default()
            });
            let server = test_server(instances, starter.clone(), cli.clone());
            let mut fake = fake_session(name, "", false).await;
            let mut parts = fake.term.into_parts();
            parts.command = vec!["ls".to_owned()];
            fake.term = Box::new(FakeTerm(Some(parts)));
            dispatch(&server, Authenticated::User(user("frank")), fake.term).await;
            assert_eq!(cli.user.lock().unwrap().as_ref().unwrap().name, "frank");
            assert_eq!(cli.args.lock().unwrap().as_ref().unwrap(), &["ls"]);
            assert_eq!(*fake.exit.0.lock().unwrap(), Some(3));
            assert!(starter.started.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn dispatch_forwards_an_accessible_instance() {
        let instances = Arc::new(FakeInstances::default());
        instances
            .by_name
            .lock()
            .unwrap()
            .insert("web".to_owned(), web_instance(State::Running));
        instances
            .access
            .lock()
            .unwrap()
            .insert("uuid-web".to_owned(), vec![1]);
        let cli = Arc::new(FakeCli::default());
        let server = test_server(instances, Arc::new(FakeStarter::default()), cli.clone());
        let fake = fake_session("web", "", false).await;
        dispatch(&server, Authenticated::User(user("frank")), fake.term).await;
        assert!(cli.args.lock().unwrap().is_none());
        assert!(
            fake.stderr
                .text()
                .contains("did not accept an SSH connection")
        );
    }

    #[tokio::test]
    async fn proxy_starts_a_stopped_instance_and_times_out_clearly() {
        let instances = Arc::new(FakeInstances::default());
        let starter = Arc::new(FakeStarter::default());
        let server = test_server(
            instances.clone(),
            starter.clone(),
            Arc::new(FakeCli::default()),
        );
        instances
            .by_name
            .lock()
            .unwrap()
            .insert("web".to_owned(), web_instance(State::Stopped));
        instances
            .access
            .lock()
            .unwrap()
            .insert("uuid-web".to_owned(), vec![1]);
        let fake = fake_session("web", "", false).await;
        dispatch(&server, Authenticated::User(user("frank")), fake.term).await;
        assert_eq!(starter.started.lock().unwrap().as_slice(), &["web"]);
        assert!(fake.stdout.text().contains("bento: starting web"));
        assert!(
            fake.stderr
                .text()
                .contains("did not accept an SSH connection within 2m0s")
        );
        assert_eq!(*fake.exit.0.lock().unwrap(), Some(1));
        assert!(instances.touched.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn running_instance_does_not_start() {
        let instances = Arc::new(FakeInstances::default());
        instances
            .by_name
            .lock()
            .unwrap()
            .insert("web".to_owned(), web_instance(State::Running));
        instances
            .access
            .lock()
            .unwrap()
            .insert("uuid-web".to_owned(), vec![1]);
        let starter = Arc::new(FakeStarter::default());
        let server = test_server(instances, starter.clone(), Arc::new(FakeCli::default()));
        let fake = fake_session("web", "", false).await;
        dispatch(&server, Authenticated::User(user("frank")), fake.term).await;
        assert!(starter.started.lock().unwrap().is_empty());
        assert!(!fake.stdout.text().contains("starting"));
    }

    #[tokio::test]
    async fn wait_ssh_retries_then_reuses_the_successful_stream() {
        let (client, _server_end) = tokio::io::duplex(32);
        let dialer = Arc::new(RefusingDialer {
            attempts: AtomicUsize::new(0),
            succeed_on: 3,
            stream: Mutex::new(Some(Box::pin(client))),
        });
        let instances = Arc::new(FakeInstances::default());
        let mut server = test_server(
            instances,
            Arc::new(FakeStarter::default()),
            Arc::new(FakeCli::default()),
        );
        server.dialer = dialer.clone();
        let result = wait_ssh(
            &server,
            "10.0.0.2:22",
            Duration::from_millis(120),
            Duration::from_millis(1),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(dialer.attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn wait_ssh_times_out_without_an_extra_attempt() {
        let dialer = Arc::new(RefusingDialer {
            attempts: AtomicUsize::new(0),
            succeed_on: usize::MAX,
            stream: Mutex::new(None),
        });
        let mut server = test_server(
            Arc::new(FakeInstances::default()),
            Arc::new(FakeStarter::default()),
            Arc::new(FakeCli::default()),
        );
        server.dialer = dialer.clone();
        assert!(
            wait_ssh(
                &server,
                "10.0.0.2:22",
                Duration::from_millis(10),
                Duration::from_millis(2),
            )
            .await
            .is_err()
        );
        assert_eq!(dialer.attempts.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn registration_records_answers_and_preserves_the_key() {
        let registrar = Arc::new(FakeRegistrar::default());
        *registrar.user.lock().unwrap() = Some(User {
            id: 9,
            subnet: "10.100.7.0/24".to_owned(),
            ..user("carol")
        });
        let mut server = test_server(
            Arc::new(FakeInstances::default()),
            Arc::new(FakeStarter::default()),
            Arc::new(FakeCli::default()),
        );
        server.registrar = Some(registrar.clone());
        let fake = fake_session("", "carol\ncarol@example.com\n", false).await;
        dispatch(
            &server,
            Authenticated::Registration(Registration {
                public_key: "ssh-ed25519 AAAA carol@laptop".to_owned(),
                fingerprint: "SHA256:abcdef".to_owned(),
                ..Registration::default()
            }),
            fake.term,
        )
        .await;
        let got = registrar.got.lock().unwrap().clone().unwrap();
        assert_eq!(got.name, "carol");
        assert_eq!(got.email, "carol@example.com");
        assert_eq!(got.public_key, "ssh-ed25519 AAAA carol@laptop");
        assert_eq!(got.fingerprint, "SHA256:abcdef");
        assert!(fake.stdout.text().contains("registered carol"));
        assert!(fake.stdout.text().contains("10.100.7.0/24"));
        assert_eq!(*fake.exit.0.lock().unwrap(), Some(0));
    }

    #[tokio::test]
    async fn registration_retries_invalid_input() {
        let registrar = Arc::new(FakeRegistrar::default());
        *registrar.user.lock().unwrap() = Some(User {
            subnet: "10.100.7.0/24".to_owned(),
            ..user("carol")
        });
        let mut server = test_server(
            Arc::new(FakeInstances::default()),
            Arc::new(FakeStarter::default()),
            Arc::new(FakeCli::default()),
        );
        server.registrar = Some(registrar.clone());
        let fake = fake_session(
            "",
            "Carol Smith\ncarol\nnot-an-email\ncarol@example.com\n",
            false,
        )
        .await;
        dispatch(
            &server,
            Authenticated::Registration(Registration::default()),
            fake.term,
        )
        .await;
        let got = registrar.got.lock().unwrap().clone().unwrap();
        assert_eq!(got.name, "carol");
        assert_eq!(got.email, "carol@example.com");
        assert!(fake.stdout.text().contains("lowercase"));
        assert!(fake.stdout.text().contains("email address"));
    }

    #[tokio::test]
    async fn registration_failure_is_reported() {
        let registrar = Arc::new(FakeRegistrar::default());
        *registrar.error.lock().unwrap() = Some("subnets exhausted".to_owned());
        let mut server = test_server(
            Arc::new(FakeInstances::default()),
            Arc::new(FakeStarter::default()),
            Arc::new(FakeCli::default()),
        );
        server.registrar = Some(registrar);
        let fake = fake_session("", "carol\ncarol@example.com\n", false).await;
        dispatch(
            &server,
            Authenticated::Registration(Registration::default()),
            fake.term,
        )
        .await;
        assert_eq!(*fake.exit.0.lock().unwrap(), Some(1));
        assert!(fake.stderr.text().contains("registration failed"));
    }

    #[tokio::test]
    async fn read_line_handles_newlines_backspace_eof_and_cancel() {
        for (input, echo, expected, echoed) in [
            ("abc\n", false, Some("abc"), ""),
            ("abc\r", false, Some("abc"), ""),
            ("abd\x7fc\r", true, Some("abc"), "abd\x08 \x08c\r\n"),
            ("abc", false, Some("abc"), ""),
            ("ab\x03", false, None, ""),
        ] {
            let fake = fake_session("", input, false).await;
            let mut parts = fake.term.into_parts();
            let result = read_line(&mut parts.stdin, &mut parts.stdout, echo).await;
            match expected {
                Some(expected) => assert_eq!(result.unwrap(), expected),
                None => assert!(result.is_err()),
            }
            assert_eq!(fake.stdout.text(), echoed);
        }
    }

    #[test]
    fn command_words_follow_shell_quoting() {
        assert_eq!(
            super::split_command("new 'my vm' --image=debian\\ stable"),
            ["new", "my vm", "--image=debian stable"]
        );
    }
}
