// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! UDP leg detection (§4.2): after the TCP handshake, the server offers a
//! UDP endpoint and the client probes it. Lossy channels only move to UDP
//! when a valid `ProbeAck` comes back; otherwise everything stays on TCP.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use protocol::crypt::{consts, derive_datagram_keys, derive_udp_token};
use protocol::{ControlMessage, Probe, ProbeAck, channel, compute_rtt};

use crate::datagram::EncryptedDatagram;
use crate::error::{Result, TransportError};
use crate::stream::EncryptedStream;
use crate::traits::{DatagramTransport, ReliableTransport};
use crate::udp::{PendingUdpTransport, UdpTransport};

/// A detected, ready-to-use UDP leg plus the initial RTT measurement.
pub struct UdpLeg<T: DatagramTransport> {
    /// Encrypted datagram channel, ready for traffic.
    pub dgram: EncryptedDatagram<T>,
    /// RTT measured during detection, in the units of the clock passed to
    /// [`udp_detect`] (micros by convention).
    pub initial_rtt_micros: u64,
}

/// Server side of the detection (§4.2): binds the UDP socket and announces
/// port + session token over the encrypted control channel.
///
/// The returned [`PendingUdpTransport`] still needs [`PendingUdpTransport::wait_first`]
/// to learn the client peer from its first datagram. The session token is
/// derived from ticket+secret, so it never travels over the wire in direct
/// mode (the message carries it anyway for future relay-assigned legs).
pub async fn udp_offer<T: ReliableTransport>(
    stream: &mut EncryptedStream<T>,
    udp_bind: &str,
    ticket: &[u8; consts::TICKET_LENGTH],
    secret: &[u8; consts::KEY_LENGTH],
) -> Result<(PendingUdpTransport, [u8; consts::TOKEN_LENGTH])> {
    let pending = UdpTransport::accept_first(udp_bind)?;
    // The datagram leg uses the SAME per-session salt as the stream (§6.1),
    // so its keys and its token are unique per session.
    let salt = stream
        .session_salt()
        .copied()
        .ok_or_else(|| TransportError::UnexpectedMessage("no session salt".into()))?;
    let token = derive_udp_token(ticket, &salt, secret)?;
    let offer = ControlMessage::UdpOffer {
        port: pending.local_addr()?.port(),
        token,
    };
    stream.send(channel::CONTROL, &offer.encode()?).await?;
    Ok((pending, token))
}

/// Client side of the detection (§4.2): reads the `UdpOffer`, then probes
/// the UDP leg with NTP-style probes (`Probe` → `ProbeAck`), up to
/// `max_attempts` tries with `timeout` each.
///
/// Returns `Ok(None)` when no valid ack arrives (UDP filtered/blackholed):
/// the caller keeps lossy channels on TCP. `clock` must return monotonic
/// timestamps (micros by convention); it is never read by this crate itself.
pub async fn udp_detect<T: ReliableTransport>(
    stream: &mut EncryptedStream<T>,
    server_ip: IpAddr,
    ticket: &[u8; consts::TICKET_LENGTH],
    secret: &[u8; consts::KEY_LENGTH],
    clock: impl Fn() -> u64,
    max_attempts: u32,
    timeout: Duration,
) -> Result<Option<UdpLeg<UdpTransport>>> {
    udp_detect_with_progress(
        stream,
        server_ip,
        ticket,
        secret,
        clock,
        max_attempts,
        timeout,
        UDP_OFFER_WAIT,
        |_| {},
    )
    .await
}

/// How long the client waits for the server's `UdpOffer`. The server may spend
/// a while bringing the console up (autologon, mode change) before it offers
/// the datagram leg; the bound keeps a silent server from holding the client.
pub const UDP_OFFER_WAIT: Duration = Duration::from_secs(60);

