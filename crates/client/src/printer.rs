// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Client side of printer redirection (§18.57, phase 0: PDF jobs).
//!
//! The server streams a finished PDF from the VM's "UDS Printer" queue
//! (`CreateJob` → `JobData`* → `CloseJob`) on the reliable
//! `channel::PRINTER`; we materialize it in a local directory (atomic
//! `.part` + rename, same convention as storage) and report the finished
//! path as a [`ClientEvent::PrintJobDone`]. What happens next (open a
//! save-as dialog, send to a local printer via the uds-client
//! `PrinterIntegration` wrapper) is the consumer's business — the
//! testclient just keeps the file.
//!
//! Synchronous `std::fs`: chunks are ≤ `MAX_JOB_CHUNK` and called from the
//! session receive path, so blocking time is negligible.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use protocol::{JOB_FORMAT_PDF, MAX_JOB_BYTES, PrinterMessage, STATUS_SUCCESS};
use tracing::{info, warn};

use crate::events::ClientEvent;

/// One in-flight job being written to disk.
struct OpenJob {
    /// Temporary `.part` file being appended (`None` once closed for the
    /// final rename: Windows cannot rename an open file).
    file: Option<File>,
    /// Path of `file`.
    tmp_path: PathBuf,
    /// Suggested final name from the server.
    document_name: String,
    /// Bytes written so far.
    received: u64,
    /// Set once the file reached its final name: Drop must not remove it.
    committed: bool,
}

impl Drop for OpenJob {
    fn drop(&mut self) {
        // Aborted or session-ended jobs never leave partial files.
        if !self.committed
            && self.tmp_path.exists()
            && let Err(e) = fs::remove_file(&self.tmp_path)
        {
            warn!(path = %self.tmp_path.display(), "cannot remove partial print job: {e}");
        }
    }
}

/// Printer job receiver: open jobs plus the replies to flush.
pub struct PrinterJobs {
    /// Directory where finished jobs land.
    dir: PathBuf,
    /// Open jobs by server-assigned id.
    open: HashMap<u32, OpenJob>,
    /// Replies to send on the reliable stream.
    out: Vec<PrinterMessage>,
}

impl PrinterJobs {
    /// Creates the receiver, ensuring the destination directory exists.
    pub fn new(dir: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            open: HashMap::new(),
            out: Vec::new(),
        })
    }

    /// Drains the replies queued since the last flush.
    pub fn drain_out(&mut self) -> impl Iterator<Item = PrinterMessage> + '_ {
        self.out.drain(..)
    }

    /// Applies one server message; returns the events to emit.
    pub fn handle(&mut self, msg: &PrinterMessage) -> Vec<ClientEvent> {
        match msg {
            PrinterMessage::CreateJob {
                job_id,
                format,
                printer_name,
                document_name,
            } => self.create_job(*job_id, *format, printer_name, document_name),
            PrinterMessage::JobData { job_id, payload } => self.job_data(*job_id, payload),
            PrinterMessage::CloseJob { job_id } => self.close_job(*job_id),
            PrinterMessage::AbortJob { job_id } => {
                if self.open.remove(job_id).is_some() {
                    info!(job_id, "print job aborted by server");
                }
                Vec::new()
            }
            // Client-originated replies never come back to us.
            PrinterMessage::JobCreated { .. } | PrinterMessage::JobClosed { .. } => Vec::new(),
        }
    }

    fn create_job(
        &mut self,
        job_id: u32,
        format: u32,
        printer_name: &str,
        document_name: &str,
    ) -> Vec<ClientEvent> {
        if format != JOB_FORMAT_PDF {
            warn!(job_id, format, "unsupported print job format");
            self.out.push(PrinterMessage::JobCreated {
                job_id,
                error: 0xC000_0002, // STATUS_NOT_IMPLEMENTED
            });
            return Vec::new();
        }
        // A reused job id means the previous one never closed: discard it.
        self.open.remove(&job_id);
        let tmp_path = self.dir.join(format!("uds-print-{job_id}.part"));
        match File::create(&tmp_path) {
            Ok(file) => {
                info!(
                    job_id,
                    printer = printer_name,
                    document = document_name,
                    "print job started"
                );
                self.open.insert(
                    job_id,
                    OpenJob {
                        file: Some(file),
                        tmp_path,
                        document_name: document_name.to_string(),
                        received: 0,
                        committed: false,
                    },
                );
                self.out.push(PrinterMessage::JobCreated {
                    job_id,
                    error: STATUS_SUCCESS,
                });
            }
            Err(e) => {
                warn!(job_id, "cannot create print job file: {e}");
                self.out.push(PrinterMessage::JobCreated {
                    job_id,
                    // Raw OS error codes do not map to NTSTATUS; use a
                    // generic access-denied-style failure and log the real
                    // one locally.
                    error: 0xC000_0022,
                });
            }
        }
        Vec::new()
    }

    fn job_data(&mut self, job_id: u32, payload: &[u8]) -> Vec<ClientEvent> {
        let Some(job) = self.open.get_mut(&job_id) else {
            warn!(job_id, "job data for unknown job");
            return Vec::new();
        };
        job.received += payload.len() as u64;
        if job.received > MAX_JOB_BYTES {
            warn!(job_id, received = job.received, "print job exceeds the size cap");
            self.open.remove(&job_id);
            return Vec::new();
        }
        let broken = match &mut job.file {
            Some(file) => file.write_all(payload).is_err(),
            None => true,
        };
        if broken {
            warn!(job_id, "cannot write print job chunk");
            self.open.remove(&job_id);
        }
        Vec::new()
    }

    fn close_job(&mut self, job_id: u32) -> Vec<ClientEvent> {
        let Some(mut job) = self.open.remove(&job_id) else {
            warn!(job_id, "close for unknown job");
            return Vec::new();
        };
        // Close the handle before the rename (Windows forbids renaming an
        // open file).
        drop(job.file.take());
        let final_path = unique_path(&self.dir, &sanitize_name(&job.document_name));
        let bytes = job.received;
        let tmp_path = job.tmp_path.clone();
        match fs::rename(&tmp_path, &final_path) {
            Ok(()) => {
                job.committed = true; // Drop must keep the file.
                info!(job_id, path = %final_path.display(), bytes, "print job done");
                self.out.push(PrinterMessage::JobClosed {
                    job_id,
                    error: STATUS_SUCCESS,
                });
                vec![ClientEvent::PrintJobDone {
                    path: final_path,
                    bytes,
                }]
            }
            Err(e) => {
                warn!(job_id, "cannot commit print job: {e}");
                self.out.push(PrinterMessage::JobClosed {
                    job_id,
                    error: 0xC000_0022,
                });
                Vec::new()
            }
        }
    }
}

