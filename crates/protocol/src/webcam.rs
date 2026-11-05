// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Webcam redirection wire conventions (§18.57, Fase 4).
//!
//! The client captures its local camera, VP9-encodes one frame per logical
//! element and sends it on `channel::WEBCAM` (lossy leg). The datagrams
//! reuse **exactly** the display fragmentation format ([`crate::fragment`]):
//!
//! ```text
//! | stream_id (1B, always 0) | frame_id (3B BE) | frag_idx (2B BE) |
//! frag_count (2B BE) | capture_ts_ms (4B BE) | chunk ... |
//! ```
//!
//! - `stream_id` is the fragment header's `display_id` field, kept at 0
//!   (a single webcam per session in this phase).
//! - `capture_ts_ms` is the client's capture clock (ms); the server uses it
//!   only for drop statistics, never for A/V sync (the webcam is a remote
//!   input device, not part of the desktop mix).
//! - Each reassembled element opens with the codec wire id (u16 BE, §8.5),
//!   exactly like display frames: the per-frame tag, not the last
//!   `WebcamConfig`, is authoritative for the server's decoder choice.
//! - A missing fragment drops the whole frame ([`crate::fragment`]
//!   semantics). The server's reassembly gap detector asks the client for a
//!   keyframe with `ControlMessage::WebcamRequestKeyframe` — the same
//!   recovery loop as the display, rate-limited the same way.
//!
//! Only VP9 travels on this channel (decision §18.57: VP9 decode measured
//! fast enough that an MJPEG backup adds nothing); the codec id in
//! `ControlMessage::WebcamConfig` is a `VideoCodecType` wire id so future
//! codecs (AV1) fit without wire changes.

/// Stream identifier used in the fragment header (the `display_id` byte).
pub const STREAM_ID: u8 = 0;

/// Default capture geometry the server asks for when its policy does not
/// fix one and the camera supports it.
pub const DEFAULT_WIDTH: u32 = 1280;
/// Default capture height for [`DEFAULT_WIDTH`].
pub const DEFAULT_HEIGHT: u32 = 720;
/// Default capture frame rate.
pub const DEFAULT_FPS: u8 = 30;
/// Default VP9 target bitrate (kbps) for the webcam stream.
pub const DEFAULT_BITRATE_KBPS: u32 = 1500;
