// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Commands delivered from the consumer to the protocol session (§15.3).
//!
//! The session returns a [`CommandSender`] on construction and drains it
//! from its Tokio loop with a `select!` branch — no wakeup event is needed
//! (unlike freerdp-integration's Win32 command event). The channel is
//! bounded; producers should use `try_send` and tolerate drops (a pointer
//! move dropped under load is harmless, the next one carries the current
//! position).

use protocol::InputMessage;

/// Bound of the session command channel created by [`crate::Session::new`].
pub const COMMAND_CHANNEL_CAPACITY: usize = 256;

/// Consumer → protocol commands (§15.3).
#[derive(Debug, Clone)]
pub enum ClientCommand {
    /// One input event (key, pointer move/button/wheel), forwarded on the
    /// reliable input channel (§10).
    Input(InputMessage),
    /// Enables or disables microphone capture at runtime (§9.2). Capture
    /// still requires the server's `AudioInConfig` announcement and a
    /// configured [`crate::mic::MicMode`].
    SetMic(bool),
    /// The local clipboard changed (M8); the session announces the new
    /// content to the server, which pulls it on demand.
    ClipboardChanged(String),
    /// Requests a remote display-mode change. Reserved: the wire protocol
    /// has no `SetDisplayMode` yet (§15.2), so the session currently logs
    /// and ignores it.
    Resize {
        /// Requested width in pixels.
        width: u32,
        /// Requested height in pixels.
        height: u32,
    },
    /// Asks the server for a keyframe on the given display. The session
    /// rate-limits these (1 per 500 ms, riesgos §2.1), so consumers may
    /// send freely.
    RequestKeyframe {
        /// Display identifier.
        display_id: u8,
    },
    /// Ends the session: the loop finishes, `ClientEvent::Disconnected` is
    /// emitted and [`crate::Session::run`] returns.
    Close,
    /// Lists the server's storage spool (M8, model B); the answer arrives
    /// as [`crate::ClientEvent::StorageList`].
    StorageList,
    /// Uploads a local file into the server spool. The spool name is the
    /// path's leaf name. Progress ends in `TransferDone`/`TransferError`.
    StorageUpload {
        /// Local file to send.
        path: std::path::PathBuf,
    },
    /// Downloads a file from the server spool to a local path.
    StorageDownload {
        /// Leaf name in the server spool.
        name: String,
        /// Local destination path.
        dest: std::path::PathBuf,
    },
    /// Cancels an active transfer (upload or download).
    StorageAbort {
        /// Transfer to cancel.
        transfer_id: u32,
    },
    /// Enables or disables smartcard redirection (§17) at runtime. The
    /// channel is fully server-driven — there is nothing to announce —
    /// but the dispatcher only runs while enabled; while disabled (or
    /// without a PC/SC backend) requests are answered
    /// `SCARD_E_NO_SERVICE`. Note the HELLO capability flag is sent at
    /// connect time from [`crate::ClientConfig::smartcard`], so toggling
    /// afterwards does not change what the server believes.
    SmartcardEnable(bool),
    /// Asks the server to lock the remote session (§18.50): the GUI maps
    /// Ctrl+Alt+Shift+L to this because Win+L is always grabbed by the local OS.
    LockRemote,
}

/// Sending half of the command channel, handed to the consumer by
/// [`crate::Session::new`].
pub type CommandSender = flume::Sender<ClientCommand>;

/// Receiving half, owned by the session loop.
pub(crate) type CommandReceiver = flume::Receiver<ClientCommand>;
