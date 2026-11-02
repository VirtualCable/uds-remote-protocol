// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Secure-desktop helper pipe (§18.41, M9): the wire between the session
//! agent (pipe server, user token) and the secure-desktop helper (pipe
//! client, SYSTEM token on the Winlogon desktop).
//!
//! Two unidirectional byte-mode pipes, both length-prefixed
//! `[u32 LE payload_len][u8 tag][payload]`:
//!
//! - `UDSRP-SecureFrames-<session>`: helper → agent, tag [`TAG_FRAME`] with
//!   `u32 width, u32 height, BGRA pixels` (stride = width*4).
//! - `UDSRP-SecureInput-<session>`: agent → helper, tag [`TAG_INPUT`] with a
//!   raw encoded [`protocol::InputMessage`] (forwarded untouched from the
//!   wire; the helper sanitizes and injects).
//!
//! Two pipes instead of one duplex so each direction gets a plain blocking
//! thread with no overlapped plumbing.

use std::io::{Read, Write};

use anyhow::{Result, bail};

/// Frame message tag (helper → agent).
pub const TAG_FRAME: u8 = 0;
/// Input tag (agent → helper): payload is an encoded `InputMessage`.
pub const TAG_INPUT: u8 = 1;

/// Pipe DACL: SYSTEM and Administrators full access, Creator/Owner full
/// access (the agent user). Nobody else can connect.
#[cfg(all(windows, feature = "windows-capture"))]
const PIPE_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;CO)";

/// Frames pipe name (helper → agent) for a console session id.
#[must_use]
pub fn frames_pipe_name(session_id: u32) -> String {
    format!(r"\\.\pipe\UDSRP-SecureFrames-{session_id}")
}

/// Input pipe name (agent → helper) for a console session id.
#[must_use]
pub fn input_pipe_name(session_id: u32) -> String {
    format!(r"\\.\pipe\UDSRP-SecureInput-{session_id}")
}

/// This process's session id (pipe name component).
#[cfg(windows)]
pub fn own_session_id() -> Result<u32> {
    use anyhow::Context as _;
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    let mut session = 0u32;
    // SAFETY: valid out pointer; our own pid always resolves.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &raw mut session) }
        .context("ProcessIdToSessionId(self)")?;
    Ok(session)
}

/// This process's session id (pipe name component). `0` off Windows (the
/// secure pipes only exist in Windows orchestrated sessions).
#[cfg(not(windows))]
pub fn own_session_id() -> Result<u32> {
    Ok(0)
}

/// Writes one tagged message (blocking).
pub fn write_msg(w: &mut impl Write, tag: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(payload.len() + 1).map_err(std::io::Error::other)?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&[tag])?;
    w.write_all(payload)
}

/// Reads one tagged message (blocking); `Ok(None)` on clean EOF.
pub fn read_msg(r: &mut impl Read) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e)
            if e.kind() == std::io::ErrorKind::UnexpectedEof
                || e.raw_os_error() == Some(109) =>
        {
            // 109 = ERROR_BROKEN_PIPE: the peer closed between messages.
            return Ok(None);
        }
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > 32 * 1024 * 1024 {
        return Err(std::io::Error::other(format!("bad pipe message length {len}")));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(Some((buf[0], buf.split_off(1))))
}

/// Encodes a frame message payload.
#[must_use]
pub fn encode_frame(width: u32, height: u32, bgra: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + bgra.len());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(bgra);
    out
}

/// Decodes a frame message payload; stride is width*4 by construction.
pub fn decode_frame(payload: &[u8]) -> Result<(u32, u32, &[u8])> {
    if payload.len() < 8 {
        bail!("frame payload too short");
    }
    let width = u32::from_le_bytes(payload[0..4].try_into().expect("4 bytes"));
    let height = u32::from_le_bytes(payload[4..8].try_into().expect("4 bytes"));
    let pixels = &payload[8..];
    let expected = width as usize * height as usize * 4;
    anyhow::ensure!(pixels.len() == expected, "frame size mismatch: {expected} expected");
    Ok((width, height, pixels))
}

