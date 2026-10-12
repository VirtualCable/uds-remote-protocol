//! Pure, host-testable driver logic (ring buffers, format tables, topology
//! descriptors). No kernel APIs in here: this module must compile and be
//! tested on any platform (cargo test runs on Linux CI too).

/// PCM format the virtual microphone accepts from the session agent.
/// Matches the protocol's audio-in wire format (48 kHz mono s16).
pub const MIC_FORMAT: PcmFormat = PcmFormat {
    sample_rate: 48_000,
    channels: 1,
    bits_per_sample: 16,
};

/// PCM format the virtual speaker renders (48 kHz stereo s16), matching the
/// audio-out pipeline.
pub const SPEAKER_FORMAT: PcmFormat = PcmFormat {
    sample_rate: 48_000,
    channels: 2,
    bits_per_sample: 16,
};

/// A PCM format description (interleaved samples).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    /// Samples per second.
    pub sample_rate: u32,
    /// Interleaved channel count.
    pub channels: u16,
    /// Bits per sample (16 = s16).
    pub bits_per_sample: u16,
}

impl PcmFormat {
    /// Bytes per sample frame (all channels).
    #[must_use]
    pub const fn frame_bytes(&self) -> usize {
        (self.channels as usize) * (self.bits_per_sample as usize) / 8
    }

    /// Bytes per millisecond of audio.
    #[must_use]
    pub const fn bytes_per_ms(&self) -> usize {
        self.frame_bytes() * (self.sample_rate as usize) / 1000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_format_matches_wire_format() {
        assert_eq!(MIC_FORMAT.frame_bytes(), 2);
        assert_eq!(MIC_FORMAT.bytes_per_ms(), 96); // 48k mono s16: 96 B/ms
    }

    #[test]
    fn speaker_format_is_stereo_48k() {
        assert_eq!(SPEAKER_FORMAT.frame_bytes(), 4);
        assert_eq!(SPEAKER_FORMAT.bytes_per_ms(), 192);
    }

    #[test]
    fn twenty_ms_mic_frame_is_1920_bytes() {
        // One Opus 20 ms frame at 48 kHz mono s16 = 960 samples = 1920 bytes.
        assert_eq!(MIC_FORMAT.bytes_per_ms() * 20, 1920);
    }
}
