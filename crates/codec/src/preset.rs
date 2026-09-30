//! Named codec presets (design doc §8.4).
//!
//! A codec without its parameters is only half the story (VP9 with libvpx
//! defaults gave 3.6 fps; tuned for realtime it gives 50+). Parameters are
//! not independent knobs: `cpu_used`, bitrate and friends form coherent
//! combinations — "more CPU, less bandwidth", "less latency, more CPU" — so
//! the catalog is a fixed set of named `(codec, params)` tuples.
//!
//! The congestion controller (§12) never touches loose knobs: it adjusts the
//! bitrate *within* the active preset (`set_bitrate`) and only jumps to
//! another preset on large regime changes.

use crate::{RawDecoder, RawEncoder, VideoCodecType, VideoDecoder, VideoEncoder};

/// Tunable encoder parameters of a preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecParams {
    /// Target bitrate in kbps.
    pub bitrate_kbps: u32,
    /// libvpx speed/quality knob (VP9: 0-8 realtime range; VP8: -16..16).
    /// `None` leaves the backend default.
    pub cpu_used: Option<i32>,
}

/// A named preset: a coherent `(codec, params)` unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecPreset {
    /// Stable CLI/config name (e.g. `"vp9-balanced"`).
    pub name: &'static str,
    /// The codec; the only part the decoder peer needs to know.
    pub codec: VideoCodecType,
    /// Encoder-side parameters.
    pub params: CodecParams,
}

/// Default target bitrate when a preset does not say otherwise (2 Mbps).
///
/// Bitrate sweep at 1080p (2026-09-30, `bitrate_sweep_1080p`): VP9 reaches
/// ~44 dB PSNR at 2 Mbps and ~42 dB at 1 Mbps on the synthetic pattern, so
/// 4 Mbps is overkill for VDI (bandwidth is a *server* resource there).
pub const DEFAULT_BITRATE_KBPS: u32 = 2000;

const fn p(name: &'static str, codec: VideoCodecType, cpu_used: Option<i32>) -> CodecPreset {
    CodecPreset {
        name,
        codec,
        params: CodecParams {
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
            cpu_used,
        },
    }
}

const fn with_kbps(mut preset: CodecPreset, kbps: u32) -> CodecPreset {
    preset.params.bitrate_kbps = kbps;
    preset
}

/// The preset catalog. Selection is by name (CLI/config); the wire only
/// carries the [`VideoCodecType`] wire ID.
///
/// Bitrate defaults come from the 1080p sweep (see `DEFAULT_BITRATE_KBPS`):
/// VP9 tracks its target; VP8 has a ~1.7 Mbps floor and a ~2.3 Mbps ceiling
/// in realtime mode at 1080p, so VP8 presets never go below 2 Mbps (its
/// niche is saving CPU, not bandwidth).
pub const PRESETS: &[CodecPreset] = &[
    p("raw", VideoCodecType::Raw, None),
    // VP9: cpu_used 0-8, higher = faster/lower ratio. NOTE: cpu_used is the
    // CPU knob, NOT the bandwidth knob — cpu_used 4 costs 250% of a core at
    // 1080p30 while cpu_used 6 costs ~108% (2026-09-30 sweep), so
    // bandwidth-saver keeps cpu 6 and saves via the bitrate.
    with_kbps(p("vp9-low-latency", VideoCodecType::Vp9, Some(8)), 2500),
    p("vp9-balanced", VideoCodecType::Vp9, Some(6)),
    with_kbps(p("vp9-bandwidth-saver", VideoCodecType::Vp9, Some(6)), 1000),
    // VP8: cpu_used -16..16, lower = slower/better quality.
    with_kbps(p("vp8-low-latency", VideoCodecType::Vp8, Some(-6)), 2500),
    with_kbps(p("vp8-balanced", VideoCodecType::Vp8, Some(-10)), 2500),
    with_kbps(p("vp8-quality", VideoCodecType::Vp8, Some(-14)), 4000),
];

impl CodecPreset {
    /// Finds a preset by catalog name.
    #[must_use]
    pub fn by_name(name: &str) -> Option<Self> {
        PRESETS.iter().copied().find(|p| p.name == name)
    }

    /// The same preset with a different target bitrate (e.g. `--bitrate`).
    #[must_use]
    pub fn with_bitrate(mut self, kbps: u32) -> Self {
        self.params.bitrate_kbps = kbps;
        self
    }

