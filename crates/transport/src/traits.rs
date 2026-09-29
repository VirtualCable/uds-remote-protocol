//! Contratos de transporte (estudio §4.4).
//!
//! El core del protocolo solo conoce estos traits: no sabe si habla con
//! sockets directos o con un túnel intermedio (tunnel-server UDS, fase 4).

use std::net::SocketAddr;

use async_trait::async_trait;

use crate::error::Result;

/// Destino de una conexión: dirección, ticket de sesión (48 bytes) y clave
/// precompartida (32 bytes; PSK de la PoC, sustituida por ML-KEM después).
#[derive(Debug, Clone)]
pub struct Target {
    addr: SocketAddr,
    ticket: [u8; protocol::TICKET_LENGTH],
    key: [u8; crypt::consts::KEY_LENGTH],
}

impl Target {
    /// Crea un destino directo.
    #[must_use]
    pub fn new(
        addr: SocketAddr,
        ticket: [u8; protocol::TICKET_LENGTH],
        key: [u8; crypt::consts::KEY_LENGTH],
    ) -> Self {
        Self { addr, ticket, key }
    }

    /// Dirección del destino.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Ticket de sesión.
    #[must_use]
    pub fn ticket(&self) -> &[u8; protocol::TICKET_LENGTH] {
        &self.ticket
    }

    /// Clave precompartida.
    #[must_use]
    pub fn key(&self) -> &[u8; crypt::consts::KEY_LENGTH] {
        &self.key
    }
}

/// Stream fiable y ordenado de bytes crudos (equivalente TCP).
///
/// No aplica framing propio: eso es responsabilidad de la capa de protocolo
/// ([`crate::EncryptedStream`]). Así mapea 1:1 con el stream opaco que
/// ofrecerá el futuro adaptador a tunnel-server.
#[async_trait]
pub trait ReliableTransport: Send {
    /// Envía los bytes indicados (escritura completa).
    async fn send(&mut self, bytes: &[u8]) -> Result<()>;
    /// Recibe bytes en `buf`, devolviendo cuántos se leyeron.
    async fn recv_into(&mut self, buf: &mut [u8]) -> Result<usize>;
    /// Cierra el stream.
    async fn close(self: Box<Self>) -> Result<()>;
}

/// Transporte de datagramas no fiables (equivalente UDP). Opcional:
/// si el conector no lo ofrece, todo va por el [`ReliableTransport`].
///
/// Sin implementación en Fase 0 (llega en Fase 1).
#[async_trait]
pub trait DatagramTransport: Send {
    /// Envía un datagrama completo.
    async fn send(&mut self, dgram: &[u8]) -> Result<()>;
    /// Recibe un datagrama completo en `buf`, devolviendo su tamaño.
    async fn recv_into(&mut self, buf: &mut [u8]) -> Result<usize>;
}

/// Fábrica de conexiones (lado cliente).
#[async_trait]
pub trait Connector: Send + Sync {
    /// Conecta con `target`, devolviendo el stream fiable crudo y,
    /// opcionalmente, un transporte de datagramas.
    async fn connect(
        &self,
        target: &Target,
    ) -> Result<(Box<dyn ReliableTransport>, Option<Box<dyn DatagramTransport>>)>;
}
