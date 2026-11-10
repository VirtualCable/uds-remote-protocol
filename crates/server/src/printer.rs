// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Server side of printer redirection (§18.57, phase 0: PDF-only).
//!
//! The VM has a "UDS Printer" queue whose driver is "Microsoft Print To
//! PDF" attached to a localmon port that IS a file path: the spooler
//! renders each job and writes the finished PDF to
//! `<spool>/uds-job.pdf` with no dialogs and no custom driver. This module
//! polls for that file, and once the spooler lets go of it (a successful
//! rename means the handle is closed) streams it to the client on the
//! reliable `channel::PRINTER` as `CreateJob` → `JobData`* → `CloseJob`.
//!
//! Phase-0 limits, by design: one port = one file = jobs serialize at the
//! spooler; the document name is synthetic (the job name does not reach the
//! port); a custom port monitor (named pipe, real job metadata) is the
//! future upgrade path. Polling is one rename attempt per tick — cheap,
//! and no filesystem watchers to wedge in a service context.
//!
//! Synchronous `std::fs`: chunks are ≤ `MAX_JOB_CHUNK` and the session loop
//! pumps with a tight per-tick budget, like the storage spool.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use protocol::{JOB_FORMAT_PDF, MAX_JOB_CHUNK, PrinterMessage};
use tracing::{debug, info, warn};

/// Name the printer port writes to inside the spool directory.
pub const PORT_FILE_NAME: &str = "uds-job.pdf";

/// Jobs injected while one is already in flight wait here (bounded: each
/// entry is a finished file on disk, and the spooler serializes them).
const MAX_QUEUED_JOBS: usize = 16;

/// One job being streamed to the client.
struct SendJob {
    /// Server-assigned job id (the wire's `job_id`).
    id: u32,
    /// Open file being read.
    file: File,
    /// Its path (removed once fully sent).
    path: PathBuf,
    /// Total size in bytes.
    size: u64,
    /// Bytes already sent.
    sent: u64,
}

impl Drop for SendJob {
    fn drop(&mut self) {
        // Finished or aborted jobs never linger in the spool.
        if self.path.exists()
            && let Err(e) = fs::remove_file(&self.path)
        {
            warn!(path = %self.path.display(), "cannot remove sent print job: {e}");
        }
    }
}

/// Printer spool: watched directory plus the outgoing job state.
pub struct PrinterSpool {
    /// Directory the printer port writes into.
    dir: PathBuf,
    /// Finished job files waiting to be sent.
    queue: VecDeque<PathBuf>,
    /// Job currently streaming.
    current: Option<SendJob>,
    /// Next wire job id.
    next_job_id: u32,
    /// Size seen on the previous scan: a job is only taken when its size is
    /// non-zero and stable across two consecutive scans (guards slow
    /// writers on platforms without an exclusive-open check).
    last_size: Option<u64>,
}

