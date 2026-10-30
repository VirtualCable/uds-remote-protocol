//! # protocol
//!
//! Protocol core: handshake, channel 0 messages, channel definitions and
//! common error types. **Synchronous and pure** crate: no dependency on
//! networking, cryptography or async runtimes (compilable to `wasm32-unknown-unknown`).

pub mod audio;
pub mod channel;
pub mod fragment;
pub mod hex;
pub mod jitter;
mod clipboard;
mod control;
mod cursor;
mod error;
mod hello;
mod input;
mod probe;
mod quality;

pub use channel::ChannelId;
pub use clipboard::{
    ClipboardError, ClipboardMessage, ClipboardSession, FORMAT_TEXT, MAX_CLIPBOARD_PAYLOAD,
};
pub use control::{Capability, ControlMessage};
pub use cursor::{CursorError, CursorMessage, MAX_CURSOR_DIM};
pub use error::ProtocolError;
pub use hello::{Command, FLAG_PRIVACY_SCREEN, HANDSHAKE_SIGNATURE, Hello, PROTOCOL_VERSION, TICKET_LENGTH};
pub use input::{InputError, InputMessage, PointerButton};
pub use probe::{Probe, ProbeAck, ProbeError, compute_rtt};
pub use quality::{
    FEC_OFF_LOSS, FEC_ON_LOSS, LEVEL_DEGRADED, LEVEL_FULL, QualityController, QualityEvent,
    QualityLevel, QualityPolicy, QualityTargets, Sample, fec_update, targets,
};
