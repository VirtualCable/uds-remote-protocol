//! # protocol
//!
//! Núcleo del protocolo: handshake, mensajes del canal 0, definición de
//! canales y tipos de error comunes. Crate **síncrono y puro**: no depende de
//! red, criptografía ni runtime async (compilable a `wasm32-unknown-unknown`).

pub mod channel;
mod control;
mod error;
mod hello;

pub use channel::ChannelId;
pub use control::{Capability, ControlMessage};
pub use error::ProtocolError;
pub use hello::{Command, HANDSHAKE_SIGNATURE, Hello, PROTOCOL_VERSION, TICKET_LENGTH};
