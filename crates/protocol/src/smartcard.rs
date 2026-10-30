//! Smartcard redirection messages (M8, phase 1: physical card relay).
//!
//! The operation flow mirrors the MS-RDPESC cycle (EstablishContext →
//! ListReaders → GetStatusChange → Connect → Status/Transmit/Control/
//! Transactions → Disconnect/ReleaseContext) because that conversation is
//! proven against the real Windows smartcard stack: the server-side virtual
//! reader driver must behave like a real PC/SC reader towards SCardSvr, so
//! talking to the client with the same choreography is the natural fit.
//! What is ours is the encoding — typed tag byte + big-endian fields, same
//! convention as [`crate::clipboard`] / [`crate::storage`], no NDR — and the
//! implementation.
//!
//! Direction: the **server** (where the virtual reader lives) sends
//! requests; the **client** (where the physical card is, via PC/SC)
//! executes them and replies. Every request carries a `request_id` echoed
//! by its reply, so several operations may be in flight (one blocking
//! `GetStatusChange` plus regular traffic). Replies are either the
//! operation's success message or [`SmartcardMessage::Error`] with a raw
//! SCARD error code.
//!
//! All messages travel on the reliable stream (`channel::SMARTCARD`):
//! APDUs tolerate neither loss nor reordering, and the volume is trivial.

/// Wire tag for [`SmartcardMessage::EstablishContext`].
const TAG_ESTABLISH_CONTEXT: u8 = 1;
/// Wire tag for [`SmartcardMessage::ContextEstablished`].
const TAG_CONTEXT_ESTABLISHED: u8 = 2;
/// Wire tag for [`SmartcardMessage::ReleaseContext`].
const TAG_RELEASE_CONTEXT: u8 = 3;
/// Wire tag for [`SmartcardMessage::ContextReleased`].
const TAG_CONTEXT_RELEASED: u8 = 4;
/// Wire tag for [`SmartcardMessage::ListReaders`].
const TAG_LIST_READERS: u8 = 5;
/// Wire tag for [`SmartcardMessage::Readers`].
const TAG_READERS: u8 = 6;
/// Wire tag for [`SmartcardMessage::GetStatusChange`].
const TAG_GET_STATUS_CHANGE: u8 = 7;
/// Wire tag for [`SmartcardMessage::StatusChange`].
const TAG_STATUS_CHANGE: u8 = 8;
/// Wire tag for [`SmartcardMessage::Cancel`].
const TAG_CANCEL: u8 = 9;
/// Wire tag for [`SmartcardMessage::Cancelled`].
const TAG_CANCELLED: u8 = 10;
/// Wire tag for [`SmartcardMessage::Connect`].
const TAG_CONNECT: u8 = 11;
/// Wire tag for [`SmartcardMessage::Connected`].
const TAG_CONNECTED: u8 = 12;
/// Wire tag for [`SmartcardMessage::Disconnect`].
const TAG_DISCONNECT: u8 = 13;
/// Wire tag for [`SmartcardMessage::Disconnected`].
const TAG_DISCONNECTED: u8 = 14;
/// Wire tag for [`SmartcardMessage::Reconnect`].
const TAG_RECONNECT: u8 = 15;
/// Wire tag for [`SmartcardMessage::Reconnected`].
const TAG_RECONNECTED: u8 = 16;
/// Wire tag for [`SmartcardMessage::Status`].
const TAG_STATUS: u8 = 17;
/// Wire tag for [`SmartcardMessage::StatusReply`].
const TAG_STATUS_REPLY: u8 = 18;
/// Wire tag for [`SmartcardMessage::Transmit`].
const TAG_TRANSMIT: u8 = 19;
/// Wire tag for [`SmartcardMessage::TransmitReply`].
const TAG_TRANSMIT_REPLY: u8 = 20;
/// Wire tag for [`SmartcardMessage::Control`].
const TAG_CONTROL: u8 = 21;
/// Wire tag for [`SmartcardMessage::ControlReply`].
const TAG_CONTROL_REPLY: u8 = 22;
/// Wire tag for [`SmartcardMessage::BeginTransaction`].
const TAG_BEGIN_TRANSACTION: u8 = 23;
/// Wire tag for [`SmartcardMessage::TransactionBegun`].
const TAG_TRANSACTION_BEGUN: u8 = 24;
/// Wire tag for [`SmartcardMessage::EndTransaction`].
const TAG_END_TRANSACTION: u8 = 25;
/// Wire tag for [`SmartcardMessage::TransactionEnded`].
const TAG_TRANSACTION_ENDED: u8 = 26;
/// Wire tag for [`SmartcardMessage::Error`].
const TAG_ERROR: u8 = 27;

/// Maximum byte length of a reader name on the wire.
pub const MAX_READER_NAME_LEN: u16 = 512;
/// Maximum ATR length (PC/SC caps it at 33).
pub const MAX_ATR_LEN: u16 = 33;
/// Maximum APDU / control payload per message.
pub const MAX_APDU_LEN: u32 = 65_540;
/// Maximum readers listed in one `Readers` reply.
pub const MAX_READERS: u16 = 64;
/// Maximum reader states in a `GetStatusChange` / `StatusChange`.
pub const MAX_STATES: u16 = 64;

