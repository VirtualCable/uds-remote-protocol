// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Audio format adaptation between device capture/playback shapes and the
//! wire format (design doc §9).
//!
//! Pure DSP helpers with no platform dependencies, so the whole adaptation
//! chain is unit-testable on any host:
//!
//! ```text
//! device packet (f32/i16, device channels, device rate)
//!   ─▶ i16 conversion (clamp)          [`f32_to_i16`]
//!   ─▶ channel remix to the wire shape [`remix_channels`]
//!   ─▶ resample to the wire rate       [`LinearResampler`]
//!   ─▶ bounded ring for frame pulls    [`CaptureRing`]
//! ```
//!
//! [`DevicePipeline`] chains the four stages for one captured packet; both
//! capture sides use it (server WASAPI loopback, client cpal microphone).
//! The resampler is a PoC-grade linear interpolator (§9.1 lists a proper
//! resampler as a refinement): transparent enough for 44.1↔48 kHz speech and
//! desktop audio, with no anti-aliasing on the downsample path.

use std::collections::VecDeque;

/// Converts one f32 sample to i16 with clamping: `-1.0` maps to `i16::MIN`,
/// `+1.0` (and anything above, or NaN) saturates instead of wrapping.
#[must_use]
pub fn f32_to_i16(v: f32) -> i16 {
    // Clamped before the cast, so the conversion cannot wrap; NaN casts to
    // 0, which is the right fallback for a broken sample.
    #[allow(clippy::cast_possible_truncation)]
    let s = (f64::from(v) * 32768.0).clamp(-32768.0, 32767.0) as i16;
    s
}

/// Remaps interleaved PCM from `in_channels` to `out_channels` (1 or 2):
/// mono is duplicated to stereo, stereo folds to its left channel for mono,
/// and >2 channels keep only the first ones. Trailing partial frames are
/// dropped. Appends to `output`.
pub fn remix_channels(input: &[i16], in_channels: u8, out_channels: u8, output: &mut Vec<i16>) {
    let in_ch = usize::from(in_channels.max(1));
    let out_ch = usize::from(out_channels.clamp(1, 2));
    if in_ch == out_ch {
        output.extend_from_slice(input);
        return;
    }
    for frame in input.chunks_exact(in_ch) {
        for c in 0..out_ch {
            output.push(frame[c.min(in_ch - 1)]);
        }
    }
}

/// Linear interpolating resampler for interleaved i16 PCM, streaming across
/// `process` calls (one frame of history is kept for block-boundary lerp).
///
/// Positions are tracked in f64: at 48 kHz the accumulated error stays under
/// one sample for centuries of continuous audio.
pub struct LinearResampler {
    /// Input frames advanced per output frame (`in_rate / out_rate`).
    step: f64,
    channels: usize,
    /// Global fractional input-frame position of the next output frame.
    pos: f64,
    /// Global count of input frames consumed so far.
    consumed: u64,
    /// Last input frame of the previous block (block-boundary lerp).
    prev: Vec<i16>,
}

impl LinearResampler {
    /// Creates a resampler from `in_rate` to `out_rate` for interleaved
    /// audio with `channels` channels. For equal rates the caller should
    /// skip the resampler entirely (exact passthrough).
    #[must_use]
    pub fn new(in_rate: u32, out_rate: u32, channels: u8) -> Self {
        debug_assert!(in_rate > 0 && out_rate > 0 && channels > 0);
        Self {
            step: f64::from(in_rate) / f64::from(out_rate),
            channels: usize::from(channels),
            pos: 0.0,
            consumed: 0,
            prev: vec![0; usize::from(channels)],
        }
    }

