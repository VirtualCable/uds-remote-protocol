//! Transporte fiable sobre `TcpStream` y conector/escucha directos (PoC).

use std::net::SocketAddr;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::debug;

use crate::error::{Result, TransportError};
use crate::traits::{Connector, DatagramTransport, ReliableTransport, Target};

/// [`ReliableTransport`] sobre un `TcpStream` real.
pub struct TcpTransport {
    stream: TcpStream,
}

impl TcpTransport {
    /// Envuelve un `TcpStream` ya conectado.
    #[must_use]
    pub fn new(stream: TcpStream) -> Self {
        Self { stream }
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

/// Conector directo TCP (fase PoC). El futuro `TunnelConnector` (fase 4)
/// implementará el mismo trait [`Connector`] hablando con tunnel-server.
#[derive(Debug, Default, Clone, Copy)]
pub struct DirectConnector;

impl DirectConnector {
    /// Crea un conector directo.
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
    ) -> Result<(Box<dyn ReliableTransport>, Option<Box<dyn DatagramTransport>>)> {
        debug!(addr = %target.addr(), "conectando TCP directo");
        let stream = TcpStream::connect(target.addr()).await?;
        // Sin datagramas en Fase 0 (UDP llega en Fase 1).
        Ok((Box::new(TcpTransport::new(stream)), None))
    }
}

/// Escucha directa TCP (lado servidor, fase PoC).
pub struct DirectListener {
    listener: TcpListener,
}

impl DirectListener {
    /// Abre un escucha TCP en `addr` (p.ej. `"0.0.0.0:19790"`).
    pub async fn bind(addr: &str) -> Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        Ok(Self { listener })
    }

    /// Dirección local del escucha.
    ///
    /// # Errors
    /// Devuelve error de E/S si el socket ya no es válido.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// Acepta una conexión entrante, devolviendo el transporte crudo y la
    /// dirección remota. El handshake Hello corre a cargo de
    /// [`crate::EncryptedStream::server_handshake`].
    pub async fn accept(&self) -> Result<(Box<dyn ReliableTransport>, SocketAddr)> {
        let (stream, peer) = self.listener.accept().await?;
        debug!(%peer, "conexión TCP aceptada");
        Ok((Box::new(TcpTransport::new(stream)), peer))
    }
}
