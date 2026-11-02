// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Audio capture abstraction (design doc §9).
//!
//! [`AudioSource`] is the seam between the encoder loop and the capture
//! backends: [`SyntheticSource`] (two-tone generator, every platform) and
//! `audio_wasapi::WasapiLoopbackSource` (real desktop loopback, Windows with
//! the `windows-capture` feature). The format adaptation they share (f32→i16
//! conversion, remix, resampling, capture ring) lives in `codec::audio_dsp`.
//!
//! [`SilenceGate`] implements the PoC-level DTX policy (§9.1): frames that
//! are exact digital silence go out only as a clean silence onset (optional
//! keepalives, disabled by default).

/// A source of audio frames: interleaved signed 16-bit PCM, exactly
/// `frame_samples * channels` samples per frame.
pub trait AudioSource: Send {
    /// Sample rate in Hz (e.g. 48000).
    fn sample_rate(&self) -> u32;

    /// Number of channels (1 = mono, 2 = stereo).
    fn channels(&self) -> u8;

    /// Samples per channel in one frame (e.g. 960 for 20 ms @ 48 kHz).
    fn frame_samples(&self) -> usize;

    /// Fills `out` with the next frame. `out` must hold exactly
    /// `frame_samples * channels` samples.
    fn fill_frame(&mut self, out: &mut [i16]) -> anyhow::Result<()>;
}

/// Default keepalive cadence for silent frames (PoC-level DTX, §9.1): one
/// frame in every `SILENCE_KEEPALIVE_EVERY` is still sent while the source
/// produces exact silence — at 20 ms frames that is 2 packets/s. Kept for
/// future gateway/NAT needs; production default is **no keepalives** (see
/// [`SilenceGate::without_keepalives`]): the video leg already keeps the
/// UDP path warm, and keepalives arriving every ~500 ms flapped the
/// client's A/V clock liveness (silent desktop ⇒ stutter).
pub const SILENCE_KEEPALIVE_EVERY: u32 = 25;

/// PoC-level discontinuous transmission (§9.1): a frame is "silent" when
/// every sample is exactly zero — what WASAPI loopback delivers on an idle
/// desktop (including its `AUDCLNT_BUFFERFLAGS_SILENT` packets, which the
/// capture pipeline materializes as zeros). The first silent frame after
/// activity always goes out (a clean silence onset), then one keepalive per
/// `keepalive_every` frames — or none when keepalives are disabled. The
/// sequence counter does NOT advance for skipped frames (the client sees
/// fewer frames, not losses).
#[derive(Debug, Default)]
pub struct SilenceGate {
    /// Consecutive silent frames seen so far.
    silent_run: u32,
    /// One silent frame in every N is sent as a keepalive; 0 disables
    /// keepalives (only the silence onset goes out).
    keepalive_every: u32,
}

impl SilenceGate {
    /// Creates a gate in the "activity" state (the first silent frame is
    /// sent), without keepalives.
    #[must_use]
    pub fn new() -> Self {
        Self::without_keepalives()
    }

    /// Creates a gate that sends one keepalive every `n` silent frames
    /// (0 = never, the production default).
    #[must_use]
    pub fn with_keepalive_every(n: u32) -> Self {
        Self {
            silent_run: 0,
            keepalive_every: n,
        }
    }

    /// Creates a gate with keepalives disabled: only the first silent frame
    /// after activity goes out.
    #[must_use]
    pub fn without_keepalives() -> Self {
        Self::with_keepalive_every(0)
    }

    /// Returns true when this frame must be encoded and sent.
    pub fn should_send(&mut self, frame: &[i16]) -> bool {
        if frame.iter().any(|&s| s != 0) {
            self.silent_run = 0;
            return true;
        }
        self.silent_run = self.silent_run.saturating_add(1);
        // Send the 1st silent frame (clean onset), then every Nth.
        self.silent_run == 1
            || (self.keepalive_every != 0
                && (self.silent_run - 1).is_multiple_of(self.keepalive_every))
    }
}