/// Scope for [`SmartcardMessage::EstablishContext`] (`SCARD_SCOPE_*`).
pub const SCOPE_USER: u32 = 0;
/// System scope (`SCARD_SCOPE_SYSTEM`).
pub const SCOPE_SYSTEM: u32 = 2;

/// Errors decoding smartcard messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SmartcardError {
    /// Buffer shorter than the message needs.
    #[error("truncated smartcard message")]
    Truncated,
    /// Unknown message tag byte.
    #[error("unknown smartcard tag: {0}")]
    UnknownTag(u8),
    /// Reader name exceeds [`MAX_READER_NAME_LEN`] or is not UTF-8.
    #[error("bad reader name")]
    BadReaderName,
    /// ATR exceeds [`MAX_ATR_LEN`].
    #[error("atr too long: {0}")]
    AtrTooLong(u16),
    /// APDU/control payload exceeds [`MAX_APDU_LEN`].
    #[error("apdu too long: {0}")]
    ApduTooLong(u32),
    /// More readers/states than the caps allow.
    #[error("too many entries: {0}")]
    TooManyEntries(u16),
}

/// State of one reader in a `GetStatusChange` watch or its reply. Mirrors
/// `SCARD_READERSTATE`: the request carries the `current_state` the caller
/// last saw, the reply carries the new `event_state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderState {
    /// Reader name as PC/SC reports it.
    pub reader: String,
    /// State the caller believes the reader is in (`SCARD_STATE_*`).
    pub current_state: u32,
    /// State the reader is now in (`SCARD_STATE_*`).
    pub event_state: u32,
    /// Card ATR when `event_state` has `SCARD_STATE_PRESENT`.
    pub atr: Vec<u8>,
}

