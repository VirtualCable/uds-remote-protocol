//! # client
//!
//! Client side of the remote desktop protocol: session management,
//! video/audio decoding, keyboard/mouse event sending and connection
//! recovery (Recover). No GUI included: rendering is provided by the
//! consumer (testclient during development, UDS launcher in production).

/// Audio jitter buffer (§9.1): reorders datagrams by `seq` and requests PLC
/// for lost frames.
pub mod jitter;
