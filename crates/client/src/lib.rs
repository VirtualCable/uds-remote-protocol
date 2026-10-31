//! # client
//!
//! Client side of the remote desktop protocol: session management,
//! video/audio decoding, keyboard/mouse event sending and connection
//! recovery (Recover). No GUI included (§15.3): the session talks to its
//! consumer through an event/command channel pair modeled after
//! freerdp-integration, and rendering is provided by the consumer
//! (testclient during development, UDS launcher in production).
//!
//! ```text
//! consumer ──ClientCommand──▶ Session::run() (Tokio loop)
//! consumer ◀──ClientEvent──── Session   (pixels stay in Displays)
//! ```

/// Audio jitter buffer (§9.1): reorders datagrams by `seq` and requests PLC
/// for lost frames. Lives in `protocol` (both audio directions use it: the
/// client for audio-out, the server for audio-in); re-exported here for
/// existing client-side users.
pub use protocol::jitter;

pub mod audio;
pub mod avsync;
mod command;
mod events;
mod framebuffer;
pub mod mic;
mod session;
pub mod smartcard;
pub mod stats;
mod storage;

pub use command::{ClientCommand, CommandSender};
pub use events::{ClientEvent, Rect, TransferDirection};
pub use stats::StatsSnapshot;
pub use framebuffer::{DisplayBuffer, Displays, SharedDisplay};
pub use session::{ClientConfig, Session};

pub use events::EVENT_CHANNEL_CAPACITY;
