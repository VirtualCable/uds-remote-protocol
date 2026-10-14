//! Host-independent logic for the UDS Credential Provider: the wire protocol
//! spoken over the local named pipe between the `uds-rp` service (pipe
//! server, runs as SYSTEM in session 0) and the credential provider DLL
//! (pipe client, loaded by LogonUI).
//!
//! Everything here is free of Windows types so it compiles and is unit-tested
//! on Linux; the Windows side (`imp`, cfg-gated) only moves the frames
//! through the named pipe. The same module is linked by `udscredprov-ctl`,
//! which plays the server role for spike verification.
//!
//! Wire format (all integers little-endian):
//!
//! ```text
//! [magic: u32][version: u16][kind: u16][payload_len: u32][payload…]
//! ```
//!
//! Payloads are sequences of fields; strings are `[len: u16][utf-8 bytes]`.
//! The protocol is versioned: a peer that does not understand `version` must
//! drop the connection.

use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

/// Magic frame header (`"USRP"` in ASCII, little-endian).
pub const MAGIC: u32 = 0x5553_5250;

/// Current protocol version. Bump on any incompatible wire change.
pub const PROTOCOL_VERSION: u16 = 1;

/// Well-known pipe name. The service (`uds-rp`) is the server; the
/// credential provider connects as a client from inside LogonUI (SYSTEM).
pub const PIPE_NAME: &str = r"\\.\pipe\udsrp-credprov";

/// Maximum accepted payload size. Credentials are small; anything larger is
/// a protocol violation and the frame is rejected.
pub const MAX_PAYLOAD_SIZE: usize = 4096;

/// Maximum length of a single string field (username, domain, password,
/// detail message).
pub const MAX_STRING_LEN: usize = 512;

/// Frame header size in bytes: magic + version + kind + payload length.
pub const HEADER_SIZE: usize = 4 + 2 + 2 + 4;

/// Wire identifier of each request kind (service → credential provider).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum RequestKind {
    /// Deliver a full credential set for auto-logon / unlock.
    SetCredentials = 1,
    /// Forget any previously delivered credentials.
    ClearCredentials = 2,
    /// Liveness check; the provider answers with [`Response::Pong`].
    Ping = 3,
}

impl RequestKind {
    /// Converts a raw wire value into a request kind.
    pub fn from_wire(value: u16) -> Result<Self, ProtocolError> {
        match value {
            1 => Ok(Self::SetCredentials),
            2 => Ok(Self::ClearCredentials),
            3 => Ok(Self::Ping),
            other => Err(ProtocolError::UnknownRequestKind(other)),
        }
    }
}

/// Wire identifier of each response kind (credential provider → service).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ResponseKind {
    /// Sent right after the provider connects; carries its protocol version.
    Hello = 100,
    /// Acknowledgement of a request (`SetCredentials` / `ClearCredentials`).
    Ack = 101,
    /// Result of the logon attempt, forwarded from `ReportResult`.
    LogonResult = 102,
    /// Answer to [`RequestKind::Ping`].
    Pong = 103,
}

impl ResponseKind {
    /// Converts a raw wire value into a response kind.
    pub fn from_wire(value: u16) -> Result<Self, ProtocolError> {
        match value {
            100 => Ok(Self::Hello),
            101 => Ok(Self::Ack),
            102 => Ok(Self::LogonResult),
            103 => Ok(Self::Pong),
            other => Err(ProtocolError::UnknownResponseKind(other)),
        }
    }
}

