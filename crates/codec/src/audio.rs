//! Audio codec traits and presets (design doc §9).
//!
//! Mirror of the video abstraction (§8): the protocol negotiates a codec via
//! capabilities and the core only sees the [`AudioEncoder`]/[`AudioDecoder`]
//! traits. There is no audio codec ladder: Opus covers the whole useful range
//! (6-96 kbps) and the congestion controller just adjusts the bitrate within
//! the active preset (§9, decision 2026-10-11).
//!
//! PCM is always interleaved signed 16-bit, exactly one frame per call
//! (`frame_samples * channels` samples). Loss is handled with the codec's
//! native packet loss concealment (PLC): the decoder is asked for a frame
//! with `None` instead of a packet.

use crate::error::{CodecError, Result};

/// Audio codec identifier, with stable wire IDs for capability negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodecType {
    /// Uncompressed interleaved i16 PCM. Reserved for tests; no
    /// implementation is provided and it is never negotiated in production.
    Raw,
    /// Opus (RFC 6716; royalty-free), the production codec.
    Opus,
}

impl AudioCodecType {
    /// Stable numeric ID for the wire (capability negotiation).
    #[must_use]
    pub fn wire_id(self) -> u16 {
        match self {
            Self::Raw => 0,
            Self::Opus => 1,
        }
    }

    /// Inverse of [`AudioCodecType::wire_id`]; `None` for unknown IDs.
    #[must_use]
    pub fn from_wire_id(id: u16) -> Option<Self> {
        match id {
            0 => Some(Self::Raw),
            1 => Some(Self::Opus),
            _ => None,
        }
    }
}

/// Audio encoder: one frame of interleaved i16 PCM in → packet bytes out.
///
/// The encoder is configured at construction (sample rate, channels, frame
/// duration); the only runtime knob is the bitrate, for the congestion
/// controller (§12). `Send`: the server runs encoding on a dedicated audio
/// thread.
pub trait AudioEncoder: Send {
    /// Encodes exactly one frame (`frame_samples() * channels()` interleaved
    /// samples), appending the packet to `out` (a caller-owned scratch
    /// buffer, so the hot path does not allocate — §12.6). Returns the
    /// number of bytes appended.
    fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> Result<usize>;

    /// Changes the target bitrate (hot, per-packet granularity where the
    /// backend allows it) for the adaptive bitrate controller (§12).
    fn set_bitrate_kbps(&mut self, kbps: u32) -> Result<()>;

    /// Switches in-band FEC on/off (hot) for the congestion controller
    /// (§12.5): on when the link shows real loss, off when it subsides.
    /// Encoder-local: Opus frames are self-describing, the decoder needs no
    /// announcement and simply uses the redundancy when it is present.
    fn set_fec(&mut self, enabled: bool) -> Result<()>;

    /// Codec implemented by this encoder.
    fn codec_type(&self) -> AudioCodecType;

    /// Sample rate in Hz (e.g. 48000).
    fn sample_rate(&self) -> u32;

    /// Number of channels (1 = mono, 2 = stereo).
    fn channels(&self) -> u8;

    /// Samples per channel in one frame (e.g. 960 for 20 ms @ 48 kHz).
    fn frame_samples(&self) -> usize;
}

/// Audio decoder: packet in → one frame of interleaved i16 PCM out.
///
/// `Send`: the client decodes on a dedicated audio pump thread.
pub trait AudioDecoder: Send {
    /// Decodes one packet into `out` (interleaved i16, room for at least
    /// `frame_samples() * channels()` samples). `None` means the packet was
    /// lost: the decoder synthesizes a plausible frame with its native PLC
    /// (§9) instead of returning silence. Returns the samples written
    /// *per channel*.
    fn decode(&mut self, packet: Option<&[u8]>, out: &mut [i16]) -> Result<usize>;

    /// Codec implemented by this decoder.
    fn codec_type(&self) -> AudioCodecType;

    /// Sample rate in Hz (e.g. 48000).
    fn sample_rate(&self) -> u32;

    /// Number of channels (1 = mono, 2 = stereo).
    fn channels(&self) -> u8;

