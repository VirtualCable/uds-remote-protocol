// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Connection-provider abstraction (rdphtml5 `BrokerConnection` pattern,
//! plan §18.39): where a session's secrets and parameters come from.
//!
//! Two modes, selected by `connection_method` in the server config:
//!
//! - **standalone** ([`StandaloneProvider`]): everything comes from the
//!   local TOML; the fixed ticket is validated by constant-time compare.
//!   This is the reference/broker-less mode used in the lab.
//! - **pq** ([`PqProvider`]): the service calls the UDS broker — the
//!   `connection` kind of the `actor/v3` `ticket` action, authenticated
//!   with the actor token — with the ticket the client presented in its
//!   Hello, and gets back a [`SessionGrant`]: the per-session PSK (the
//!   broker response's `shared_secret`), auto-logon credentials and
//!   session parameters (`remotes[0].extra`). The payload schema is ours
//!   (`payload` module).
//!
//! The ticket mapping differs per mode: standalone tickets are human
//! strings mapped with SHA-384 (`server_config::ticket_bytes`); pq tickets
//! are the broker's ticket string travelling raw (UTF-8, NUL-padded) in the
//! 48-byte wire field (`wire_ticket_from_str` / `str_from_wire_ticket`).

mod payload;
mod pq;
mod standalone;

pub use payload::{ChannelPolicy, DisplayPolicy, SessionCredentials, SessionGrant, TicketPayload};
pub use pq::PqProvider;
pub use standalone::StandaloneProvider;

use anyhow::Result;

/// What a provider answers to a validated ticket, plus how the session
/// ends. Mirrors rdphtml5's `BrokerConnection` (start/stop), reduced to
/// our two modes.
#[async_trait::async_trait]
pub trait ConnectionProvider: Send + Sync {
    /// Validates `ticket` and returns the session grant (PSK + params).
    /// `client_ip` is the connecting peer, forwarded to the broker for
    /// auditing/routing decisions.
    ///
    /// Any error rejects the connection before the handshake keys exist.
    async fn start(
        &self,
        ticket: &[u8; protocol::TICKET_LENGTH],
        client_ip: &str,
    ) -> Result<SessionGrant>;

    /// Notifies that the session ended (duration + traffic counters).
    /// Best-effort by default: standalone has nothing to notify.
    async fn stop(&self, _grant: &SessionGrant, _stats: &SessionStats) {}

    /// Human-readable mode name for logs.
    fn mode(&self) -> &'static str;
}

/// End-of-session statistics for the `stop` notification.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionStats {
    /// How long the session lasted.
    pub elapsed: std::time::Duration,
    /// Bytes sent towards the client.
    pub bytes_sent: u64,
    /// Bytes received from the client.
    pub bytes_received: u64,
}

// The ticket wire mapping (raw UTF-8, NUL-padded, pq mode) lives in the
// protocol crate so clients (launcher, labclient) share the exact mapping.
pub use protocol::{str_from_wire_ticket, wire_ticket_from_str};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_ticket_roundtrip() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let wire = wire_ticket_from_str(uuid).unwrap();
        assert_eq!(str_from_wire_ticket(&wire).unwrap(), uuid);
    }

    #[test]
    fn wire_ticket_rejects_overlong() {
        assert!(wire_ticket_from_str(&"x".repeat(49)).is_err());
    }

    #[test]
    fn str_from_wire_rejects_binary() {
        // A standalone (SHA-384) ticket is random bytes, not valid UTF-8.
        let wire = server_config::ticket_bytes("lab-ticket");
        assert!(str_from_wire_ticket(&wire).is_err());
    }
}
