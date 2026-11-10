// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Printer sink contract: the device side of printer redirection,
//! wire-free.
//!
//! [`PrinterSink`] is shaped after uds-client's `PrinterIntegration` (same
//! job lifecycle, same u32 error codes) with one deliberate difference:
//! job ids are SERVER-assigned (our wire protocol works that way), so
//! `create_job` takes the id instead of returning one. A future bridge to
//! `PrinterIntegration` only has to mint ids.
//!
//! Implementations (PDF-to-directory phase 0 today, WinSpool RAW / CUPS
//! later) live in `channels::printer`.

/// The device side of printer redirection: receives finished-job lifecycle
/// calls from the session (server-driven, ids server-assigned).
pub trait PrinterSink: Send {
    /// Opens a job under the server-assigned id.
    fn create_job(
        &mut self,
        job_id: u32,
        printer_name: &str,
        document_name: &str,
    ) -> Result<(), u32>;
    /// Appends one chunk; returns the byte count accepted.
    fn write_data(&mut self, job_id: u32, data: &[u8]) -> Result<usize, u32>;
    /// Finalizes the job (commits it to the local output).
    fn close_job(&mut self, job_id: u32) -> Result<(), u32>;
    /// Cancels the job, dropping any partial output.
    fn abort_job(&mut self, job_id: u32);
    /// Drains the jobs finished since the last call as `(path, bytes)`
    /// (phase-0 file sinks). Spooler sinks report through their own UI and
    /// keep the default.
    fn drain_done(&mut self) -> Vec<(std::path::PathBuf, u64)> {
        Vec::new()
    }
}

/// Generic backend failure (the wire maps raw OS errors to NTSTATUS-style
/// codes; backends keep to the few values the session knows).
pub const STATUS_FAILED: u32 = 0xC000_0022;
