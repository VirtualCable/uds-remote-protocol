//! End-to-end encrypted stream over any [`ReliableTransport`].
//!
//! This layer is identical both in a direct connection and through the future
//! tunnel: it applies the Hello handshake and the encrypted framing of `crypt`
//! (record `|seq 8B|len 2B|encrypted channel_id|payload|tag|`, see design doc §6).

use crypt::{SessionKeys, StreamCrypt, StreamDecrypt, consts, derive_session_keys};
use protocol::{ChannelId, Command, Hello, PROTOCOL_VERSION, TICKET_LENGTH};

use crate::error::{Result, TransportError};
use crate::traits::ReliableTransport;

/// Reliable stream with Hello handshake and E2E encryption on top of a raw
/// [`ReliableTransport`] (direct TCP or tunnel stream).
pub struct EncryptedStream<T: ReliableTransport> {
    inner: T,
    tx: StreamCrypt,
    rx: StreamDecrypt,
    peer_hello: Option<Hello>,
    // Reusable receive buffer (no allocs on the hot path, riesgos r2 §2.1).
    rx_buf: Vec<u8>,
    /// Bytes of the current record already read (header + body). Kept
    /// across calls so `recv_into` is cancellation-safe (see its docs).
    rx_filled: usize,
    /// Declared body length once the header is fully read.
    rx_record_len: Option<usize>,
}

/// Stream sequence numbers of one end (recovery, §4.3).
///
/// `send` is the seq of the next record this end will emit; `recv` is the
/// next seq it expects from the peer. Both must be saved before a drop and
/// presented again on `Recover`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamSeqs {
    /// Next outgoing seq.
    pub send: u64,
    /// Next expected incoming seq.
    pub recv: u64,
}

/// Outcome of a client `Recover` attempt.
pub enum RecoverOutcome<T: ReliableTransport> {
    /// Session resumed with merged seqs.
    Resumed(Box<EncryptedStream<T>>),
    /// The server has no stored state for this ticket: the caller must
    /// reconnect with a full `Open`.
    SessionUnknown,
}

impl<T: ReliableTransport> EncryptedStream<T> {
    /// Creates the encrypted stream with explicit send/receive keys.
    #[must_use]
    pub fn new(inner: T, tx_key: &[u8; consts::KEY_LENGTH], rx_key: &[u8; consts::KEY_LENGTH]) -> Self {
        Self {
            inner,
            tx: StreamCrypt::new(tx_key),
            rx: StreamDecrypt::new(rx_key),
            peer_hello: None,
            rx_buf: Vec::with_capacity(2048),
            rx_filled: 0,
            rx_record_len: None,
        }
    }

    /// Current sequence numbers (save them before a drop for `Recover`).
    #[must_use]
    pub fn seqs(&self) -> StreamSeqs {
        StreamSeqs {
            send: self.tx.seq(),
            recv: self.rx.next_seq(),
        }
    }

    /// Client handshake: sends the Hello (`Open` + ticket) and derives the
    /// session keys from the ticket and the pre-shared key (PoC PSK;
    /// ML-KEM comes later).
    ///
    /// `flags` is the HELLO flags byte (see `protocol::FLAG_PRIVACY_SCREEN`).
    pub async fn client_handshake(
        mut inner: T,
        ticket: &[u8; TICKET_LENGTH],
        key: &[u8; consts::KEY_LENGTH],
        flags: u8,
    ) -> Result<Self> {
        let hello = Hello {
            version: PROTOCOL_VERSION,
            command: Command::Open,
            flags,
            ticket: *ticket,
        };
        inner.send(&hello.encode()).await?;
        let keys = derive_session_keys(ticket, key)?;
        Ok(Self::with_keys(inner, &keys, Role::Client))
    }