/// Shared congestion-control knobs for the audio encoder thread (§12.5).
///
/// The async control loop (owner of the `QualityController`) writes; the
/// audio thread, which owns the encoder, reads and applies at frame
/// boundaries — the same shared-atomic mechanism the capture thread uses
/// for `fps`. Bitrate is capped by the negotiated ceiling at the writer
/// side (`protocol::targets`); the floor (6 kbps) comes from the policy.
#[derive(Clone, Debug)]
pub struct AudioControl {
    /// Target encoder bitrate in kbps.
    pub target_kbps: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// In-band FEC on/off (loss-driven, with hysteresis — §12.5).
    pub fec: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl AudioControl {
    /// Creates the knobs at the preset's initial values.
    #[must_use]
    pub fn new(target_kbps: u32, fec: bool) -> Self {
        Self {
            target_kbps: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(target_kbps)),
            fec: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(fec)),
        }
    }

    /// Stores a new target bitrate (kbps).
    pub fn set_target_kbps(&self, kbps: u32) {
        self.target_kbps
            .store(kbps, std::sync::atomic::Ordering::Relaxed);
    }

    /// Stores the FEC switch.
    pub fn set_fec(&self, enabled: bool) {
        self.fec.store(enabled, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Applies pending control changes to `encoder`, updating `applied`
/// (`(kbps, fec)` last applied). Encoder calls happen only on change, so
/// this is safe to call once per frame (§12.5). Failures are logged and the
/// new value is still recorded as applied (retrying a rejected knob every
/// frame would spam; the next change retries it).
pub fn apply_control(
    encoder: &mut dyn codec::AudioEncoder,
    control: &AudioControl,
    applied: &mut (u32, bool),
) {
    use std::sync::atomic::Ordering;
    use tracing::{info, warn};

    let kbps = control.target_kbps.load(Ordering::Relaxed);
    if kbps != applied.0 {
        match encoder.set_bitrate_kbps(kbps) {
            Ok(()) => info!(kbps, "audio bitrate applied"),
            Err(e) => warn!(kbps, "audio set_bitrate failed: {e}"),
        }
        applied.0 = kbps;
    }
    let fec = control.fec.load(Ordering::Relaxed);
    if fec != applied.1 {
        match encoder.set_fec(fec) {
            Ok(()) => info!(fec, "audio FEC applied"),
            Err(e) => warn!(fec, "audio set_fec failed: {e}"),
        }
        applied.1 = fec;
    }
}

#[cfg(test)]
mod control_tests {
    use super::*;

    /// Records the control calls instead of encoding.
    struct MockEncoder {
        calls: Vec<(u32, bool)>,
    }

    impl codec::AudioEncoder for MockEncoder {
        fn encode(&mut self, _pcm: &[i16], _out: &mut Vec<u8>) -> codec::Result<usize> {
            Ok(0)
        }
        fn set_bitrate_kbps(&mut self, kbps: u32) -> codec::Result<()> {
            self.calls.push((kbps, false));
            Ok(())
        }
        fn set_fec(&mut self, enabled: bool) -> codec::Result<()> {
            self.calls.push((0, enabled));
            Ok(())
        }
        fn codec_type(&self) -> codec::AudioCodecType {
            codec::AudioCodecType::Opus
        }
        fn sample_rate(&self) -> u32 {
            48_000
        }
        fn channels(&self) -> u8 {
            2
        }
        fn frame_samples(&self) -> usize {
            960
        }
    }

    #[test]
    fn applies_only_on_change() {
        let control = AudioControl::new(64, false);
        let mut enc = MockEncoder { calls: Vec::new() };
        let mut applied = (64, false);
        // Nothing pending: no encoder calls.
        apply_control(&mut enc, &control, &mut applied);
        assert_eq!(enc.calls, []);
        // New bitrate: exactly one call.
        control.set_target_kbps(32);
        apply_control(&mut enc, &control, &mut applied);
        assert_eq!(enc.calls, [(32, false)]);
        // Same value again: no repeat.
        apply_control(&mut enc, &control, &mut applied);
        assert_eq!(enc.calls.len(), 1);
        // FEC on, then off: two calls.
        control.set_fec(true);
        apply_control(&mut enc, &control, &mut applied);
        control.set_fec(false);
        apply_control(&mut enc, &control, &mut applied);
        assert_eq!(enc.calls, [(32, false), (0, true), (0, false)]);
        assert_eq!(applied, (32, false));
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    #[test]
    fn non_silent_always_passes() {
        let mut gate = SilenceGate::new();
        let frame = [0i16, 5, 0];
        for _ in 0..100 {
            assert!(gate.should_send(&frame));
        }
    }

    #[test]
    fn silence_sends_first_then_every_nth() {
        let mut gate = SilenceGate::with_keepalive_every(SILENCE_KEEPALIVE_EVERY);
        let silent = [0i16; 1920];
        let sent = (0..100).filter(|_| gate.should_send(&silent)).count();
        assert_eq!(sent, 4, "frames 1, 26, 51, 76 of 100");
    }

    #[test]
    fn without_keepalives_only_the_onset_goes_out() {
        let mut gate = SilenceGate::new();
        let silent = [0i16; 1920];
        assert!(gate.should_send(&silent), "silence onset is sent");
        let sent = (1..200).filter(|_| gate.should_send(&silent)).count();
        assert_eq!(sent, 0, "no keepalives after the onset");
    }

    #[test]
    fn activity_resets_the_silent_run() {
        let mut gate = SilenceGate::with_keepalive_every(SILENCE_KEEPALIVE_EVERY);
        let silent = [0i16; 960];
        let tone = [1i16; 960];
        assert!(gate.should_send(&silent)); // 1st: sent
        for _ in 0..30 {
            gate.should_send(&silent);
        }
        assert!(gate.should_send(&tone), "activity passes");
        assert!(gate.should_send(&silent), "silence restarts: sent");
        for _ in 0..24 {
            assert!(!gate.should_send(&silent), "skipped until the keepalive");
        }
        assert!(gate.should_send(&silent), "keepalive 25 frames later");
    }
}

/// everywhere in mono), with continuous phase across frames so no clicks
/// appear at frame boundaries. Deterministic given the same shape.
pub struct SyntheticSource {
    sample_rate: u32,
    channels: u8,
    frame_samples: usize,
    /// Phase accumulators (radians), one per tone; kept across frames.
    phase: [f64; 2],
}

/// Tone frequencies in Hz (left, right).
const TONES: [f64; 2] = [440.0, 660.0];

/// Peak amplitude as a fraction of the i16 range (0.3: audible, no clipping
/// headroom issues after encoding).
const AMPLITUDE: f64 = 0.3;

impl SyntheticSource {
    /// Creates a source with the given stream shape.
    #[must_use]
    pub fn new(sample_rate: u32, channels: u8, frame_samples: usize) -> Self {
        Self {
            sample_rate,
            channels,
            frame_samples,
            phase: [0.0; 2],
        }
    }
}

impl AudioSource for SyntheticSource {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    fn fill_frame(&mut self, out: &mut [i16]) -> anyhow::Result<()> {
        let expected = self.frame_samples * usize::from(self.channels);
        anyhow::ensure!(
            out.len() == expected,
            "audio frame buffer must be exactly {expected} samples, got {}",
            out.len()
        );
        let full_scale = f64::from(i16::MAX);
        let step = [
            TONES[0] * 2.0 * std::f64::consts::PI / f64::from(self.sample_rate),
            TONES[1] * 2.0 * std::f64::consts::PI / f64::from(self.sample_rate),
        ];
        for frame in 0..self.frame_samples {
            for ch in 0..usize::from(self.channels) {
                let tone = ch.min(1); // mono always gets tone 0
                // Phase error accumulation over a session is inaudible for a
                // test tone; the cast of the bounded sine value cannot
                // truncate meaningfully.
                #[allow(clippy::cast_possible_truncation)]
                let sample = (self.phase[tone].sin() * AMPLITUDE * full_scale) as i16;
                out[frame * usize::from(self.channels) + ch] = sample;
                self.phase[tone] += step[tone];
                // Keep the phase bounded so f64 resolution does not degrade
                // over multi-hour sessions.
                if self.phase[tone] > 2.0 * std::f64::consts::PI {
                    self.phase[tone] -= 2.0 * std::f64::consts::PI;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_exactly_one_frame() {
        let mut src = SyntheticSource::new(48_000, 2, 960);
        let mut buf = vec![0i16; 960 * 2];
        src.fill_frame(&mut buf).unwrap();
        assert!(buf.iter().any(|&s| s != 0), "not silence");
        // Wrong buffer size is an error, not a partial fill.
        let mut short = vec![0i16; 100];
        assert!(src.fill_frame(&mut short).is_err());
    }

    #[test]
    fn same_shape_is_deterministic() {
        let mut a = SyntheticSource::new(48_000, 2, 960);
        let mut b = SyntheticSource::new(48_000, 2, 960);
        let (mut fa, mut fb) = (vec![0i16; 1920], vec![0i16; 1920]);
        a.fill_frame(&mut fa).unwrap();
        b.fill_frame(&mut fb).unwrap();
        assert_eq!(fa, fb);
    }

    #[test]
    fn stereo_channels_carry_different_tones() {
        let mut src = SyntheticSource::new(48_000, 2, 960);
        let mut buf = vec![0i16; 1920];
        src.fill_frame(&mut buf).unwrap();
        assert!(
            buf.as_chunks::<2>().0.iter().any(|&[l, r]| l != r),
            "left and right must differ (440 vs 660 Hz)"
        );
    }

    #[test]
    fn phase_is_continuous_across_frames() {
        // A click at the frame boundary would show as a sample-to-sample
        // jump far larger than the in-frame maximum step. Compared per
        // channel (the interleaved buffer alternates tones).
        let mut src = SyntheticSource::new(48_000, 2, 960);
        let mut all = vec![0i16; 4 * 1920];
        for frame in all.chunks_mut(1920) {
            src.fill_frame(frame).unwrap();
        }
        let max_step = |ch: usize| {
            all.iter()
                .skip(ch)
                .step_by(2)
                .collect::<Vec<_>>()
                .windows(2)
                .map(|w| i32::from(*w[1]) - i32::from(*w[0]))
                .map(i32::abs)
                .max()
                .expect("non-empty")
        };
        // 660 Hz at 0.3 amplitude: max |Δ| ≈ 0.3 * 32767 * 2π * 660/48000 ≈ 850.
        assert!(max_step(0) < 1000, "left discontinuity: {}", max_step(0));
        assert!(max_step(1) < 1000, "right discontinuity: {}", max_step(1));
    }

    #[test]
    fn amplitude_stays_in_bounds() {
        let mut src = SyntheticSource::new(48_000, 1, 960);
        let mut buf = vec![0i16; 960];
        for _ in 0..50 {
            src.fill_frame(&mut buf).unwrap();
        }
        let peak = buf.iter().map(|s| s.unsigned_abs()).max().expect("non-empty");
        assert!(peak > 5000, "tone is audible: {peak}");
        assert!(peak <= 10_000, "0.3 amplitude respected: {peak}");
    }
}
