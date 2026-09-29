//! # protocol
//!
//! Protocol core: handshake, channel 0 messages, channel definitions and
//! common error types. **Synchronous and pure** crate: no dependency on
//! networking, cryptography or async runtimes (compilable to `wasm32-unknown-unknown`).

pub mod channel;
pub mod fragment;
mod control;
mod error;
mod hello;
mod probe;

pub use channel::ChannelId;
pub use control::{Capability, ControlMessage};
pub use error::ProtocolError;
pub use hello::{Command, HANDSHAKE_SIGNATURE, Hello, PROTOCOL_VERSION, TICKET_LENGTH};
pub use probe::{Probe, ProbeAck, ProbeError, compute_rtt};
