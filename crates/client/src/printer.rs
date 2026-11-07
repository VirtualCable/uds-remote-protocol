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
//! the injected [`PrinterSink`](crate::integrations::printer::PrinterSink)
//! calls and the replies back, and reports finished jobs as
//! [`ClientEvent::PrintJobDone`]. All device-side work (files, real
//! spoolers) lives behind the sink implementation.
//!
//! Synchronous `std::fs`: chunks are ≤ `MAX_JOB_CHUNK` and called from the
//! session receive path, so blocking time is negligible.

use protocol::{JOB_FORMAT_PDF, PrinterMessage, STATUS_SUCCESS};

use crate::integrations::printer::PrinterSink;

use crate::events::ClientEvent;

/// Printer job receiver: the sink plus the replies to flush.
pub struct PrinterJobs {
    sink: Box<dyn PrinterSink>,
    /// Replies to send on the reliable stream.
    out: Vec<PrinterMessage>,
}

impl PrinterJobs {
    /// Creates the receiver around the injected sink.
    #[must_use]
    pub fn new(sink: Box<dyn PrinterSink>) -> Self {
        Self {
            sink,
            out: Vec::new(),
        }
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
            .into_iter()
            .map(|(path, bytes)| ClientEvent::PrintJobDone { path, bytes })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::printer::STATUS_FAILED;
    use protocol::JOB_FORMAT_PDF;
    use std::collections::HashMap;

    /// In-memory sink: the wire adapter tests assert the session↔sink
    /// contract; file materialization is covered by `channels`' own
    /// `PdfToDirSink` tests.
    #[derive(Default)]
    struct MemSink {
        jobs: HashMap<u32, Vec<u8>>,
        done: Vec<(std::path::PathBuf, u64)>,
    }

    impl PrinterSink for MemSink {
        fn create_job(&mut self, job_id: u32, _printer: &str, _doc: &str) -> Result<(), u32> {
            self.jobs.insert(job_id, Vec::new());
            Ok(())
        }

        fn write_data(&mut self, job_id: u32, data: &[u8]) -> Result<usize, u32> {
            let job = self.jobs.get_mut(&job_id).ok_or(STATUS_FAILED)?;
            job.extend_from_slice(data);
            Ok(data.len())
        }

        fn close_job(&mut self, job_id: u32) -> Result<(), u32> {
            let data = self.jobs.remove(&job_id).ok_or(STATUS_FAILED)?;
            self.done.push((std::path::PathBuf::from(format!("{job_id}.pdf")), data.len() as u64));
            Ok(())
        }

        fn abort_job(&mut self, job_id: u32) {
            self.jobs.remove(&job_id);
        }

        fn drain_done(&mut self) -> Vec<(std::path::PathBuf, u64)> {
            std::mem::take(&mut self.done)
        }
    }

    #[test]
    fn full_job_roundtrip() {
        let mut jobs = PrinterJobs::new(Box::new(MemSink::default()));
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
        assert!(path.ends_with("1.pdf"));
        // Replies: JobCreated ok, JobClosed ok.
        let replies: Vec<_> = jobs.drain_out().collect();
        assert_eq!(replies.len(), 2);
    }

    #[test]
    fn wrong_format_refused() {
        let mut jobs = PrinterJobs::new(Box::new(MemSink::default()));
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
    }
}
