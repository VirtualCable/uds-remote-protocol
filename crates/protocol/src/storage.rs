// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Storage messages: explicit file transfer, bidirectional (M8, model B).
//!
//! No drive redirection and no filesystem browsing of the peer: the only
//! thing on the wire is *explicit* transfers. The client uploads files the
//! user picked; the server exposes its own spool directory (list + pull
//! with offset, so downloads are resumable). The server can never ask the
//! client for arbitrary paths — there is simply no message for it. The
//! wire format is tag byte + big-endian fields, same convention as
//! [`crate::clipboard`]; richer models (drive redirection, WebDAV-style
//! mounts) would be new message families on this same channel, so the
//! design does not close that door.
//!
//! All messages travel on the reliable stream (`channel::STORAGE`), never
//! on the lossy leg.

/// Wire tag for [`StorageMessage::ListRequest`].
const TAG_LIST_REQUEST: u8 = 1;
/// Wire tag for [`StorageMessage::ListEntry`].
const TAG_LIST_ENTRY: u8 = 2;
/// Wire tag for [`StorageMessage::ListDone`].
const TAG_LIST_DONE: u8 = 3;
/// Wire tag for [`StorageMessage::PutStart`].
const TAG_PUT_START: u8 = 4;
/// Wire tag for [`StorageMessage::PutChunk`].
const TAG_PUT_CHUNK: u8 = 5;
/// Wire tag for [`StorageMessage::PutDone`].
const TAG_PUT_DONE: u8 = 6;
/// Wire tag for [`StorageMessage::PutAbort`].
const TAG_PUT_ABORT: u8 = 7;
/// Wire tag for [`StorageMessage::PutAck`].
const TAG_PUT_ACK: u8 = 8;
/// Wire tag for [`StorageMessage::GetStart`].
const TAG_GET_START: u8 = 9;
/// Wire tag for [`StorageMessage::GetChunk`].
const TAG_GET_CHUNK: u8 = 10;
/// Wire tag for [`StorageMessage::GetDone`].
const TAG_GET_DONE: u8 = 11;
/// Wire tag for [`StorageMessage::GetAbort`].
const TAG_GET_ABORT: u8 = 12;
/// Wire tag for [`StorageMessage::Error`].
const TAG_ERROR: u8 = 13;

/// Chunk payload size senders should use. The reliable stream record
/// budget is 65 517 bytes; a chunk message adds 17 bytes of header
/// (tag + id + offset + len), so 65 500 is the largest payload that
/// always fits in one record.
pub const STORAGE_CHUNK_SIZE: usize = 65_500;

/// Maximum chunk payload accepted on the wire (sanity cap on decode).
pub const MAX_CHUNK_PAYLOAD: u32 = 1024 * 1024;

/// Maximum file size accepted for a transfer (4 GiB).
pub const MAX_FILE_SIZE: u64 = 4 * 1024 * 1024 * 1024;

/// Maximum byte length of a file name on the wire.
pub const MAX_NAME_LEN: u16 = 255;

/// Maximum byte length of an error message.
pub const MAX_ERROR_MSG_LEN: u16 = 512;

/// I/O failure (disk error, spool not writable, ...).
pub const ERR_IO: u32 = 1;
/// The file name failed validation ([`validate_name`]).
pub const ERR_BAD_NAME: u32 = 2;
/// The requested file does not exist in the spool.
pub const ERR_NOT_FOUND: u32 = 3;
/// The file exceeds [`MAX_FILE_SIZE`].
pub const ERR_TOO_LARGE: u32 = 4;
/// Chunk offset does not match the receiver's position, or a resume
/// offset lies past the end of the file.
pub const ERR_BAD_OFFSET: u32 = 5;
/// Unknown transfer id (no such active transfer).
pub const ERR_BAD_TRANSFER: u32 = 6;
/// The spool quota (§18.78) would be exceeded by the transfer.
pub const ERR_QUOTA_EXCEEDED: u32 = 7;

/// Validates a spool file name: a plain leaf name — no path separators,
/// no parent references, no control characters, no leading dot.
///
/// # Errors
/// Returns [`StorageError::BadName`] when the name is not a safe leaf.
pub fn validate_name(name: &str) -> Result<(), StorageError> {
    let ok = !name.is_empty()
        && name.len() <= usize::from(MAX_NAME_LEN)
        && !name.starts_with('.')
        && !name.ends_with(['.', ' '])
        && name
            .chars()
            .all(|c| !c.is_control() && c != '/' && c != '\\' && c != ':');
    if ok {
        Ok(())
    } else {
        Err(StorageError::BadName)
    }
}

