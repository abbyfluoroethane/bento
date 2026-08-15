use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http::Request;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;

use crate::{BoxError, Error, Proxy};

/// An accepted bidirectional byte stream.
pub trait AsyncIo: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T> AsyncIo for T where T: AsyncRead + AsyncWrite + Unpin + Send + 'static {}

/// Type-erased stream returned by an injected [`Listener`].
pub type BoxIo = Box<dyn AsyncIo>;

/// A listener used by [`Proxy::serve`]. Production wraps
/// [`tokio::net::TcpListener`]; tests can inject a listener that opens no
/// socket.
#[async_trait]
pub trait Listener: Send + Sync + 'static {
    async fn accept(&self) -> io::Result<(BoxIo, SocketAddr)>;
}

/// Future returned by [`ListenFunc`].
pub type ListenFuture = Pin<Box<dyn Future<Output = io::Result<Box<dyn Listener>>> + Send>>;

/// Binds one address. Pass `None` to [`Proxy::serve`] for the production
/// TCP implementation, or inject a function in tests so no socket opens.
pub type ListenFunc = Arc<dyn Fn(String, String) -> ListenFuture + Send + Sync>;

/// Boxes an async binding function as a [`ListenFunc`].
pub fn listen_fn<F, Fut, L>(listen: F) -> ListenFunc
where
    F: Fn(String, String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = io::Result<L>> + Send + 'static,
    L: Listener,
{
    Arc::new(move |network, address| {
        let future = listen(network, address);
        Box::pin(async move { Ok(Box::new(future.await?) as Box<dyn Listener>) })
    })
}

struct TcpListenerAdapter(TcpListener);

#[async_trait]
impl Listener for TcpListenerAdapter {
    async fn accept(&self) -> io::Result<(BoxIo, SocketAddr)> {
        let (stream, peer) = self.0.accept().await?;
        Ok((Box::new(stream), peer))
    }
}

fn default_listen() -> ListenFunc {
    listen_fn(|network, address| async move {
        if network != "tcp" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported network {network:?}"),
            ));
        }
        TcpListener::bind(address).await.map(TcpListenerAdapter)
    })
}

impl Proxy {
    /// Binds every proxy port on `bind_host` and serves all of them. When
    /// `tls_config` is `Some`, every accepted stream terminates TLS; `None`
    /// serves plain HTTP for deployments with an external TLS terminator.
    /// Binding is all-or-nothing. The method returns after `shutdown`
    /// resolves or a listener fails, allowing active connections up to five
    /// seconds to finish gracefully. An intentional shutdown returns
    /// [`Error::Shutdown`].
    pub async fn serve<F>(
        self: Arc<Self>,
        bind_host: &str,
        tls_config: Option<Arc<rustls::ServerConfig>>,
        listen: Option<ListenFunc>,
        shutdown: F,
    ) -> Result<(), Error>
    where
        F: Future<Output = ()> + Send,
    {
        let listeners = listen_all(
            bind_host,
            &self.ports(),
            listen.unwrap_or_else(default_listen),
        )
        .await?;
        let cancel = CancellationToken::new();
        let (accepted_tx, mut accepted_rx) = mpsc::channel::<Accepted>(256);
        let (error_tx, mut error_rx) = mpsc::channel::<(u16, io::Error)>(1);
        let mut acceptors = JoinSet::new();

        for (port, listener) in listeners {
            let accepted_tx = accepted_tx.clone();
            let error_tx = error_tx.clone();
            let cancel = cancel.clone();
            acceptors.spawn(async move {
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => break,
                        result = listener.accept() => match result {
                            Ok((stream, peer)) => {
                                if accepted_tx.send(Accepted { port, stream, peer }).await.is_err() {
                                    break;
                                }
                            }
                            Err(error) => {
                                let _ = error_tx.send((port, error)).await;
                                break;
                            }
                        },
                    }
                }
            });
        }
        drop(accepted_tx);
        drop(error_tx);

        tokio::pin!(shutdown);
        let mut connections = JoinSet::new();
        let result = loop {
            tokio::select! {
                () = &mut shutdown => break Err(Error::Shutdown),
                Some((port, source)) = error_rx.recv() => break Err(Error::Serve { port, source }),
                Some(accepted) = accepted_rx.recv() => {
                    let proxy = self.clone();
                    let tls_config = tls_config.clone();
                    let cancel = cancel.clone();
                    connections.spawn(async move {
                        serve_connection(proxy, accepted, tls_config, cancel).await;
                    });
                }
                else => break Ok(()),
            }
        };

        cancel.cancel();
        acceptors.abort_all();
        while acceptors.join_next().await.is_some() {}
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while connections.join_next().await.is_some() {}
        })
        .await;
        connections.abort_all();
        result
    }
}

struct Accepted {
    port: u16,
    stream: BoxIo,
    peer: SocketAddr,
}

pub(crate) async fn listen_all(
    bind_host: &str,
    ports: &[u16],
    listen: ListenFunc,
) -> Result<Vec<(u16, Box<dyn Listener>)>, Error> {
    let mut listeners = Vec::with_capacity(ports.len());
    for &port in ports {
        let address = format_address(bind_host, port);
        match listen("tcp".to_owned(), address).await {
            Ok(listener) => listeners.push((port, listener)),
            Err(source) => {
                // Dropping the vector closes every successfully bound
                // listener before the error is returned.
                drop(listeners);
                return Err(Error::Bind { port, source });
            }
        }
    }
    Ok(listeners)
}

fn format_address(bind_host: &str, port: u16) -> String {
    if bind_host.contains(':') && !bind_host.starts_with('[') {
        format!("[{bind_host}]:{port}")
    } else {
        format!("{bind_host}:{port}")
    }
}

async fn serve_connection(
    proxy: Arc<Proxy>,
    accepted: Accepted,
    tls_config: Option<Arc<rustls::ServerConfig>>,
    cancel: CancellationToken,
) {
    if let Some(config) = tls_config {
        let Ok(stream) = TlsAcceptor::from(config).accept(accepted.stream).await else {
            return;
        };
        let server_name = stream.get_ref().1.server_name().map(str::to_owned);
        run_http(
            proxy,
            stream,
            accepted.port,
            accepted.peer,
            server_name,
            true,
            cancel,
        )
        .await;
    } else {
        run_http(
            proxy,
            accepted.stream,
            accepted.port,
            accepted.peer,
            None,
            false,
            cancel,
        )
        .await;
    }
}

async fn run_http<IO>(
    proxy: Arc<Proxy>,
    stream: IO,
    port: u16,
    peer: SocketAddr,
    server_name: Option<String>,
    tls: bool,
    cancel: CancellationToken,
) where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |request: Request<Incoming>| {
        let proxy = proxy.clone();
        let server_name = server_name.clone();
        async move {
            let request = request.map(|body| {
                body.map_err(|error| -> BoxError { Box::new(error) })
                    .boxed()
            });
            Ok::<_, std::convert::Infallible>(
                proxy
                    .handle_connection(request, port, peer, server_name.as_deref(), tls)
                    .await,
            )
        }
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(10));
    let connection = builder
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades();
    tokio::pin!(connection);
    tokio::select! {
        _ = &mut connection => {}
        () = cancel.cancelled() => {
            connection.as_mut().graceful_shutdown();
            let _ = connection.await;
        }
    }
}
