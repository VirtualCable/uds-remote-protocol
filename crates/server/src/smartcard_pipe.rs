// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Virtual reader driver origin (Windows): hosts the two `udsscard-proto`
//! named pipes and maps the driver's simple card model (inject / eject /
//! transmit raw APDUs) onto the channel-8 RDPESC conversation towards the
//! client, where the physical card lives.
//!
//! One origin per session: it creates both pipes, waits for the UMDF
//! driver, runs the RDPESC handshake (EstablishContext → first reader with
//! a card → Connect → Status) and injects the real ATR into the virtual
//! reader. From then on every driver `TRANSMIT` is relayed as a
//! [`SmartcardMessage::Transmit`] and the raw response goes back to the
//! driver. On session end ([`PipeOrigin`] drop) the pipe threads stop, the
//! card is disconnected and released client-side, and the driver sees the
//! pipes close (its implicit eject).
//!
//! The pipe endpoint uses overlapped I/O with a stop-flag poll (same
//! pattern as the credential-provider pipe in the service crate) so a
//! session teardown never leaks blocked threads: the server process
//! accepts several sessions over its lifetime.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use protocol::{SCOPE_USER, SmartcardMessage};
use tokio::runtime::Handle;
use tracing::{info, warn};
use udsscard_proto::{
    CMD_INJECT_CARD, CMD_PIPE_NAME, IO_PIPE_NAME, IO_RESP_ERROR, IO_RESP_OK, IO_TRANSMIT,
    MAX_PAYLOAD, encode_frame,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::PCWSTR;

use crate::smartcard::{RelayError, RouterRelay, SmartcardRelay as _};

/// `SCARD_SHARE_SHARED`.
const SCARD_SHARE_SHARED: u32 = 2;
/// `SCARD_PROTOCOL_T0 | SCARD_PROTOCOL_T1`.
const SCARD_PROTOCOL_ANY: u32 = 0x3;
/// `SCARD_LEAVE_CARD`.
const SCARD_LEAVE_CARD: u32 = 0;
/// `SCARD_E_UNEXPECTED` (relay-level failure, no better code available).
const SCARD_E_UNEXPECTED: u32 = 0x8010_001F;

/// Stop-flag poll interval while waiting on pipe I/O.
const POLL: Duration = Duration::from_millis(100);
/// Bound for a single pipe write (they are tiny and local).
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Pipe DACL: full access to SYSTEM, Administrators and LocalService. The
/// session process runs as SYSTEM (or the user, in dev runs) and the UMDF
/// host (WUDFHost) connects as LocalService.
const PIPE_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;LS)";

