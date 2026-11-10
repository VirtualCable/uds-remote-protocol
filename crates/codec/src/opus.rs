// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Opus audio codec (RFC 6716) via the `opus` crate, which bundles libopus
//! and builds it with `cc` — no system library needed (design doc §9).
//!
//! One self-contained Opus frame per packet: frames carry no inter-frame
//! dependencies, so a lost datagram never breaks the stream and is masked by
//! the decoder's native PLC (packet loss concealment).
//!
//! Note: the `opus` 0.3 API does not expose `OPUS_SET_COMPLEXITY`, so
//! [`AudioPreset::complexity`] is carried by the preset but not applied to
//! the backend (libopus runs at its default complexity). Bitrate, frame
//! duration and in-band FEC are fully applied.

use crate::audio::{AudioCodecType, AudioDecoder, AudioEncoder, AudioPreset};
use crate::error::{CodecError, Result};

/// Scratch buffer size for one encoded packet. A single-frame Opus packet
/// never exceeds 1275 bytes (RFC 6716); 1500 leaves headroom and doubles as
/// the wire sanity bound (one datagram per frame, §9).
const MAX_OPUS_PACKET_BYTES: usize = 1500;

/// Expected packet loss (percent) hinted to the encoder when in-band FEC is
/// enabled; libopus uses it to decide when the redundant copy is worth the
/// bitrate. 10 % is the usual operating point for FEC to pay off.
const FEC_PACKET_LOSS_PERC: i32 = 10;

fn map_err(e: &::opus::Error) -> CodecError {
    CodecError::Backend(format!("{e}"))
}

fn opus_channels(channels: u8) -> Result<::opus::Channels> {
    match channels {
        1 => Ok(::opus::Channels::Mono),
        2 => Ok(::opus::Channels::Stereo),
        _ => Err(CodecError::InvalidConfig(
            "channels must be 1 (mono) or 2 (stereo)",
        )),
    }
}

fn check_sample_rate(sample_rate: u32) -> Result<()> {
    match sample_rate {
        8_000 | 12_000 | 16_000 | 24_000 | 48_000 => Ok(()),
        _ => Err(CodecError::InvalidConfig(
            "sample rate must be 8000, 12000, 16000, 24000 or 48000",
        )),
    }
}

fn check_frame_ms(frame_ms: u8) -> Result<()> {
    match frame_ms {
        10 | 20 | 40 | 60 => Ok(()),
        _ => Err(CodecError::InvalidConfig(
            "frame duration must be 10, 20, 40 or 60 ms",
        )),
    }
}

/// Opus encoder (`OPUS_APPLICATION_AUDIO`), configured from an
/// [`AudioPreset`] at construction.
#[derive(Debug)]
pub struct OpusEncoder {
    inner: ::opus::Encoder,
    sample_rate: u32,
    channels: u8,
    frame_samples: usize,
}

impl OpusEncoder {
    /// Creates an encoder from a preset. The preset's codec must be
    /// [`AudioCodecType::Opus`]; in-band FEC is enabled only when the preset
    /// says so (§9: the congestion controller decides).
    pub fn new(preset: &AudioPreset) -> Result<Self> {
        if preset.codec != AudioCodecType::Opus {
            return Err(CodecError::InvalidConfig(
                "OpusEncoder needs an Opus preset",
            ));
        }
        check_sample_rate(preset.sample_rate)?;
        check_frame_ms(preset.frame_ms)?;
        let channels = opus_channels(preset.channels)?;
        let mut inner =
            ::opus::Encoder::new(preset.sample_rate, channels, ::opus::Application::Audio)
                .map_err(|e| map_err(&e))?;
        inner
            .set_bitrate(::opus::Bitrate::Bits(kbps_to_bps(preset.bitrate_kbps)))
            .map_err(|e| map_err(&e))?;
        // NOTE: `preset.complexity` is intentionally not applied — the opus
        // 0.3 crate does not expose OPUS_SET_COMPLEXITY (see module docs).
        if preset.fec {
            inner.set_inband_fec(true).map_err(|e| map_err(&e))?;
            inner
                .set_packet_loss_perc(FEC_PACKET_LOSS_PERC)
                .map_err(|e| map_err(&e))?;
        }
        Ok(Self {
            inner,
            sample_rate: preset.sample_rate,
            channels: preset.channels,
            frame_samples: preset.frame_samples(),
        })
    }
}