/// One smartcard channel message. Requests are S→C, replies C→S; see the
/// module docs. Every request starts with a `request_id` (client-chosen
/// correlation) that its reply echoes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmartcardMessage {
    // -- Context lifecycle (RDPESC: EstablishContext / ReleaseContext) --
    /// S→C: open a PC/SC context. `scope` is one of `SCOPE_*`.
    EstablishContext {
        /// Correlation id.
        request_id: u32,
        /// `SCARD_SCOPE_*` value.
        scope: u32,
    },
    /// C→S: context opened; `context` is the client-chosen handle used by
    /// every later operation of this context.
    ContextEstablished {
        /// Correlation id.
        request_id: u32,
        /// Client-chosen context handle.
        context: u32,
    },
    /// S→C: close a context (cancels its watches and connections).
    ReleaseContext {
        /// Correlation id.
        request_id: u32,
        /// Context handle.
        context: u32,
    },
    /// C→S: context closed.
    ContextReleased {
        /// Correlation id.
        request_id: u32,
    },
    // -- Reader discovery / watching --
    /// S→C: list the readers of a context.
    ListReaders {
        /// Correlation id.
        request_id: u32,
        /// Context handle.
        context: u32,
    },
    /// C→S: the reader names.
    Readers {
        /// Correlation id.
        request_id: u32,
        /// Reader names as PC/SC reports them.
        readers: Vec<String>,
    },
    /// S→C: block until one of the watched readers changes state or
    /// `timeout_ms` elapses. Replies carry the states even on timeout —
    /// an empty body makes SCardSvr re-enumerate in a flood (RDPESC
    /// lesson).
    GetStatusChange {
        /// Correlation id.
        request_id: u32,
        /// Context handle.
        context: u32,
        /// Timeout in milliseconds (0 = return immediately).
        timeout_ms: u32,
        /// Watched readers with their last known states.
        states: Vec<ReaderState>,
    },
    /// C→S: new states (or unchanged ones on timeout).
    StatusChange {
        /// Correlation id.
        request_id: u32,
        /// New reader states.
        states: Vec<ReaderState>,
    },
    /// S→C: cancel pending `GetStatusChange` watches of a context.
    Cancel {
        /// Correlation id.
        request_id: u32,
        /// Context handle.
        context: u32,
    },
    /// C→S: cancellation done.
    Cancelled {
        /// Correlation id.
        request_id: u32,
    },
    // -- Card connections --
    /// S→C: connect to the card in `reader` (`SCARD_SHARE_*`,
    /// `SCARD_PROTOCOL_*`).
    Connect {
        /// Correlation id.
        request_id: u32,
        /// Context handle.
        context: u32,
        /// Reader name.
        reader: String,
        /// `SCARD_SHARE_*` mode.
        share_mode: u32,
        /// `SCARD_PROTOCOL_*` bitmask of acceptable protocols.
        preferred_protocols: u32,
    },
    /// C→S: connected; `card` is the client-chosen card handle.
    Connected {
        /// Correlation id.
        request_id: u32,
        /// Client-chosen card handle.
        card: u32,
        /// `SCARD_PROTOCOL_*` value actually in use.
        active_protocol: u32,
    },
    /// S→C: disconnect a card (`SCARD_LEAVE/RESET/UNPOWER/EJECT`).
    Disconnect {
        /// Correlation id.
        request_id: u32,
        /// Card handle.
        card: u32,
        /// `SCARD_*` disposition.
        disposition: u32,
    },
    /// C→S: disconnected.
    Disconnected {
        /// Correlation id.
        request_id: u32,
    },
    /// S→C: re-establish an existing connection (msclmd does this when it
    /// finds the card in a bad state).
    Reconnect {
        /// Correlation id.
        request_id: u32,
        /// Card handle.
        card: u32,
        /// `SCARD_SHARE_*` mode.
        share_mode: u32,
        /// `SCARD_PROTOCOL_*` bitmask.
        preferred_protocols: u32,
        /// `SCARD_*` initialization disposition.
        initialization: u32,
    },
    /// C→S: reconnected.
    Reconnected {
        /// Correlation id.
        request_id: u32,
        /// `SCARD_PROTOCOL_*` value now in use.
        active_protocol: u32,
    },
    // -- Card operations --
    /// S→C: card status (reader, state, protocol, ATR).
    Status {
        /// Correlation id.
        request_id: u32,
        /// Card handle.
        card: u32,
    },
    /// C→S: the status.
    StatusReply {
        /// Correlation id.
        request_id: u32,
        /// Reader name.
        reader: String,
        /// `SCARD_STATE_*` value.
        state: u32,
        /// `SCARD_PROTOCOL_*` value.
        protocol: u32,
        /// Card ATR.
        atr: Vec<u8>,
    },
    /// S→C: exchange one APDU. The reply is returned **as-is** — the
    /// remote minidriver chains GET RESPONSE itself; relaying it raw is
    /// the proven-correct behaviour (RDPESC lesson).
    Transmit {
        /// Correlation id.
        request_id: u32,
        /// Card handle.
        card: u32,
        /// `SCARD_PROTOCOL_*` of the send PCI (T=0/T=1).
        send_protocol: u32,
        /// APDU bytes.
        data: Vec<u8>,
    },
    /// C→S: raw card response (including status word).
    TransmitReply {
        /// Correlation id.
        request_id: u32,
        /// `SCARD_PROTOCOL_*` of the recv PCI.
        recv_protocol: u32,
        /// Response bytes.
        data: Vec<u8>,
    },
    /// S→C: `SCardControl` escape (reader-specific features).
    Control {
        /// Correlation id.
        request_id: u32,
        /// Card handle.
        card: u32,
        /// Control code.
        code: u32,
        /// Control payload.
        data: Vec<u8>,
    },
    /// C→S: control response.
    ControlReply {
        /// Correlation id.
        request_id: u32,
        /// Response bytes.
        data: Vec<u8>,
    },
    /// S→C: begin a transaction (exclusive card access for this handle).
    BeginTransaction {
        /// Correlation id.
        request_id: u32,
        /// Card handle.
        card: u32,
    },
    /// C→S: transaction acquired.
    TransactionBegun {
        /// Correlation id.
        request_id: u32,
    },
    /// S→C: end the transaction (`SCARD_LEAVE/RESET/UNPOWER/EJECT`).
    EndTransaction {
        /// Correlation id.
        request_id: u32,
        /// Card handle.
        card: u32,
        /// `SCARD_*` disposition.
        disposition: u32,
    },
    /// C→S: transaction released.
    TransactionEnded {
        /// Correlation id.
        request_id: u32,
    },
    /// C→S: any request failed; `code` is a raw SCARD error (`u32`).
    Error {
        /// Correlation id of the failed request.
        request_id: u32,
        /// SCARD error code.
        code: u32,
    },
}

// Note: failures on the wire are raw SCARD codes inside `Error` messages;
// `SmartcardError` only covers local decode problems.

fn encode_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(MAX_READER_NAME_LEN).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn decode_str(buf: &[u8]) -> Result<(String, usize), SmartcardError> {
    if buf.len() < 2 {
        return Err(SmartcardError::Truncated);
    }
    let len = usize::from(u16::from_be_bytes([buf[0], buf[1]]));
    if len > usize::from(MAX_READER_NAME_LEN) {
        return Err(SmartcardError::BadReaderName);
    }
    let raw = buf.get(2..2 + len).ok_or(SmartcardError::Truncated)?;
    let s = String::from_utf8(raw.to_vec()).map_err(|_| SmartcardError::BadReaderName)?;
    Ok((s, 2 + len))
}

fn encode_bytes_u32(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(data);
}

fn decode_bytes_u32(buf: &[u8], cap: u32) -> Result<Vec<u8>, SmartcardError> {
    if buf.len() < 4 {
        return Err(SmartcardError::Truncated);
    }
    let len = u32::from_be_bytes(buf[0..4].try_into().expect("4 bytes"));
    if len > cap {
        return Err(SmartcardError::ApduTooLong(len));
    }
    Ok(buf
        .get(4..4 + len as usize)
        .ok_or(SmartcardError::Truncated)?
        .to_vec())
}

