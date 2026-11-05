// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Minimal file logger for the vcam DLL. The filter is loaded in-process by
//! arbitrary applications (browser, Teams, ...), so the only reliable
//! diagnostics channel is a file: `%ProgramData%\UdsRp\vcam.log` (append,
//! one line per event, process-id tagged). Volume is tiny (lifecycle
//! events and errors only — no per-frame logging).

use std::io::Write as _;
use std::sync::Mutex;

/// Serializes concurrent log writers within the process.
static LOG_LOCK: Mutex<()> = Mutex::new(());

/// Appends one line to the vcam log.
pub fn line(msg: &str) {
    let _guard = LOG_LOCK.lock();
    let pid = std::process::id();
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(r"C:\ProgramData\UdsRp\vcam.log")
    else {
        return;
    };
    let _ = writeln!(f, "[pid {pid}] {msg}");
}

/// Formats and logs one line. Silent once the DLL is detached from the
/// process (see `globals::DETACHED`).
#[macro_export]
macro_rules! vlog {
    ($($arg:tt)*) => {
        if !$crate::globals::is_detached() {
            $crate::log::line(&format!($($arg)*));
        }
    };
}