    /// Resamples one block of interleaved input, appending to `output`.
    /// A trailing partial frame (not a multiple of the channel count) is
    /// dropped.
    // Frame positions and counts fit f64 with full precision for any real
    // session length (2^53 frames ≈ 5900 years at 48 kHz).
    #[allow(clippy::cast_precision_loss)]
    pub fn process(&mut self, input: &[i16], output: &mut Vec<i16>) {
        let ch = self.channels;
        let frames = input.len() / ch;
        if frames == 0 {
            return;
        }
        debug_assert_eq!(input.len() % ch, 0, "trailing partial frame dropped");
        loop {
            // Position of the next output frame relative to this block's
            // first frame. Invariant: rel >= -1 at block start (pos never
            // falls more than one frame behind the stream), so at most the
            // first lerp reads the previous block's last frame.
            let rel = self.pos - self.consumed as f64;
            if rel >= (frames - 1) as f64 {
                break;
            }
            let base = rel.floor();
            let frac = rel - base;
            // `rel` is in [-1, frames-1) here; use the previous block's last
            // frame only at the boundary, otherwise two in-block frames.
            for c in 0..ch {
                // Block-local interpolation over two real frames.
                let lerp = |a: i16, b: i16| {
                    let v = f64::from(b).mul_add(frac, f64::from(a) * (1.0 - frac));
                    // Lerp of two i16 stays in range; the cast only rounds.
                    #[allow(clippy::cast_possible_truncation)]
                    let s = v.round() as i16;
                    s
                };
                if base < 0.0 {
                    output.push(lerp(self.prev[c], input[c]));
                } else {
                    // `base` is in 0..=frames-2, so both casts fit usize.
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let i = base as usize;
                    output.push(lerp(input[i * ch + c], input[(i + 1) * ch + c]));
                }
            }
            self.pos += self.step;
        }
        self.prev.copy_from_slice(&input[(frames - 1) * ch..frames * ch]);
        self.consumed += frames as u64;
    }
}

/// Bounded sample ring between a capture thread and `fill_frame`: on
/// overflow the oldest samples are dropped (fresh audio matters more than
/// continuity); on underrun [`CaptureRing::fill`] emits silence.
#[derive(Debug)]
pub struct CaptureRing {
    samples: VecDeque<i16>,
    max_samples: usize,
    /// Samples dropped by overflow (cumulative).
    dropped_samples: u64,
}

impl CaptureRing {
    /// Creates a ring holding at most `max_samples` interleaved samples.
    #[must_use]
    pub fn new(max_samples: usize) -> Self {
        Self {
            samples: VecDeque::new(),
            max_samples,
            dropped_samples: 0,
        }
    }

    /// Buffered length in samples.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// True when no samples are buffered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Samples dropped by overflow so far.
    #[must_use]
    pub fn dropped_samples(&self) -> u64 {
        self.dropped_samples
    }

    /// Appends captured samples, dropping the oldest on overflow.
    pub fn push(&mut self, data: &[i16]) {
        self.samples.extend(data);
        if self.samples.len() > self.max_samples {
            let drop = self.samples.len() - self.max_samples;
            self.samples.drain(..drop);
            self.dropped_samples += drop as u64;
        }
    }

    /// Fills `out` with buffered samples, zero-filling the shortfall.
    /// Returns the number of samples that had to be synthesized (underrun
    /// accounting for the caller).
    pub fn fill(&mut self, out: &mut [i16]) -> usize {
        let take = out.len().min(self.samples.len());
        for (dst, src) in out.iter_mut().zip(self.samples.drain(..take)) {
            *dst = src;
        }
        out[take..].fill(0);
        out.len() - take
    }
}

/// Device sample formats understood by [`DevicePipeline`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// 32-bit float in [-1.0, 1.0] (the usual WASAPI mix format).
    F32,
    /// Signed 16-bit PCM.
    I16,
}

impl SampleFormat {
    /// Bytes per sample of this format.
    #[must_use]
    pub const fn bytes(self) -> usize {
        match self {
            Self::F32 => 4,
            Self::I16 => 2,
        }
    }
}

/// The full capture-side adaptation chain for one device: format conversion,
/// channel remix and resampling from the device shape to the wire shape.
/// Owns its scratch buffers; feed it one device packet per call.
pub struct DevicePipeline {
    format: SampleFormat,
    in_channels: u8,
    out_channels: u8,
    /// Device bytes per frame (one sample per channel).
    block_align: usize,
    resampler: Option<LinearResampler>,
    /// Converted i16 at the device channel count.
    converted: Vec<i16>,
    /// Remixed to the wire channel count, still at the device rate.
    remixed: Vec<i16>,
    /// Resampled to the wire rate and channel count.
    resampled: Vec<i16>,
}