/// [`udp_detect`] that also reports the server's `SessionProgress` stages sent
/// while it waits for the UDP offer, instead of treating them as a protocol
/// violation.
#[allow(clippy::too_many_arguments)]
pub async fn udp_detect_with_progress<T: ReliableTransport>(
    stream: &mut EncryptedStream<T>,
    server_ip: IpAddr,
    ticket: &[u8; consts::TICKET_LENGTH],
    secret: &[u8; consts::KEY_LENGTH],
    clock: impl Fn() -> u64,
    max_attempts: u32,
    timeout: Duration,
    offer_wait: Duration,
    mut on_progress: impl FnMut(u8),
) -> Result<Option<UdpLeg<UdpTransport>>> {
    let offer = tokio::time::timeout(offer_wait, await_udp_offer(stream, &mut on_progress))
        .await
        .map_err(|_| TransportError::UnexpectedMessage("no UDP offer from the server".into()))??;
    let (port, token) = match offer {
        ControlMessage::UdpOffer { port, token } => (port, token),
        other => return Err(TransportError::UnexpectedMessage(format!("{other:?}"))),
    };
    // The datagram leg uses the SAME per-session salt as the stream (§6.1),
    // so its keys and its token are unique per session.
    let salt = stream
        .session_salt()
        .copied()
        .ok_or_else(|| TransportError::UnexpectedMessage("no session salt".into()))?;
    udp_probe(
        server_ip,
        ticket,
        &salt,
        secret,
        port,
        token,
        clock,
        max_attempts,
        timeout,
    )
    .await
}

/// Probing half of [`udp_detect`] with an already-received offer (§18.53:
/// the client's pre-session gate may have consumed the `UdpOffer` while
/// waiting for the service's `SessionReady`).
#[allow(clippy::too_many_arguments)]
pub async fn udp_probe(
    server_ip: IpAddr,
    ticket: &[u8; consts::TICKET_LENGTH],
    session_salt: &[u8; consts::SESSION_SALT_LENGTH],
    secret: &[u8; consts::KEY_LENGTH],
    port: u16,
    token: [u8; consts::TOKEN_LENGTH],
    clock: impl Fn() -> u64,
    max_attempts: u32,
    timeout: Duration,
) -> Result<Option<UdpLeg<UdpTransport>>> {
    let mut buf = [0u8; 1024];
    let raw = UdpTransport::connect("0.0.0.0:0", SocketAddr::new(server_ip, port)).await?;
    let keys = derive_datagram_keys(ticket, session_salt, secret)?;
    let mut dgram = EncryptedDatagram::new(raw, &keys.c2s, &keys.s2c, &token);

    for _ in 0..max_attempts {
        let t1 = clock();
        dgram.send(channel::CONTROL, &Probe { t1 }.encode()).await?;
        let Ok(Ok((_ch, n))) = tokio::time::timeout(timeout, dgram.recv_into(&mut buf)).await
        else {
            continue; // timeout or noise: retry
        };
        if let Ok(ack) = ProbeAck::decode(&buf[..n])
            && ack.t1 == t1
        {
            let rtt = compute_rtt(t1, ack.t2, ack.t3, clock());
            return Ok(Some(UdpLeg {
                dgram,
                initial_rtt_micros: rtt,
            }));
        }
    }
    Ok(None)
}

/// Reads control messages until the server's UDP offer, reporting progress
/// stages on the way; anything else is a protocol violation.
async fn await_udp_offer<T: ReliableTransport>(
    stream: &mut EncryptedStream<T>,
    on_progress: &mut impl FnMut(u8),
) -> Result<ControlMessage> {
    let mut buf = [0u8; 1024];
    loop {
        let (ch, n) = stream.recv_into(&mut buf).await?;
        if ch != channel::CONTROL {
            return Err(TransportError::UnexpectedMessage(format!("channel {ch:?}")));
        }
        match ControlMessage::decode(&buf[..n])? {
            offer @ ControlMessage::UdpOffer { .. } => return Ok(offer),
            ControlMessage::SessionProgress { stage } => on_progress(stage),
            other => return Err(TransportError::UnexpectedMessage(format!("{other:?}"))),
        }
    }
}
