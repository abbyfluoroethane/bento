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

use tokio::time::Instant;

use crate::server::{GuestClient, KeyLinker, PairingRequest, PendingLink, Server};
use crate::{DEFAULT_DIAL_INTERVAL, DEFAULT_START_TIMEOUT};

type SessionInput = Pin<Box<dyn AsyncRead + Send>>;
type SessionOutput = Pin<Box<dyn AsyncWrite + Send>>;

/// Translates bare newlines to CRLF on the way to the channel.
///
/// A client that asked for a PTY puts its own terminal into raw mode, and
/// there a bare `\n` moves down one row without returning the carriage: the
/// output arrives as a staircase running off the right of the screen. A stock
/// sshd never has to think about this because the kernel pty's `ONLCR` does
/// the translation; this frontend writes to the channel itself, so it has to.
///
/// Everything sshfront prints spells out `\r\n` already. The command
/// interpreter is the exception on purpose: it is transport-agnostic and
/// writes plain `\n`, which is what a redirect to a file should get. So the
/// translation belongs here, and only when a PTY was actually requested.
struct CrlfWriter {
    inner: SessionOutput,
    /// Translated bytes not yet accepted by the channel.
    pending: Vec<u8>,
    offset: usize,
    /// Whether the last byte written was a carriage return, so that a CRLF
    /// the interpreter already wrote is not turned into CR CR LF.
    after_cr: bool,
}