impl DevicePipeline {
    /// Creates the adaptation chain from a device shape to the wire shape
    /// (`out_channels` is clamped to 1..=2).
    pub fn new(
        format: SampleFormat,
        in_channels: u8,
        in_rate: u32,
        out_channels: u8,
        out_rate: u32,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(in_channels > 0, "device reports zero channels");
        anyhow::ensure!(in_rate >= 8000, "implausible device sample rate");
        anyhow::ensure!(out_rate >= 8000, "implausible output sample rate");
        let resampler =
            (in_rate != out_rate).then(|| LinearResampler::new(in_rate, out_rate, out_channels));
        Ok(Self {
            format,
            in_channels,
            out_channels,
            block_align: format.bytes() * usize::from(in_channels),
            resampler,
            converted: Vec::new(),
            remixed: Vec::new(),
            resampled: Vec::new(),
        })
    }

    /// Feeds one device packet: `data` holds `frames * block_align` bytes in
    /// the device format (a shorter buffer clamps the frame count), unless
    /// `silent` marks the whole packet as digital silence (its bytes are
    /// then undefined and ignored, but the frame count is trusted). The
    /// converted samples land in `ring`.
    pub fn push_packet(&mut self, data: &[u8], frames: usize, silent: bool, ring: &mut CaptureRing) {
        let frames = if silent {
            frames
        } else {
            frames.min(data.len() / self.block_align)
        };
        if frames == 0 {
            return;
        }
        let in_ch = usize::from(self.in_channels);
        self.converted.clear();
        if silent {
            self.converted.resize(frames * in_ch, 0);
        } else {
            let bytes = &data[..frames * self.block_align];
            match self.format {
                SampleFormat::F32 => self.converted.extend(
                    bytes
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|b| f32_to_i16(f32::from_le_bytes(*b))),
                ),
                SampleFormat::I16 => self.converted.extend(
                    bytes
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| i16::from_le_bytes(*b)),
                ),
            }
        }
        self.remixed.clear();
        remix_channels(&self.converted, self.in_channels, self.out_channels, &mut self.remixed);
        if let Some(resampler) = &mut self.resampler {
            self.resampled.clear();
            resampler.process(&self.remixed, &mut self.resampled);
            ring.push(&self.resampled);
        } else {
            ring.push(&self.remixed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_conversion_clamps() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(0.5), 16384);
        assert_eq!(f32_to_i16(1.0), i16::MAX);
        assert_eq!(f32_to_i16(-1.0), i16::MIN);
        assert_eq!(f32_to_i16(2.5), i16::MAX, "overrange clamps, no wrap");
        assert_eq!(f32_to_i16(-2.5), i16::MIN);
        assert_eq!(f32_to_i16(f32::NAN), 0, "NaN degrades to silence");
    }

    #[test]
    fn remix_mono_to_stereo_duplicates() {
        let mut out = Vec::new();
        remix_channels(&[1, 2, 3], 1, 2, &mut out);
        assert_eq!(out, [1, 1, 2, 2, 3, 3]);
    }

    #[test]
    fn remix_stereo_to_mono_takes_left() {
        let mut out = Vec::new();
        remix_channels(&[10, 20, 30, 40], 2, 1, &mut out);
        assert_eq!(out, [10, 30]);
    }

    #[test]
    fn remix_multichannel_keeps_first_two() {
        let mut out = Vec::new();
        remix_channels(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], 6, 2, &mut out);
        assert_eq!(out, [1, 2, 7, 8]);
    }

    #[test]
    fn remix_identity_is_a_copy() {
        let mut out = Vec::new();
        remix_channels(&[1, 2, 3, 4], 2, 2, &mut out);
        assert_eq!(out, [1, 2, 3, 4]);
    }

    /// Generates `frames` of a mono sine test tone starting at global frame
    /// `start_frame`.
    // Test sample counts are tiny; no cast can lose anything.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn sine_block(rate: u32, freq: f64, start_frame: u64, frames: usize) -> Vec<i16> {
        (0..frames as u64)
            .map(|i| {
                let t = (start_frame + i) as f64 / f64::from(rate);
                ((t * freq * 2.0 * std::f64::consts::PI).sin() * 10_000.0) as i16
            })
            .collect()
    }

    /// Counts rising zero crossings (≈ Hz over a 1 s signal).
    fn rising_zero_crossings(samples: &[i16]) -> usize {
        samples.windows(2).filter(|w| w[0] <= 0 && w[1] > 0).count()
    }

    /// 44.1 kHz → 48 kHz of a 440 Hz sine must yield the same tone.
    #[test]
    fn resample_upsample_preserves_tone() {
        let mut rs = LinearResampler::new(44_100, 48_000, 1);
        // One second of 440 Hz in 10 ms blocks (441 frames).
        let mut out = Vec::new();
        for block in 0..100u64 {
            rs.process(&sine_block(44_100, 440.0, block * 441, 441), &mut out);
        }
        // Length: 44100 input frames → ~48000 output frames.
        assert!(out.len().abs_diff(48_000) <= 2, "got {}", out.len());
        let hz = rising_zero_crossings(&out);
        assert!(hz.abs_diff(440) <= 5, "expected ~440 Hz, measured {hz} Hz");
    }

    /// 48 kHz → 44.1 kHz (downsample path) preserves tone and length.
    #[test]
    fn resample_downsample_preserves_tone() {
        let mut rs = LinearResampler::new(48_000, 44_100, 1);
        let mut out = Vec::new();
        for block in 0..100u64 {
            rs.process(&sine_block(48_000, 440.0, block * 480, 480), &mut out);
        }
        assert!(out.len().abs_diff(44_100) <= 2, "got {}", out.len());
        let hz = rising_zero_crossings(&out);
        assert!(hz.abs_diff(440) <= 5, "expected ~440 Hz, measured {hz} Hz");
    }

    /// Blocks must join seamlessly: the lerp across a block boundary uses
    /// the previous block's last frame.
    // The sawtooth values are bounded by construction (|v| < 1000).
    #[allow(clippy::cast_possible_truncation)]
    #[test]
    fn resample_is_continuous_across_blocks() {
        let mut whole = LinearResampler::new(44_100, 48_000, 1);
        let mut split = LinearResampler::new(44_100, 48_000, 1);
        let input: Vec<i16> = (0..2000i32)
            .map(|i| (i * 7 % 2000 - 1000) as i16)
            .collect();
        let mut a = Vec::new();
        whole.process(&input, &mut a);
        let mut b = Vec::new();
        for block in input.chunks(441) {
            split.process(block, &mut b);
        }
        // Blocked processing may lag by at most one output frame at the end.
        assert!(a.len().abs_diff(b.len()) <= 1);
        let common = a.len().min(b.len());
        assert_eq!(a[..common], b[..common]);
    }

    #[test]
    fn ring_overflow_drops_oldest() {
        let mut ring = CaptureRing::new(4);
        ring.push(&[1, 2, 3]);
        ring.push(&[4, 5, 6]);
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.dropped_samples(), 2);
        let mut out = [0i16; 4];
        assert_eq!(ring.fill(&mut out), 0);
        assert_eq!(out, [3, 4, 5, 6], "oldest (1, 2) dropped");
    }

    #[test]
    fn ring_underrun_fills_silence() {
        let mut ring = CaptureRing::new(16);
        ring.push(&[1, 2]);
        let mut out = [9i16; 4];
        assert_eq!(ring.fill(&mut out), 2);
        assert_eq!(out, [1, 2, 0, 0]);
        assert_eq!(ring.len(), 0);
    }

    fn f32_bytes(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    /// f32 stereo 48 kHz → 48 kHz stereo wire: convert, no remix, no
    /// resampling.
    #[test]
    fn pipeline_passthrough_f32_stereo() {
        let mut pipe = DevicePipeline::new(SampleFormat::F32, 2, 48_000, 2, 48_000).unwrap();
        let mut ring = CaptureRing::new(1024);
        let packet = f32_bytes(&[0.5, -0.5, 1.0, -1.0]);
        pipe.push_packet(&packet, 2, false, &mut ring);
        let mut out = [0i16; 4];
        assert_eq!(ring.fill(&mut out), 0);
        assert_eq!(out, [16384, -16384, i16::MAX, i16::MIN]);
    }

    /// i16 mono 48 kHz → stereo: duplication through the pipeline.
    #[test]
    fn pipeline_i16_mono_to_stereo() {
        let mut pipe = DevicePipeline::new(SampleFormat::I16, 1, 48_000, 2, 48_000).unwrap();
        let mut ring = CaptureRing::new(1024);
        let packet: Vec<u8> = [100i16, 200].iter().flat_map(|s| s.to_le_bytes()).collect();
        pipe.push_packet(&packet, 2, false, &mut ring);
        let mut out = [0i16; 4];
        assert_eq!(ring.fill(&mut out), 0);
        assert_eq!(out, [100, 100, 200, 200]);
    }

    /// 6-channel f32 device → stereo wire keeps the first two channels.
    #[test]
    fn pipeline_multichannel_keeps_first_two() {
        let mut pipe = DevicePipeline::new(SampleFormat::F32, 6, 48_000, 2, 48_000).unwrap();
        let mut ring = CaptureRing::new(1024);
        let packet = f32_bytes(&[0.25, -0.25, 0.9, 0.9, 0.9, 0.9]);
        pipe.push_packet(&packet, 1, false, &mut ring);
        let mut out = [0i16; 2];
        assert_eq!(ring.fill(&mut out), 0);
        assert_eq!(out, [8192, -8192]);
    }

    /// A silent-flagged packet carries undefined bytes: the pipeline must
    /// emit exactly `frames` of zeros (and keep the resampler continuous).
    #[test]
    fn pipeline_silent_packet_is_zeros() {
        let mut pipe = DevicePipeline::new(SampleFormat::F32, 2, 48_000, 2, 48_000).unwrap();
        let mut ring = CaptureRing::new(1024);
        pipe.push_packet(&[0xAA, 0xBB], 10, true, &mut ring);
        let mut out = [1i16; 20];
        assert_eq!(ring.fill(&mut out), 0);
        assert_eq!(out, [0i16; 20]);
    }

    /// 44.1 kHz device, 48 kHz wire: 441 input frames per packet yield
    /// ~960 output samples per packet after enough packets.
    // Test tone amplitudes fit f32 trivially; no cast can lose anything.
    #[allow(clippy::cast_possible_truncation)]
    #[test]
    fn pipeline_resamples_to_wire_rate() {
        let mut pipe = DevicePipeline::new(SampleFormat::F32, 2, 44_100, 2, 48_000).unwrap();
        let mut ring = CaptureRing::new(1 << 20);
        let tone: Vec<f32> = (0..441i32)
            .map(|i| {
                (f64::from(i) * 440.0 * 2.0 * std::f64::consts::PI / 44_100.0).sin() as f32 * 0.3
            })
            .flat_map(|s| [s, s])
            .collect();
        let packet = f32_bytes(&tone);
        for _ in 0..10 {
            pipe.push_packet(&packet, 441, false, &mut ring);
        }
        // 4410 device frames ≈ 4800 wire frames = 9600 stereo samples.
        assert!(ring.len().abs_diff(9600) <= 4, "got {}", ring.len());
    }

    /// A short buffer clamps the frame count instead of reading past it.
    #[test]
    fn pipeline_clamps_short_buffers() {
        let mut pipe = DevicePipeline::new(SampleFormat::F32, 2, 48_000, 2, 48_000).unwrap();
        let mut ring = CaptureRing::new(1024);
        // Claims 4 frames but holds only 2 (16 bytes).
        let packet = f32_bytes(&[0.5, 0.5, -0.5, -0.5]);
        pipe.push_packet(&packet, 4, false, &mut ring);
        assert_eq!(ring.len(), 4);
    }
}
