// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! # protocol
//!
//! Protocol core: handshake, channel 0 messages, channel definitions,
//! common error types and cryptography ([`crypt`]). **Synchronous and pure**
//! crate: no dependency on networking or async runtimes (compilable to
//! `wasm32-unknown-unknown`); the crypto is pure Rust (aes-gcm, hkdf, sha2).

pub mod audio;
pub mod crypt;
pub mod service_ipc;
pub mod channel;
pub mod fragment;
pub mod hex;
pub mod jitter;
pub mod webcam;
mod clipboard;
mod control;
mod cursor;
mod error;
mod hello;
mod input;
mod printer;
mod probe;
mod quality;
mod smartcard;
mod storage;

pub use channel::ChannelId;
pub use clipboard::{
    ClipboardError, ClipboardMessage, ClipboardSession, FORMAT_TEXT, MAX_CLIPBOARD_PAYLOAD,
};
pub use control::{
    Capability, ControlMessage, CONSOLE_STATE_ACTIVE, CONSOLE_STATE_DISCONNECTED,
    CONSOLE_STATE_LOCKED, SESSION_END_CONSOLE_BUSY, SESSION_END_DENIED_BY_USER,
    SESSION_STAGE_AUTOLOGON, SESSION_STAGE_EVICTING,
};
pub use cursor::{CursorError, CursorMessage, MAX_CURSOR_DIM};
pub use error::ProtocolError;
pub use hello::{
    Command, FLAG_H264_DECODE, FLAG_PRINTER, FLAG_PRIVACY_SCREEN, FLAG_SMARTCARD, FLAG_WEBCAM,
    HANDSHAKE_SIGNATURE,
    Hello, PROTOCOL_VERSION, TICKET_LENGTH,
};
pub use input::{InputError, InputMessage, PointerButton};
pub use printer::{
    JOB_FORMAT_PDF, JOB_FORMAT_POSTSCRIPT, MAX_JOB_BYTES, MAX_JOB_CHUNK, PrinterError,
    PrinterMessage, STATUS_SUCCESS,
};
pub use probe::{Probe, ProbeAck, ProbeError, compute_rtt};
pub use quality::{
    FEC_OFF_LOSS, FEC_ON_LOSS, LEVEL_DEGRADED, LEVEL_FULL, LevelSpec, PressureController,
    PressurePolicy, PressureSample, QualityController, QualityEvent, QualityLevel, QualityPolicy,
    QualityTargets, Sample, default_levels, fec_update, resolve_targets, targets,
};
pub use smartcard::{
    MAX_APDU_LEN, MAX_ATR_LEN, MAX_READER_NAME_LEN, MAX_READERS, MAX_STATES, ReaderState,
    SCOPE_SYSTEM, SCOPE_USER, SmartcardError, SmartcardMessage,
};
pub use storage::{
    ERR_BAD_NAME, ERR_BAD_OFFSET, ERR_BAD_TRANSFER, ERR_IO, ERR_NOT_FOUND, ERR_TOO_LARGE,
    MAX_CHUNK_PAYLOAD, MAX_ERROR_MSG_LEN, MAX_FILE_SIZE, MAX_NAME_LEN, STORAGE_CHUNK_SIZE,
    StorageError, StorageMessage, validate_name,
};