/// Keeps only a safe leaf name from the server's suggestion, ensuring a
/// `.pdf` suffix.
fn sanitize_name(name: &str) -> String {
    let leaf = name
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(name)
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '.' | '-' | '_' | '(' | ')') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let leaf = leaf.trim().trim_matches('.').trim();
    let base = if leaf.is_empty() { "uds-print" } else { leaf };
    if base.to_ascii_lowercase().ends_with(".pdf") {
        base.to_string()
    } else {
        format!("{base}.pdf")
    }
}

/// Picks `dir/name`, or `dir/name (N)` when it already exists.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = name.trim_end_matches(".pdf");
    for n in 1..1000u32 {
        let candidate = dir.join(format!("{stem} ({n}).pdf"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("uds-print-{}.pdf", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_leaf_and_pdf_suffix() {
        assert_eq!(sanitize_name("informe"), "informe.pdf");
        assert_eq!(sanitize_name("informe.pdf"), "informe.pdf");
        assert_eq!(sanitize_name("..\\..\\evil.pdf"), "evil.pdf");
        assert_eq!(sanitize_name("a/b\\c.txt"), "c.txt.pdf");
        assert_eq!(sanitize_name("..."), "uds-print.pdf");
    }

    #[test]
    fn full_job_roundtrip() {
        let dir = std::env::temp_dir().join(format!("uds-print-test-{}", std::process::id()));
        let mut jobs = PrinterJobs::new(dir.clone()).expect("dir");
        let events = jobs.handle(&PrinterMessage::CreateJob {
            job_id: 1,
            format: JOB_FORMAT_PDF,
            printer_name: "UDS Printer".into(),
            document_name: "test".into(),
        });
        assert!(events.is_empty(), "CreateJob emits no events");
        jobs.handle(&PrinterMessage::JobData {
            job_id: 1,
            payload: b"%PDF-1.4 fake".to_vec(),
        });
        let events = jobs.handle(&PrinterMessage::CloseJob { job_id: 1 });
        let Some(ClientEvent::PrintJobDone { path, bytes }) = events.first() else {
            panic!("expected PrintJobDone");
        };
        assert_eq!(*bytes, 13);
        assert_eq!(fs::read(path).expect("read"), b"%PDF-1.4 fake");
        assert!(path.ends_with("test.pdf"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn abort_leaves_no_file() {
        let dir = std::env::temp_dir().join(format!("uds-print-test-abort-{}", std::process::id()));
        let mut jobs = PrinterJobs::new(dir.clone()).expect("dir");
        jobs.handle(&PrinterMessage::CreateJob {
            job_id: 9,
            format: JOB_FORMAT_PDF,
            printer_name: "UDS Printer".into(),
            document_name: "x".into(),
        });
        jobs.handle(&PrinterMessage::JobData {
            job_id: 9,
            payload: vec![1, 2, 3],
        });
        jobs.handle(&PrinterMessage::AbortJob { job_id: 9 });
        assert!(fs::read_dir(&dir).expect("dir").next().is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
