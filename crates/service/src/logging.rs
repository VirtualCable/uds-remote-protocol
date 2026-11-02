// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! File logging for the service: `C:\ProgramData\UdsRp\service.log`.
//!
//! Services have no console; a plain append-only file behind a mutex is the
//! simplest reliable sink. Log rotation is out of scope for the spike.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

/// Directory for service state (log, future config cache).
pub const STATE_DIR: &str = r"C:\ProgramData\UdsRp";

struct LogFile {
    writer: Mutex<Option<fs::File>>,
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
        let mut guard = self.writer.lock().map_err(|_| io::Error::other("log lock"))?;
        if guard.is_none() {
            *guard = open_log().ok().flatten();
        }
        let file = guard
            .as_mut()
            .ok_or_else(|| io::Error::other("cannot open service log"))?;
        file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut guard = self.writer.lock().map_err(|_| io::Error::other("log lock"))?;
        if let Some(file) = guard.as_mut() {
            file.flush()?;
        }
        Ok(())
    }
}

fn open_log() -> io::Result<Option<fs::File>> {
    fs::create_dir_all(STATE_DIR)?;
    let path = PathBuf::from(STATE_DIR).join("service.log");
    OpenOptions::new().create(true).append(true).open(path).map(Some)
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