fn kbps_to_bps(kbps: u32) -> i32 {
    i32::try_from(kbps.saturating_mul(1000)).unwrap_or(i32::MAX)
}

impl AudioEncoder for OpusEncoder {
    fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> Result<usize> {
        let expected = self.frame_samples * self.channels as usize;
        if pcm.len() != expected {
            return Err(CodecError::BadPcmLength {
                got: pcm.len(),
                expected,
            });
        }
        // Encode into a stack scratch buffer, then append: `out` is the
        // caller's reusable buffer and the hot path stays allocation-free.
        let mut scratch = [0u8; MAX_OPUS_PACKET_BYTES];
        let n = self
            .inner
            .encode(pcm, &mut scratch)
            .map_err(|e| map_err(&e))?;
        out.extend_from_slice(&scratch[..n]);
        Ok(n)
    }

    fn set_bitrate_kbps(&mut self, kbps: u32) -> Result<()> {
        self.inner
            .set_bitrate(::opus::Bitrate::Bits(kbps_to_bps(kbps)))
            .map_err(|e| map_err(&e))
    }

    fn set_fec(&mut self, enabled: bool) -> Result<()> {
        self.inner
            .set_inband_fec(enabled)
            .map_err(|e| map_err(&e))?;
        if enabled {
            // Tell the encoder how much loss to expect so it sizes the
            // redundant copy (same hint as the preset-time setup).
            self.inner
                .set_packet_loss_perc(FEC_PACKET_LOSS_PERC)
                .map_err(|e| map_err(&e))?;
        }
        Ok(())
    }

    fn codec_type(&self) -> AudioCodecType {
        AudioCodecType::Opus
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn frame_samples(&self) -> usize {
        self.frame_samples
    }
}

/// Opus decoder with native PLC: a lost packet is decoded as `None` and the
/// decoder synthesizes a plausible frame from its internal state (§9).
#[derive(Debug)]
pub struct OpusDecoder {
    inner: ::opus::Decoder,
    sample_rate: u32,
    channels: u8,
    frame_samples: usize,
}

impl OpusDecoder {
    /// Creates a decoder for a negotiated stream (sample rate, channels and
    /// frame duration come from the `AudioConfig` control message).
    pub fn new(sample_rate: u32, channels: u8, frame_ms: u8) -> Result<Self> {
        check_sample_rate(sample_rate)?;
        check_frame_ms(frame_ms)?;
        let opus_ch = opus_channels(channels)?;
        let inner = ::opus::Decoder::new(sample_rate, opus_ch).map_err(|e| map_err(&e))?;
        let frame_samples = sample_rate as usize * frame_ms as usize / 1000;
        Ok(Self {
            inner,
            sample_rate,
            channels,
            frame_samples,
        })
    }
}

impl AudioDecoder for OpusDecoder {
    fn decode(&mut self, packet: Option<&[u8]>, out: &mut [i16]) -> Result<usize> {
        let needed = self.frame_samples * self.channels as usize;
        if out.len() < needed {
            return Err(CodecError::OutputTooSmall {
                got: out.len(),
                needed,
            });
        }
        // Slice to exactly one frame: for PLC (`None`) libopus generates as
        // many samples as the output buffer allows, and the contract is one
        // frame per call. An empty slice is the PLC call in the opus crate
        // (it passes a null pointer to opus_decode).
        let frame_out = &mut out[..needed];
        let input = packet.unwrap_or(&[]);
        self.inner
            .decode(input, frame_out, false)
            .map_err(|e| map_err(&e))
    }