impl PrinterSpool {
    /// Opens (creating if needed) the spool directory.
    pub fn open(dir: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            queue: VecDeque::new(),
            current: None,
            next_job_id: 1,
            last_size: None,
        })
    }

    /// Default spool: `C:\ProgramData\UdsRp\print-spool` on Windows,
    /// `<temp>/uds-print-spool` elsewhere (dev convenience).
    #[must_use]
    pub fn default_dir() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\ProgramData\UdsRp\print-spool")
        } else {
            std::env::temp_dir().join("uds-print-spool")
        }
    }

    /// True while there is a job to pump.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.current.is_some() || !self.queue.is_empty()
    }

    /// Polls the watch file. A job is taken only when the spooler has
    /// finished with it, which is decided by two gates:
    ///
    /// 1. On Windows, an exclusive open (share mode 0) must succeed — while
    ///    the spooler holds the file the open fails. (A plain rename is NOT
    ///    enough: localmon opens the file allowing delete-sharing, so a
    ///    rename succeeds mid-write and wrecks the job. Learned the hard
    ///    way, E2E 2026-11-06.)
    /// 2. The size must be non-zero and unchanged since the previous scan
    ///    (guards slow writers everywhere, including non-Windows).
    pub fn scan(&mut self) {
        let watch = self.dir.join(PORT_FILE_NAME);
        if !watch.exists() {
            self.last_size = None;
            return;
        }
        let size = fs::metadata(&watch).map_or(0, |m| m.len());
        if size == 0 || self.last_size != Some(size) {
            self.last_size = Some(size);
            return; // New, growing, or just appeared: wait one more tick.
        }
        if !file_is_free(&watch) {
            return;
        }
        let dest = self.dir.join(format!(
            "job-{}-{}.pdf",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis()),
            self.next_job_id
        ));
        match fs::rename(&watch, &dest) {
            Ok(()) => {
                info!(path = %dest.display(), size, "print job ready");
                if self.queue.len() >= MAX_QUEUED_JOBS {
                    warn!("print queue full; dropping oldest job");
                    if let Some(old) = self.queue.pop_front() {
                        let _ = fs::remove_file(old);
                    }
                }
                self.queue.push_back(dest);
            }
            Err(e) => {
                // ERROR_ACCESS_DENIED / sharing violation while the spooler
                // still writes is the normal case; anything else is odd.
                debug!("print job still being written: {e}");
            }
        }
    }

    /// Produces up to `budget` data chunks (plus the framing messages) for
    /// the job in flight, starting it if the queue has one.
    pub fn pump(&mut self, budget: usize) -> Vec<PrinterMessage> {
        let mut out = Vec::new();
        if self.current.is_none() {
            let Some(path) = self.queue.pop_front() else {
                return out;
            };
            let id = self.next_job_id;
            self.next_job_id = self.next_job_id.wrapping_add(1).max(1);
            match File::open(&path).and_then(|f| f.metadata().map(|m| (f, m.len()))) {
                Ok((file, size)) => {
                    info!(id, path = %path.display(), size, "streaming print job");
                    out.push(PrinterMessage::CreateJob {
                        job_id: id,
                        format: JOB_FORMAT_PDF,
                        printer_name: "UDS Printer".to_string(),
                        document_name: format!("UDS print job {id}"),
                    });
                    self.current = Some(SendJob {
                        id,
                        file,
                        path,
                        size,
                        sent: 0,
                    });
                }
                Err(e) => {
                    warn!(path = %path.display(), "cannot open print job: {e}");
                    let _ = fs::remove_file(&path);
                    return out;
                }
            }
        }
        let mut budget_left = budget;
        while budget_left > 0 {
            let Some(job) = &mut self.current else { break };
            let remaining = usize::try_from(job.size - job.sent).unwrap_or(MAX_JOB_CHUNK);
            let mut chunk = vec![0u8; MAX_JOB_CHUNK.min(remaining)];
            if chunk.is_empty() {
                // Fully sent: close it out.
                let id = job.id;
                self.current = None; // Drop removes the file.
                out.push(PrinterMessage::CloseJob { job_id: id });
                break;
            }
            match job.file.read(&mut chunk) {
                Ok(0) => {
                    let id = job.id;
                    self.current = None;
                    out.push(PrinterMessage::CloseJob { job_id: id });
                    break;
                }
                Ok(n) => {
                    chunk.truncate(n);
                    job.sent += n as u64;
                    out.push(PrinterMessage::JobData {
                        job_id: job.id,
                        payload: chunk,
                    });
                    budget_left -= 1;
                }
                Err(e) => {
                    let id = job.id;
                    warn!(id, "cannot read print job: {e}");
                    self.current = None;
                    out.push(PrinterMessage::AbortJob { job_id: id });
                    break;
                }
            }
        }
        out
    }

    /// Applies one client reply. An error on `JobCreated` aborts the job in
    /// flight; the rest is logged and the job plays out (phase 0 keeps it
    /// simple — the file is local and finite).
    pub fn on_reply(&mut self, msg: &PrinterMessage) {
        match msg {
            PrinterMessage::JobCreated { job_id, error } => {
                if *error != 0 {
                    warn!(job_id, error, "client refused the print job; aborting");
                    if self.current.as_ref().is_some_and(|j| j.id == *job_id) {
                        self.current = None;
                    }
                }
            }
            PrinterMessage::JobClosed { job_id, error } => {
                if *error == 0 {
                    info!(job_id, "print job delivered");
                } else {
                    warn!(job_id, error, "client failed to finalize the print job");
                }
            }
            other => debug!(?other, "unexpected printer reply"),
        }
    }
}

