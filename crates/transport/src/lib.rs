// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! # transport
//!
//! Transport abstraction of the protocol: the core does not know whether it
//! is talking to direct sockets or to an intermediate tunnel.
//!
//! - [`ReliableTransport`]: reliable, ordered stream (TCP equivalent).
//! - [`DatagramTransport`]: unreliable datagrams (UDP equivalent), optional.
//! - [`Connector`]: connection factory (client side).
//! - [`EncryptedStream`]: Hello handshake + E2E encryption over any
//!   [`ReliableTransport`].
//! - [`EncryptedDatagram`]: E2E encryption over any [`DatagramTransport`].
//!
//! Implementations:
//! - [`DirectConnector`] / [`DirectListener`]: real TCP.
//! - [`UdpTransport`]: real UDP.
//!
//! There is no tunnel adapter crate: gateway access is provided by the UDS
//! launcher, which exposes the tunnel as a local TCP+UDP listener (freerdp
//! style). This connector then targets `127.0.0.1:<tunnel port>` unchanged.

mod datagram;
mod detect;
mod error;
mod stream;
mod tcp;
mod traits;
mod udp;

pub use datagram::{DatagramSink, DatagramSource, EncryptedDatagram};
pub use detect::{UdpLeg, udp_detect, udp_offer};
pub use error::{Result, TransportError};
pub use stream::{EncryptedStream, RecoverOutcome, StreamSeqs};
pub use tcp::{DirectConnector, DirectListener, TcpTransport};
pub use traits::{Connector, DatagramTransport, ReliableTransport, Target};
pub use udp::{PendingUdpTransport, UdpTransport};