    /// Client recovery (§4.3): sends Hello (`Recover` + ticket) and the
    /// saved seqs, and resumes the stream if the server still holds session
    /// state for the ticket.
    ///
    /// Wire (cleartext, after the Hello):
    /// `S→C status(1B)`; if 1: `C→S |send 8B|recv 8B|`, `S→C |send 8B|recv 8B|`
    /// with the seqs actually applied by the server. Each side resumes at
    /// `max(own, peer expectation)` so a `(key, nonce)` pair never repeats,
    /// even for records lost in flight during the drop.
    /// `flags` is the HELLO flags byte (see `protocol::FLAG_PRIVACY_SCREEN`).
    pub async fn client_recover(
        mut inner: T,
        ticket: &[u8; TICKET_LENGTH],
        key: &[u8; consts::KEY_LENGTH],
        seqs: StreamSeqs,
        flags: u8,
    ) -> Result<RecoverOutcome<T>> {
        let hello = Hello {
            version: PROTOCOL_VERSION,
            command: Command::Recover,
            flags,
            ticket: *ticket,
        };
        inner.send(&hello.encode()).await?;

        let mut status = [0u8; 1];
        read_full(&mut inner, &mut status).await?;
        if status[0] != RECOVER_OK {
            return Ok(RecoverOutcome::SessionUnknown);
        }

        let mut out = [0u8; 16];
        out[..8].copy_from_slice(&seqs.send.to_be_bytes());
        out[8..].copy_from_slice(&seqs.recv.to_be_bytes());
        inner.send(&out).await?;

        let mut applied = [0u8; 16];
        read_full(&mut inner, &mut applied).await?;
        let server_send = u64::from_be_bytes(applied[..8].try_into().expect("8-byte slice"));
        let server_recv = u64::from_be_bytes(applied[8..].try_into().expect("8-byte slice"));

        let keys = derive_session_keys(ticket, key)?;
        let mut stream = Self::with_keys(inner, &keys, Role::Client);
        stream.tx = StreamCrypt::with_seq(&keys.c2s, seqs.send.max(server_recv));
        stream.rx = StreamDecrypt::with_seq(&keys.s2c, seqs.recv.max(server_send));
        Ok(RecoverOutcome::Resumed(Box::new(stream)))
    }

    /// Server handshake: reads and validates the peer's Hello and derives
    /// the session keys from the received ticket and the pre-shared key.
    ///
    /// `session` is the server's stored seqs for the ticket (see
    /// [`EncryptedStream::seqs`]): required to honour a `Recover`; pass
    /// `None` when no state is held (a `Recover` is then answered with
    /// "session unknown" and the call fails with
    /// [`TransportError::UnexpectedMessage`]).
    pub async fn server_handshake(
        mut inner: T,
        key: &[u8; consts::KEY_LENGTH],
        session: Option<StreamSeqs>,
    ) -> Result<Self> {
        let mut buf = [0u8; Hello::ENCODED_LEN];
        read_full(&mut inner, &mut buf).await?;
        let hello = Hello::decode(&buf)?;
        // Wire formats differ between versions (v1→v2 changed the fragment
        // header); a mismatching peer must fail loudly, never parse silently.
        if hello.version != PROTOCOL_VERSION {
            return Err(TransportError::VersionMismatch {
                expected: PROTOCOL_VERSION,
                got: hello.version,
            });
        }
        let keys = derive_session_keys(&hello.ticket, key)?;
        let mut stream = Self::with_keys(inner, &keys, Role::Server);
        stream.peer_hello = Some(hello);

        if stream.peer_hello.as_ref().expect("just set").command == Command::Recover {
            let Some(stored) = session else {
                stream.inner.send(&[RECOVER_UNKNOWN]).await?;
                return Err(TransportError::UnexpectedMessage(
                    "Recover without stored session".into(),
                ));
            };
            stream.inner.send(&[RECOVER_OK]).await?;

            let mut declared = [0u8; 16];
            read_full(&mut stream.inner, &mut declared).await?;
            let client_send = u64::from_be_bytes(declared[..8].try_into().expect("8-byte slice"));
            let client_recv = u64::from_be_bytes(declared[8..].try_into().expect("8-byte slice"));

            let send = stored.send.max(client_recv);
            let recv = stored.recv.max(client_send);
            let mut applied = [0u8; 16];
            applied[..8].copy_from_slice(&send.to_be_bytes());
            applied[8..].copy_from_slice(&recv.to_be_bytes());
            stream.inner.send(&applied).await?;

            stream.tx = StreamCrypt::with_seq(&keys.s2c, send);
            stream.rx = StreamDecrypt::with_seq(&keys.c2s, recv);
        }
        Ok(stream)
    }

