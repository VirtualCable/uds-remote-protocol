//! UDP leg detection (§4.2): after the TCP handshake, the server offers a
//! UDP endpoint and the client probes it. Lossy channels only move to UDP
//! when a valid `ProbeAck` comes back; otherwise everything stays on TCP.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use crypt::{consts, derive_datagram_keys, derive_udp_token};
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
    let token = derive_udp_token(ticket, secret)?;
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
    let mut buf = [0u8; 1024];
    let (ch, n) = stream.recv_into(&mut buf).await?;
    let msg = ControlMessage::decode(&buf[..n])?;
    let ControlMessage::UdpOffer { port, token } = msg else {
        return Err(TransportError::UnexpectedMessage(format!("{msg:?}")));
    };
    debug_assert_eq!(ch, channel::CONTROL);

    let raw = UdpTransport::connect("0.0.0.0:0", SocketAddr::new(server_ip, port)).await?;
    let keys = derive_datagram_keys(ticket, secret)?;
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
