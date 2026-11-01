//! Credential-provider named-pipe server (M4).
//!
//! The credential provider DLL loaded by LogonUI connects to
//! `\\.\pipe\udsrp-credprov` as a client; this module plays the server role
//! inside the service. On each connection it reads the provider's `Hello`,
//! optionally pushes the lab credentials with `SetCredentials` (the provider
//! then re-enumerates and auto-logs on), and logs `Ack` / `LogonResult`
//! frames until the provider disconnects or the service stops.
//!
//! Credential source (M4 lab only): `C:\ProgramData\UdsRp\credprov-creds.conf`
//! with three lines — username, domain (empty = local account), password.
//! In production the password is never stored: the broker hands per-session
//! credentials to the service at connection time.

// The `windows` crate FFI methods take raw pointers; implicit borrows are
// the idiomatic call style for that API (same policy as udscredprov-ctl).
#![allow(clippy::borrow_as_ptr)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use tracing::{error, info, warn};
use udscredprov::logic::{self, Credentials, FrameDecoder, Request, Response};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::{
    ACL, ACL_REVISION, AddAccessAllowedAce, CreateWellKnownSid, InitializeAcl,
    InitializeSecurityDescriptor, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl, WinLocalSystemSid,
};
use windows::Win32::Storage::FileSystem::{FlushFileBuffers, ReadFile, WriteFile};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::PCWSTR;

/// Lab credential file: username / domain / password, one per line.
pub(crate) const CREDS_PATH: &str = r"C:\ProgramData\UdsRp\credprov-creds.conf";

/// Lab auto-logon arm flag (one-shot): credentials are pushed to the
/// provider only while this file exists, and it is removed once the
/// provider reports the logon result. Without it the provider is never
/// armed, so the machine never auto-logs on at boot just because LogonUI
/// enumerated providers (§18.30). Production arms on a validated remote
/// connection instead of a flag file.
pub(crate) const ARM_PATH: &str = r"C:\ProgramData\UdsRp\arm-autologon";

/// How long to wait for a response frame before re-checking the stop flag.
const POLL: Duration = Duration::from_millis(500);

/// Starts the pipe server on a background thread. `creds_override` (the
/// standalone configuration credentials) takes precedence over the lab
/// credentials file when present.
pub fn spawn(stop: &'static AtomicBool, creds_override: Option<Credentials>) {
    std::thread::Builder::new()
        .name("credprov-pipe".into())
        .spawn(move || pipe_loop(stop, creds_override))
        .expect("cannot spawn the credential-provider pipe thread");
}

fn pipe_loop(stop: &AtomicBool, creds_override: Option<Credentials>) {
    let creds = match creds_override {
        Some(creds) => Some(creds),
        None => match load_credentials() {
            Ok(creds) => creds,
            Err(err) => {
                error!(error = %format!("{err:#}"), "cannot read {CREDS_PATH}; serving without credentials");
                None
            }
        },
    };
    info!(has_credentials = creds.is_some(), "credential-provider pipe server listening");
    while !stop.load(Ordering::Relaxed) {
        match serve_one(stop, creds.as_ref()) {
            Ok(()) => info!("credential provider disconnected; recreating the pipe"),
            Err(err) => warn!(error = %format!("{err:#}"), "credential-provider pipe cycle failed"),
        }
        // Avoid a tight recreate loop if creation itself keeps failing.
        std::thread::sleep(POLL);
    }
    info!("credential-provider pipe server stopped");
}

/// One pipe instance: wait for the provider, exchange frames, return when it
/// disconnects or `stop` is set.
fn serve_one(stop: &AtomicBool, creds: Option<&Credentials>) -> Result<()> {
    let mut pipe = Pipe::create()?;
    if !pipe.wait_connected(stop)? {
        return Ok(()); // stopped while waiting
    }
    info!("credential provider connected");

    let armed = std::path::Path::new(ARM_PATH).exists();
    if let Some(creds) = creds.filter(|_| armed) {
        pipe.send_request(&Request::SetCredentials(creds.clone()))?;
        info!(user = %creds.username, "credentials pushed to the provider (armed)");
    } else if creds.is_some() {
        info!("credentials available but auto-logon not armed; not pushing");
    }

    // Read frames until the provider goes away or we are asked to stop.
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        match pipe.read_response(POLL)? {
            Some(Response::Hello { version }) => info!(version, "provider Hello"),
            Some(Response::Ack {
                request_kind,
                accepted,
                detail,
            }) => info!(?request_kind, accepted, detail, "provider Ack"),
            Some(Response::LogonResult {
                success,
                status,
                substatus,
                message,
            }) => {
                info!(
                    success,
                    status = format_args!("0x{status:08X}"),
                    substatus = format_args!("0x{substatus:08X}"),
                    message,
                    "provider LogonResult"
                );
                // One-shot: consume the arm flag whatever the outcome, and
                // scrub the provider's store on failure so a bad credential
                // cannot retry forever behind our back.
                if std::fs::remove_file(ARM_PATH).is_ok() {
                    info!("auto-logon arm flag consumed");
                }
                if !success {
                    pipe.send_request(&Request::ClearCredentials)?;
                    info!("provider credentials cleared after failed logon");
                }
            }
            Some(Response::Pong) => info!("provider Pong"),
            None => {} // poll timeout: re-check the stop flag
        }
    }
}