    fn codec_type(&self) -> AudioCodecType {
        AudioCodecType::Opus
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn frame_samples(&self) -> usize {
        self.frame_samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 20 ms of a 440 Hz sine, interleaved i16, for the given preset shape.
    // The casts lose precision/truncate by design: a rough sine is all the
    // roundtrip tests need.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn sine_frame(sample_rate: u32, channels: u8, frame_samples: usize) -> Vec<i16> {
        let mut pcm = Vec::with_capacity(frame_samples * channels as usize);
        for i in 0..frame_samples {
            let t = i as f32 / sample_rate as f32;
            let s = (t * 440.0 * 2.0 * std::f32::consts::PI).sin();
            let v = (s * 8000.0) as i16;
            for _ in 0..channels {
                pcm.push(v);
            }
        }
        pcm
    }

    fn default_pair() -> (OpusEncoder, OpusDecoder) {
        let preset = AudioPreset::default();
        let enc = OpusEncoder::new(&preset).unwrap();
        let dec = OpusDecoder::new(preset.sample_rate, preset.channels, preset.frame_ms).unwrap();
        (enc, dec)
    }

    #[test]
    fn encode_one_frame_produces_a_small_packet() {
        let (mut enc, _) = default_pair();
        let pcm = sine_frame(enc.sample_rate(), enc.channels(), enc.frame_samples());
        let mut out = Vec::new();
        let n = enc.encode(&pcm, &mut out).unwrap();
        assert_eq!(n, out.len());
        assert!(n > 0);
        assert!(n < MAX_OPUS_PACKET_BYTES, "packet fits one datagram: {n}");
    }

    #[test]
    fn encode_appends_to_the_caller_buffer() {
        let (mut enc, _) = default_pair();
        let pcm = sine_frame(enc.sample_rate(), enc.channels(), enc.frame_samples());
        let mut out = vec![0xAA, 0xBB];
        let n = enc.encode(&pcm, &mut out).unwrap();
        assert_eq!(out.len(), n + 2);
        assert_eq!(&out[..2], &[0xAA, 0xBB]);
    }

    #[test]
    fn roundtrip_decodes_a_full_frame() {
        let (mut enc, mut dec) = default_pair();
        let pcm = sine_frame(48_000, 2, 960);
        let mut packet = Vec::new();
        enc.encode(&pcm, &mut packet).unwrap();
        let mut out = vec![0i16; 960 * 2];
        let n = dec.decode(Some(&packet), &mut out).unwrap();
        assert_eq!(n, 960, "samples per channel");
        // Lossy codec: just check the energy came through (not silence).
        let energy: u64 = out.iter().map(|&s| u64::from(s.unsigned_abs())).sum();
        assert!(energy > 0);
    }

    #[test]
    fn plc_after_a_real_frame_yields_a_full_frame() {
        let (mut enc, mut dec) = default_pair();
        let pcm = sine_frame(48_000, 2, 960);
        let mut packet = Vec::new();
        enc.encode(&pcm, &mut packet).unwrap();
        let mut out = vec![0i16; 960 * 2];
        dec.decode(Some(&packet), &mut out).unwrap();
        // Lost packet: PLC must synthesize a full frame without error.
        let n = dec.decode(None, &mut out).unwrap();
        assert_eq!(n, 960);
    }

    #[test]
    fn plc_without_any_prior_frame_still_works() {
        let (_, mut dec) = default_pair();
        let mut out = vec![0i16; 960 * 2];
        let n = dec.decode(None, &mut out).unwrap();
        assert_eq!(n, 960);
    }

    #[test]
    fn set_bitrate_kbps_changes_the_encoder_target() {
        let (mut enc, _) = default_pair();
        enc.set_bitrate_kbps(24).unwrap();
        assert_eq!(
            enc.inner.get_bitrate().unwrap(),
            ::opus::Bitrate::Bits(24_000)
        );
        enc.set_bitrate_kbps(96).unwrap();
        assert_eq!(
            enc.inner.get_bitrate().unwrap(),
            ::opus::Bitrate::Bits(96_000)
        );
    }

    #[test]
    fn set_fec_toggles_live_and_encoding_continues() {
        // §12.5: the congestion controller flips FEC at runtime; the encoder
        // must keep producing valid frames across the toggle.
        let (mut enc, mut dec) = default_pair();
        let pcm = vec![1_000i16; 960 * 2];
        let mut packet = Vec::new();
        enc.encode(&pcm, &mut packet).unwrap();
        enc.set_fec(true).unwrap();
        assert!(enc.inner.get_inband_fec().unwrap());
        packet.clear();
        enc.encode(&pcm, &mut packet).unwrap();
        enc.set_fec(false).unwrap();
        assert!(!enc.inner.get_inband_fec().unwrap());
        packet.clear();
        enc.encode(&pcm, &mut packet).unwrap();
        // The decoder (never informed) still decodes: frames are
        // self-describing, FEC is purely encoder-local.
        let mut out = vec![0i16; 960 * 2];
        assert_eq!(dec.decode(Some(&packet), &mut out).unwrap(), 960);
    }

    #[test]
    fn wrong_pcm_length_is_rejected() {
        let (mut enc, _) = default_pair();
        let mut out = Vec::new();
        let err = enc.encode(&[0i16; 100], &mut out).unwrap_err();
        assert!(matches!(
            err,
            CodecError::BadPcmLength {
                got: 100,
                expected: 1920
            }
        ));
    }

    #[test]
    fn small_output_buffer_is_rejected() {
        let (_, mut dec) = default_pair();
        let mut out = vec![0i16; 100];
        let err = dec.decode(None, &mut out).unwrap_err();
        assert!(matches!(
            err,
            CodecError::OutputTooSmall {
                got: 100,
                needed: 1920
            }
        ));
    }

    #[test]
    fn invalid_configurations_are_rejected() {
        let base = AudioPreset::default();
        for (rate, ch, ms) in [
            (44_100, 2, 20), // not an Opus rate
            (48_000, 3, 20), // 3 channels unsupported
            (48_000, 2, 15), // not a valid Opus frame duration
        ] {
            let preset = AudioPreset {
                sample_rate: rate,
                channels: ch,
                frame_ms: ms,
                ..base
            };
            assert!(matches!(
                OpusEncoder::new(&preset),
                Err(CodecError::InvalidConfig(_))
            ));
            assert!(matches!(
                OpusDecoder::new(rate, ch, ms),
                Err(CodecError::InvalidConfig(_))
            ));
        }
    }

    #[test]
    fn wrong_codec_preset_is_rejected() {
        let preset = AudioPreset {
            codec: AudioCodecType::Raw,
            ..AudioPreset::default()
        };
        assert!(matches!(
            OpusEncoder::new(&preset),
            Err(CodecError::InvalidConfig(_))
        ));
    }

    #[test]
    fn voice_preset_encodes_mono() {
        let preset = AudioPreset::by_name("opus-voice").unwrap();
        let mut enc = OpusEncoder::new(&preset).unwrap();
        assert_eq!(enc.channels(), 1);
        let pcm = sine_frame(preset.sample_rate, 1, preset.frame_samples());
        let mut out = Vec::new();
        let n = enc.encode(&pcm, &mut out).unwrap();
        assert!(n > 0);
    }

    #[test]
    fn fec_preset_encodes() {
        // In-band FEC on: packets grow (redundant copy of the previous
        // frame) but still fit one datagram.
        let preset = AudioPreset::by_name("opus-lowbw").unwrap();
        assert!(preset.fec);
        let mut enc = OpusEncoder::new(&preset).unwrap();
        let mut dec =
            OpusDecoder::new(preset.sample_rate, preset.channels, preset.frame_ms).unwrap();
        let pcm = sine_frame(preset.sample_rate, 2, preset.frame_samples());
        let mut packet = Vec::new();
        enc.encode(&pcm, &mut packet).unwrap();
        assert!(!packet.is_empty() && packet.len() < MAX_OPUS_PACKET_BYTES);
        let mut out = vec![0i16; preset.frame_samples() * 2];
        assert_eq!(
            dec.decode(Some(&packet), &mut out).unwrap(),
            preset.frame_samples()
        );
    }

    #[test]
    fn build_functions_construct_opus_pair() {
        let preset = AudioPreset::default();
        let mut enc = crate::audio::build_audio_encoder(&preset).unwrap();
        assert_eq!(enc.codec_type(), AudioCodecType::Opus);
        assert_eq!(enc.frame_samples(), 960);
        let pcm = sine_frame(48_000, 2, 960);
        let mut packet = Vec::new();
        enc.encode(&pcm, &mut packet).unwrap();
        let mut dec = crate::audio::build_audio_decoder(&preset).unwrap();
        let mut out = vec![0i16; 1920];
        assert_eq!(dec.decode(Some(&packet), &mut out).unwrap(), 960);
    }
}