/// Errors decoding storage messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    /// Buffer shorter than the message needs.
    #[error("truncated storage message")]
    Truncated,
    /// Unknown message tag byte.
    #[error("unknown storage tag: {0}")]
    UnknownTag(u8),
    /// Chunk payload exceeds [`MAX_CHUNK_PAYLOAD`].
    #[error("storage chunk too large: {0}")]
    PayloadTooLarge(u32),
    /// Declared size exceeds [`MAX_FILE_SIZE`].
    #[error("file too large: {0}")]
    FileTooLarge(u64),
    /// File name failed [`validate_name`].
    #[error("bad file name")]
    BadName,
    /// Error message exceeds [`MAX_ERROR_MSG_LEN`].
    #[error("error message too long")]
    ErrorMsgTooLong,
}

/// One storage channel message (either direction; see the module docs for
/// which side may send what).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageMessage {
    /// C→S: list the server spool directory.
    ListRequest,
    /// S→C: one entry of the listing.
    ListEntry {
        /// Leaf file name.
        name: String,
        /// File size in bytes.
        size: u64,
    },
    /// S→C: listing finished.
    ListDone,
    /// C→S: begin an upload of `size` bytes under `name` (a leaf name;
    /// the server writes into its spool).
    PutStart {
        /// Client-chosen transfer identifier.
        transfer_id: u32,
        /// Destination leaf file name.
        name: String,
        /// Total file size in bytes.
        size: u64,
    },
    /// C→S: one chunk of an active upload. Offsets must be sequential
    /// (the receiver rejects out-of-order writes with `ERR_BAD_OFFSET`);
    /// explicit offsets keep the door open to resumable uploads.
    PutChunk {
        /// Transfer the chunk belongs to.
        transfer_id: u32,
        /// Byte offset of the chunk in the file.
        offset: u64,
        /// Chunk bytes.
        data: Vec<u8>,
    },
    /// C→S: upload finished; the receiver verifies the byte count and
    /// commits the file (atomic rename), then answers `PutAck` or `Error`.
    PutDone {
        /// Transfer being committed.
        transfer_id: u32,
    },
    /// C→S: abort an active upload (partial file is discarded).
    PutAbort {
        /// Transfer being aborted.
        transfer_id: u32,
    },
    /// S→C: the upload was verified and committed to the spool.
    PutAck {
        /// Transfer that was committed.
        transfer_id: u32,
    },
    /// C→S: begin (or resume, with `offset` > 0) a download from the
    /// server spool.
    GetStart {
        /// Client-chosen transfer identifier.
        transfer_id: u32,
        /// Leaf file name in the server spool.
        name: String,
        /// Byte offset to start from.
        offset: u64,
    },
    /// S→C: one chunk of an active download.
    GetChunk {
        /// Transfer the chunk belongs to.
        transfer_id: u32,
        /// Byte offset of the chunk in the file.
        offset: u64,
        /// Chunk bytes.
        data: Vec<u8>,
    },
    /// S→C: download finished (all bytes sent).
    GetDone {
        /// Transfer that finished.
        transfer_id: u32,
    },
    /// C→S: cancel an active download.
    GetAbort {
        /// Transfer being cancelled.
        transfer_id: u32,
    },
    /// Both directions: a request or transfer failed. `transfer_id` is 0
    /// when the error is not tied to a transfer (e.g. a bad `GetStart`).
    Error {
        /// Related transfer, or 0.
        transfer_id: u32,
        /// One of the `ERR_*` codes.
        code: u32,
        /// Human-readable detail (for logs, not UI).
        message: String,
    },
}

/// Encodes a leaf name: `u16` length + UTF-8 bytes.
fn encode_name(out: &mut Vec<u8>, name: &str) {
    // Validated names fit (≤ MAX_NAME_LEN); clamp defensively on encode.
    out.extend_from_slice(
        &u16::try_from(name.len())
            .unwrap_or(MAX_NAME_LEN)
            .to_be_bytes(),
    );
    out.extend_from_slice(name.as_bytes());
}