/// Errors produced while encoding or decoding protocol frames.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    /// Frame magic does not match [`MAGIC`].
    #[error("bad frame magic: {0:#010x}")]
    BadMagic(u32),
    /// Frame version is not supported by this peer.
    #[error("unsupported protocol version: {0}")]
    UnsupportedVersion(u16),
    /// Unknown request kind on the wire.
    #[error("unknown request kind: {0}")]
    UnknownRequestKind(u16),
    /// Unknown response kind on the wire.
    #[error("unknown response kind: {0}")]
    UnknownResponseKind(u16),
    /// Payload exceeds [`MAX_PAYLOAD_SIZE`].
    #[error("payload too large: {0} bytes")]
    PayloadTooLarge(usize),
    /// A string field exceeds [`MAX_STRING_LEN`].
    #[error("string field too large: {0} bytes")]
    StringTooLarge(usize),
    /// The payload ended before all fields could be read.
    #[error("truncated payload")]
    TruncatedPayload,
    /// A string field is not valid UTF-8.
    #[error("string field is not valid UTF-8")]
    InvalidUtf8,
}

/// A full credential set delivered by the service. The password is zeroized
/// on drop; `Debug` redacts it so logs never leak secrets.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// Bare username (no domain prefix).
    pub username: String,
    /// Domain or machine name; empty means "local machine".
    pub domain: String,
    /// Account password (zeroized on drop).
    password: Zeroizing<String>,
}

impl Credentials {
    /// Creates a credential set.
    #[must_use]
    pub fn new(username: &str, domain: &str, password: &str) -> Self {
        Self {
            username: username.to_string(),
            domain: domain.to_string(),
            password: Zeroizing::new(password.to_string()),
        }
    }

    /// Borrows the password.
    #[must_use]
    pub fn password(&self) -> &str {
        &self.password
    }

    /// Whether all mandatory fields are populated.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.username.is_empty() && !self.password.is_empty()
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("domain", &self.domain)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl Drop for Credentials {
    fn drop(&mut self) {
        self.username.zeroize();
        self.domain.zeroize();
    }
}

/// A request travelling from the service to the credential provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Deliver credentials for an auto-logon / unlock attempt.
    SetCredentials(Credentials),
    /// Forget any previously delivered credentials.
    ClearCredentials,
    /// Liveness check.
    Ping,
}

/// A response travelling from the credential provider to the service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// Sent right after connecting; carries the provider protocol version.
    Hello {
        /// Protocol version spoken by the provider.
        version: u16,
    },
    /// Acknowledgement of a request.
    Ack {
        /// Kind of the request being acknowledged.
        request_kind: u16,
        /// Whether the request was accepted.
        accepted: bool,
        /// Human-readable detail (error cause when rejected).
        detail: String,
    },
    /// Outcome of a logon attempt, forwarded from `ReportResult`.
    LogonResult {
        /// Whether the logon succeeded.
        success: bool,
        /// `NTSTATUS` of the logon attempt.
        status: i32,
        /// `NTSTATUS` substatus of the logon attempt.
        substatus: i32,
        /// Human-readable detail (e.g. "The username or password is incorrect").
        message: String,
    },
    /// Answer to [`Request::Ping`].
    Pong,
}

// ---------------------------------------------------------------------------
// Low-level payload cursor
// ---------------------------------------------------------------------------

/// Bounds-checked cursor over a frame payload.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], ProtocolError> {
        if self.data.len() - self.pos < len {
            return Err(ProtocolError::TruncatedPayload);
        }
        let slice = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ProtocolError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn i32(&mut self) -> Result<i32, ProtocolError> {
        let b = self.take(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn string(&mut self) -> Result<String, ProtocolError> {
        let len = usize::from(self.u16()?);
        if len > MAX_STRING_LEN {
            return Err(ProtocolError::StringTooLarge(len));
        }
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| ProtocolError::InvalidUtf8)
    }
}

