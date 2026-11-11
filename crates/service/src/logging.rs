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
use std::path::PathBuf;
use std::sync::Mutex;

/// Directory for service state (log, future config cache).
pub const STATE_DIR: &str = r"C:\ProgramData\UdsRp";

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
    fs::create_dir_all(STATE_DIR)?;
    let path = log_path();
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    Ok(Some((file, size)))
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
