// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Windows implementation of udscredprov-ctl. See `main.rs` for the command
//! list. Plays the named-pipe **server** role that the `uds-rp` service owns
//! in production, so the credential provider DLL loaded by LogonUI (which
//! runs as SYSTEM) can connect to it as a client. The pipe ACL grants access
//! only to SYSTEM and the current user — mirroring the production ACL.

// The `windows` crate FFI methods take raw pointers; implicit borrows are
// the idiomatic call style for that API (same policy as udsaudio-ctl).
#![allow(clippy::borrow_as_ptr)]

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use udscredprov::logic::{self, FrameDecoder, Request, Response};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::{
    ACL, ACL_REVISION, AddAccessAllowedAce, CreateWellKnownSid, GetTokenInformation, InitializeAcl,
    InitializeSecurityDescriptor, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER, TokenUser,
    WinLocalSystemSid,
};
use windows::Win32::Storage::FileSystem::{FlushFileBuffers, ReadFile, WriteFile};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenProcessToken, WaitForSingleObject,
};
use windows::core::PCWSTR;

/// Default time to wait for the provider to connect / answer.
const DEFAULT_TIMEOUT_SECS: u32 = 30;

/// CLI entry.
pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map_or("help", String::as_str);
    match cmd {
        "listen" => {
            let timeout = opt_timeout(&args[1..]);
            let mut pipe = Pipe::create_and_wait(timeout)?;
            match pipe.read_response(timeout)? {
                Some(resp @ Response::Hello { .. }) => {
                    println!("provider connected: {resp:?}");
                    Ok(())
                }
                other => bail!("expected Hello, got {other:?}"),
            }
        }
        "ping" => {
            let timeout = opt_timeout(&args[1..]);
            let mut pipe = Pipe::create_and_wait(timeout)?;
            expect_hello(&mut pipe, timeout)?;
            pipe.send_request(&Request::Ping)?;
            match pipe.read_response(timeout)? {
                Some(Response::Pong) => {
                    println!("Pong received");
                    Ok(())
                }
                other => bail!("expected Pong, got {other:?}"),
            }
        }
        "send" => {
            let user = args.get(1).context("send: missing <user>")?;
            let domain = args.get(2).map_or("", String::as_str);
            let password = args.get(3).context("send: missing <password>")?;
            let await_result = opt_flag_timeout(&args[4..], "--await-result");
            let timeout = opt_timeout(&args[4..]);

            let mut pipe = Pipe::create_and_wait(timeout)?;
            expect_hello(&mut pipe, timeout)?;
            pipe.send_request(&Request::SetCredentials(logic::Credentials::new(
                user, domain, password,
            )))?;
            match pipe.read_response(timeout)? {
                Some(Response::Ack {
                    accepted, detail, ..
                }) => {
                    println!("Ack: accepted={accepted} detail={detail:?}");
                    if !accepted {
                        bail!("provider rejected the credentials: {detail}");
                    }
                }
                other => bail!("expected Ack, got {other:?}"),
            }
            if let Some(result_timeout) = await_result {
                match pipe.read_response(result_timeout)? {
                    Some(Response::LogonResult {
                        success,
                        status,
                        substatus,
                        message,
                    }) => {
                        println!(
                            "LogonResult: success={success} status=0x{status:08X} substatus=0x{substatus:08X} message={message:?}"
                        );
                        if success {
                            Ok(())
                        } else {
                            bail!("logon failed: {message}");
                        }
                    }
                    other => bail!("expected LogonResult, got {other:?}"),
                }
            } else {
                Ok(())
            }
        }
        "clear" => {
            let timeout = opt_timeout(&args[1..]);
            let mut pipe = Pipe::create_and_wait(timeout)?;
            expect_hello(&mut pipe, timeout)?;
            pipe.send_request(&Request::ClearCredentials)?;
            match pipe.read_response(timeout)? {
                Some(Response::Ack { accepted, .. }) if accepted => {
                    println!("credentials cleared");
                    Ok(())
                }
                other => bail!("expected accepting Ack, got {other:?}"),
            }
        }
        _ => {
            eprintln!(
                "usage: udscredprov-ctl <listen | ping | send <user> <domain> <password> [--await-result secs] | clear> [--timeout secs]"
            );
            std::process::exit(64);
        }
    }
}

fn expect_hello(pipe: &mut Pipe, timeout: Duration) -> Result<()> {
    match pipe.read_response(timeout)? {
        Some(resp @ Response::Hello { .. }) => {
            println!("provider connected: {resp:?}");
            Ok(())
        }
        other => bail!("expected Hello, got {other:?}"),
    }
}

fn opt_timeout(args: &[String]) -> Duration {
    opt_flag_timeout(args, "--timeout").unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS.into()))
}

fn opt_flag_timeout(args: &[String], flag: &str) -> Option<Duration> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .and_then(|w| w[1].parse::<u64>().ok())
        .map(Duration::from_secs)
}

// ---------------------------------------------------------------------------
// Pipe server plumbing
// ---------------------------------------------------------------------------

/// SECURITY_ATTRIBUTES with a DACL granting read/write to SYSTEM and to the
/// current user. The buffers are owned by the struct so the pointers in
/// `attrs` stay valid.
struct PipeSecurity {
    attrs: SECURITY_ATTRIBUTES,
    _sec_desc: Box<SECURITY_DESCRIPTOR>,
    _acl: Vec<u8>,
    _system_sid: Vec<u8>,
    _user_sid: Vec<u8>,
}