fn encode_atr(out: &mut Vec<u8>, atr: &[u8]) {
    out.extend_from_slice(&u16::try_from(atr.len()).unwrap_or(MAX_ATR_LEN).to_be_bytes());
    out.extend_from_slice(atr);
}

fn decode_atr(buf: &[u8]) -> Result<(Vec<u8>, usize), SmartcardError> {
    if buf.len() < 2 {
        return Err(SmartcardError::Truncated);
    }
    let len = u16::from_be_bytes([buf[0], buf[1]]);
    if len > MAX_ATR_LEN {
        return Err(SmartcardError::AtrTooLong(len));
    }
    let raw = buf
        .get(2..2 + usize::from(len))
        .ok_or(SmartcardError::Truncated)?;
    Ok((raw.to_vec(), 2 + usize::from(len)))
}

fn encode_state(out: &mut Vec<u8>, state: &ReaderState) {
    encode_str(out, &state.reader);
    out.extend_from_slice(&state.current_state.to_be_bytes());
    out.extend_from_slice(&state.event_state.to_be_bytes());
    encode_atr(out, &state.atr);
}

fn decode_state(buf: &[u8]) -> Result<(ReaderState, usize), SmartcardError> {
    let (reader, used) = decode_str(buf)?;
    let rest = buf.get(used..).ok_or(SmartcardError::Truncated)?;
    if rest.len() < 8 {
        return Err(SmartcardError::Truncated);
    }
    let current_state = u32::from_be_bytes(rest[0..4].try_into().expect("4 bytes"));
    let event_state = u32::from_be_bytes(rest[4..8].try_into().expect("4 bytes"));
    let (atr, atr_used) = decode_atr(&rest[8..])?;
    Ok((
        ReaderState {
            reader,
            current_state,
            event_state,
            atr,
        },
        used + 8 + atr_used,
    ))
}

fn encode_states(out: &mut Vec<u8>, states: &[ReaderState]) {
    out.extend_from_slice(&u16::try_from(states.len()).unwrap_or(MAX_STATES).to_be_bytes());
    for s in states {
        encode_state(out, s);
    }
}

fn decode_states(buf: &[u8]) -> Result<Vec<ReaderState>, SmartcardError> {
    if buf.len() < 2 {
        return Err(SmartcardError::Truncated);
    }
    let count = u16::from_be_bytes([buf[0], buf[1]]);
    if count > MAX_STATES {
        return Err(SmartcardError::TooManyEntries(count));
    }
    let mut rest = &buf[2..];
    let mut states = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let (state, used) = decode_state(rest)?;
        states.push(state);
        rest = rest.get(used..).ok_or(SmartcardError::Truncated)?;
    }
    Ok(states)
}

fn encode_readers(out: &mut Vec<u8>, readers: &[String]) {
    out.extend_from_slice(&u16::try_from(readers.len()).unwrap_or(MAX_READERS).to_be_bytes());
    for r in readers {
        encode_str(out, r);
    }
}

fn decode_readers(buf: &[u8]) -> Result<Vec<String>, SmartcardError> {
    if buf.len() < 2 {
        return Err(SmartcardError::Truncated);
    }
    let count = u16::from_be_bytes([buf[0], buf[1]]);
    if count > MAX_READERS {
        return Err(SmartcardError::TooManyEntries(count));
    }
    let mut rest = &buf[2..];
    let mut readers = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let (reader, used) = decode_str(rest)?;
        readers.push(reader);
        rest = rest.get(used..).ok_or(SmartcardError::Truncated)?;
    }
    Ok(readers)
}

impl SmartcardMessage {
    /// Correlation id of the message (every variant carries one).
    #[must_use]
    pub fn request_id(&self) -> u32 {
        match self {
            Self::EstablishContext { request_id, .. }
            | Self::ContextEstablished { request_id, .. }
            | Self::ReleaseContext { request_id, .. }
            | Self::ContextReleased { request_id }
            | Self::ListReaders { request_id, .. }
            | Self::Readers { request_id, .. }
            | Self::GetStatusChange { request_id, .. }
            | Self::StatusChange { request_id, .. }
            | Self::Cancel { request_id, .. }
            | Self::Cancelled { request_id }
            | Self::Connect { request_id, .. }
            | Self::Connected { request_id, .. }
            | Self::Disconnect { request_id, .. }
            | Self::Disconnected { request_id }
            | Self::Reconnect { request_id, .. }
            | Self::Reconnected { request_id, .. }
            | Self::Status { request_id, .. }
            | Self::StatusReply { request_id, .. }
            | Self::Transmit { request_id, .. }
            | Self::TransmitReply { request_id, .. }
            | Self::Control { request_id, .. }
            | Self::ControlReply { request_id, .. }
            | Self::BeginTransaction { request_id, .. }
            | Self::TransactionBegun { request_id }
            | Self::EndTransaction { request_id, .. }
            | Self::TransactionEnded { request_id }
            | Self::Error { request_id, .. } => *request_id,
        }
    }