fn push_string(out: &mut Vec<u8>, value: &str) -> Result<(), ProtocolError> {
    if value.len() > MAX_STRING_LEN {
        return Err(ProtocolError::StringTooLarge(value.len()));
    }
    let len = u16::try_from(value.len()).map_err(|_| ProtocolError::StringTooLarge(value.len()))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

// ---------------------------------------------------------------------------
// Encode / decode
// ---------------------------------------------------------------------------

fn encode_frame(kind: u16, payload: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    if payload.len() > MAX_PAYLOAD_SIZE {
        return Err(ProtocolError::PayloadTooLarge(payload.len()));
    }
    let payload_len =
        u32::try_from(payload.len()).map_err(|_| ProtocolError::PayloadTooLarge(payload.len()))?;
    let mut out = Vec::with_capacity(HEADER_SIZE + payload.len());
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&payload_len.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Encodes a request into a wire frame.
pub fn encode_request(request: &Request) -> Result<Vec<u8>, ProtocolError> {
    let mut payload = Vec::new();
    let kind = match request {
        Request::SetCredentials(creds) => {
            push_string(&mut payload, &creds.username)?;
            push_string(&mut payload, &creds.domain)?;
            push_string(&mut payload, creds.password())?;
            RequestKind::SetCredentials
        }
        Request::ClearCredentials => RequestKind::ClearCredentials,
        Request::Ping => RequestKind::Ping,
    };
    let result = encode_frame(kind as u16, &payload);
    payload.zeroize(); // May contain the password
    result
}

/// Encodes a response into a wire frame.
pub fn encode_response(response: &Response) -> Result<Vec<u8>, ProtocolError> {
    let mut payload = Vec::new();
    let kind = match response {
        Response::Hello { version } => {
            payload.extend_from_slice(&version.to_le_bytes());
            ResponseKind::Hello
        }
        Response::Ack {
            request_kind,
            accepted,
            detail,
        } => {
            payload.extend_from_slice(&request_kind.to_le_bytes());
            payload.push(u8::from(*accepted));
            push_string(&mut payload, detail)?;
            ResponseKind::Ack
        }
        Response::LogonResult {
            success,
            status,
            substatus,
            message,
        } => {
            payload.push(u8::from(*success));
            payload.extend_from_slice(&status.to_le_bytes());
            payload.extend_from_slice(&substatus.to_le_bytes());
            push_string(&mut payload, message)?;
            ResponseKind::LogonResult
        }
        Response::Pong => ResponseKind::Pong,
    };
    encode_frame(kind as u16, &payload)
}

/// Decodes the payload of a request frame.
pub fn decode_request(kind: u16, payload: &[u8]) -> Result<Request, ProtocolError> {
    match RequestKind::from_wire(kind)? {
        RequestKind::SetCredentials => {
            let mut cursor = Cursor::new(payload);
            let username = cursor.string()?;
            let domain = cursor.string()?;
            let password = cursor.string()?;
            let creds = Credentials {
                username,
                domain,
                password: Zeroizing::new(password),
            };
            Ok(Request::SetCredentials(creds))
        }
        RequestKind::ClearCredentials => Ok(Request::ClearCredentials),
        RequestKind::Ping => Ok(Request::Ping),
    }
}

/// Decodes the payload of a response frame.
pub fn decode_response(kind: u16, payload: &[u8]) -> Result<Response, ProtocolError> {
    match ResponseKind::from_wire(kind)? {
        ResponseKind::Hello => {
            let mut cursor = Cursor::new(payload);
            Ok(Response::Hello {
                version: cursor.u16()?,
            })
        }
        ResponseKind::Ack => {
            let mut cursor = Cursor::new(payload);
            let request_kind = cursor.u16()?;
            let accepted = cursor.u8()? != 0;
            let detail = cursor.string()?;
            Ok(Response::Ack {
                request_kind,
                accepted,
                detail,
            })
        }
        ResponseKind::LogonResult => {
            let mut cursor = Cursor::new(payload);
            let success = cursor.u8()? != 0;
            let status = cursor.i32()?;
            let substatus = cursor.i32()?;
            let message = cursor.string()?;
            Ok(Response::LogonResult {
                success,
                status,
                substatus,
                message,
            })
        }
        ResponseKind::Pong => Ok(Response::Pong),
    }
}

// ---------------------------------------------------------------------------
// Streaming frame decoder
// ---------------------------------------------------------------------------

/// Incremental frame decoder for a byte stream (named pipes do not preserve
/// our frame boundaries in byte mode, so frames are reassembled here).
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    /// Creates an empty decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds bytes read from the pipe.
    pub fn feed(&mut self, data: &[u8]) -> Result<(), ProtocolError> {
        if self.buf.len() + data.len() > HEADER_SIZE + MAX_PAYLOAD_SIZE {
            return Err(ProtocolError::PayloadTooLarge(self.buf.len() + data.len()));
        }
        self.buf.extend_from_slice(data);
        Ok(())
    }

    /// Pops the next complete frame as `(kind, payload)`, or `None` if the
    /// buffered bytes do not yet form a full frame.
    pub fn next_frame(&mut self) -> Result<Option<(u16, Vec<u8>)>, ProtocolError> {
        if self.buf.len() < HEADER_SIZE {
            return Ok(None);
        }
        let magic = u32::from_le_bytes(self.buf[0..4].try_into().expect("slice length checked"));
        if magic != MAGIC {
            return Err(ProtocolError::BadMagic(magic));
        }
        let version = u16::from_le_bytes(self.buf[4..6].try_into().expect("slice length checked"));
        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }
        let kind = u16::from_le_bytes(self.buf[6..8].try_into().expect("slice length checked"));
        let payload_len = usize::try_from(u32::from_le_bytes(
            self.buf[8..12].try_into().expect("slice length checked"),
        ))
        .map_err(|_| ProtocolError::PayloadTooLarge(usize::MAX))?;
        if payload_len > MAX_PAYLOAD_SIZE {
            return Err(ProtocolError::PayloadTooLarge(payload_len));
        }
        if self.buf.len() < HEADER_SIZE + payload_len {
            return Ok(None); // Frame incomplete; wait for more data
        }
        let payload = self.buf[HEADER_SIZE..HEADER_SIZE + payload_len].to_vec();
        self.buf.drain(..HEADER_SIZE + payload_len);
        Ok(Some((kind, payload)))
    }

    /// Pops the next complete request frame, if any.
    pub fn next_request(&mut self) -> Result<Option<Request>, ProtocolError> {
        match self.next_frame()? {
            Some((kind, payload)) => Ok(Some(decode_request(kind, &payload)?)),
            None => Ok(None),
        }
    }

    /// Pops the next complete response frame, if any.
    pub fn next_response(&mut self) -> Result<Option<Response>, ProtocolError> {
        match self.next_frame()? {
            Some((kind, payload)) => Ok(Some(decode_response(kind, &payload)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip_set_credentials() {
        let req = Request::SetCredentials(Credentials::new("alice", "EXAMPLE", "s3cret"));
        let frame = encode_request(&req).unwrap();
        let mut dec = FrameDecoder::new();
        dec.feed(&frame).unwrap();
        let decoded = dec.next_request().unwrap().unwrap();
        assert_eq!(decoded, req);
        assert!(dec.next_request().unwrap().is_none());
    }

    #[test]
    fn request_round_trip_clear_and_ping() {
        for req in [Request::ClearCredentials, Request::Ping] {
            let frame = encode_request(&req).unwrap();
            let mut dec = FrameDecoder::new();
            dec.feed(&frame).unwrap();
            assert_eq!(dec.next_request().unwrap().unwrap(), req);
        }
    }

    #[test]
    fn response_round_trips() {
        let responses = [
            Response::Hello {
                version: PROTOCOL_VERSION,
            },
            Response::Ack {
                request_kind: RequestKind::SetCredentials as u16,
                accepted: true,
                detail: "stored".to_string(),
            },
            Response::LogonResult {
                success: false,
                status: -1_073_741_718, // STATUS_LOGON_FAILURE
                substatus: 0,
                message: "The username or password is incorrect".to_string(),
            },
            Response::Pong,
        ];
        for resp in responses {
            let frame = encode_response(&resp).unwrap();
            let mut dec = FrameDecoder::new();
            dec.feed(&frame).unwrap();
            assert_eq!(dec.next_response().unwrap().unwrap(), resp);
        }
    }

    #[test]
    fn fragmented_delivery_is_reassembled() {
        let req = Request::SetCredentials(Credentials::new("bob", "", "pw"));
        let frame = encode_request(&req).unwrap();
        let mut dec = FrameDecoder::new();
        // Feed the frame byte by byte; only the last feed completes it
        for (i, byte) in frame.iter().enumerate() {
            dec.feed(&[*byte]).unwrap();
            let got = dec.next_request().unwrap();
            if i + 1 < frame.len() {
                assert!(got.is_none(), "frame complete after {} bytes?", i + 1);
            } else {
                assert_eq!(got.unwrap(), req);
            }
        }
    }

    #[test]
    fn two_frames_in_one_read() {
        let mut stream = encode_request(&Request::Ping).unwrap();
        stream.extend_from_slice(&encode_request(&Request::ClearCredentials).unwrap());
        let mut dec = FrameDecoder::new();
        dec.feed(&stream).unwrap();
        assert_eq!(dec.next_request().unwrap().unwrap(), Request::Ping);
        assert_eq!(
            dec.next_request().unwrap().unwrap(),
            Request::ClearCredentials
        );
        assert!(dec.next_request().unwrap().is_none());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut frame = encode_request(&Request::Ping).unwrap();
        frame[0] ^= 0xFF;
        let mut dec = FrameDecoder::new();
        dec.feed(&frame).unwrap();
        assert!(matches!(
            dec.next_request(),
            Err(ProtocolError::BadMagic(_))
        ));
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut frame = encode_request(&Request::Ping).unwrap();
        frame[4] = 0xEE; // version low byte
        let mut dec = FrameDecoder::new();
        dec.feed(&frame).unwrap();
        assert!(matches!(
            dec.next_request(),
            Err(ProtocolError::UnsupportedVersion(_))
        ));
    }

    #[test]
    fn rejects_oversized_payload() {
        let creds = Credentials::new(&"u".repeat(MAX_STRING_LEN + 1), "", "pw");
        assert!(matches!(
            encode_request(&Request::SetCredentials(creds)),
            Err(ProtocolError::StringTooLarge(_))
        ));
    }

    #[test]
    fn rejects_truncated_payload_field() {
        // A SetCredentials frame whose payload claims a string longer than
        // the payload itself.
        let kind = RequestKind::SetCredentials as u16;
        let mut payload = Vec::new();
        payload.extend_from_slice(&(100u16).to_le_bytes()); // username len = 100
        payload.extend_from_slice(b"short"); // but only 5 bytes follow
        let frame = encode_frame(kind, &payload).unwrap();
        let mut dec = FrameDecoder::new();
        dec.feed(&frame).unwrap();
        assert!(matches!(
            dec.next_request(),
            Err(ProtocolError::TruncatedPayload)
        ));
    }

    #[test]
    fn rejects_unknown_kinds() {
        let frame = encode_frame(77, &[]).unwrap();
        let mut dec = FrameDecoder::new();
        dec.feed(&frame).unwrap();
        assert!(matches!(
            dec.next_request(),
            Err(ProtocolError::UnknownRequestKind(77))
        ));
        let mut dec = FrameDecoder::new();
        dec.feed(&frame).unwrap();
        assert!(matches!(
            dec.next_response(),
            Err(ProtocolError::UnknownResponseKind(77))
        ));
    }

    #[test]
    fn credentials_debug_redacts_password() {
        let creds = Credentials::new("alice", "EXAMPLE", "s3cret");
        let dbg = format!("{creds:?}");
        assert!(dbg.contains("alice"));
        assert!(!dbg.contains("s3cret"));
    }

    #[test]
    fn credentials_validity() {
        assert!(Credentials::new("alice", "", "pw").is_valid());
        assert!(!Credentials::new("", "", "pw").is_valid());
        assert!(!Credentials::new("alice", "", "").is_valid());
    }
}