// The SID/ACL helpers below manipulate byte buffers with layouts dictated by
// the Win32 security APIs, hence the raw-pointer and cast lints are allowed.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_ptr_alignment,
    clippy::ptr_as_ptr,
    clippy::ref_as_ptr
)]
fn well_known_system_sid() -> Result<Vec<u8>> {
    let mut sid = vec![0u8; 68]; // well over the size of a single-subauthority SID
    let mut len = sid.len() as u32;
    unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            None,
            Some(PSID(sid.as_mut_ptr().cast())),
            &mut len,
        )
        .ok()
        .context("CreateWellKnownSid failed")?;
    }
    sid.truncate(len as usize);
    Ok(sid)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_ptr_alignment,
    clippy::ptr_as_ptr,
    clippy::ref_as_ptr
)]
fn current_user_sid() -> Result<Vec<u8>> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let mut size_needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut size_needed);
        let mut buf = vec![0u8; size_needed as usize];
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            size_needed,
            &mut size_needed,
        )?;
        CloseHandle(token).ok().context("CloseHandle failed")?;
        let token_user = &*buf.as_ptr().cast::<TOKEN_USER>();
        let sid_len = windows::Win32::Security::GetLengthSid(token_user.User.Sid) as usize;
        let mut sid = vec![0u8; sid_len];
        std::ptr::copy_nonoverlapping(token_user.User.Sid.0.cast::<u8>(), sid.as_mut_ptr(), sid_len);
        Ok(sid)
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_ptr_alignment,
    clippy::ptr_as_ptr,
    clippy::ref_as_ptr
)]
fn pipe_security() -> Result<PipeSecurity> {
    let system_sid = well_known_system_sid()?;
    let user_sid = current_user_sid()?;

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
        let access = GENERIC_READ.0 | GENERIC_WRITE.0;
        AddAccessAllowedAce(
            acl.as_mut_ptr().cast::<ACL>(),
            ACL_REVISION,
            access,
            PSID(system_sid.as_ptr().cast_mut().cast()),
        )
        .ok()
        .context("AddAccessAllowedAce(SYSTEM) failed")?;
        AddAccessAllowedAce(
            acl.as_mut_ptr().cast::<ACL>(),
            ACL_REVISION,
            access,
            PSID(user_sid.as_ptr().cast_mut().cast()),
        )
        .ok()
        .context("AddAccessAllowedAce(user) failed")?;
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
        _user_sid: user_sid,
    })
}

/// Overlapped named-pipe server endpoint with a decoded-response reader.
struct Pipe {
    handle: HANDLE,
    event: HANDLE,
    decoder: FrameDecoder,
}

impl Pipe {
    /// Creates the pipe and waits up to `timeout` for the provider to connect.
    fn create_and_wait(timeout: Duration) -> Result<Self> {
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
        let pipe = Self {
            handle,
            event,
            decoder: FrameDecoder::new(),
        };
        pipe.wait_connected(timeout)?;
        Ok(pipe)
    }

    fn wait_connected(&self, timeout: Duration) -> Result<()> {
        let mut overlapped = self.overlapped();
        let ok = unsafe { ConnectNamedPipe(self.handle, Some(&mut overlapped)) };
        match ok {
            Ok(()) => Ok(()), // already connected
            Err(e) if e.code() == windows::core::HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) => {
                Ok(())
            }
            Err(e) if e.code() == windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0) => {
                self.wait_overlapped(&mut overlapped, timeout)
                    .context("timed out waiting for the credential provider to connect")?;
                Ok(())
            }
            Err(e) => Err(e).context("ConnectNamedPipe failed"),
        }
    }

    fn overlapped(&self) -> OVERLAPPED {
        OVERLAPPED {
            hEvent: self.event,
            ..OVERLAPPED::default()
        }
    }

    /// Waits for the pending overlapped operation; on timeout cancels it.
    fn wait_overlapped(&self, overlapped: &mut OVERLAPPED, timeout: Duration) -> Result<u32> {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let wait = unsafe { WaitForSingleObject(self.event, millis) };
        if wait == WAIT_TIMEOUT {
            unsafe {
                CancelIoEx(self.handle, Some(overlapped))
                    .ok()
                    .context("CancelIoEx failed")?;
            }
            bail!("timeout after {}s", timeout.as_secs());
        }
        if wait != WAIT_OBJECT_0 {
            bail!("WaitForSingleObject failed: {wait:?}");
        }
        let mut transferred = 0u32;
        unsafe {
            GetOverlappedResult(self.handle, overlapped, &mut transferred, false)
                .ok()
                .context("GetOverlappedResult failed")?;
        }
        Ok(transferred)
    }

    /// Sends one request frame.
    fn send_request(&self, request: &Request) -> Result<()> {
        let frame = logic::encode_request(request)?;
        let mut overlapped = self.overlapped();
        let result = unsafe {
            WriteFile(
                self.handle,
                Some(&frame),
                None,
                Some(&mut overlapped),
            )
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) if e.code() == windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0) => {
                self.wait_overlapped(&mut overlapped, Duration::from_secs(10))?;
                Ok(())
            }
            Err(e) => Err(e).context("WriteFile failed"),
        }
    }

    /// Reads response frames until one is complete or the timeout expires.
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
            let result = unsafe {
                ReadFile(self.handle, Some(&mut buf), None, Some(&mut overlapped))
            };
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
                    self.wait_overlapped(&mut overlapped, remaining)?
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