    /// Samples per channel in one frame (e.g. 960 for 20 ms @ 48 kHz).
    fn frame_samples(&self) -> usize;
}

/// An audio preset: a coherent `(codec, params)` unit (§9), the audio
/// counterpart of the video `CodecPreset` (§8.4).
///
/// On the wire only the `AudioConfig` control message travels
/// (`codec`, `sample_rate`, `channels`, `bitrate_kbps`, `frame_ms`, `fec`);
/// `complexity` is a pure encoder-side knob and is never announced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPreset {
    /// The codec; the only part the decoder peer needs to know.
    pub codec: AudioCodecType,
    /// Sample rate in Hz (Opus supports 8/12/16/24/48 kHz).
    pub sample_rate: u32,
    /// Channels: 1 (mono) or 2 (stereo).
    pub channels: u8,
    /// Target bitrate in kbps (Opus useful range: 6-96).
    pub bitrate_kbps: u32,
    /// Encoder computational complexity (0-10, higher = more CPU/better
    /// quality). Encoder-side only; not sent on the wire.
    pub complexity: u8,
    /// Frame duration in milliseconds (Opus: 10, 20, 40 or 60).
    pub frame_ms: u8,
    /// In-band FEC: each packet carries a redundant copy of the previous
    /// frame (+10-20 % bitrate). The congestion controller enables it only
    /// when it detects loss (§9).
    pub fec: bool,
}

impl AudioPreset {
    /// Samples per channel in one frame of this preset.
    #[must_use]
    pub fn frame_samples(&self) -> usize {
        (self.sample_rate as usize) * (self.frame_ms as usize) / 1000
    }

    /// The default preset: `opus-music` (48 kHz stereo, 64 kbps).
    #[must_use]
    pub fn default_preset() -> Self {
        Self::by_name("opus-music").expect("opus-music is in the catalog")
    }

    /// Finds a preset by catalog name.
    #[must_use]
    pub fn by_name(name: &str) -> Option<Self> {
        AUDIO_PRESETS
            .iter()
            .copied()
            .find(|(n, _)| *n == name)
            .map(|(_, preset)| preset)
    }

    /// The same preset with a different target bitrate.
    #[must_use]
    pub fn with_bitrate(mut self, kbps: u32) -> Self {
        self.bitrate_kbps = kbps;
        self
    }
}

impl Default for AudioPreset {
    fn default() -> Self {
        Self::default_preset()
    }
}

const fn opus(sample_rate: u32, channels: u8, kbps: u32, fec: bool) -> AudioPreset {
    AudioPreset {
        codec: AudioCodecType::Opus,
        sample_rate,
        channels,
        bitrate_kbps: kbps,
        complexity: 5,
        frame_ms: 20,
        fec,
    }
}

/// The audio preset catalog. Selection is by name (CLI/config); the wire
/// only carries the fields of `AudioConfig`.
///
/// - `opus-music` (default): full-band stereo for desktop audio.
/// - `opus-voice`: mono with FEC, for voice-centric deployments.
/// - `opus-lowbw`: harsh links; stereo kept (desktop audio is stereo) but
///   with FEC and the lowest bitrate that still sounds acceptable.
pub const AUDIO_PRESETS: &[(&str, AudioPreset)] = &[
    ("opus-music", opus(48_000, 2, 64, false)),
    ("opus-voice", opus(48_000, 1, 32, true)),
    ("opus-lowbw", opus(48_000, 2, 24, true)),
];

/// Builds the encoder for a preset; fails if the codec feature is not
/// compiled in or the preset is not encodable (e.g. `Raw`).
pub fn build_audio_encoder(preset: &AudioPreset) -> Result<Box<dyn AudioEncoder>> {
    match preset.codec {
        #[cfg(feature = "codec-opus")]
        AudioCodecType::Opus => Ok(Box::new(crate::OpusEncoder::new(preset)?)),
        other => Err(CodecError::Unsupported(match other {
            #[cfg(not(feature = "codec-opus"))]
            AudioCodecType::Opus => "opus (build with --features codec/codec-opus)",
            AudioCodecType::Raw => "raw audio (reserved wire id, no implementation)",
            #[cfg(feature = "codec-opus")]
            AudioCodecType::Opus => unreachable!("opus handled above"),
        })),
    }
}

