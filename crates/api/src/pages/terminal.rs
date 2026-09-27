//! The web terminal socket (SPEC 14.6). One WebSocket is one guest shell,
//! with the same access as `ssh $NAME@` the base domain.

use axum::Extension;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bento_types::{Instance, User};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use super::{Viewer, readable_instance};
use crate::{AppState, BoxError, ConsoleEnd, ConsoleTerminal, StatusError, TerminalSize};

/// The largest size a page may ask for, in each direction (SPEC 14.6).
const MAX_CELLS: u32 = 1000;
/// Keyboard input arrives a key or a paste at a time.
const MAX_MESSAGE: usize = 1 << 20;
/// The close code for a stopped instance the page did not ask to start.
/// Codes 4000 to 4999 are free for the application (RFC 6455 7.4.2).
pub(crate) const CLOSE_NOT_RUNNING: u16 = 4409;
/// A close reason has at most 123 bytes (RFC 6455 5.5).
const MAX_REASON: usize = 123;

#[derive(Debug, Deserialize)]
pub(crate) struct SocketParams {
    #[serde(default)]
    cols: Option<u32>,
    #[serde(default)]
    rows: Option<u32>,
    #[serde(default)]
    start: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Control {
    Resize { cols: u32, rows: u32 },
}

fn clamped(cols: Option<u32>, rows: Option<u32>) -> TerminalSize {
    TerminalSize {
        cols: cols.unwrap_or(80).clamp(1, MAX_CELLS),
        rows: rows.unwrap_or(24).clamp(1, MAX_CELLS),
    }
}

fn refuse(error: BoxError) -> Response {
    let (status, message) = crate::error_parts(&error);
    (status, message).into_response()
}

/// Checks SPEC 14.6 steps 2 to 4, then upgrades. The pages router has done
/// step 1: a request that reaches a handler has a session.
pub(crate) async fn socket(
    State(state): State<AppState>,
    Extension(user): Viewer,
    AxumPath(uuid): AxumPath<String>,
    Query(params): Query<SocketParams>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    // Step 2. A browser sends the session cookie with a WebSocket request
    // from any page on the same site, and an instance subdomain is the same
    // site. Only the `Origin` header tells the dashboard from a guest page.
    let expected = format!("https://{}", state.0.base_domain);
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if origin != Some(expected.as_str()) {
        tracing::warn!(user = %user.name, %uuid, ?origin, "web terminal: origin refused");
        return refuse(Box::new(StatusError::new(
            StatusCode::FORBIDDEN,
            "the terminal only opens from the dashboard",
        )));
    }
    // Steps 3 and 4. A user without access gets the same answer as for a
    // UUID that does not exist.
    let instance = match readable_instance(&state, &uuid, &user).await {
        Ok(Some(instance)) => instance,
        Ok(None) => return refuse(Box::new(crate::StoreError::NotFound)),
        Err(error) => return refuse(error),
    };
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    let size = clamped(params.cols, params.rows);
    let start = params.start.as_deref() == Some("1");
    let console = state.0.console.clone();
    upgrade
        .max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| run(socket, console, user, instance, size, start))
}

async fn run(
    socket: WebSocket,
    console: std::sync::Arc<dyn crate::Console>,
    user: User,
    instance: Instance,
    size: TerminalSize,
    start: bool,
) {
    let uuid = instance.uuid.clone();
    tracing::info!(user = %user.name, %uuid, "web terminal opened");

    let (mut sink, mut stream) = socket.split();
    let (mut keyboard, input) = tokio::io::duplex(64 * 1024);
    let (output, mut screen) = tokio::io::duplex(64 * 1024);
    let (resize_sender, resizes) = mpsc::channel(16);
    let terminal = ConsoleTerminal {
        size,
        resizes,
        input: Box::pin(input),
        output: Box::pin(output),
        start,
    };

    // Browser to guest. This ends when the page closes the socket.
    let from_browser = async move {
        while let Some(Ok(message)) = stream.next().await {
            match message {
                Message::Binary(bytes) => {
                    if keyboard.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                Message::Text(text) => {
                    if let Ok(Control::Resize { cols, rows }) = serde_json::from_str(&text) {
                        let _ = resize_sender.send(clamped(Some(cols), Some(rows))).await;
                    }
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => {}
            }
        }
    };

    // Guest to browser, then the close frame. The output ends when the
    // session ends, so the last bytes arrive before the close.
    let session = async {
        let to_browser = async {
            let mut buffer = vec![0_u8; 32 * 1024];
            loop {
                match screen.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        let bytes = bytes_of(&buffer[..count]);
                        if sink.send(Message::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                }
            }
        };
        let (end, ()) = tokio::join!(console.attach(instance, terminal), to_browser);
        let _ = sink.send(Message::Close(Some(close_frame(&end)))).await;
        end
    };

    // When the page goes first, dropping the session future closes the
    // guest connection. The server keeps no session (SPEC 14.6).
    tokio::select! {
        end = session => {
            tracing::info!(user = %user.name, %uuid, ?end, "web terminal closed");
        }
        () = from_browser => {
            tracing::info!(user = %user.name, %uuid, "web terminal closed by the page");
        }
    }
}

fn bytes_of(slice: &[u8]) -> axum::body::Bytes {
    axum::body::Bytes::copy_from_slice(slice)
}

pub(crate) fn close_frame(end: &ConsoleEnd) -> CloseFrame {
    let (code, reason) = match end {
        ConsoleEnd::Exited(status) => (1000, format!("exit {status}")),
        ConsoleEnd::Failed(message) => (1011, message.clone()),
        ConsoleEnd::NotRunning => (CLOSE_NOT_RUNNING, "the VM is not running".to_owned()),
    };
    CloseFrame {
        code,
        reason: truncate(&reason, MAX_REASON).into(),
    }
}

fn truncate(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_clamped_and_defaulted() {
        assert_eq!(clamped(None, None), TerminalSize { cols: 80, rows: 24 });
        assert_eq!(
            clamped(Some(0), Some(5000)),
            TerminalSize {
                cols: 1,
                rows: MAX_CELLS
            }
        );
    }

    #[test]
    fn a_long_reason_is_cut_on_a_character_boundary() {
        let frame = close_frame(&ConsoleEnd::Failed("é".repeat(100)));
        assert_eq!(frame.code, 1011);
        assert!(frame.reason.len() <= MAX_REASON);
        assert!(frame.reason.as_str().chars().all(|c| c == 'é'));
    }

    #[test]
    fn close_codes_follow_the_spec() {
        assert_eq!(close_frame(&ConsoleEnd::Exited(0)).code, 1000);
        assert_eq!(
            close_frame(&ConsoleEnd::Exited(0)).reason.as_str(),
            "exit 0"
        );
        assert_eq!(close_frame(&ConsoleEnd::NotRunning).code, CLOSE_NOT_RUNNING);
    }
}
