// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Local printer sinks: implementations of the
//! [`PrinterSink`](client::integrations::printer::PrinterSink) contract.
//!
//! [`PdfToDirSink`] materializes jobs as PDF files in a local directory
//! (atomic `.part` + rename) — the phase-0 backend. Real printing
//! (WinSpool RAW / CUPS, like uds-client's backends) plugs in later as
//! another `PrinterSink` without touching the session code.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

pub use client::integrations::printer::{PrinterSink, STATUS_FAILED};

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

/// Phase-0 backend: every job becomes one PDF file in `dir`.
pub struct PdfToDirSink {
    /// Directory where finished jobs land.
    dir: PathBuf,
    /// Refuse jobs larger than this (disk-flood protection).
    max_job_bytes: u64,
    /// Open jobs by server-assigned id.
    open: HashMap<u32, OpenJob>,
    /// Finished jobs as (final path, bytes), drained by the session.
    done: Vec<(PathBuf, u64)>,
}

impl PdfToDirSink {
    /// Creates the sink, ensuring the destination directory exists.
    pub fn new(dir: PathBuf, max_job_bytes: u64) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            max_job_bytes,
            open: HashMap::new(),
            done: Vec::new(),
        })
    }

}

impl PrinterSink for PdfToDirSink {
    fn create_job(
        &mut self,
        job_id: u32,
        printer_name: &str,
        document_name: &str,
    ) -> Result<(), u32> {
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
                Ok(())
            }
            Err(e) => {
                warn!(job_id, "cannot create print job file: {e}");
                Err(STATUS_FAILED)
            }
        }
    }

    fn write_data(&mut self, job_id: u32, data: &[u8]) -> Result<usize, u32> {
        let Some(job) = self.open.get_mut(&job_id) else {
            warn!(job_id, "job data for unknown job");
            return Err(STATUS_FAILED);
        };
        job.received += data.len() as u64;
        if job.received > self.max_job_bytes {
            warn!(job_id, received = job.received, "print job exceeds the size cap");
            self.open.remove(&job_id);
            return Err(STATUS_FAILED);
        }
        let broken = match &mut job.file {
            Some(file) => file.write_all(data).is_err(),
            None => true,
        };
        if broken {
            warn!(job_id, "cannot write print job chunk");
            self.open.remove(&job_id);
            return Err(STATUS_FAILED);
        }
        Ok(data.len())
    }

    fn close_job(&mut self, job_id: u32) -> Result<(), u32> {
        let Some(mut job) = self.open.remove(&job_id) else {
            warn!(job_id, "close for unknown job");
            return Err(STATUS_FAILED);
        };
        // Close the handle before the rename (Windows forbids renaming an
        // open file).
        drop(job.file.take());
        let final_path = unique_path(&self.dir, &sanitize_name(&job.document_name));
        let bytes = job.received;
        match fs::rename(&job.tmp_path, &final_path) {
            Ok(()) => {
                job.committed = true; // Drop must keep the file.
                info!(job_id, path = %final_path.display(), bytes, "print job done");
                self.done.push((final_path, bytes));
                Ok(())
            }
            Err(e) => {
                warn!(job_id, "cannot commit print job: {e}");
                Err(STATUS_FAILED)
            }
        }
    }

    fn drain_done(&mut self) -> Vec<(PathBuf, u64)> {
        std::mem::take(&mut self.done)
    }

    fn abort_job(&mut self, job_id: u32) {
        if self.open.remove(&job_id).is_some() {
            info!(job_id, "print job aborted");
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
        let mut sink = PdfToDirSink::new(dir.clone(), 1 << 20).expect("dir");
        sink.create_job(1, "UDS Printer", "test").unwrap();
        sink.write_data(1, b"%PDF-1.4 fake").unwrap();
        sink.close_job(1).unwrap();
        let (path, bytes) = sink.drain_done().into_iter().next().expect("done");
        assert_eq!(bytes, 13);
        assert_eq!(fs::read(&path).expect("read"), b"%PDF-1.4 fake");
        assert!(path.ends_with("test.pdf"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn abort_leaves_no_file() {
        let dir = std::env::temp_dir().join(format!("uds-print-test-abort-{}", std::process::id()));
        let mut sink = PdfToDirSink::new(dir.clone(), 1 << 20).expect("dir");
        sink.create_job(9, "UDS Printer", "x").unwrap();
        sink.write_data(9, &[1, 2, 3]).unwrap();
        sink.abort_job(9);
        assert!(fs::read_dir(&dir).expect("dir").next().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversize_job_is_dropped() {
        let dir = std::env::temp_dir().join(format!("uds-print-test-cap-{}", std::process::id()));
        let mut sink = PdfToDirSink::new(dir.clone(), 4).expect("dir");
        sink.create_job(2, "UDS Printer", "big").unwrap();
        assert!(sink.write_data(2, &[1, 2]).is_ok());
        assert!(sink.write_data(2, &[3, 4, 5]).is_err());
        assert!(sink.close_job(2).is_err(), "dropped job cannot close");
        let _ = fs::remove_dir_all(&dir);
    }
}
