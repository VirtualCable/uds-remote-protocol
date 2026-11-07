// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Audio output contract: one open playback stream on the local device.
//!
//! The session's audio pump decodes Opus and pushes PCM into the shared
//! [`PlaybackBuffer`](codec::audio_dsp::PlaybackBuffer); the device
//! callback drains it. The trait is intentionally minimal — the pump owns
//! jitter, clock mapping and resampling policy; the implementation only
//! owns the device stream.
//!
//! The cpal implementation lives in `channels::audio::output`.

use codec::audio_dsp::SharedBuffer;

/// One open audio output stream. Dropping it stops playback.
pub trait AudioOutput: Send {
    /// Sample rate the device actually runs at: the requested rate when
    /// supported, else the device default (the pump resamples, §9.1).
    fn sample_rate(&self) -> u32;
    /// Channel count of the opened stream.
    fn channels(&self) -> u8;
    /// The shared buffer the pump pushes decoded PCM into.
    fn buffer(&self) -> SharedBuffer;
}

/// Builds an output stream for the announced spec; `None` means the null
/// sink (headless or no device — the pump still decodes). Injected by the
/// consumer so `client` never touches audio devices directly.
pub type AudioOutputFactory = Box<dyn Fn(u32, u8) -> Option<Box<dyn AudioOutput>> + Send>;
