// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! File logging for the service: `C:\ProgramData\UdsRp\service.log`.
//!
//! Services have no console; a plain append-only file behind a mutex is the
//! simplest reliable sink. The file is size-capped with rotation (§18.78):
//! unauthenticated peers can force log lines (connect + reject), so the
//! sink must never grow without bound.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Directory for service state (log, future config cache).
pub const STATE_DIR: &str = r"C:\ProgramData\UdsRp";

/// SDDL for the state directory: **protected** from inheritance (`P`) with
/// full access for SYSTEM and the local Administrators group only.
///
/// `C:\ProgramData` grants the built-in `Users` group Read & Execute by
/// default, so a directory created there by inheritance is readable by every
/// local user. The service log is written by SYSTEM and carries session
/// diagnostics, so it must not be world-readable.
const STATE_DIR_SDDL: &str = "D:P(A;;FA;;;SY)(A;;FA;;;BA)";

/// Per-file size cap; past it the log rotates (§18.78).
const MAX_LOG_BYTES: u64 = 8 << 20;
/// Rotated backups kept (`service.log.1`, `service.log.2`).
const LOG_BACKUPS: u32 = 2;

struct LogFile {
    /// Open file plus its current size in bytes.
    writer: Mutex<Option<(fs::File, u64)>>,
}

impl LogFile {
    const fn new() -> Self {
        Self {
            writer: Mutex::new(None),
        }
    }
}

impl io::Write for &LogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("log lock"))?;
        if guard.is_none() {
            *guard = open_log().ok().flatten();
        }
        let (file, size) = guard
            .as_mut()
            .ok_or_else(|| io::Error::other("cannot open service log"))?;
        if *size + buf.len() as u64 > MAX_LOG_BYTES {
            rotate_log();
            *guard = open_log().ok().flatten();
            let (file, size) = guard
                .as_mut()
                .ok_or_else(|| io::Error::other("cannot reopen service log"))?;
            let written = file.write(buf)?;
            *size += written as u64;
            return Ok(written);
        }
        let written = file.write(buf)?;
        *size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("log lock"))?;
        if let Some((file, _)) = guard.as_mut() {
            file.flush()?;
        }
        Ok(())
    }
}

fn log_path() -> PathBuf {
    PathBuf::from(STATE_DIR).join("service.log")
}

fn open_log() -> io::Result<Option<(fs::File, u64)>> {
    ensure_state_dir()?;
    let path = log_path();
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    Ok(Some((file, size)))
}

/// Creates the state directory (if missing) and forces the restrictive DACL
/// on it, so the service log is not readable by unprivileged local users.
///
/// The DACL is applied explicitly (and marked protected from inheritance)
/// rather than relying on the `C:\ProgramData` default, which grants
/// `Users` Read & Execute. Applying it unconditionally also repairs a
/// directory created by an older build with the permissive inherited ACL.
fn ensure_state_dir() -> io::Result<()> {
    use windows::Win32::Foundation::WIN32_ERROR;
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };
    use windows::core::PCWSTR;

    // Best effort: the explicit ACL below is what matters, and it also
    // repairs a directory that already exists.
    let _ = fs::create_dir_all(STATE_DIR);
    if !Path::new(STATE_DIR).is_dir() {
        return Err(io::Error::other(
            "cannot create the service state directory",
        ));
    }

    let sddl: Vec<u16> = STATE_DIR_SDDL
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: `sddl` is a valid NUL-terminated SDDL string.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            1, // SDDL_REVISION_1
            &raw mut descriptor,
            None,
        )
    }
    .map_err(|err| io::Error::other(format!("state dir SDDL: {err}")))?;
    // Extract the DACL out of the descriptor: `SetNamedSecurityInfoW` takes
    // the ACL itself, not the whole descriptor.
    let mut present = windows::core::BOOL::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut defaulted = windows::core::BOOL::default();
    // SAFETY: `descriptor` holds a valid security descriptor; the out
    // pointers are valid.
    unsafe {
        GetSecurityDescriptorDacl(
            descriptor,
            &raw mut present,
            &raw mut dacl,
            &raw mut defaulted,
        )
    }
    .map_err(|err| io::Error::other(format!("state dir SDDL DACL: {err}")))?;
    if dacl.is_null() {
        return Err(io::Error::other("state dir SDDL carries no DACL"));
    }
    let wide: Vec<u16> = STATE_DIR.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated; `descriptor` holds a valid security
    // descriptor for the duration of the call.
    let applied = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        )
    };
    // The descriptor (and the SDDL buffer) are Win32-allocated; they are
    // released by the OS when the process exits, which is one allocation
    // per service run. `PSECURITY_DESCRIPTOR` is a Copy handle, so there is
    // nothing to `forget` here.
    std::mem::forget(sddl);
    if applied != WIN32_ERROR(0) {
        return Err(io::Error::other(format!("state dir ACL: {applied:?}")));
    }
    Ok(())
}

/// Rotates `service.log` -> `service.log.1` -> ... keeping [`LOG_BACKUPS`]
/// backups. Best-effort: logging must never fail the service.
fn rotate_log() {
    let base = log_path();
    for idx in (1..=LOG_BACKUPS).rev() {
        let from = base.with_extension(format!("log.{idx}"));
        let to = base.with_extension(format!("log.{}", idx + 1));
        if idx == LOG_BACKUPS {
            let _ = fs::remove_file(&from);
        } else if from.exists() {
            let _ = fs::rename(&from, &to);
        }
    }
    let _ = fs::rename(&base, base.with_extension("log.1"));
}

/// Install the global tracing subscriber writing to the service log file.
/// Also mirrors to stderr, which is what `--console` mode shows.
///
/// Idempotent: `run_dispatch` (main thread) and `service_main` (SCM thread)
/// both call it; only the first call takes effect.
pub fn init() {
    static LOG: LogFile = LogFile::new();
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(|| &LOG)
        .with_max_level(tracing::Level::DEBUG)
        .try_init();
}
