//! # transport
//!
//! Transport abstraction of the protocol: the core does not know whether it
//! is talking to direct sockets or to an intermediate tunnel.
//!
//! - [`ReliableTransport`]: reliable, ordered stream (TCP equivalent).
//! - [`DatagramTransport`]: unreliable datagrams (UDP equivalent), optional.
//! - [`Connector`]: connection factory (client side).
//! - [`EncryptedStream`]: Hello handshake + E2E encryption over any
//!   [`ReliableTransport`] (layer shared with the future tunnel).
//! - [`EncryptedDatagram`]: E2E encryption over any [`DatagramTransport`].
//!
//! Implementations:
//! - [`DirectConnector`] / [`DirectListener`] (PoC phase): real TCP.
//! - [`UdpTransport`]: real UDP.
//! - `TunnelConnector` (phase 4, separate crate): UDS tunnel-server adapter.

mod datagram;
mod detect;
mod error;
mod stream;
mod tcp;
mod traits;
mod udp;

pub use datagram::EncryptedDatagram;
pub use detect::{UdpLeg, udp_detect, udp_offer};
pub use error::{Result, TransportError};
pub use stream::{EncryptedStream, RecoverOutcome, StreamSeqs};
pub use tcp::{DirectConnector, DirectListener, TcpTransport};
pub use traits::{Connector, DatagramTransport, ReliableTransport, Target};
pub use udp::{PendingUdpTransport, UdpTransport};