    /// Whether the message is a request (S→C). Everything else is a reply
    /// (C→S), including [`SmartcardMessage::Error`].
    #[must_use]
    pub const fn is_request(&self) -> bool {
        matches!(
            self,
            Self::EstablishContext { .. }
                | Self::ReleaseContext { .. }
                | Self::ListReaders { .. }
                | Self::GetStatusChange { .. }
                | Self::Cancel { .. }
                | Self::Connect { .. }
                | Self::Disconnect { .. }
                | Self::Reconnect { .. }
                | Self::Status { .. }
                | Self::Transmit { .. }
                | Self::Control { .. }
                | Self::BeginTransaction { .. }
                | Self::EndTransaction { .. }
        )
    }

    /// Returns the message with its correlation id replaced (used by the
    /// request origin to stamp freshly assigned ids).
    #[must_use]
    pub fn with_request_id(mut self, new_id: u32) -> Self {
        match &mut self {
            Self::EstablishContext { request_id, .. }
            | Self::ContextEstablished { request_id, .. }
            | Self::ReleaseContext { request_id, .. }
            | Self::ContextReleased { request_id }
            | Self::ListReaders { request_id, .. }
            | Self::Readers { request_id, .. }
            | Self::GetStatusChange { request_id, .. }
            | Self::StatusChange { request_id, .. }
            | Self::Cancel { request_id, .. }
            | Self::Cancelled { request_id }
            | Self::Connect { request_id, .. }
            | Self::Connected { request_id, .. }
            | Self::Disconnect { request_id, .. }
            | Self::Disconnected { request_id }
            | Self::Reconnect { request_id, .. }
            | Self::Reconnected { request_id, .. }
            | Self::Status { request_id, .. }
            | Self::StatusReply { request_id, .. }
            | Self::Transmit { request_id, .. }
            | Self::TransmitReply { request_id, .. }
            | Self::Control { request_id, .. }
            | Self::ControlReply { request_id, .. }
            | Self::BeginTransaction { request_id, .. }
            | Self::TransactionBegun { request_id }
            | Self::EndTransaction { request_id, .. }
            | Self::TransactionEnded { request_id }
            | Self::Error { request_id, .. } => *request_id = new_id,
        }
        self
    }

