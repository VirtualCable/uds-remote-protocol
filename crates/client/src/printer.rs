// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Client side of printer redirection (§18.57, phase 0: PDF jobs) — wire
//! adapter only.
//!
//! The server streams a finished PDF from the VM's "UDS Printer" queue
//! (`CreateJob` → `JobData`* → `CloseJob`) on the reliable
//! `channel::PRINTER`; this adapter translates those messages into
//! [`channels::printer::PrinterSink`] calls and the replies back, and
//! reports finished jobs as [`ClientEvent::PrintJobDone`]. All device-side
//! work (files, atomic rename, naming) lives in the channels crate.
//!
//! Synchronous `std::fs`: chunks are ≤ `MAX_JOB_CHUNK` and called from the
//! session receive path, so blocking time is negligible.

use std::path::PathBuf;

use protocol::{JOB_FORMAT_PDF, MAX_JOB_BYTES, PrinterMessage, STATUS_SUCCESS};

use crate::events::ClientEvent;

/// Printer job receiver: the sink plus the replies to flush.
pub struct PrinterJobs {
    sink: channels::printer::PdfToDirSink,
    /// Replies to send on the reliable stream.
    out: Vec<PrinterMessage>,
}

impl PrinterJobs {
    /// Creates the receiver, ensuring the destination directory exists.
    pub fn new(dir: PathBuf) -> std::io::Result<Self> {
        let sink = channels::printer::PdfToDirSink::new(dir, MAX_JOB_BYTES)?;
        Ok(Self {
            sink,
            out: Vec::new(),
        })
    }

    /// Drains the replies queued since the last flush.
    pub fn drain_out(&mut self) -> impl Iterator<Item = PrinterMessage> + '_ {
        self.out.drain(..)
    }

    /// Applies one server message; returns the events to emit.
    pub fn handle(&mut self, msg: &PrinterMessage) -> Vec<ClientEvent> {
        use channels::printer::PrinterSink as _;
        match msg {
            PrinterMessage::CreateJob {
                job_id,
                format,
                printer_name,
                document_name,
            } => {
                let error = if *format == JOB_FORMAT_PDF {
                    match self.sink.create_job(*job_id, printer_name, document_name) {
                        Ok(()) => STATUS_SUCCESS,
                        Err(code) => code,
                    }
                } else {
                    0xC000_0002 // STATUS_NOT_IMPLEMENTED
                };
                self.out.push(PrinterMessage::JobCreated {
                    job_id: *job_id,
                    error,
                });
            }
            PrinterMessage::JobData { job_id, payload } => {
                // No reply on the wire for data chunks; the sink logs and
                // drops broken jobs (the server notices the missing close).
                let _ = self.sink.write_data(*job_id, payload);
            }
            PrinterMessage::CloseJob { job_id } => {
                let error = match self.sink.close_job(*job_id) {
                    Ok(()) => STATUS_SUCCESS,
                    Err(code) => code,
                };
                self.out.push(PrinterMessage::JobClosed {
                    job_id: *job_id,
                    error,
                });
            }
            PrinterMessage::AbortJob { job_id } => {
                self.sink.abort_job(*job_id);
            }
            // Client-originated replies never come back to us.
            PrinterMessage::JobCreated { .. } | PrinterMessage::JobClosed { .. } => return Vec::new(),
        }
        self.sink
            .drain_done()
            .map(|(path, bytes)| ClientEvent::PrintJobDone { path, bytes })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::JOB_FORMAT_PDF;

    #[test]
    fn full_job_roundtrip() {
        let dir = std::env::temp_dir().join(format!("uds-print-adapter-{}", std::process::id()));
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
        assert_eq!(std::fs::read(path).expect("read"), b"%PDF-1.4 fake");
        assert!(path.ends_with("test.pdf"));
        // Replies: JobCreated ok, JobClosed ok.
        let replies: Vec<_> = jobs.drain_out().collect();
        assert_eq!(replies.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_format_refused() {
        let dir = std::env::temp_dir().join(format!("uds-print-adapter-fmt-{}", std::process::id()));
        let mut jobs = PrinterJobs::new(dir.clone()).expect("dir");
        jobs.handle(&PrinterMessage::CreateJob {
            job_id: 3,
            format: 99,
            printer_name: "UDS Printer".into(),
            document_name: "x".into(),
        });
        let replies: Vec<_> = jobs.drain_out().collect();
        assert!(
            matches!(replies.first(), Some(PrinterMessage::JobCreated { error, .. }) if *error != 0)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
