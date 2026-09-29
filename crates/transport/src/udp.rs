//! Datagram transport over a real `UdpSocket` (phase 1).

use std::net::SocketAddr;

use async_trait::async_trait;
use tokio::net::UdpSocket;
use tracing::debug;

use crate::error::Result;
use crate::traits::DatagramTransport;

/// [`DatagramTransport`] over a connected `UdpSocket` (fixed peer).
pub struct UdpTransport {
    socket: UdpSocket,
}

impl UdpTransport {
    /// Binds locally and connects to `remote` (the peer address gets fixed).
    pub async fn connect(bind: &str, remote: SocketAddr) -> Result<Self> {
        let socket = UdpSocket::bind(bind).await?;
        socket.connect(remote).await?;
        debug!(%remote, "UDP socket connected");
        Ok(Self { socket })
    }

    /// Binds a server-side socket whose peer is not known yet; call
    /// [`PendingUdpTransport::wait_first`] to learn it from the first
    /// incoming datagram.
    pub async fn accept_first(bind: &str) -> Result<PendingUdpTransport> {
        let socket = UdpSocket::bind(bind).await?;
        Ok(PendingUdpTransport { socket })
    }

    /// Local socket address.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    /// Sends raw bytes (no encryption); used by the UDP detection probe
    /// before the encrypted layer exists (§4.2).
    pub async fn send_raw(&self, bytes: &[u8]) -> Result<()> {
        self.socket.send(bytes).await?;
        Ok(())
    }
}

#[async_trait]
impl DatagramTransport for UdpTransport {
    async fn send(&mut self, dgram: &[u8]) -> Result<()> {
        self.socket.send(dgram).await?;
        Ok(())
    }

    async fn recv_into(&mut self, buf: &mut [u8]) -> Result<usize> {
        Ok(self.socket.recv(buf).await?)
    }
}

#[async_trait]
impl<T: DatagramTransport + ?Sized> DatagramTransport for Box<T> {
    async fn send(&mut self, dgram: &[u8]) -> Result<()> {
        (**self).send(dgram).await
    }

    async fn recv_into(&mut self, buf: &mut [u8]) -> Result<usize> {
        (**self).recv_into(buf).await
    }
}

/// Server-side UDP socket waiting to learn its peer from the first datagram.
pub struct PendingUdpTransport {
    socket: UdpSocket,
}

impl PendingUdpTransport {
    /// Local socket address (what the server advertises in its caps, §4.2).
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    /// Waits for the first datagram, connects the socket to its sender and
    /// returns the ready transport plus the bytes received.
    pub async fn wait_first(self) -> Result<(UdpTransport, Vec<u8>)> {
        let mut buf = vec![0u8; 2048];
        let (n, peer) = self.socket.recv_from(&mut buf).await?;
        self.socket.connect(peer).await?;
        debug!(%peer, "UDP peer learned from first datagram");
        buf.truncate(n);
        Ok((UdpTransport { socket: self.socket }, buf))
    }
}