/// Reads the lab credential file. Returns `Ok(None)` when it does not exist
/// (the pipe still serves, but no credentials are pushed).
fn load_credentials() -> Result<Option<Credentials>> {
    let text = match std::fs::read_to_string(CREDS_PATH) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).context("read failed"),
    };
    let mut lines = text.lines();
    let (Some(user), Some(domain), Some(password)) =
        (lines.next(), lines.next(), lines.next())
    else {
        bail!("expected 3 lines (username, domain, password)");
    };
    // An empty domain on a domain-joined machine resolves against the DC;
    // "." pins the logon to the local account database.
    let domain = match domain.trim() {
        "" => ".",
        d => d,
    };
    Ok(Some(Credentials::new(user.trim(), domain, password)))
}

// ---------------------------------------------------------------------------
// Pipe server plumbing (adapted from udscredprov-ctl; see that crate for the
// CLI variant). The service runs as SYSTEM and so does LogonUI, so a DACL
// granting read/write to SYSTEM is sufficient — the second ACE in the ctl
// version (current user) is the same SID here and is omitted.
// ---------------------------------------------------------------------------

/// SECURITY_ATTRIBUTES with a DACL granting read/write to SYSTEM. The buffers
/// are owned by the struct so the pointers in `attrs` stay valid.
struct PipeSecurity {
    attrs: SECURITY_ATTRIBUTES,
    _sec_desc: Box<SECURITY_DESCRIPTOR>,
    _acl: Vec<u8>,
    _system_sid: Vec<u8>,
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_ptr_alignment,
    clippy::ptr_as_ptr,
    clippy::ref_as_ptr
)]
fn pipe_security() -> Result<PipeSecurity> {
    let mut system_sid = vec![0u8; 68]; // well over a single-subauthority SID
    let mut len = system_sid.len() as u32;
    unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            None,
            Some(PSID(system_sid.as_mut_ptr().cast())),
            &mut len,
        )
        .ok()
        .context("CreateWellKnownSid failed")?;
    }
    system_sid.truncate(len as usize);

    let mut sec_desc = Box::new(SECURITY_DESCRIPTOR::default());
    let mut acl = vec![0u8; 1024];
    unsafe {
        InitializeSecurityDescriptor(
            PSECURITY_DESCRIPTOR(sec_desc.as_mut() as *mut _ as _),
            windows::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION,
        )
        .ok()
        .context("InitializeSecurityDescriptor failed")?;
        InitializeAcl(acl.as_mut_ptr().cast::<ACL>(), acl.len() as u32, ACL_REVISION)
            .ok()
            .context("InitializeAcl failed")?;
        AddAccessAllowedAce(
            acl.as_mut_ptr().cast::<ACL>(),
            ACL_REVISION,
            GENERIC_READ.0 | GENERIC_WRITE.0,
            PSID(system_sid.as_ptr().cast_mut().cast()),
        )
        .ok()
        .context("AddAccessAllowedAce(SYSTEM) failed")?;
        SetSecurityDescriptorDacl(
            PSECURITY_DESCRIPTOR(sec_desc.as_mut() as *mut _ as _),
            true,
            Some(acl.as_mut_ptr().cast::<ACL>()),
            false,
        )
        .ok()
        .context("SetSecurityDescriptorDacl failed")?;
    }

    let attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sec_desc.as_mut() as *mut _ as _,
        bInheritHandle: false.into(),
    };
    Ok(PipeSecurity {
        attrs,
        _sec_desc: sec_desc,
        _acl: acl,
        _system_sid: system_sid,
    })
}

/// Overlapped named-pipe server endpoint with a decoded-response reader.
struct Pipe {
    handle: HANDLE,
    event: HANDLE,
    decoder: FrameDecoder,
}

