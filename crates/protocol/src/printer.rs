// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Printer redirection messages (§18.57, Fase 4 phase 0: PDF jobs).
//!
//! Direction: the **server** (where the virtual "UDS Printer" queue lives)
//! sends jobs; the **client** (where the physical printers are) delivers
//! them to its local spooler and replies. The message set mirrors the
//! `PrinterIntegration` trait shape (`create_job` / `write_data` /
//! `close_job` / `abort_job` with `u32` NTSTATUS-style error codes) so the
//! future uds-client integration is a thin wrapper.
//!
//! The server owns the `job_id` space (it receives the job from the VM
//! spooler first). Job data is chunked (`MAX_JOB_CHUNK` per message); jobs
//! bigger than `MAX_JOB_BYTES` are rejected defensively. All messages
//! travel on the reliable stream (`channel::PRINTER`): print data tolerates
//! neither loss nor reordering.
//!
//! Wire format: tag byte + big-endian fields, same convention as
//! [`crate::clipboard`] / [`crate::smartcard`].

use crate::error::ProtocolError;

/// Wire tag for [`PrinterMessage::CreateJob`].
const TAG_CREATE_JOB: u8 = 1;
/// Wire tag for [`PrinterMessage::JobCreated`].
const TAG_JOB_CREATED: u8 = 2;
/// Wire tag for [`PrinterMessage::JobData`].
const TAG_JOB_DATA: u8 = 3;
/// Wire tag for [`PrinterMessage::CloseJob`].
const TAG_CLOSE_JOB: u8 = 4;
/// Wire tag for [`PrinterMessage::JobClosed`].
const TAG_JOB_CLOSED: u8 = 5;
/// Wire tag for [`PrinterMessage::AbortJob`].
const TAG_ABORT_JOB: u8 = 6;

/// Job payload format: a finished PDF document (phase 0; the VM queue
/// renders with a PDF driver and the client stores/prints the file).
pub const JOB_FORMAT_PDF: u32 = 1;
/// Job payload format: PostScript (future; the VM queue renders with the
/// "MS Publisher Imagesetter" driver and CUPS converts client-side).
pub const JOB_FORMAT_POSTSCRIPT: u32 = 2;

/// No error (`JobCreated` / `JobClosed` success code).
pub const STATUS_SUCCESS: u32 = 0;

/// Maximum payload bytes in a single `JobData` message.
pub const MAX_JOB_CHUNK: usize = 16 * 1024;
/// Maximum total bytes accepted for one job (a PDF far bigger than this is
/// an attack or a bug, not a real print job).
pub const MAX_JOB_BYTES: u64 = 256 * 1024 * 1024;
/// Maximum bytes for a name field (printer or document).
pub const MAX_NAME_LEN: usize = 512;

/// One printer channel message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrinterMessage {
    /// S→C: opens a new job. `format` is one of the `JOB_FORMAT_*`
    /// constants; `printer_name` is the announced queue the user picked;
    /// `document_name` is the suggested file/document name.
    CreateJob {
        /// Server-assigned job identifier.
        job_id: u32,
        /// `JOB_FORMAT_*` constant.
        format: u32,
        /// Announced printer queue name (UTF-8).
        printer_name: String,
        /// Document name (UTF-8), e.g. for the save-as dialog.
        document_name: String,
    },
    /// C→S: answer to `CreateJob`; `error` is 0 (`STATUS_SUCCESS`) or a
    /// Win32/NTSTATUS-style code (same convention as the
    /// `PrinterIntegration` trait).
    JobCreated {
        /// Job this reply refers to.
        job_id: u32,
        /// 0 on success, error code otherwise.
        error: u32,
    },
    /// S→C: one data chunk of an open job (≤ `MAX_JOB_CHUNK`).
    JobData {
        /// Job the chunk belongs to.
        job_id: u32,
        /// Chunk bytes.
        payload: Vec<u8>,
    },
    /// S→C: the job is complete; the client finalizes it (print/save).
    CloseJob {
        /// Job to finalize.
        job_id: u32,
    },
    /// C→S: answer to `CloseJob`; 0 on success, error code otherwise.
    /// A failed close means the job did not reach the user.
    JobClosed {
        /// Job this reply refers to.
        job_id: u32,
        /// 0 on success, error code otherwise.
        error: u32,
    },
    /// S→C: aborts an open job (spooler cancellation, session end). No
    /// reply: the client discards whatever it holds for `job_id`.
    AbortJob {
        /// Job to abort.
        job_id: u32,
    },
}

/// Errors decoding printer messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PrinterError {
    /// Buffer shorter than the message needs.
    #[error("truncated printer message")]
    Truncated,
    /// Unknown message tag byte.
    #[error("unknown printer tag: {0}")]
    UnknownTag(u8),
    /// A name or payload exceeds its cap.
    #[error("printer field too large")]
    TooLarge,
}