/// Builds the decoder for a negotiated preset; fails if the codec feature is
/// not compiled in. Unknown/future codecs are rejected here too.
pub fn build_audio_decoder(preset: &AudioPreset) -> Result<Box<dyn AudioDecoder>> {
    match preset.codec {
        #[cfg(feature = "codec-opus")]
        AudioCodecType::Opus => Ok(Box::new(crate::OpusDecoder::new(
            preset.sample_rate,
            preset.channels,
            preset.frame_ms,
        )?)),
        other => Err(CodecError::Unsupported(match other {
            #[cfg(not(feature = "codec-opus"))]
            AudioCodecType::Opus => "opus (build with --features codec/codec-opus)",
            AudioCodecType::Raw => "raw audio (reserved wire id, no implementation)",
            #[cfg(feature = "codec-opus")]
            AudioCodecType::Opus => unreachable!("opus handled above"),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_id_roundtrip() {
        for codec in [AudioCodecType::Raw, AudioCodecType::Opus] {
            assert_eq!(AudioCodecType::from_wire_id(codec.wire_id()), Some(codec));
        }
        assert_eq!(AudioCodecType::from_wire_id(2), None);
        assert_eq!(AudioCodecType::from_wire_id(u16::MAX), None);
    }

    #[test]
    fn catalog_names_are_unique_and_findable() {
        for (name, preset) in AUDIO_PRESETS {
            assert_eq!(AudioPreset::by_name(name), Some(*preset));
        }
        assert_eq!(AudioPreset::by_name("nope"), None);
    }

    #[test]
    fn default_is_opus_music() {
        let d = AudioPreset::default();
        assert_eq!(d.codec, AudioCodecType::Opus);
        assert_eq!(d.sample_rate, 48_000);
        assert_eq!(d.channels, 2);
        assert_eq!(d.bitrate_kbps, 64);
        assert_eq!(d.complexity, 5);
        assert_eq!(d.frame_ms, 20);
        assert!(!d.fec);
    }

    #[test]
    fn named_presets_match_their_documented_shape() {
        let voice = AudioPreset::by_name("opus-voice").unwrap();
        assert_eq!((voice.channels, voice.bitrate_kbps, voice.fec), (1, 32, true));
        let lowbw = AudioPreset::by_name("opus-lowbw").unwrap();
        assert_eq!((lowbw.channels, lowbw.bitrate_kbps, lowbw.fec), (2, 24, true));
    }

    #[test]
    fn frame_samples_matches_duration() {
        assert_eq!(AudioPreset::default().frame_samples(), 960); // 20 ms @ 48 kHz
        let p = AudioPreset {
            sample_rate: 16_000,
            frame_ms: 10,
            ..AudioPreset::default()
        };
        assert_eq!(p.frame_samples(), 160);
    }

    #[test]
    fn with_bitrate_overrides_only_bitrate() {
        let p = AudioPreset::default().with_bitrate(16);
        assert_eq!(p.bitrate_kbps, 16);
        assert_eq!(p.codec, AudioCodecType::Opus);
        assert_eq!(p.channels, 2);
    }

    #[cfg(not(feature = "codec-opus"))]
    #[test]
    fn opus_without_feature_is_unsupported() {
        let preset = AudioPreset::default();
        assert!(matches!(
            build_audio_encoder(&preset),
            Err(CodecError::Unsupported(_))
        ));
        assert!(matches!(
            build_audio_decoder(&preset),
            Err(CodecError::Unsupported(_))
        ));
    }

    #[test]
    fn raw_audio_has_no_implementation() {
        let preset = AudioPreset {
            codec: AudioCodecType::Raw,
            ..AudioPreset::default()
        };
        assert!(matches!(
            build_audio_encoder(&preset),
            Err(CodecError::Unsupported(_))
        ));
        assert!(matches!(
            build_audio_decoder(&preset),
            Err(CodecError::Unsupported(_))
        ));
    }
}
