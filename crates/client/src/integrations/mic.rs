// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Microphone capture contract: the [`MicSource`] frame-pull abstraction.
//! Device implementations (cpal input, test tone) live in
//! `channels::audio::input`; the protocol side (Opus encode, wire pacing)
//! lives in `client`.
//!
//! The wire format is fixed 48 kHz mono (§9.2); implementations adapt
//! whatever the device delivers (remix/resample) before filling frames.

/// A source of mic frames: interleaved signed 16-bit mono PCM, exactly
/// `frame_samples` samples per frame (the audio-in wire shape, §9.2).
pub trait MicSource: Send {
    /// Fills `out` with the next frame; `out` holds exactly one frame.
    fn fill_frame(&mut self, out: &mut [i16]) -> anyhow::Result<()>;
}

/// Builds a mic source capturing frames of `frame_samples` samples;
/// `None` means no input device (the mic then stays off for the session).
/// Injected by the consumer so `client` never touches audio devices
/// directly.
pub type MicFactory = Box<dyn Fn(usize) -> Option<Box<dyn MicSource>> + Send>;