/// Windows named-pipe endpoints (blocking byte mode).
#[cfg(all(windows, feature = "windows-capture"))]
pub mod win {
    use super::PIPE_SDDL;
    use anyhow::{Context, Result, bail};
    use std::os::windows::io::FromRawHandle;
    use std::time::{Duration, Instant};
    use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING,
        PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
        WaitNamedPipeW,
    };
    use windows::core::PCWSTR;

    /// Builds the SECURITY_ATTRIBUTES for our pipe SDDL. The descriptor
    /// memory leaks (a few hundred bytes per session) so the returned
    /// attributes stay valid for the created pipe's lifetime.
    fn pipe_security() -> Result<SECURITY_ATTRIBUTES> {
        let sddl: Vec<u16> = PIPE_SDDL.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sddl` is a valid NUL-terminated SDDL string.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                1, // SDDL_REVISION_1
                &raw mut sd,
                None,
            )
            .context("ConvertStringSecurityDescriptorToSecurityDescriptorW")?;
        }
        std::mem::forget(sddl);
        Ok(SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).expect("fits u32"),
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        })
    }

    /// Creates the pipe server end (agent side) and blocks until the helper
    /// connects. `inbound`: server reads (frames pipe); otherwise server
    /// writes (input pipe).
    pub fn accept_pipe(name: &str, inbound: bool) -> Result<std::fs::File> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let security = pipe_security()?;
        let access = if inbound { PIPE_ACCESS_INBOUND } else { PIPE_ACCESS_OUTBOUND };
        // SAFETY: `wide` is NUL-terminated; `security` points to a valid
        // descriptor that outlives the pipe (leaked on purpose).
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                access,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                64 * 1024,
                64 * 1024,
                0,
                Some(&raw const security),
            )
        };
        if handle.is_invalid() {
            bail!("CreateNamedPipeW({name}): {:?}", windows::core::Error::from_thread());
        }
        // SAFETY: valid pipe handle; blocking mode waits for the client.
        unsafe { ConnectNamedPipe(handle, None) }.context("ConnectNamedPipe")?;
        // SAFETY: we own the handle; File takes ownership.
        Ok(unsafe { std::fs::File::from_raw_handle(handle.0 as _) })
    }

    /// Connects to a pipe as the client (helper side), retrying while the
    /// server end is not up yet.
    pub fn connect_pipe(name: &str, timeout: Duration, write: bool) -> Result<std::fs::File> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let access = if write { FILE_GENERIC_WRITE } else { FILE_GENERIC_READ };
        let deadline = Instant::now() + timeout;
        loop {
            // SAFETY: `wide` is NUL-terminated; no template handle.
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    access.0,
                    windows::Win32::Storage::FileSystem::FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )
            };
            if let Ok(handle) = handle {
                // SAFETY: we own the handle; File takes ownership.
                return Ok(unsafe { std::fs::File::from_raw_handle(handle.0 as _) });
            }
            if Instant::now() >= deadline {
                bail!("connect_pipe({name}): timed out waiting for the server");
            }
            // SAFETY: `wide` is NUL-terminated; waits for a free instance.
            let _ = unsafe { WaitNamedPipeW(PCWSTR(wide.as_ptr()), 1000) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_roundtrip() {
        let mut wire = Vec::new();
        write_msg(&mut wire, TAG_INPUT, b"hello").unwrap();
        write_msg(&mut wire, TAG_FRAME, b"").unwrap();
        let mut cursor = std::io::Cursor::new(wire);
        let (tag, payload) = read_msg(&mut cursor).unwrap().unwrap();
        assert_eq!((tag, payload.as_slice()), (TAG_INPUT, b"hello".as_slice()));
        let (tag, payload) = read_msg(&mut cursor).unwrap().unwrap();
        assert_eq!((tag, payload.as_slice()), (TAG_FRAME, b"".as_slice()));
        assert!(read_msg(&mut cursor).unwrap().is_none()); // clean EOF
    }

    #[test]
    fn bad_length_is_rejected() {
        let wire = (30 * 1024 * 1024u32).to_le_bytes(); // over the 32 MiB cap w/ tag
        let mut cursor = std::io::Cursor::new(wire.to_vec());
        assert!(read_msg(&mut cursor).is_err());
        let wire = 0u32.to_le_bytes();
        let mut cursor = std::io::Cursor::new(wire.to_vec());
        assert!(read_msg(&mut cursor).is_err());
    }

    #[test]
    fn frame_roundtrip() {
        let pixels = vec![7u8; 2 * 3 * 4];
        let payload = encode_frame(2, 3, &pixels);
        let (w, h, decoded) = decode_frame(&payload).unwrap();
        assert_eq!((w, h), (2, 3));
        assert_eq!(decoded, pixels);
        assert!(decode_frame(&payload[..7]).is_err());
        assert!(decode_frame(&encode_frame(4, 4, &pixels)).is_err());
    }
}