impl Pipe {
    /// Creates one pipe instance (not yet connected).
    fn create() -> Result<Self> {
        let security = pipe_security()?;
        let name = widestring::U16CString::from_str(logic::PIPE_NAME)?;
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR::from_raw(name.as_ptr()),
                windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX
                    | windows::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                8192,
                8192,
                0,
                Some(&security.attrs),
            )
        };
        if handle.is_invalid() {
            bail!(
                "CreateNamedPipeW({}) failed: {:?}",
                logic::PIPE_NAME,
                windows::core::Error::from_thread()
            );
        }
        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null())? };
        Ok(Self {
            handle,
            event,
            decoder: FrameDecoder::new(),
        })
    }

    /// Waits for the provider to connect, polling `stop`. Returns `Ok(false)`
    /// when stopped before any client connected.
    fn wait_connected(&self, stop: &AtomicBool) -> Result<bool> {
        let mut overlapped = self.overlapped();
        let ok = unsafe { ConnectNamedPipe(self.handle, Some(&mut overlapped)) };
        match ok {
            Ok(()) => Ok(true),
            Err(e) if e.code() == windows::core::HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) => {
                Ok(true)
            }
            Err(e) if e.code() == windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0) => loop {
                if self.wait_overlapped(&mut overlapped, POLL)?.is_some() {
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

    fn overlapped(&self) -> OVERLAPPED {
        OVERLAPPED {
            hEvent: self.event,
            ..OVERLAPPED::default()
        }
    }

    fn cancel(&self, overlapped: &mut OVERLAPPED) {
        unsafe {
            let _ = CancelIoEx(self.handle, Some(overlapped));
        }
    }

    /// Waits for the pending overlapped operation up to `timeout`.
    /// `Ok(None)` means the timeout expired (the operation stays pending).
    fn wait_overlapped(&self, overlapped: &mut OVERLAPPED, timeout: Duration) -> Result<Option<u32>> {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let wait = unsafe { WaitForSingleObject(self.event, millis) };
        if wait == WAIT_TIMEOUT {
            return Ok(None);
        }
        if wait != WAIT_OBJECT_0 {
            self.cancel(overlapped);
            bail!("WaitForSingleObject failed: {wait:?}");
        }
        let mut transferred = 0u32;
        unsafe {
            GetOverlappedResult(self.handle, overlapped, &mut transferred, false)
                .ok()
                .context("GetOverlappedResult failed")?;
        }
        Ok(Some(transferred))
    }

    /// Sends one request frame (blocking up to 10s).
    fn send_request(&self, request: &Request) -> Result<()> {
        let frame = logic::encode_request(request)?;
        let mut overlapped = self.overlapped();
        let result = unsafe { WriteFile(self.handle, Some(&frame), None, Some(&mut overlapped)) };
        match result {
            Ok(()) => Ok(()),
            Err(e) if e.code() == windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0) => {
                if self
                    .wait_overlapped(&mut overlapped, Duration::from_secs(10))?
                    .is_none()
                {
                    self.cancel(&mut overlapped);
                    bail!("WriteFile timed out");
                }
                Ok(())
            }
            Err(e) => Err(e).context("WriteFile failed"),
        }
    }

    /// Reads response frames until one is complete or `timeout` expires.
    /// I/O errors propagate as `Err` (caller drops the pipe and recreates).
    fn read_response(&mut self, timeout: Duration) -> Result<Option<Response>> {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 2048];
        loop {
            if let Some(resp) = self.decoder.next_response()? {
                return Ok(Some(resp));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let mut overlapped = self.overlapped();
            let result = unsafe { ReadFile(self.handle, Some(&mut buf), None, Some(&mut overlapped)) };
            let read = match result {
                Ok(()) => {
                    let mut transferred = 0u32;
                    unsafe {
                        GetOverlappedResult(self.handle, &overlapped, &mut transferred, false)
                            .ok()
                            .context("GetOverlappedResult failed")?;
                    };
                    transferred
                }
                Err(e) if e.code() == windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0) => {
                    if let Some(n) = self.wait_overlapped(&mut overlapped, remaining)? {
                        n
                    } else {
                        self.cancel(&mut overlapped);
                        return Ok(None);
                    }
                }
                Err(e) => return Err(e).context("ReadFile failed"),
            };
            if read > 0 {
                self.decoder.feed(&buf[..read as usize])?;
            }
        }
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        unsafe {
            let _ = FlushFileBuffers(self.handle);
            let _ = CloseHandle(self.handle);
            let _ = CloseHandle(self.event);
        }
    }
}
