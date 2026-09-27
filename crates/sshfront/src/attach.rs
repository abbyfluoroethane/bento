//! A guest shell for a transport other than SSH (SPEC 14.6).
//!
//! The web terminal is equal to `ssh $NAME@bento.foid.space`, so it takes the
//! same path through SPEC 10 steps 7 to 10 as an SSH session: the same start,
//! the same wait for sshd, the same frontend key, and the same join. Only the
//! bytes on the near side come from somewhere else. The caller has done steps
//! 1 to 6 already: it knows the user and has checked the access.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use async_trait::async_trait;
use bento_types::{Instance, State};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

use crate::server::{CLIRunner, InstanceStore, KeyStore, Server, Starter};
use crate::session::{self, PtyRequest, SessionExit, SessionOutput, SessionParts, Window};

/// The terminal type the guest PTY gets. ghostty-web answers as xterm does.
pub const WEB_TERM: &str = "xterm-256color";

/// A terminal size in character cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSize {
    pub cols: u32,
    pub rows: u32,
}

/// The near side of one guest shell.
pub struct Terminal {
    /// The size of the PTY the guest gets first.
    pub size: TerminalSize,
    /// Each later size. A closed channel ends the resizes, not the session.
    pub resizes: mpsc::Receiver<TerminalSize>,
    /// Keyboard input. End of file ends the input to the guest.
    pub input: Pin<Box<dyn AsyncRead + Send>>,
    /// Guest output. With a PTY the guest joins stdout and stderr, so one
    /// stream is enough.
    pub output: Pin<Box<dyn AsyncWrite + Send>>,
    /// Starts a stopped instance first (SPEC 10 step 7). The web terminal
    /// sets this only after a click, because a page load must not start an
    /// instance (SPEC 14.6).
    pub start: bool,
}

/// How a guest shell ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attached {
    /// The guest session ended with this exit status.
    Exited(u32),
    /// The frontend could not join the guest. The message is also in the
    /// output, for the terminal to show.
    Failed(String),
    /// The instance is stopped and the terminal did not ask to start it.
    NotRunning,
}

impl Server {
    /// A frontend that only attaches terminals: it serves no SSH listener,
    /// knows no client keys, and runs no command line interface. The control
    /// plane uses it for the web terminal (SPEC 14.6). The guest key stands
    /// in for the host key, which [`Server::serve`] alone presents; do not
    /// serve this value.
    pub fn for_guests(
        instances: Arc<dyn InstanceStore>,
        starter: Arc<dyn Starter>,
        guest_key: Arc<russh::keys::PrivateKey>,
    ) -> Self {
        Self::new(
            Arc::new(NoKeys),
            instances,
            starter,
            Arc::new(NoCli),
            Arc::clone(&guest_key),
            guest_key,
        )
    }

    /// Joins `terminal` to a shell in `instance`. This is SPEC 10 steps 7
    /// to 10, the same function an SSH session uses.
    pub async fn attach(&self, instance: Instance, terminal: Terminal) -> Attached {
        if instance.state == State::Stopped && !terminal.start {
            return Attached::NotRunning;
        }
        let Terminal {
            size,
            mut resizes,
            input,
            output,
            ..
        } = terminal;

        let (window_sender, windows) = mpsc::channel(16);
        tokio::spawn(async move {
            while let Some(size) = resizes.recv().await {
                if window_sender.send(window(size)).await.is_err() {
                    return;
                }
            }
        });

        let output = SharedOutput(Arc::new(Mutex::new(output)));
        let (sender, outcome) = oneshot::channel();
        let parts = SessionParts {
            user: instance.name.clone(),
            raw_command: Vec::new(),
            command: Vec::new(),
            pty: Some(PtyRequest {
                term: WEB_TERM.to_owned(),
                window: window(size),
            }),
            windows,
            stdin: input,
            stdout: Box::pin(output.clone()),
            stderr: Box::pin(output),
            exit: Arc::new(Outcome(Mutex::new(Some(sender)))),
        };
        session::proxy(self, instance, parts).await;
        outcome
            .await
            .unwrap_or_else(|_| Attached::Failed("bento: the session ended".to_owned()))
    }
}

fn window(size: TerminalSize) -> Window {
    Window {
        col_width: size.cols,
        row_height: size.rows,
        pix_width: 0,
        pix_height: 0,
    }
}

/// Records the first way the session ended.
struct Outcome(Mutex<Option<oneshot::Sender<Attached>>>);

impl Outcome {
    fn send(&self, attached: Attached) {
        if let Some(sender) = self.0.lock().expect("outcome lock").take() {
            let _ = sender.send(attached);
        }
    }
}

#[async_trait]
impl SessionExit for Outcome {
    async fn exit(&self, code: u32) {
        self.send(Attached::Exited(code));
    }

    async fn fail(&self, message: &str) {
        self.send(Attached::Failed(message.to_owned()));
    }
}

/// One output stream behind both stdout and stderr. Each write finishes
/// under the lock, so the two never split a write.
#[derive(Clone)]
struct SharedOutput(Arc<Mutex<SessionOutput>>);

impl AsyncWrite for SharedOutput {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0
            .lock()
            .expect("output lock")
            .as_mut()
            .poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0.lock().expect("output lock").as_mut().poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.0
            .lock()
            .expect("output lock")
            .as_mut()
            .poll_shutdown(cx)
    }
}

/// The key store of [`Server::for_guests`]. Nothing authenticates there.
struct NoKeys;

#[async_trait]
impl KeyStore for NoKeys {
    async fn ssh_key_by_fingerprint(
        &self,
        _fingerprint: &str,
    ) -> bento_store::Result<bento_types::SshKey> {
        Err(bento_store::Error::NotFound)
    }

    async fn user_by_id(&self, _id: i64) -> bento_store::Result<bento_types::User> {
        Err(bento_store::Error::NotFound)
    }
}

/// The command line of [`Server::for_guests`]. Nothing reaches it.
struct NoCli;

#[async_trait]
impl CLIRunner for NoCli {
    async fn run(
        &self,
        _user: bento_types::User,
        _args: Vec<String>,
        _stdin: Pin<Box<dyn AsyncRead + Send>>,
        _stdout: Pin<Box<dyn AsyncWrite + Send>>,
        _stderr: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> i32 {
        1
    }
}