    fn with_keys(inner: T, keys: &SessionKeys, role: Role) -> Self {
        let (tx_key, rx_key) = match role {
            Role::Client => (&keys.c2s, &keys.s2c),
            Role::Server => (&keys.s2c, &keys.c2s),
        };
        Self::new(inner, tx_key, rx_key)
    }

    /// Peer Hello (only available on the server side after the handshake).
    #[must_use]
    pub fn peer_hello(&self) -> Option<&Hello> {
        self.peer_hello.as_ref()
    }

    /// Sends `payload` encrypted on the given channel, as a single record.
    pub async fn send(&mut self, channel: ChannelId, payload: &[u8]) -> Result<()> {
        if payload.len() > consts::MAX_PLAINTEXT {
            return Err(TransportError::TooLarge {
                size: payload.len(),
                max: consts::MAX_PLAINTEXT,
            });
        }
        let record = self.tx.encrypt(channel, payload)?;
        self.inner.send(&record).await
    }

    /// Receives a full record: returns the channel and the length of the
    /// decrypted payload, copied into `buf`.
    ///
    /// Cancellation-safe: the read progress (`rx_filled`, `rx_record_len`)
    /// lives in `self`, so a `select!` that drops this future mid-record
    /// resumes exactly where it stopped instead of losing the bytes
    /// already pulled from the transport and desynchronizing the stream.
    /// (Large records arrive in several TCP segments and genuinely pend;
    /// small ones used to complete synchronously, hiding the hazard.)
    pub async fn recv_into(&mut self, buf: &mut [u8]) -> Result<(ChannelId, usize)> {
        // Phase 1: header (STREAM_HEADER_SIZE bytes).
        if self.rx_record_len.is_none() {
            self.rx_buf.resize(consts::STREAM_HEADER_SIZE, 0);
            while self.rx_filled < consts::STREAM_HEADER_SIZE {
                self.rx_filled += self
                    .inner
                    .recv_into(&mut self.rx_buf[self.rx_filled..consts::STREAM_HEADER_SIZE])
                    .await?;
            }
            let len = u16::from_be_bytes([self.rx_buf[8], self.rx_buf[9]]) as usize;
            self.rx_buf.resize(consts::STREAM_HEADER_SIZE + len, 0);
            self.rx_record_len = Some(len);
        }
        // Phase 2: body.
        let total = consts::STREAM_HEADER_SIZE + self.rx_record_len.expect("set above");
        while self.rx_filled < total {
            self.rx_filled += self
                .inner
                .recv_into(&mut self.rx_buf[self.rx_filled..total])
                .await?;
        }
        self.rx_filled = 0;
        self.rx_record_len = None;

        let (channel, plain) = self.rx.decrypt(&self.rx_buf)?;
        if plain.len() > buf.len() {
            return Err(TransportError::TooLarge {
                size: plain.len(),
                max: buf.len(),
            });
        }
        buf[..plain.len()].copy_from_slice(&plain);
        Ok((channel, plain.len()))
    }
}

#[derive(Clone, Copy)]
enum Role {
    Client,
    Server,
}

/// Recovery status byte: the server holds session state for the ticket.
const RECOVER_OK: u8 = 1;
/// Recovery status byte: no stored session; the client must re-`Open`.
const RECOVER_UNKNOWN: u8 = 0;

/// Reads exactly `buf.len()` bytes from the transport (a TCP `read` may
/// return fewer bytes than requested).
async fn read_full<T: ReliableTransport>(t: &mut T, buf: &mut [u8]) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        filled += t.recv_into(&mut buf[filled..]).await?;
    }
    Ok(())
}
