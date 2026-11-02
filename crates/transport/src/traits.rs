// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Transport contracts (design doc §4.4).
//!
//! The protocol core only knows these traits: it does not know whether it is
//! talking to direct sockets or to a local listener proxied through the UDS
//! tunnel by the launcher.

use std::net::SocketAddr;

use async_trait::async_trait;

use crate::error::Result;

/// Destination of a connection: address, session ticket (48 bytes) and
/// pre-shared key (32 bytes; PoC PSK, later replaced by ML-KEM).
#[derive(Debug, Clone)]
pub struct Target {
    addr: SocketAddr,
    ticket: [u8; protocol::TICKET_LENGTH],
    key: [u8; crypt::consts::KEY_LENGTH],
}

impl Target {
    /// Creates a direct destination.
    #[must_use]
    pub fn new(
        addr: SocketAddr,
        ticket: [u8; protocol::TICKET_LENGTH],
        key: [u8; crypt::consts::KEY_LENGTH],
    ) -> Self {
        Self { addr, ticket, key }
    }

    /// Destination address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Session ticket.
    #[must_use]
    pub fn ticket(&self) -> &[u8; protocol::TICKET_LENGTH] {
        &self.ticket
    }

    /// Pre-shared key.
    #[must_use]
    pub fn key(&self) -> &[u8; crypt::consts::KEY_LENGTH] {
        &self.key
    }
}

/// Reliable, ordered stream of raw bytes (TCP equivalent).
///
/// It does not apply its own framing: that is the responsibility of the
/// protocol layer ([`crate::EncryptedStream`]). This maps 1:1 to the opaque
/// stream the launcher's local tunnel listener offers.
#[async_trait]
pub trait ReliableTransport: Send {
    /// Sends the given bytes (full write).
    async fn send(&mut self, bytes: &[u8]) -> Result<()>;
    /// Receives bytes into `buf`, returning how many were read.
    async fn recv_into(&mut self, buf: &mut [u8]) -> Result<usize>;
    /// Closes the stream.
    async fn close(self: Box<Self>) -> Result<()>;
}

/// Unreliable datagram transport (UDP equivalent). Optional:
/// if the connector does not offer it, everything goes over the [`ReliableTransport`].
///
/// No implementation in Phase 0 (arrives in Phase 1).
#[async_trait]
pub trait DatagramTransport: Send + Sync {
    /// Sends a full datagram. Takes `&self` so the transport can be shared
    /// (`Arc`) between a sender task and a receiver loop (§12.6 pacing).
    async fn send(&self, dgram: &[u8]) -> Result<()>;
    /// Receives a full datagram into `buf`, returning its size.
    async fn recv_into(&self, buf: &mut [u8]) -> Result<usize>;
}

/// Connection factory (client side).
#[async_trait]
pub trait Connector: Send + Sync {
    /// Connects to `target`, returning the raw reliable stream and,
    /// optionally, a datagram transport.
    async fn connect(
        &self,
        target: &Target,
    ) -> Result<(Box<dyn ReliableTransport>, Option<Box<dyn DatagramTransport>>)>;
}