/// Decodes a `(name, size)` pair from the front of `buf`.
fn decode_name_size(buf: &[u8]) -> Result<(String, u64), StorageError> {
    let name_len = decode_name_len(buf)?;
    let end = 2 + name_len;
    if buf.len() < end + 8 {
        return Err(StorageError::Truncated);
    }
    let name = decode_name(&buf[2..end])?;
    let size = u64::from_be_bytes(buf[end..end + 8].try_into().expect("8 bytes"));
    Ok((name, size))
}

/// Reads the `u16` name length prefix.
fn decode_name_len(buf: &[u8]) -> Result<usize, StorageError> {
    if buf.len() < 2 {
        return Err(StorageError::Truncated);
    }
    let len = usize::from(u16::from_be_bytes([buf[0], buf[1]]));
    if len > usize::from(MAX_NAME_LEN) {
        return Err(StorageError::BadName);
    }
    Ok(len)
}

/// Validates and converts raw name bytes.
fn decode_name(raw: &[u8]) -> Result<String, StorageError> {
    let name = String::from_utf8(raw.to_vec()).map_err(|_| StorageError::BadName)?;
    validate_name(&name)?;
    Ok(name)
}

/// Decodes `(transfer_id, offset, data)` from the front of `buf`.
fn decode_chunk(buf: &[u8]) -> Result<(u32, u64, Vec<u8>), StorageError> {
    if buf.len() < 16 {
        return Err(StorageError::Truncated);
    }
    let transfer_id = u32::from_be_bytes(buf[0..4].try_into().expect("4 bytes"));
    let offset = u64::from_be_bytes(buf[4..12].try_into().expect("8 bytes"));
    let len = u32::from_be_bytes(buf[12..16].try_into().expect("4 bytes"));
    if len > MAX_CHUNK_PAYLOAD {
        return Err(StorageError::PayloadTooLarge(len));
    }
    let data = buf
        .get(16..16 + len as usize)
        .ok_or(StorageError::Truncated)?
        .to_vec();
    Ok((transfer_id, offset, data))
}

/// Encodes a `(transfer_id, offset, data)` triple.
fn encode_chunk(out: &mut Vec<u8>, transfer_id: u32, offset: u64, data: &[u8]) {
    out.extend_from_slice(&transfer_id.to_be_bytes());
    out.extend_from_slice(&offset.to_be_bytes());
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(data);
}

