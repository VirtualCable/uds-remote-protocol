// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Reliable transport over `TcpStream` and direct connector/listener (PoC).

use std::net::SocketAddr;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::debug;

use crate::error::{Result, TransportError};
use crate::traits::{Connector, DatagramTransport, ReliableTransport, Target};

/// [`ReliableTransport`] over a real `TcpStream`.
pub struct TcpTransport {
    stream: TcpStream,
}

impl TcpTransport {
    /// Wraps an already connected `TcpStream`.
    #[must_use]
    pub fn new(stream: TcpStream) -> Self {
        Self { stream }
    }

    /// Extracts the underlying `TcpStream` (e.g. to duplicate the socket
    /// into another process for a connection handoff, §18.32).
    #[must_use]
    pub fn into_stream(self) -> TcpStream {
        self.stream
    }
}

#[async_trait]
impl ReliableTransport for TcpTransport {
    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.stream.write_all(bytes).await?;
        Ok(())
    }

    async fn recv_into(&mut self, buf: &mut [u8]) -> Result<usize> {
        let n = self.stream.read(buf).await?;
        if n == 0 {
            return Err(TransportError::Closed);
        }
        Ok(n)
    }

    async fn close(self: Box<Self>) -> Result<()> {
        let mut stream = self.stream;
        stream.shutdown().await?;
        Ok(())
    }
}

#[async_trait]
impl<T: ReliableTransport + ?Sized> ReliableTransport for Box<T> {
    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        (**self).send(bytes).await
    }

    async fn recv_into(&mut self, buf: &mut [u8]) -> Result<usize> {
        (**self).recv_into(buf).await
    }

    async fn close(self: Box<Self>) -> Result<()> {
        (*self).close().await
    }
}

/// Direct TCP connector. Also covers the gateway case: the UDS launcher
/// exposes the tunnel as a local TCP+UDP listener, which is just another
/// address for this connector.
#[derive(Debug, Default, Clone, Copy)]
pub struct DirectConnector;

impl DirectConnector {
    /// Creates a direct connector.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Connector for DirectConnector {
    async fn connect(
        &self,
        target: &Target,
    ) -> Result<(
        Box<dyn ReliableTransport>,
        Option<Box<dyn DatagramTransport>>,
    )> {
        debug!(addr = %target.addr(), "connecting direct TCP");
        let stream = TcpStream::connect(target.addr()).await?;
        // No datagrams in Phase 0 (UDP arrives in Phase 1).
        Ok((Box::new(TcpTransport::new(stream)), None))
    }
}

/// Direct TCP listener (server side, PoC phase).
pub struct DirectListener {
    listener: TcpListener,
}

impl DirectListener {
    /// Opens a TCP listener on `addr` (e.g. `"0.0.0.0:19790"`).
    pub async fn bind(addr: &str) -> Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        Ok(Self { listener })
    }

    /// Local address of the listener.
    ///
    /// # Errors
    /// Returns an I/O error if the socket is no longer valid.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// Accepts an incoming connection, returning the raw transport and the
    /// remote address. The Hello handshake is handled by
    /// [`crate::EncryptedStream::server_handshake`].
    pub async fn accept(&self) -> Result<(Box<dyn ReliableTransport>, SocketAddr)> {
        let (stream, peer) = self.listener.accept().await?;
        debug!(%peer, "TCP connection accepted");
        Ok((Box::new(TcpTransport::new(stream)), peer))
    }
}