/// True when `dir` looks like a printer spool the installer set up: the
/// directory exists. (The queue itself is an OS object; if it is missing no
/// file ever appears and the watcher just idles.)
#[must_use]
pub fn spool_present(dir: &Path) -> bool {
    dir.is_dir()
}

/// Exclusive-open probe: true only when nothing else holds `path` open.
/// Windows only; elsewhere the size-stability gate in `scan` does the work.
#[cfg(all(windows, feature = "windows-capture"))]
fn file_is_free(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE, OPEN_EXISTING,
    };
    use windows::Win32::Storage::FileSystem::{FILE_GENERIC_READ, FILE_GENERIC_WRITE};
    use windows::core::PCWSTR;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is NUL-terminated; share mode 0 = exclusive probe.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    match handle {
        Ok(h) => {
            // SAFETY: we own the probe handle.
            unsafe {
                let _ = CloseHandle(h);
            }
            true
        }
        Err(_) => false,
    }
}

/// Non-Windows builds: no exclusive-open probe (the size-stability gate in
/// `scan` is the guard there).
#[cfg(not(all(windows, feature = "windows-capture")))]
fn file_is_free(_path: &Path) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spool(tag: &str) -> (PathBuf, PrinterSpool) {
        let dir =
            std::env::temp_dir().join(format!("uds-printer-test-{tag}-{}", std::process::id()));
        let spool = PrinterSpool::open(dir.clone()).expect("open");
        (dir, spool)
    }

    #[test]
    fn scan_queues_and_pump_streams() {
        let (dir, mut spool) = spool("stream");
        fs::write(dir.join(PORT_FILE_NAME), b"%PDF-1.4 test payload").expect("write");
        spool.scan(); // First sighting: size recorded only.
        assert!(!spool.has_pending());
        spool.scan(); // Stable size: taken.
        assert!(spool.has_pending());
        assert!(!dir.join(PORT_FILE_NAME).exists());

        let msgs = spool.pump(8);
        assert!(matches!(
            msgs.first(),
            Some(PrinterMessage::CreateJob { job_id: 1, .. })
        ));
        assert!(
            msgs.iter()
                .any(|m| matches!(m, PrinterMessage::JobData { .. }))
        );
        assert!(matches!(msgs.last(), Some(PrinterMessage::CloseJob { .. })));
        assert!(!spool.has_pending());
        // The spooled file is cleaned up after streaming.
        assert!(fs::read_dir(&dir).expect("dir").next().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_watch_file_is_ignored() {
        let (dir, mut spool) = spool("empty");
        fs::write(dir.join(PORT_FILE_NAME), b"").expect("write");
        spool.scan();
        assert!(!spool.has_pending());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refused_job_aborts() {
        let (dir, mut spool) = spool("abort");
        fs::write(dir.join(PORT_FILE_NAME), b"%PDF-x").expect("write");
        spool.scan();
        spool.scan();
        let msgs = spool.pump(1); // CreateJob only.
        let Some(PrinterMessage::CreateJob { job_id, .. }) = msgs.first() else {
            panic!("expected CreateJob");
        };
        let id = *job_id;
        spool.on_reply(&PrinterMessage::JobCreated {
            job_id: id,
            error: 0xC000_0022,
        });
        let rest = spool.pump(8);
        assert!(
            !rest
                .iter()
                .any(|m| matches!(m, PrinterMessage::JobData { .. }))
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
