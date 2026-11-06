// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Audio device side: playback buffer + output stream (output.rs) and
//! microphone capture (input.rs). The protocol side of audio (jitter
//! buffer, Opus, wire pacing, A/V clock mapping) stays in `client`; the
//! pure DSP helpers (format conversion, remix, resampling, capture ring)
//! live in [`dsp`] and are shared with the server's capture side.

pub mod dsp;
pub mod input;
pub mod output;

#[cfg(feature = "audio")]
pub use input::CpalMicSource;
pub use input::MicSource;
pub use output::{PlaybackBuffer, PlaybackStream, SharedBuffer, start_playback};