    /// Builds the encoder for this preset; fails if the codec feature is not
    /// compiled in.
    pub fn build_encoder(
        &self,
        width: u32,
        height: u32,
    ) -> crate::Result<Box<dyn VideoEncoder>> {
        let kbps = self.params.bitrate_kbps;
        let cpu = self.params.cpu_used;
        // Used only by the feature-gated arms below; without codec features
        // they would be flagged as unused.
        let _ = (width, height, kbps, cpu);
        match self.codec {
            VideoCodecType::Raw => Ok(Box::new(RawEncoder::new())),
            #[cfg(feature = "codec-vp9")]
            VideoCodecType::Vp9 => Ok(Box::new(match cpu {
                Some(c) => crate::Vp9Encoder::with_cpu_used(width, height, kbps, c)?,
                None => crate::Vp9Encoder::new(width, height, kbps)?,
            })),
            #[cfg(feature = "codec-vp8")]
            VideoCodecType::Vp8 => Ok(Box::new(match cpu {
                Some(c) => crate::Vp8Encoder::with_cpu_used(width, height, kbps, c)?,
                None => crate::Vp8Encoder::new(width, height, kbps)?,
            })),
            other => Err(crate::CodecError::Unsupported(match other {
                VideoCodecType::Vp9 => "vp9 (build with --features codec/codec-vp9)",
                VideoCodecType::Vp8 => "vp8 (build with --features codec/codec-vp8)",
                VideoCodecType::H264 => "h264 (not implemented yet)",
                VideoCodecType::Av1 => "av1 (not implemented yet)",
                VideoCodecType::Raw => unreachable!("raw handled above"),
            })),
        }
    }
}

/// Builds the decoder for a negotiated codec; fails if the codec feature is
/// not compiled in. Unknown/future codecs are rejected here too.
pub fn build_decoder(codec: VideoCodecType) -> crate::Result<Box<dyn VideoDecoder>> {
    match codec {
        VideoCodecType::Raw => Ok(Box::new(RawDecoder::new())),
        #[cfg(feature = "codec-vp9")]
        VideoCodecType::Vp9 => Ok(Box::new(crate::Vp9Decoder::new()?)),
        #[cfg(feature = "codec-vp8")]
        VideoCodecType::Vp8 => Ok(Box::new(crate::Vp8Decoder::new()?)),
        other => Err(crate::CodecError::Unsupported(match other {
            VideoCodecType::Vp9 => "vp9 (build with --features codec/codec-vp9)",
            VideoCodecType::Vp8 => "vp8 (build with --features codec/codec-vp8)",
            VideoCodecType::H264 => "h264 (not implemented yet)",
            VideoCodecType::Av1 => "av1 (not implemented yet)",
            VideoCodecType::Raw => unreachable!("raw handled above"),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_names_are_unique_and_findable() {
        for preset in PRESETS {
            assert_eq!(CodecPreset::by_name(preset.name), Some(*preset));
        }
        assert_eq!(CodecPreset::by_name("nope"), None);
    }

    #[test]
    fn every_codec_has_a_preset_and_defaults() {
        assert!(PRESETS.iter().any(|p| p.codec == VideoCodecType::Vp9));
        assert!(PRESETS.iter().any(|p| p.codec == VideoCodecType::Vp8));
        assert!(PRESETS.iter().any(|p| p.codec == VideoCodecType::Raw));
        for preset in PRESETS {
            assert!(preset.params.bitrate_kbps > 0);
        }
    }

    #[test]
    fn with_bitrate_overrides_only_bitrate() {
        let p = CodecPreset::by_name("vp9-balanced").unwrap().with_bitrate(8000);
        assert_eq!(p.params.bitrate_kbps, 8000);
        assert_eq!(p.codec, VideoCodecType::Vp9);
        assert_eq!(p.params.cpu_used, Some(6));
    }

    #[test]
    fn raw_preset_builds_encoder_and_decoder() {
        let p = CodecPreset::by_name("raw").unwrap();
        let mut enc = p.build_encoder(64, 64).unwrap();
        assert_eq!(enc.codec_type(), VideoCodecType::Raw);
        let mut frame_data = vec![0u8; 64 * 64 * 4];
        frame_data[0] = 7;
        let frame = crate::VideoFrame {
            width: 64,
            height: 64,
            stride: 64 * 4,
            data: &frame_data,
        };
        let packet = enc.encode(&frame).unwrap();
        let mut dec = build_decoder(VideoCodecType::Raw).unwrap();
        let decoded = dec.decode(&packet).unwrap();
        assert_eq!((decoded.width, decoded.height), (64, 64));
        assert_eq!(decoded.data[0], 7);
    }

    #[cfg(feature = "codec-vp9")]
    #[test]
    fn vp9_presets_build_encoders() {
        for name in ["vp9-low-latency", "vp9-balanced", "vp9-bandwidth-saver"] {
            let p = CodecPreset::by_name(name).unwrap();
            let enc = p.build_encoder(320, 240).unwrap();
            assert_eq!(enc.codec_type(), VideoCodecType::Vp9);
        }
        assert!(build_decoder(VideoCodecType::Vp9).is_ok());
    }

    #[cfg(feature = "codec-vp8")]
    #[test]
    fn vp8_presets_build_encoders() {
        for name in ["vp8-low-latency", "vp8-balanced", "vp8-quality"] {
            let p = CodecPreset::by_name(name).unwrap();
            let enc = p.build_encoder(320, 240).unwrap();
            assert_eq!(enc.codec_type(), VideoCodecType::Vp8);
        }
        assert!(build_decoder(VideoCodecType::Vp8).is_ok());
    }
}