impl StorageMessage {
    /// Encodes the message, appending to `out` (allows batching).
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::ListRequest => out.push(TAG_LIST_REQUEST),
            Self::ListEntry { name, size } => {
                out.push(TAG_LIST_ENTRY);
                encode_name(out, name);
                out.extend_from_slice(&size.to_be_bytes());
            }
            Self::ListDone => out.push(TAG_LIST_DONE),
            Self::PutStart {
                transfer_id,
                name,
                size,
            } => {
                out.push(TAG_PUT_START);
                out.extend_from_slice(&transfer_id.to_be_bytes());
                encode_name(out, name);
                out.extend_from_slice(&size.to_be_bytes());
            }
            Self::PutChunk {
                transfer_id,
                offset,
                data,
            } => {
                out.push(TAG_PUT_CHUNK);
                encode_chunk(out, *transfer_id, *offset, data);
            }
            Self::PutDone { transfer_id } | Self::PutAbort { transfer_id } => {
                out.push(if matches!(self, Self::PutDone { .. }) {
                    TAG_PUT_DONE
                } else {
                    TAG_PUT_ABORT
                });
                out.extend_from_slice(&transfer_id.to_be_bytes());
            }
            Self::PutAck { transfer_id } => {
                out.push(TAG_PUT_ACK);
                out.extend_from_slice(&transfer_id.to_be_bytes());
            }
            Self::GetStart {
                transfer_id,
                name,
                offset,
            } => {
                out.push(TAG_GET_START);
                out.extend_from_slice(&transfer_id.to_be_bytes());
                encode_name(out, name);
                out.extend_from_slice(&offset.to_be_bytes());
            }
            Self::GetChunk {
                transfer_id,
                offset,
                data,
            } => {
                out.push(TAG_GET_CHUNK);
                encode_chunk(out, *transfer_id, *offset, data);
            }
            Self::GetDone { transfer_id } | Self::GetAbort { transfer_id } => {
                out.push(if matches!(self, Self::GetDone { .. }) {
                    TAG_GET_DONE
                } else {
                    TAG_GET_ABORT
                });
                out.extend_from_slice(&transfer_id.to_be_bytes());
            }
            Self::Error {
                transfer_id,
                code,
                message,
            } => {
                out.push(TAG_ERROR);
                out.extend_from_slice(&transfer_id.to_be_bytes());
                out.extend_from_slice(&code.to_be_bytes());
                out.extend_from_slice(
                    &u16::try_from(message.len())
                        .unwrap_or(MAX_ERROR_MSG_LEN)
                        .to_be_bytes(),
                );
                out.extend_from_slice(message.as_bytes());
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

    /// Decodes one message from the front of `buf`.
    ///
    /// # Errors
    /// Returns [`StorageError`] on truncation, unknown tags, oversized
    /// fields or invalid file names.
    pub fn decode(buf: &[u8]) -> Result<Self, StorageError> {
        let (&tag, rest) = buf.split_first().ok_or(StorageError::Truncated)?;
        let id = |b: &[u8]| -> Result<u32, StorageError> {
            if b.len() < 4 {
                return Err(StorageError::Truncated);
            }
            Ok(u32::from_be_bytes(b[0..4].try_into().expect("4 bytes")))
        };
        match tag {
            TAG_LIST_REQUEST => Ok(Self::ListRequest),
            TAG_LIST_ENTRY => {
                let (name, size) = decode_name_size(rest)?;
                Ok(Self::ListEntry { name, size })
            }
            TAG_LIST_DONE => Ok(Self::ListDone),
            TAG_PUT_START => {
                let transfer_id = id(rest)?;
                let (name, size) = decode_name_size(&rest[4..])?;
                if size > MAX_FILE_SIZE {
                    return Err(StorageError::FileTooLarge(size));
                }
                Ok(Self::PutStart {
                    transfer_id,
                    name,
                    size,
                })
            }
            TAG_PUT_CHUNK => {
                let (transfer_id, offset, data) = decode_chunk(rest)?;
                Ok(Self::PutChunk {
                    transfer_id,
                    offset,
                    data,
                })
            }
            TAG_PUT_DONE => Ok(Self::PutDone {
                transfer_id: id(rest)?,
            }),
            TAG_PUT_ABORT => Ok(Self::PutAbort {
                transfer_id: id(rest)?,
            }),
            TAG_PUT_ACK => Ok(Self::PutAck {
                transfer_id: id(rest)?,
            }),
            TAG_GET_START => {
                let transfer_id = id(rest)?;
                let name_len = decode_name_len(&rest[4..])?;
                let end = 4 + 2 + name_len;
                if rest.len() < end + 8 {
                    return Err(StorageError::Truncated);
                }
                let name = decode_name(&rest[6..end])?;
                let offset = u64::from_be_bytes(rest[end..end + 8].try_into().expect("8 bytes"));
                Ok(Self::GetStart {
                    transfer_id,
                    name,
                    offset,
                })
            }
            TAG_GET_CHUNK => {
                let (transfer_id, offset, data) = decode_chunk(rest)?;
                Ok(Self::GetChunk {
                    transfer_id,
                    offset,
                    data,
                })
            }
            TAG_GET_DONE => Ok(Self::GetDone {
                transfer_id: id(rest)?,
            }),
            TAG_GET_ABORT => Ok(Self::GetAbort {
                transfer_id: id(rest)?,
            }),
            TAG_ERROR => {
                if rest.len() < 10 {
                    return Err(StorageError::Truncated);
                }
                let transfer_id = u32::from_be_bytes(rest[0..4].try_into().expect("4 bytes"));
                let code = u32::from_be_bytes(rest[4..8].try_into().expect("4 bytes"));
                let msg_len = usize::from(u16::from_be_bytes([rest[8], rest[9]]));
                if msg_len > usize::from(MAX_ERROR_MSG_LEN) {
                    return Err(StorageError::ErrorMsgTooLong);
                }
                let raw = rest.get(10..10 + msg_len).ok_or(StorageError::Truncated)?;
                let message =
                    String::from_utf8(raw.to_vec()).map_err(|_| StorageError::ErrorMsgTooLong)?;
                Ok(Self::Error {
                    transfer_id,
                    code,
                    message,
                })
            }
            other => Err(StorageError::UnknownTag(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: StorageMessage) {
        assert_eq!(StorageMessage::decode(&msg.encode()), Ok(msg));
    }

    #[test]
    fn simple_roundtrips() {
        roundtrip(StorageMessage::ListRequest);
        roundtrip(StorageMessage::ListDone);
        roundtrip(StorageMessage::PutDone { transfer_id: 3 });
        roundtrip(StorageMessage::PutAbort { transfer_id: 4 });
        roundtrip(StorageMessage::PutAck { transfer_id: 5 });
        roundtrip(StorageMessage::GetDone { transfer_id: 6 });
        roundtrip(StorageMessage::GetAbort { transfer_id: 7 });
    }

    #[test]
    fn named_roundtrips() {
        roundtrip(StorageMessage::ListEntry {
            name: "informe.pdf".into(),
            size: 12_345,
        });
        roundtrip(StorageMessage::PutStart {
            transfer_id: 1,
            name: "subida.bin".into(),
            size: MAX_FILE_SIZE,
        });
        roundtrip(StorageMessage::GetStart {
            transfer_id: 2,
            name: "bajada.bin".into(),
            offset: 4096,
        });
    }

    #[test]
    fn chunk_roundtrips() {
        roundtrip(StorageMessage::PutChunk {
            transfer_id: 9,
            offset: 65_536,
            data: vec![0xAB; 1024],
        });
        roundtrip(StorageMessage::GetChunk {
            transfer_id: 9,
            offset: 0,
            data: Vec::new(),
        });
    }

    #[test]
    fn error_roundtrip() {
        roundtrip(StorageMessage::Error {
            transfer_id: 0,
            code: ERR_NOT_FOUND,
            message: "no such file".into(),
        });
    }

    #[test]
    fn name_validation() {
        assert!(validate_name("informe final.pdf").is_ok());
        assert!(validate_name("a").is_ok());
        for bad in [
            "",
            ".oculto",
            "..",
            "dir/file",
            "dir\\file",
            "C:file",
            "fin.",
            "fin ",
            "con\ttab",
        ] {
            assert!(validate_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn oversized_fields_are_rejected() {
        let mut buf = vec![TAG_PUT_START];
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.push(b'a');
        buf.extend_from_slice(&(MAX_FILE_SIZE + 1).to_be_bytes());
        assert_eq!(
            StorageMessage::decode(&buf),
            Err(StorageError::FileTooLarge(MAX_FILE_SIZE + 1))
        );

        let mut buf = vec![TAG_PUT_CHUNK];
        buf.extend_from_slice(&1u32.to_be_bytes());
        buf.extend_from_slice(&0u64.to_be_bytes());
        buf.extend_from_slice(&(MAX_CHUNK_PAYLOAD + 1).to_be_bytes());
        assert_eq!(
            StorageMessage::decode(&buf),
            Err(StorageError::PayloadTooLarge(MAX_CHUNK_PAYLOAD + 1))
        );
    }

    #[test]
    fn bad_wire_names_are_rejected() {
        // Name longer than the cap.
        let mut buf = vec![TAG_LIST_ENTRY];
        buf.extend_from_slice(&(MAX_NAME_LEN + 1).to_be_bytes());
        assert_eq!(StorageMessage::decode(&buf), Err(StorageError::BadName));
        // Path traversal inside an otherwise well-formed entry.
        let mut buf = vec![TAG_LIST_ENTRY];
        let name = "../evil".as_bytes();
        buf.extend_from_slice(&u16::try_from(name.len()).expect("short name").to_be_bytes());
        buf.extend_from_slice(name);
        buf.extend_from_slice(&0u64.to_be_bytes());
        assert!(matches!(
            StorageMessage::decode(&buf),
            Err(StorageError::BadName)
        ));
    }

    #[test]
    fn empty_and_truncated_buffers() {
        assert_eq!(StorageMessage::decode(&[]), Err(StorageError::Truncated));
        assert_eq!(
            StorageMessage::decode(&[TAG_ERROR, 0, 0]),
            Err(StorageError::Truncated)
        );
        assert_eq!(
            StorageMessage::decode(&[222]),
            Err(StorageError::UnknownTag(222))
        );
    }
}