/// Running pipe origin; dropping it stops the worker and tears the card
/// connection down (see module docs).
#[derive(Debug)]
pub struct PipeOrigin {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl PipeOrigin {
    /// Spawns the origin on a dedicated thread. `relay` talks to the
    /// session's smartcard router; `runtime` resolves its async calls from
    /// the blocking worker thread.
    ///
    /// Returns `None` when the pipes cannot be created (for example a
    /// previous session still owns them): the channel stays inert, which is
    /// the pre-driver behaviour anyway.
    #[must_use]
    pub fn spawn(relay: RouterRelay, runtime: Handle) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("scard-pipe".into())
            .spawn(move || origin_main(&relay, &runtime, &worker_stop))
            .map_err(|e| warn!("smartcard pipe origin: spawn failed: {e}"))
            .ok()?;
        Some(Self {
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for PipeOrigin {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The worker polls the flag every POLL interval; a blocked relay
        // call resolves with `Disconnected` once the session's router goes
        // away, so the join is bounded in practice.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Pipe endpoint (overlapped, stop-aware)
// ---------------------------------------------------------------------------

/// Overlapped named-pipe server endpoint.
struct Pipe {
    handle: HANDLE,
    event: HANDLE,
}

// SAFETY: pipe/event handles are usable from any thread; each Pipe is only
// touched by its owning worker thread.
unsafe impl Send for Pipe {}

impl Pipe {
    /// Creates one pipe instance and starts waiting for the driver.
    fn create(name: &str) -> Result<Self> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let sddl: Vec<u16> = PIPE_SDDL.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sddl` is a valid NUL-terminated SDDL string; the produced
        // descriptor is only read by CreateNamedPipeW below.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                1, // SDDL_REVISION_1
                &raw mut sd,
                None,
            )
            .context("ConvertStringSecurityDescriptorToSecurityDescriptorW")?;
        }
        let security = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).expect("fits u32"),
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        // SAFETY: `wide` is a valid NUL-terminated UTF-16 string; `security`
        // points to a valid SECURITY_ATTRIBUTES for the duration of the call.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                8192,
                8192,
                0,
                Some(&raw const security),
            )
        };
        if handle.is_invalid() {
            bail!("CreateNamedPipeW({name}) failed: {:?}", windows::core::Error::from_thread());
        }
        // SAFETY: default event attributes; the handle is checked.
        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null())? };
        Ok(Self { handle, event })
    }

    fn overlapped(&self) -> OVERLAPPED {
        OVERLAPPED {
            hEvent: self.event,
            ..OVERLAPPED::default()
        }
    }

    fn cancel(&self, overlapped: &mut OVERLAPPED) {
        // SAFETY: both handles are live; cancelling a finished operation is
        // harmless.
        unsafe {
            let _ = CancelIoEx(self.handle, Some(overlapped));
        }
    }

    /// Waits for the pending overlapped operation up to `timeout`.
    /// `Ok(None)` means the timeout expired (the operation stays pending).
    fn wait_io(&self, overlapped: &mut OVERLAPPED, timeout: Duration) -> Result<Option<u32>> {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: `event` is a live event handle.
        let wait = unsafe { WaitForSingleObject(self.event, millis) };
        if wait == WAIT_TIMEOUT {
            return Ok(None);
        }
        if wait != WAIT_OBJECT_0 {
            self.cancel(overlapped);
            bail!("WaitForSingleObject failed: {wait:?}");
        }
        let mut transferred = 0u32;
        // SAFETY: `overlapped` refers to the completed operation on
        // `handle`; `transferred` is a valid out pointer.
        unsafe {
            GetOverlappedResult(self.handle, overlapped, &raw mut transferred, false)
                .context("GetOverlappedResult failed")?;
        }
        Ok(Some(transferred))
    }

    /// Waits for the driver to connect. `Ok(false)` = stopped first.
    fn wait_client(&self, stop: &AtomicBool) -> Result<bool> {
        let mut overlapped = self.overlapped();
        // SAFETY: `handle` is a live pipe server handle; `overlapped` has a
        // valid event and outlives the wait loop.
        let result = unsafe { ConnectNamedPipe(self.handle, Some(&raw mut overlapped)) };
        match result {
            Ok(()) => Ok(true),
            Err(e) if e.code() == windows::core::HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) => {
                Ok(true)
            }
            Err(e) if e.code() == windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0) => loop {
                if self.wait_io(&mut overlapped, POLL)?.is_some() {
                    return Ok(true);
                }
                if stop.load(Ordering::Relaxed) {
                    self.cancel(&mut overlapped);
                    return Ok(false);
                }
            },
            Err(e) => Err(e).context("ConnectNamedPipe failed"),
        }
    }

    /// Reads up to `buf.len()` bytes. `Ok(None)` = stopped or peer gone
    /// (distinguishing them is the caller's business; both end the loop).
    fn read_some(&self, buf: &mut [u8], stop: &AtomicBool) -> Result<Option<usize>> {
        let mut overlapped = self.overlapped();
        // SAFETY: `handle` is live; `buf` is valid writable memory that
        // outlives the operation (we wait for completion or cancel).
        let result = unsafe { ReadFile(self.handle, Some(buf), None, Some(&raw mut overlapped)) };
        match result {
            Ok(()) => {
                let mut transferred = 0u32;
                // SAFETY: the operation completed synchronously;
                // `transferred` is a valid out pointer.
                unsafe {
                    GetOverlappedResult(self.handle, &raw const overlapped, &raw mut transferred, false)
                        .context("GetOverlappedResult failed")?;
                }
                if transferred == 0 {
                    return Ok(None); // peer closed
                }
                Ok(Some(transferred as usize))
            }
            Err(e) if e.code() == windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0) => loop {
                match self.wait_io(&mut overlapped, POLL)? {
                    Some(0) => return Ok(None),
                    Some(n) => return Ok(Some(n as usize)),
                    None if stop.load(Ordering::Relaxed) => {
                        self.cancel(&mut overlapped);
                        return Ok(None);
                    }
                    None => {}
                }
            },
            Err(e) if e.code() == windows::core::HRESULT::from_win32(ERROR_BROKEN_PIPE.0) => {
                Ok(None)
            }
            Err(e) => Err(e).context("ReadFile failed"),
        }
    }

    /// Writes the whole buffer (bounded by [`WRITE_TIMEOUT`]).
    fn write_all(&self, mut data: &[u8]) -> Result<()> {
        while !data.is_empty() {
            let mut overlapped = self.overlapped();
            // SAFETY: `handle` is live; `data` outlives the operation (we
            // wait for completion or bail after cancelling).
            let result = unsafe { WriteFile(self.handle, Some(data), None, Some(&raw mut overlapped)) };
            let written = match result {
                Ok(()) => data.len(),
                Err(e) if e.code() == windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0) => {
                    let Some(n) = self.wait_io(&mut overlapped, WRITE_TIMEOUT)? else {
                        self.cancel(&mut overlapped);
                        bail!("pipe write timed out");
                    };
                    n as usize
                }
                Err(e) => return Err(e).context("WriteFile failed"),
            };
            data = &data[written..];
        }
        Ok(())
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        // SAFETY: both handles are owned and closed exactly once.
        unsafe {
            let _ = CloseHandle(self.handle);
            let _ = CloseHandle(self.event);
        }
    }
}

// ---------------------------------------------------------------------------
// Frame I/O over Pipe (udsscard-proto wire format)
// ---------------------------------------------------------------------------

/// Reads one frame: `[tag][len:u32 LE][payload]`. `Ok(None)` = stopped or
/// disconnected.
fn read_frame(pipe: &Pipe, stop: &AtomicBool) -> Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    let mut got = 0usize;
    while got < header.len() {
        match pipe.read_some(&mut header[got..], stop)? {
            Some(n) => got += n,
            None => return Ok(None),
        }
    }
    let tag = header[0];
    let len = u32::from_le_bytes(header[1..5].try_into().expect("4 bytes")) as usize;
    if len > MAX_PAYLOAD {
        bail!("frame payload too large: {len}");
    }
    let mut payload = vec![0u8; len];
    let mut got = 0usize;
    while got < len {
        match pipe.read_some(&mut payload[got..], stop)? {
            Some(n) => got += n,
            None => return Ok(None),
        }
    }
    Ok(Some((tag, payload)))
}

fn write_frame(pipe: &Pipe, tag: u8, payload: &[u8]) -> Result<()> {
    pipe.write_all(&encode_frame(tag, payload))
}

// ---------------------------------------------------------------------------
// Origin worker
// ---------------------------------------------------------------------------

fn origin_main(relay: &RouterRelay, runtime: &Handle, stop: &Arc<AtomicBool>) {
    if let Err(e) = run(relay, runtime, stop)
        && !stop.load(Ordering::Relaxed)
    {
        warn!("smartcard pipe origin ended: {e:#}");
    }
}

fn run(relay: &RouterRelay, runtime: &Handle, stop: &Arc<AtomicBool>) -> Result<()> {
    let cmd = Pipe::create(CMD_PIPE_NAME)?;
    let io = Pipe::create(IO_PIPE_NAME)?;
    info!("smartcard pipe origin: waiting for the virtual reader driver");
    // The driver connects the command pipe first, then the I/O pipe.
    if !cmd.wait_client(stop)? || !io.wait_client(stop)? {
        return Ok(());
    }
    info!("smartcard pipe origin: driver connected");

    // RDPESC handshake against the client's physical card.
    let Some(session) = handshake(relay, runtime)? else {
        warn!("smartcard pipe origin: no client card available; reader stays empty");
        return Ok(());
    };
    write_frame(&cmd, CMD_INJECT_CARD, &session.atr)?;
    info!(atr = %protocol::hex::encode(&session.atr), "smartcard injected into virtual reader");

    // Transmit loop: one outstanding APDU at a time (the driver serializes
    // transmits on its side too).
    let result = transmit_loop(&io, relay, runtime, &session, stop);

    // Card teardown, best effort: the session may already be gone.
    let _ = runtime.block_on(relay.call(SmartcardMessage::Disconnect {
        request_id: 0,
        card: session.card,
        disposition: SCARD_LEAVE_CARD,
    }));
    let _ = runtime.block_on(relay.call(SmartcardMessage::ReleaseContext {
        request_id: 0,
        context: session.context,
    }));
    result
}

/// Client-side card connection, as established by [`handshake`].
struct ClientCard {
    context: u32,
    card: u32,
    active_protocol: u32,
    atr: Vec<u8>,
}

/// EstablishContext → first reader that yields a card → Status (for the
/// ATR). `Ok(None)` = the client has no usable card right now.
fn handshake(relay: &RouterRelay, runtime: &Handle) -> Result<Option<ClientCard>> {
    let reply = runtime.block_on(relay.call(SmartcardMessage::EstablishContext {
        request_id: 0,
        scope: SCOPE_USER,
    }))?;
    let SmartcardMessage::ContextEstablished { context, .. } = reply else {
        bail!("EstablishContext: unexpected reply {reply:?}");
    };

    let result = handshake_inner(relay, runtime, context);
    if matches!(result, Ok(None)) {
        let _ = runtime.block_on(relay.call(SmartcardMessage::ReleaseContext {
            request_id: 0,
            context,
        }));
    }
    result
}

fn handshake_inner(
    relay: &RouterRelay,
    runtime: &Handle,
    context: u32,
) -> Result<Option<ClientCard>> {
    let reply = runtime.block_on(relay.call(SmartcardMessage::ListReaders {
        request_id: 0,
        context,
    }))?;
    let SmartcardMessage::Readers { readers, .. } = reply else {
        bail!("ListReaders: unexpected reply {reply:?}");
    };

    for reader in readers {
        let reply = runtime.block_on(relay.call(SmartcardMessage::Connect {
            request_id: 0,
            context,
            reader: reader.clone(),
            share_mode: SCARD_SHARE_SHARED,
            preferred_protocols: SCARD_PROTOCOL_ANY,
        }));
        let Ok(SmartcardMessage::Connected {
            card,
            active_protocol,
            ..
        }) = reply
        else {
            continue; // no card in this reader, or connect failed
        };
        let reply = runtime.block_on(relay.call(SmartcardMessage::Status {
            request_id: 0,
            card,
        }))?;
        let SmartcardMessage::StatusReply { atr, .. } = reply else {
            bail!("Status: unexpected reply {reply:?}");
        };
        info!(%reader, "smartcard pipe origin: using client reader");
        return Ok(Some(ClientCard {
            context,
            card,
            active_protocol,
            atr,
        }));
    }
    Ok(None)
}

fn transmit_loop(
    io: &Pipe,
    relay: &RouterRelay,
    runtime: &Handle,
    session: &ClientCard,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    loop {
        let Some((tag, apdu)) = read_frame(io, stop)? else {
            return Ok(()); // stopped or driver gone
        };
        if tag != IO_TRANSMIT {
            warn!(tag, "smartcard pipe origin: unexpected io frame; dropped");
            continue;
        }
        let reply = runtime.block_on(relay.call(SmartcardMessage::Transmit {
            request_id: 0,
            card: session.card,
            send_protocol: session.active_protocol,
            data: apdu,
        }));
        let (resp_tag, payload) = match reply {
            Ok(SmartcardMessage::TransmitReply { data, .. }) => (IO_RESP_OK, data),
            Ok(SmartcardMessage::Error { code, .. }) => {
                (IO_RESP_ERROR, code.to_le_bytes().to_vec())
            }
            Ok(other) => {
                warn!(?other, "smartcard pipe origin: unexpected transmit reply");
                (IO_RESP_ERROR, SCARD_E_UNEXPECTED.to_le_bytes().to_vec())
            }
            Err(RelayError::Disconnected) => {
                // Session gone: tell the driver the card is unreachable
                // and stop; the session teardown closes us.
                write_frame(io, IO_RESP_ERROR, &SCARD_E_UNEXPECTED.to_le_bytes())?;
                bail!("transmit relay failed: channel disconnected");
            }
            Err(e) => {
                // One slow operation timed out (§17.9, the eDNI lesson):
                // report it to the driver and KEEP GOING — tearing the
                // whole virtual card down for a single late reply wedges
                // every caller until the next reconnect.
                warn!("smartcard transmit failed: {e}; reporting to the driver");
                (IO_RESP_ERROR, SCARD_E_UNEXPECTED.to_le_bytes().to_vec())
            }
        };
        write_frame(io, resp_tag, &payload)?;
    }
}
