// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Datagram transport over a real `UdpSocket` (phase 1).

use std::net::SocketAddr;

use async_trait::async_trait;
use tokio::net::UdpSocket;
use tracing::debug;

use crate::error::Result;
use crate::traits::DatagramTransport;

/// Kernel socket buffer target for UDP endpoints (§12.6): display frames
/// arrive in bursts of thousands of datagrams per tick, and the default
/// buffer (~200 KiB) drops them under load. The kernel may clamp this to
/// `net.core.{r,w}mem_max`; on hosts for high-throughput video, raise those
/// sysctls.
const SOCKET_BUFFER_SIZE: usize = 4 << 20;

/// Binds a non-blocking UDP socket with enlarged kernel buffers.
fn bind_udp(bind: &str) -> Result<UdpSocket> {
    let addr: SocketAddr = bind.parse().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("bad bind addr {bind}: {e}"),
        )
    })?;
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::DGRAM,
        None,
    )?;
    socket.set_nonblocking(true)?;
    socket.set_send_buffer_size(SOCKET_BUFFER_SIZE)?;
    socket.set_recv_buffer_size(SOCKET_BUFFER_SIZE)?;
    socket.bind(&addr.into())?;
    Ok(UdpSocket::from_std(socket.into())?)
}

/// [`DatagramTransport`] over a connected `UdpSocket` (fixed peer).
pub struct UdpTransport {
    socket: UdpSocket,
}

impl UdpTransport {
    /// Binds locally and connects to `remote` (the peer address gets fixed).
    pub async fn connect(bind: &str, remote: SocketAddr) -> Result<Self> {
        let socket = bind_udp(bind)?;
        socket.connect(remote).await?;
        debug!(%remote, "UDP socket connected");
        Ok(Self { socket })
    }

    /// Binds a server-side socket whose peer is not known yet; call
    /// [`PendingUdpTransport::wait_first`] to learn it from the first
    /// incoming datagram.
    pub fn accept_first(bind: &str) -> Result<PendingUdpTransport> {
        let socket = bind_udp(bind)?;
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
    async fn send(&self, dgram: &[u8]) -> Result<()> {
        self.socket.send(dgram).await?;
        Ok(())
    }

    async fn recv_into(&self, buf: &mut [u8]) -> Result<usize> {
        Ok(self.socket.recv(buf).await?)
    }
}

#[async_trait]
impl<T: DatagramTransport + ?Sized> DatagramTransport for Box<T> {
    async fn send(&self, dgram: &[u8]) -> Result<()> {
        (**self).send(dgram).await
    }

    async fn recv_into(&self, buf: &mut [u8]) -> Result<usize> {
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

    /// Waits for the first datagram carrying the session token, connects
    /// the socket to its sender and returns the ready transport plus the
    /// bytes received (§18.78: the token travels cleartext at offset 0, so
    /// the peer lock must require it — otherwise any unsolicited datagram
    /// would hijack the leg before authentication).
    pub async fn wait_first(
        self,
        expected_token: &[u8; protocol::crypt::consts::TOKEN_LENGTH],
    ) -> Result<(UdpTransport, Vec<u8>)> {
        let mut buf = vec![0u8; 2048];
        loop {
            let (n, peer) = self.socket.recv_from(&mut buf).await?;
            if n < protocol::crypt::consts::TOKEN_LENGTH
                || buf[..protocol::crypt::consts::TOKEN_LENGTH] != *expected_token
            {
                debug!(%peer, "ignoring tokenless datagram before the UDP peer lock");
                continue;
            }
            self.socket.connect(peer).await?;
            debug!(%peer, "UDP peer learned from the first session datagram");
            buf.truncate(n);
            return Ok((
                UdpTransport {
                    socket: self.socket,
                },
                buf,
            ));
        }
    }
}