impl CrlfWriter {
    fn new(inner: SessionOutput) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            offset: 0,
            after_cr: false,
        }
    }

    /// Pushes buffered bytes into the channel until it takes them all.
    fn poll_drain(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<io::Result<()>> {
        use std::task::Poll;
        while self.offset < self.pending.len() {
            match self
                .inner
                .as_mut()
                .poll_write(cx, &self.pending[self.offset..])
            {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
                Poll::Ready(Ok(written)) => self.offset += written,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            }
        }
        self.pending.clear();
        self.offset = 0;
        std::task::Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for CrlfWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        use std::task::Poll;
        // Anything left from a previous call goes first, to keep the order.
        if let Poll::Ready(Err(error)) = self.poll_drain(cx) {
            return Poll::Ready(Err(error));
        }
        if !self.pending.is_empty() {
            return Poll::Pending;
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        for &byte in buf {
            if byte == b'\n' && !self.after_cr {
                self.pending.push(b'\r');
            }
            self.after_cr = byte == b'\r';
            self.pending.push(byte);
        }
        // The whole input is buffered, so it counts as written whether or not
        // the channel takes it now; `poll_flush` and the next call drain it.
        if let Poll::Ready(Err(error)) = self.poll_drain(cx) {
            return Poll::Ready(Err(error));
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        use std::task::Poll;
        match self.poll_drain(cx) {
            Poll::Ready(Ok(())) => self.inner.as_mut().poll_flush(cx),
            other => other,
        }
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        use std::task::Poll;
        match self.poll_drain(cx) {
            Poll::Ready(Ok(())) => self.inner.as_mut().poll_shutdown(cx),
            other => other,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Authenticated {
    User(User),
    Unlinked(PairingRequest),
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
/// which instance names exist. An unknown key is always offered a link,
/// whatever the user name says (SPEC 13).
async fn dispatch(server: &Server, authenticated: Authenticated, session: Box<dyn TermSession>) {
    let parts = session.into_parts();
    match authenticated {
        Authenticated::User(user) => {
            if let Some(instance) = resolve_instance(server, &parts.user, user.id).await {
                proxy(server, instance, parts).await;
            } else {
                let SessionParts {
                    command,
                    pty,
                    stdin,
                    stdout,
                    stderr,
                    exit,
                    ..
                } = parts;
                // With a PTY the client's terminal is in raw mode, where the
                // interpreter's bare newlines would staircase down the screen.
                let (stdout, stderr): (SessionOutput, SessionOutput) = if pty.is_some() {
                    (
                        Box::pin(CrlfWriter::new(stdout)),
                        Box::pin(CrlfWriter::new(stderr)),
                    )
                } else {
                    (stdout, stderr)
                };
                let code = server.cli.run(user, command, stdin, stdout, stderr).await;
                exit.exit(exit_code(code)).await;
            }
        }
        Authenticated::Unlinked(request) => {
            offer_link(server, request, parts).await;
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

/// How often the waiting session asks whether the link has been used.
const LINK_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Runs the SPEC 13 flow for an unknown key: mint a link, print it, and
/// wait for a browser to confirm the fingerprint.
///
/// Nothing is created for this key until that confirmation. The session
/// waits rather than hanging up because the alternative is a user who has
/// clicked the link and has no idea whether it worked; the fingerprint is
/// printed for them to compare against the one on the page.
async fn offer_link(server: &Server, request: PairingRequest, mut session: SessionParts) {
    let Some(linker) = &server.linker else {
        session.exit.exit(1).await;
        return;
    };
    let fingerprint = request.fingerprint.clone();
    let link = match linker.begin(request).await {
        Ok(link) => link,
        Err(error) => {
            let _ = session
                .stderr
                .write_all(format!("bento: could not start a link: {error}\r\n").as_bytes())
                .await;
            session.exit.exit(1).await;
            return;
        }
    };

    let announcement = format!(
        "bento: this key is not linked to an account\r\n\
         bento:   {fingerprint}\r\n\
         bento: open this within {}, sign in, and confirm that fingerprint:\r\n\
         bento:   {}\r\n\
         bento: waiting...\r\n",
        display_duration(link.valid_for),
        link.url,
    );
    if session
        .stdout
        .write_all(announcement.as_bytes())
        .await
        .is_err()
    {
        session.exit.exit(1).await;
        return;
    }

    match wait_for_link(linker.as_ref(), &link, &mut session.stdin).await {
        LinkOutcome::Linked(user) => {
            let _ = session
                .stdout
                .write_all(
                    format!(
                        "bento: linked to {}, subnet {}\r\n\
                         bento: reconnect for the command line; run \"help\" for the \
                         command list\r\n",
                        user.name, user.subnet
                    )
                    .as_bytes(),
                )
                .await;
            session.exit.exit(0).await;
        }
        LinkOutcome::Expired => {
            let _ = session
                .stdout
                .write_all(b"bento: the link expired; connect again for a new one\r\n")
                .await;
            session.exit.exit(1).await;
        }
        LinkOutcome::Cancelled => {
            session.exit.exit(1).await;
        }
        LinkOutcome::Failed(error) => {
            let _ = session
                .stderr
                .write_all(format!("bento: waiting for the link failed: {error}\r\n").as_bytes())
                .await;
            session.exit.exit(1).await;
        }
    }
}

enum LinkOutcome {
    Linked(Box<User>),
    Expired,
    Cancelled,
    Failed(String),
}

/// Polls until the link is confirmed, runs out of time, or the user gives
/// up. Under a PTY the client's terminal is in raw mode, so control-C
/// arrives as a byte on stdin rather than a signal; reading it is what
/// keeps the session from ignoring an interrupt for the whole window.
async fn wait_for_link(
    linker: &dyn KeyLinker,
    link: &PendingLink,
    stdin: &mut SessionInput,
) -> LinkOutcome {
    let deadline = Instant::now() + link.valid_for;
    let mut byte = [0_u8; 1];
    // A session with no keyboard behind it -- `ssh host < /dev/null`, or any
    // client that half-closes -- reads end-of-file at once. That is not the
    // user giving up, and reading a closed stream in a loop would spin, so
    // the watch is simply dropped and the wait runs on the timer alone.
    let mut watch_stdin = true;
    loop {
        match linker.linked_user(link.id).await {
            Ok(Some(user)) => return LinkOutcome::Linked(Box::new(user)),
            Ok(None) => {}
            Err(error) => return LinkOutcome::Failed(error.to_string()),
        }
        let now = Instant::now();
        if now >= deadline {
            return LinkOutcome::Expired;
        }
        let pause = LINK_POLL_INTERVAL.min(deadline - now);
        if !watch_stdin {
            tokio::time::sleep(pause).await;
            continue;
        }
        tokio::select! {
            () = tokio::time::sleep(pause) => {}
            // `AsyncReadExt::read` is cancel-safe, so losing this branch to
            // the timer drops nothing.
            read = stdin.read(&mut byte) => match read {
                Ok(0) | Err(_) => watch_stdin = false,
                Ok(_) if matches!(byte[0], 0x03 | 0x04) => return LinkOutcome::Cancelled,
                // Anything else is a stray keystroke, not an answer.
                Ok(_) => {}
            },
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
        wait_ssh,
    };
    use crate::server::{
        BoxError, BoxedIo, CLIRunner, Dialer, InstanceStore, KeyLinker, KeyStore, PairingRequest,
        PendingLink, Server, Starter,
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
    struct FakeLinker {
        got: Mutex<Option<PairingRequest>>,
        /// Set to make `linked_user` report a confirmation.
        user: Mutex<Option<User>>,
        /// Polls to answer `None` before `user` is reported.
        pending_polls: Mutex<usize>,
        polls: AtomicUsize,
        valid_for: Mutex<Duration>,
        begin_error: Mutex<Option<String>>,
        poll_error: Mutex<Option<String>>,
    }

    impl FakeLinker {
        fn new() -> Arc<Self> {
            let linker = Arc::new(Self::default());
            *linker.valid_for.lock().unwrap() = Duration::from_secs(180);
            linker
        }

        fn confirms_after(self: &Arc<Self>, polls: usize, user: User) {
            *self.pending_polls.lock().unwrap() = polls;
            *self.user.lock().unwrap() = Some(user);
        }
    }

    #[async_trait]
    impl KeyLinker for FakeLinker {
        async fn begin(&self, request: PairingRequest) -> Result<PendingLink, BoxError> {
            if let Some(error) = self.begin_error.lock().unwrap().clone() {
                return Err(error.into());
            }
            *self.got.lock().unwrap() = Some(request);
            Ok(PendingLink {
                id: 7,
                url: "https://bento.example.org/link/tok".to_owned(),
                valid_for: *self.valid_for.lock().unwrap(),
            })
        }

        async fn linked_user(&self, id: i64) -> Result<Option<User>, BoxError> {
            assert_eq!(id, 7);
            if let Some(error) = self.poll_error.lock().unwrap().clone() {
                return Err(error.into());
            }
            let seen = self.polls.fetch_add(1, Ordering::SeqCst);
            if seen < *self.pending_polls.lock().unwrap() {
                return Ok(None);
            }
            Ok(self.user.lock().unwrap().clone())
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

    fn linked_user_row() -> User {
        User {
            id: 9,
            subnet: "10.100.7.0/24".to_owned(),
            ..user("carol")
        }
    }

    fn server_with_linker(linker: Arc<FakeLinker>) -> Server {
        let mut server = test_server(
            Arc::new(FakeInstances::default()),
            Arc::new(FakeStarter::default()),
            Arc::new(FakeCli::default()),
        );
        server.linker = Some(linker);
        server
    }

    #[tokio::test]
    async fn an_unknown_key_is_offered_a_link_and_told_when_it_is_used() {
        let linker = FakeLinker::new();
        linker.confirms_after(2, linked_user_row());
        let server = server_with_linker(linker.clone());
        let fake = fake_session("", "", false).await;

        tokio::time::pause();
        dispatch(
            &server,
            Authenticated::Unlinked(PairingRequest {
                public_key: "ssh-ed25519 AAAA carol@laptop".to_owned(),
                fingerprint: "SHA256:abcdef".to_owned(),
                comment: "carol@laptop".to_owned(),
            }),
            fake.term,
        )
        .await;

        // The key reached the linker untouched; nothing else was created.
        let got = linker.got.lock().unwrap().clone().unwrap();
        assert_eq!(got.public_key, "ssh-ed25519 AAAA carol@laptop");
        assert_eq!(got.fingerprint, "SHA256:abcdef");
        assert_eq!(got.comment, "carol@laptop");

        let output = fake.stdout.text();
        // The fingerprint is printed so it can be compared with the page.
        assert!(output.contains("SHA256:abcdef"), "{output}");
        assert!(
            output.contains("https://bento.example.org/link/tok"),
            "{output}"
        );
        assert!(output.contains("3m0s"), "{output}");
        assert!(output.contains("linked to carol"), "{output}");
        assert!(output.contains("10.100.7.0/24"), "{output}");
        // Every line is CRLF-terminated: this path runs before any PTY-only
        // translation and a raw-mode client would otherwise staircase.
        assert!(!output.replace("\r\n", "").contains('\n'), "{output:?}");
        assert_eq!(*fake.exit.0.lock().unwrap(), Some(0));
        assert!(linker.polls.load(Ordering::SeqCst) >= 3);
    }

    /// Stdin is empty here, so it reads end-of-file on the first poll --
    /// what `ssh host < /dev/null` does. That must not read as the user
    /// giving up: the wait runs its whole window on the timer.
    #[tokio::test]
    async fn a_link_that_is_never_used_expires_rather_than_waiting_forever() {
        let linker = FakeLinker::new();
        *linker.valid_for.lock().unwrap() = Duration::from_secs(6);
        let server = server_with_linker(linker.clone());
        let fake = fake_session("", "", false).await;

        tokio::time::pause();
        dispatch(
            &server,
            Authenticated::Unlinked(PairingRequest::default()),
            fake.term,
        )
        .await;

        assert!(
            fake.stdout.text().contains("expired"),
            "{}",
            fake.stdout.text()
        );
        assert_eq!(*fake.exit.0.lock().unwrap(), Some(1));
    }

    #[tokio::test]
    async fn control_c_gives_up_on_the_wait() {
        let linker = FakeLinker::new();
        // Never confirms: only the keystroke can end this session.
        *linker.pending_polls.lock().unwrap() = usize::MAX;
        let server = server_with_linker(linker.clone());
        let fake = fake_session("", "\x03", false).await;

        tokio::time::pause();
        dispatch(
            &server,
            Authenticated::Unlinked(PairingRequest::default()),
            fake.term,
        )
        .await;

        assert!(!fake.stdout.text().contains("expired"));
        assert_eq!(*fake.exit.0.lock().unwrap(), Some(1));
    }

    #[tokio::test]
    async fn a_linker_that_cannot_mint_says_so() {
        let linker = FakeLinker::new();
        *linker.begin_error.lock().unwrap() = Some("database is down".to_owned());
        let server = server_with_linker(linker);
        let fake = fake_session("", "", false).await;
        dispatch(
            &server,
            Authenticated::Unlinked(PairingRequest::default()),
            fake.term,
        )
        .await;
        assert!(fake.stderr.text().contains("database is down"));
        assert_eq!(*fake.exit.0.lock().unwrap(), Some(1));
    }

    #[test]
    fn command_words_follow_shell_quoting() {
        assert_eq!(
            super::split_command("new 'my vm' --image=debian\\ stable"),
            ["new", "my vm", "--image=debian stable"]
        );
    }

    /// A client that asks for a PTY puts its terminal in raw mode, where a
    /// bare `\n` drops a row without returning the carriage and the help
    /// screen walks off the right of the screen. The interpreter writes plain
    /// `\n` by design, so the frontend translates — but only with a PTY, so
    /// that `ssh host help > file` still gets ordinary newlines.
    #[tokio::test]
    async fn pty_sessions_get_crlf_from_the_interpreter() {
        for (pty, expected) in [(true, "cli ran\r\n"), (false, "cli ran\n")] {
            let cli = Arc::new(FakeCli::default());
            let server = test_server(
                Arc::new(FakeInstances::default()),
                Arc::new(FakeStarter::default()),
                cli.clone(),
            );
            let fake = fake_session("riley", "", pty).await;
            dispatch(&server, Authenticated::User(user("riley")), fake.term).await;
            assert_eq!(
                fake.stdout.text(),
                expected,
                "pty={pty} did not get the right line endings"
            );
        }
    }

    /// Translation must not touch a CRLF the interpreter wrote itself, or the
    /// terminal gets a blank line between every row.
    #[tokio::test]
    async fn crlf_writer_leaves_existing_crlf_alone() {
        let sink = SharedWriter::default();
        let mut writer = super::CrlfWriter::new(Box::pin(sink.clone()));
        writer.write_all(b"a\r\nb\nc\r\n").await.unwrap();
        writer.flush().await.unwrap();
        assert_eq!(sink.text(), "a\r\nb\r\nc\r\n");
    }

    /// A newline split across two writes must still come out as CRLF: the
    /// interpreter's formatter writes in whatever chunks it likes.
    #[tokio::test]
    async fn crlf_writer_translates_across_write_boundaries() {
        let sink = SharedWriter::default();
        let mut writer = super::CrlfWriter::new(Box::pin(sink.clone()));
        writer.write_all(b"row").await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.write_all(b"\r").await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.flush().await.unwrap();
        assert_eq!(sink.text(), "row\r\n\r\n");
    }
}