    /// Encodes the message, appending to `out` (allows batching).
    #[expect(clippy::too_many_lines, reason = "flat per-variant encoder, like storage")]
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let id = |out: &mut Vec<u8>, request_id: u32| out.extend_from_slice(&request_id.to_be_bytes());
        match self {
            Self::EstablishContext { request_id, scope } => {
                out.push(TAG_ESTABLISH_CONTEXT);
                id(out, *request_id);
                out.extend_from_slice(&scope.to_be_bytes());
            }
            Self::ContextEstablished { request_id, context } => {
                out.push(TAG_CONTEXT_ESTABLISHED);
                id(out, *request_id);
                out.extend_from_slice(&context.to_be_bytes());
            }
            Self::ReleaseContext { request_id, context } => {
                out.push(TAG_RELEASE_CONTEXT);
                id(out, *request_id);
                out.extend_from_slice(&context.to_be_bytes());
            }
            Self::ContextReleased { request_id }
            | Self::Cancelled { request_id }
            | Self::Disconnected { request_id }
            | Self::TransactionBegun { request_id }
            | Self::TransactionEnded { request_id } => {
                out.push(match self {
                    Self::ContextReleased { .. } => TAG_CONTEXT_RELEASED,
                    Self::Cancelled { .. } => TAG_CANCELLED,
                    Self::Disconnected { .. } => TAG_DISCONNECTED,
                    Self::TransactionBegun { .. } => TAG_TRANSACTION_BEGUN,
                    _ => TAG_TRANSACTION_ENDED,
                });
                id(out, *request_id);
            }
            Self::ListReaders { request_id, context } => {
                out.push(TAG_LIST_READERS);
                id(out, *request_id);
                out.extend_from_slice(&context.to_be_bytes());
            }
            Self::Readers { request_id, readers } => {
                out.push(TAG_READERS);
                id(out, *request_id);
                encode_readers(out, readers);
            }
            Self::GetStatusChange {
                request_id,
                context,
                timeout_ms,
                states,
            } => {
                out.push(TAG_GET_STATUS_CHANGE);
                id(out, *request_id);
                out.extend_from_slice(&context.to_be_bytes());
                out.extend_from_slice(&timeout_ms.to_be_bytes());
                encode_states(out, states);
            }
            Self::StatusChange { request_id, states } => {
                out.push(TAG_STATUS_CHANGE);
                id(out, *request_id);
                encode_states(out, states);
            }
            Self::Cancel { request_id, context } => {
                out.push(TAG_CANCEL);
                id(out, *request_id);
                out.extend_from_slice(&context.to_be_bytes());
            }
            Self::Connect {
                request_id,
                context,
                reader,
                share_mode,
                preferred_protocols,
            } => {
                out.push(TAG_CONNECT);
                id(out, *request_id);
                out.extend_from_slice(&context.to_be_bytes());
                encode_str(out, reader);
                out.extend_from_slice(&share_mode.to_be_bytes());
                out.extend_from_slice(&preferred_protocols.to_be_bytes());
            }
            Self::Connected {
                request_id,
                card,
                active_protocol,
            } => {
                out.push(TAG_CONNECTED);
                id(out, *request_id);
                out.extend_from_slice(&card.to_be_bytes());
                out.extend_from_slice(&active_protocol.to_be_bytes());
            }
            Self::Disconnect {
                request_id,
                card,
                disposition,
            } => {
                out.push(TAG_DISCONNECT);
                id(out, *request_id);
                out.extend_from_slice(&card.to_be_bytes());
                out.extend_from_slice(&disposition.to_be_bytes());
            }
            Self::Reconnect {
                request_id,
                card,
                share_mode,
                preferred_protocols,
                initialization,
            } => {
                out.push(TAG_RECONNECT);
                id(out, *request_id);
                out.extend_from_slice(&card.to_be_bytes());
                out.extend_from_slice(&share_mode.to_be_bytes());
                out.extend_from_slice(&preferred_protocols.to_be_bytes());
                out.extend_from_slice(&initialization.to_be_bytes());
            }
            Self::Reconnected {
                request_id,
                active_protocol,
            } => {
                out.push(TAG_RECONNECTED);
                id(out, *request_id);
                out.extend_from_slice(&active_protocol.to_be_bytes());
            }
            Self::Status { request_id, card } => {
                out.push(TAG_STATUS);
                id(out, *request_id);
                out.extend_from_slice(&card.to_be_bytes());
            }
            Self::StatusReply {
                request_id,
                reader,
                state,
                protocol,
                atr,
            } => {
                out.push(TAG_STATUS_REPLY);
                id(out, *request_id);
                encode_str(out, reader);
                out.extend_from_slice(&state.to_be_bytes());
                out.extend_from_slice(&protocol.to_be_bytes());
                encode_atr(out, atr);
            }
            Self::Transmit {
                request_id,
                card,
                send_protocol,
                data,
            } => {
                out.push(TAG_TRANSMIT);
                id(out, *request_id);
                out.extend_from_slice(&card.to_be_bytes());
                out.extend_from_slice(&send_protocol.to_be_bytes());
                encode_bytes_u32(out, data);
            }
            Self::TransmitReply {
                request_id,
                recv_protocol,
                data,
            } => {
                out.push(TAG_TRANSMIT_REPLY);
                id(out, *request_id);
                out.extend_from_slice(&recv_protocol.to_be_bytes());
                encode_bytes_u32(out, data);
            }
            Self::Control {
                request_id,
                card,
                code,
                data,
            } => {
                out.push(TAG_CONTROL);
                id(out, *request_id);
                out.extend_from_slice(&card.to_be_bytes());
                out.extend_from_slice(&code.to_be_bytes());
                encode_bytes_u32(out, data);
            }
            Self::ControlReply { request_id, data } => {
                out.push(TAG_CONTROL_REPLY);
                id(out, *request_id);
                encode_bytes_u32(out, data);
            }
            Self::BeginTransaction { request_id, card } => {
                out.push(TAG_BEGIN_TRANSACTION);
                id(out, *request_id);
                out.extend_from_slice(&card.to_be_bytes());
            }
            Self::EndTransaction {
                request_id,
                card,
                disposition,
            } => {
                out.push(TAG_END_TRANSACTION);
                id(out, *request_id);
                out.extend_from_slice(&card.to_be_bytes());
                out.extend_from_slice(&disposition.to_be_bytes());
            }
            Self::Error { request_id, code } => {
                out.push(TAG_ERROR);
                id(out, *request_id);
                out.extend_from_slice(&code.to_be_bytes());
            }
        }
    }

    /// Encodes the message into a fresh buffer.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// Decodes one message from the front of `buf`.
    ///
    /// # Errors
    /// Returns [`SmartcardError`] on truncation, unknown tags or oversized
    /// fields.
    #[expect(clippy::too_many_lines, reason = "flat tag dispatch, like storage")]
    pub fn decode(buf: &[u8]) -> Result<Self, SmartcardError> {
        let (&tag, rest) = buf.split_first().ok_or(SmartcardError::Truncated)?;
        let u32_at = |b: &[u8], off: usize| -> Result<u32, SmartcardError> {
            let raw = b.get(off..off + 4).ok_or(SmartcardError::Truncated)?;
            Ok(u32::from_be_bytes(raw.try_into().expect("4 bytes")))
        };
        let request_id = u32_at(rest, 0)?;
        let r = &rest[4..];
        Ok(match tag {
            TAG_ESTABLISH_CONTEXT => Self::EstablishContext {
                request_id,
                scope: u32_at(r, 0)?,
            },
            TAG_CONTEXT_ESTABLISHED => Self::ContextEstablished {
                request_id,
                context: u32_at(r, 0)?,
            },
            TAG_RELEASE_CONTEXT => Self::ReleaseContext {
                request_id,
                context: u32_at(r, 0)?,
            },
            TAG_CONTEXT_RELEASED => Self::ContextReleased { request_id },
            TAG_LIST_READERS => Self::ListReaders {
                request_id,
                context: u32_at(r, 0)?,
            },
            TAG_READERS => Self::Readers {
                request_id,
                readers: decode_readers(r)?,
            },
            TAG_GET_STATUS_CHANGE => Self::GetStatusChange {
                request_id,
                context: u32_at(r, 0)?,
                timeout_ms: u32_at(r, 4)?,
                states: decode_states(r.get(8..).ok_or(SmartcardError::Truncated)?)?,
            },
            TAG_STATUS_CHANGE => Self::StatusChange {
                request_id,
                states: decode_states(r)?,
            },
            TAG_CANCEL => Self::Cancel {
                request_id,
                context: u32_at(r, 0)?,
            },
            TAG_CANCELLED => Self::Cancelled { request_id },
            TAG_DISCONNECTED => Self::Disconnected { request_id },
            TAG_CONNECT => {
                let (reader, used) = decode_str(r.get(4..).ok_or(SmartcardError::Truncated)?)?;
                let after = r.get(4 + used..).ok_or(SmartcardError::Truncated)?;
                Self::Connect {
                    request_id,
                    context: u32_at(r, 0)?,
                    reader,
                    share_mode: u32_at(after, 0)?,
                    preferred_protocols: u32_at(after, 4)?,
                }
            }
            TAG_CONNECTED => Self::Connected {
                request_id,
                card: u32_at(r, 0)?,
                active_protocol: u32_at(r, 4)?,
            },
            TAG_DISCONNECT => Self::Disconnect {
                request_id,
                card: u32_at(r, 0)?,
                disposition: u32_at(r, 4)?,
            },
            TAG_RECONNECT => Self::Reconnect {
                request_id,
                card: u32_at(r, 0)?,
                share_mode: u32_at(r, 4)?,
                preferred_protocols: u32_at(r, 8)?,
                initialization: u32_at(r, 12)?,
            },
            TAG_RECONNECTED => Self::Reconnected {
                request_id,
                active_protocol: u32_at(r, 0)?,
            },
            TAG_STATUS => Self::Status {
                request_id,
                card: u32_at(r, 0)?,
            },
            TAG_STATUS_REPLY => {
                let (reader, used) = decode_str(r)?;
                let after = r.get(used..).ok_or(SmartcardError::Truncated)?;
                let (atr, _) = decode_atr(after.get(8..).ok_or(SmartcardError::Truncated)?)?;
                Self::StatusReply {
                    request_id,
                    reader,
                    state: u32_at(after, 0)?,
                    protocol: u32_at(after, 4)?,
                    atr,
                }
            }
            TAG_TRANSMIT => Self::Transmit {
                request_id,
                card: u32_at(r, 0)?,
                send_protocol: u32_at(r, 4)?,
                data: decode_bytes_u32(r.get(8..).ok_or(SmartcardError::Truncated)?, MAX_APDU_LEN)?,
            },
            TAG_TRANSMIT_REPLY => Self::TransmitReply {
                request_id,
                recv_protocol: u32_at(r, 0)?,
                data: decode_bytes_u32(r.get(4..).ok_or(SmartcardError::Truncated)?, MAX_APDU_LEN)?,
            },
            TAG_CONTROL => Self::Control {
                request_id,
                card: u32_at(r, 0)?,
                code: u32_at(r, 4)?,
                data: decode_bytes_u32(r.get(8..).ok_or(SmartcardError::Truncated)?, MAX_APDU_LEN)?,
            },
            TAG_CONTROL_REPLY => Self::ControlReply {
                request_id,
                data: decode_bytes_u32(r, MAX_APDU_LEN)?,
            },
            TAG_BEGIN_TRANSACTION => Self::BeginTransaction {
                request_id,
                card: u32_at(r, 0)?,
            },
            TAG_TRANSACTION_BEGUN => Self::TransactionBegun { request_id },
            TAG_END_TRANSACTION => Self::EndTransaction {
                request_id,
                card: u32_at(r, 0)?,
                disposition: u32_at(r, 4)?,
            },
            TAG_TRANSACTION_ENDED => Self::TransactionEnded { request_id },
            TAG_ERROR => Self::Error {
                request_id,
                code: u32_at(r, 0)?,
            },
            other => return Err(SmartcardError::UnknownTag(other)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: SmartcardMessage) {
        assert_eq!(SmartcardMessage::decode(&msg.encode()), Ok(msg));
    }

    #[test]
    fn context_lifecycle_roundtrips() {
        roundtrip(SmartcardMessage::EstablishContext {
            request_id: 1,
            scope: SCOPE_USER,
        });
        roundtrip(SmartcardMessage::ContextEstablished {
            request_id: 1,
            context: 42,
        });
        roundtrip(SmartcardMessage::ReleaseContext {
            request_id: 2,
            context: 42,
        });
        roundtrip(SmartcardMessage::ContextReleased { request_id: 2 });
        roundtrip(SmartcardMessage::Cancel {
            request_id: 3,
            context: 42,
        });
        roundtrip(SmartcardMessage::Cancelled { request_id: 3 });
    }

    #[test]
    fn discovery_roundtrips() {
        roundtrip(SmartcardMessage::ListReaders {
            request_id: 1,
            context: 7,
        });
        roundtrip(SmartcardMessage::Readers {
            request_id: 1,
            readers: vec![
                "ACS ACR39U 00 00".to_string(),
                "\\\\?\\PnP?\\Notification".to_string(),
            ],
        });
        let states = vec![
            ReaderState {
                reader: "ACS ACR39U 00 00".to_string(),
                current_state: 0,
                event_state: 0x22,
                atr: vec![0x3B, 0x89, 0x01, 0x45],
            },
            ReaderState {
                reader: "empty".to_string(),
                current_state: 0x10,
                event_state: 0x10,
                atr: Vec::new(),
            },
        ];
        roundtrip(SmartcardMessage::GetStatusChange {
            request_id: 2,
            context: 7,
            timeout_ms: 1000,
            states: states.clone(),
        });
        roundtrip(SmartcardMessage::StatusChange {
            request_id: 2,
            states,
        });
    }

    #[test]
    fn connection_roundtrips() {
        roundtrip(SmartcardMessage::Connect {
            request_id: 1,
            context: 7,
            reader: "ACS ACR39U 00 00".to_string(),
            share_mode: 2,
            preferred_protocols: 3,
        });
        roundtrip(SmartcardMessage::Connected {
            request_id: 1,
            card: 99,
            active_protocol: 2,
        });
        roundtrip(SmartcardMessage::Reconnect {
            request_id: 2,
            card: 99,
            share_mode: 2,
            preferred_protocols: 3,
            initialization: 1,
        });
        roundtrip(SmartcardMessage::Reconnected {
            request_id: 2,
            active_protocol: 1,
        });
        roundtrip(SmartcardMessage::Disconnect {
            request_id: 3,
            card: 99,
            disposition: 0,
        });
        roundtrip(SmartcardMessage::Disconnected { request_id: 3 });
    }

    #[test]
    fn operation_roundtrips() {
        roundtrip(SmartcardMessage::Status {
            request_id: 1,
            card: 99,
        });
        roundtrip(SmartcardMessage::StatusReply {
            request_id: 1,
            reader: "ACS ACR39U 00 00".to_string(),
            state: 0x22,
            protocol: 2,
            atr: vec![0x3B; 20],
        });
        roundtrip(SmartcardMessage::Transmit {
            request_id: 2,
            card: 99,
            send_protocol: 2,
            data: vec![0x00, 0xA4, 0x04, 0x00],
        });
        roundtrip(SmartcardMessage::TransmitReply {
            request_id: 2,
            recv_protocol: 2,
            data: vec![0x90, 0x00],
        });
        roundtrip(SmartcardMessage::Control {
            request_id: 3,
            card: 99,
            code: 0x0031_0000,
            data: vec![1, 2, 3],
        });
        roundtrip(SmartcardMessage::ControlReply {
            request_id: 3,
            data: vec![4, 5, 6],
        });
        roundtrip(SmartcardMessage::BeginTransaction {
            request_id: 4,
            card: 99,
        });
        roundtrip(SmartcardMessage::TransactionBegun { request_id: 4 });
        roundtrip(SmartcardMessage::EndTransaction {
            request_id: 5,
            card: 99,
            disposition: 0,
        });
        roundtrip(SmartcardMessage::TransactionEnded { request_id: 5 });
        roundtrip(SmartcardMessage::Error {
            request_id: 6,
            code: 0x8010_0002,
        });
    }

    #[test]
    fn oversized_fields_are_rejected() {
        // ATR too long.
        let mut buf = vec![TAG_STATUS_CHANGE, 0, 0, 0, 1, 0, 1];
        buf.extend_from_slice(&1u16.to_be_bytes()); // name len
        buf.push(b'a');
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&(MAX_ATR_LEN + 1).to_be_bytes());
        assert!(matches!(
            SmartcardMessage::decode(&buf),
            Err(SmartcardError::AtrTooLong(_))
        ));
        // APDU too long.
        let mut buf = vec![TAG_TRANSMIT_REPLY, 0, 0, 0, 1];
        buf.extend_from_slice(&2u32.to_be_bytes());
        buf.extend_from_slice(&(MAX_APDU_LEN + 1).to_be_bytes());
        assert!(matches!(
            SmartcardMessage::decode(&buf),
            Err(SmartcardError::ApduTooLong(_))
        ));
        // Too many states.
        let mut buf = vec![TAG_STATUS_CHANGE, 0, 0, 0, 1];
        buf.extend_from_slice(&(MAX_STATES + 1).to_be_bytes());
        assert_eq!(
            SmartcardMessage::decode(&buf),
            Err(SmartcardError::TooManyEntries(MAX_STATES + 1))
        );
    }

    #[test]
    fn empty_and_truncated_buffers() {
        assert_eq!(
            SmartcardMessage::decode(&[]),
            Err(SmartcardError::Truncated)
        );
        assert_eq!(
            SmartcardMessage::decode(&[TAG_CONNECT, 0, 0]),
            Err(SmartcardError::Truncated)
        );
        assert_eq!(
            SmartcardMessage::decode(&[222, 0, 0, 0, 0]),
            Err(SmartcardError::UnknownTag(222))
        );
    }
}