impl From<PrinterError> for ProtocolError {
    fn from(err: PrinterError) -> Self {
        match err {
            PrinterError::Truncated => ProtocolError::Truncated,
            PrinterError::UnknownTag(_) => ProtocolError::Malformed("unknown printer tag"),
            PrinterError::TooLarge => ProtocolError::Malformed("printer field too large"),
        }
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&u32::try_from(s.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn take<'a>(buf: &mut &'a [u8], n: usize) -> Result<&'a [u8], PrinterError> {
    if buf.len() < n {
        return Err(PrinterError::Truncated);
    }
    let (head, tail) = buf.split_at(n);
    *buf = tail;
    Ok(head)
}

fn take_u32(buf: &mut &[u8]) -> Result<u32, PrinterError> {
    Ok(u32::from_be_bytes(take(buf, 4)?.try_into().expect("4 bytes")))
}

fn take_str(buf: &mut &[u8]) -> Result<String, PrinterError> {
    let len = take_u32(buf)? as usize;
    if len > MAX_NAME_LEN {
        return Err(PrinterError::TooLarge);
    }
    let bytes = take(buf, len)?;
    String::from_utf8(bytes.to_vec()).map_err(|_| PrinterError::TooLarge)
}

impl PrinterMessage {
    /// Encodes the message, appending to `out` (allows batching).
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::CreateJob {
                job_id,
                format,
                printer_name,
                document_name,
            } => {
                out.push(TAG_CREATE_JOB);
                out.extend_from_slice(&job_id.to_be_bytes());
                out.extend_from_slice(&format.to_be_bytes());
                put_str(out, printer_name);
                put_str(out, document_name);
            }
            Self::JobCreated { job_id, error } => {
                out.push(TAG_JOB_CREATED);
                out.extend_from_slice(&job_id.to_be_bytes());
                out.extend_from_slice(&error.to_be_bytes());
            }
            Self::JobData { job_id, payload } => {
                out.push(TAG_JOB_DATA);
                out.extend_from_slice(&job_id.to_be_bytes());
                out.extend_from_slice(
                    &u32::try_from(payload.len()).unwrap_or(u32::MAX).to_be_bytes(),
                );
                out.extend_from_slice(payload);
            }
            Self::CloseJob { job_id } => {
                out.push(TAG_CLOSE_JOB);
                out.extend_from_slice(&job_id.to_be_bytes());
            }
            Self::JobClosed { job_id, error } => {
                out.push(TAG_JOB_CLOSED);
                out.extend_from_slice(&job_id.to_be_bytes());
                out.extend_from_slice(&error.to_be_bytes());
            }
            Self::AbortJob { job_id } => {
                out.push(TAG_ABORT_JOB);
                out.extend_from_slice(&job_id.to_be_bytes());
            }
        }
    }

    /// Encodes the message into a fresh buffer.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// Decodes one message; rejects trailing bytes and over-large fields.
    pub fn decode(buf: &[u8]) -> Result<Self, PrinterError> {
        let mut rest = buf;
        let tag = *take(&mut rest, 1)?.first().expect("1 byte");
        let msg = match tag {
            TAG_CREATE_JOB => {
                let job_id = take_u32(&mut rest)?;
                let format = take_u32(&mut rest)?;
                let printer_name = take_str(&mut rest)?;
                let document_name = take_str(&mut rest)?;
                Self::CreateJob {
                    job_id,
                    format,
                    printer_name,
                    document_name,
                }
            }
            TAG_JOB_CREATED => {
                let job_id = take_u32(&mut rest)?;
                let error = take_u32(&mut rest)?;
                Self::JobCreated { job_id, error }
            }
            TAG_JOB_DATA => {
                let job_id = take_u32(&mut rest)?;
                let len = take_u32(&mut rest)? as usize;
                if len == 0 || len > MAX_JOB_CHUNK {
                    return Err(PrinterError::TooLarge);
                }
                let payload = take(&mut rest, len)?.to_vec();
                Self::JobData { job_id, payload }
            }
            TAG_CLOSE_JOB => {
                let job_id = take_u32(&mut rest)?;
                Self::CloseJob { job_id }
            }
            TAG_JOB_CLOSED => {
                let job_id = take_u32(&mut rest)?;
                let error = take_u32(&mut rest)?;
                Self::JobClosed { job_id, error }
            }
            TAG_ABORT_JOB => {
                let job_id = take_u32(&mut rest)?;
                Self::AbortJob { job_id }
            }
            other => return Err(PrinterError::UnknownTag(other)),
        };
        if !rest.is_empty() {
            return Err(PrinterError::Truncated);
        }
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: &PrinterMessage) {
        let encoded = msg.encode();
        let decoded = PrinterMessage::decode(&encoded).expect("decode");
        assert_eq!(&decoded, msg);
    }

    #[test]
    fn all_messages_roundtrip() {
        roundtrip(&PrinterMessage::CreateJob {
            job_id: 7,
            format: JOB_FORMAT_PDF,
            printer_name: "UDS Printer".into(),
            document_name: "informe.pdf".into(),
        });
        roundtrip(&PrinterMessage::JobCreated {
            job_id: 7,
            error: STATUS_SUCCESS,
        });
        roundtrip(&PrinterMessage::JobData {
            job_id: 7,
            payload: vec![1, 2, 3, 250],
        });
        roundtrip(&PrinterMessage::CloseJob { job_id: 7 });
        roundtrip(&PrinterMessage::JobClosed {
            job_id: 7,
            error: 0xC000_0008,
        });
        roundtrip(&PrinterMessage::AbortJob { job_id: 7 });
    }

    #[test]
    fn rejects_unknown_tag() {
        assert_eq!(
            PrinterMessage::decode(&[250]),
            Err(PrinterError::UnknownTag(250))
        );
    }

    #[test]
    fn rejects_truncated() {
        assert_eq!(
            PrinterMessage::decode(&[TAG_CLOSE_JOB, 0, 1]),
            Err(PrinterError::Truncated)
        );
    }

    #[test]
    fn rejects_oversized_chunk() {
        let mut buf = vec![TAG_JOB_DATA];
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(&(u32::try_from(MAX_JOB_CHUNK).expect("fits") + 1).to_be_bytes());
        assert_eq!(
            PrinterMessage::decode(&buf),
            Err(PrinterError::TooLarge)
        );
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut encoded = PrinterMessage::CloseJob { job_id: 1 }.encode();
        encoded.push(0);
        assert_eq!(
            PrinterMessage::decode(&encoded),
            Err(PrinterError::Truncated)
        );
    }
}
