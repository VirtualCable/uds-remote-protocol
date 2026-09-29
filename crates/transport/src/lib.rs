//! # transport
//!
//! Abstracción de transporte del protocolo: el core no sabe si habla con
//! sockets directos o con un túnel intermedio.
//!
//! - [`ReliableTransport`]: stream fiable y ordenado (equivalente TCP).
//! - [`DatagramTransport`]: datagramas no fiables (equivalente UDP), opcional.
//! - [`Connector`]: fábrica de conexiones (lado cliente).
//! - [`EncryptedStream`]: handshake Hello + cifrado E2E sobre cualquier
//!   [`ReliableTransport`] (capa compartida con el futuro túnel).
//!
//! Implementaciones:
//! - [`DirectConnector`] / [`DirectListener`] (fase PoC): TCP real.
//! - `TunnelConnector` (fase 4, crate aparte): adaptador a tunnel-server UDS.

mod error;
mod stream;
mod tcp;
mod traits;

pub use error::{Result, TransportError};
pub use stream::EncryptedStream;
pub use tcp::{DirectConnector, DirectListener, TcpTransport};
pub use traits::{Connector, DatagramTransport, ReliableTransport, Target};
