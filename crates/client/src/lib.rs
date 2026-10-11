//! # client
//!
//! Client side of the remote desktop protocol: session management,
//! video/audio decoding, keyboard/mouse event sending and connection
//! recovery (Recover). No GUI included: rendering is provided by the
//! consumer (testclient during development, UDS launcher in production).

/// Audio jitter buffer (§9.1): reorders datagrams by `seq` and requests PLC
/// for lost frames. Lives in `protocol` (both audio directions use it: the
/// client for audio-out, the server for audio-in); re-exported here for
/// existing client-side users.
pub use protocol::jitter;

pub mod avsync;
