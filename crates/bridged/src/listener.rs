//! The seam between the server and the transport. Production uses the named pipe
//! ([`crate::pipe`]); tests use an in-memory duplex. There is deliberately no TCP implementation.

use std::future::Future;
use std::io;
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream};
use tokio::sync::mpsc;

pub trait Listener: Send + 'static {
    type Conn: AsyncRead + AsyncWrite + Send + Unpin + 'static;
    /// Wait for the next client. An error here is about the listener itself, not about a client.
    fn accept(&mut self) -> impl Future<Output = io::Result<Self::Conn>> + Send;
}

/// In-memory listener: clients are made with [`DuplexConnector::connect`].
pub struct DuplexListener {
    rx: mpsc::UnboundedReceiver<DuplexStream>,
}

#[derive(Clone)]
pub struct DuplexConnector {
    tx: mpsc::UnboundedSender<DuplexStream>,
}

pub fn duplex_listener() -> (DuplexListener, DuplexConnector) {
    let (tx, rx) = mpsc::unbounded_channel();
    (DuplexListener { rx }, DuplexConnector { tx })
}

impl DuplexConnector {
    /// A new client end; the server end is queued for `accept`.
    pub fn connect(&self) -> io::Result<DuplexStream> {
        let (client, server) = tokio::io::duplex(64 * 1024);
        self.tx
            .send(server)
            .map_err(|_| io::Error::new(io::ErrorKind::ConnectionRefused, "listener closed"))?;
        Ok(client)
    }
}

impl Listener for DuplexListener {
    type Conn = DuplexStream;
    async fn accept(&mut self) -> io::Result<DuplexStream> {
        self.rx
            .recv()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "all connectors dropped"))
    }
}
