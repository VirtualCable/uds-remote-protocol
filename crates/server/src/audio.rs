//! Audio capture abstraction (design doc §9).
//!
//! WASAPI loopback capture is deliberately NOT in this chunk: the
//! [`AudioSource`] trait is the seam for it (a future `WasapiLoopbackSource`
//! behind the `windows-capture` feature). For now the only implementation is
//! [`SyntheticSource`], a two-tone generator that lets the whole audio path
//! (Opus encode → datagram → jitter buffer → PLC → playback) be exercised
//! end to end on any platform.

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

/// Synthetic audio source: a two-tone sine (440 Hz left, 660 Hz right; 440 Hz
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
