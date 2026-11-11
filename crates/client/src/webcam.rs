// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Webcam streaming pipeline (design doc §18.57), the video counterpart of
//! the mic pipeline in `mic.rs`:
//!
//! ```text
//! WebcamSource (camera or mock) ─▶ cam thread: rgb24 → BGRA → VP9 encode
//!   ─▶ fragment (display format, stream 0) ─▶ mpsc ─▶ session loop sends
//!   each datagram on WEBCAM (lossy leg; TCP fallback when no UDP)
//! ```
//!
//! Streaming only starts after the server announces its choice
//! (`WebcamConfig`) and asks for the stream (`WebcamStart`): a server
//! without a camera sink must never receive webcam video. Fragment-loss on
//! the server side comes back as `WebcamRequestKeyframe`, which the encoder
//! honours on its next frame (rate-limited there, §12.2).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use codec::VideoCodecType;
#[cfg(feature = "vp9")]
use codec::{VideoEncoder, VideoFrame};
use protocol::ControlMessage;
#[cfg(feature = "vp9")]
use protocol::{fragment::Fragmenter, webcam as wc};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Webcam stream parameters chosen by the server (`WebcamConfig`).
#[derive(Debug, Clone, Copy)]
pub struct CamSpec {
    /// `VideoCodecType` wire ID.
    pub codec: u16,
    /// Capture width in pixels.
    pub width: u32,
    /// Capture height in pixels.
    pub height: u32,
    /// Capture frame rate.
    pub fps: u8,
    /// Target bitrate in kbps.
    pub bitrate_kbps: u32,
}

/// Datagram queue between the cam thread and the session loop. Sized like
/// the display queue (2048): a noisy 720p VP9 keyframe is ~70+ fragments
/// and the whole burst must fit, otherwise `try_send` drops its tail and
/// the frame arrives permanently incomplete (lab-confirmed: a 64-deep
/// queue made every keyframe undeecodable, poisoning the whole stream).
#[cfg(feature = "vp9")]
const CAM_QUEUE_DGRAMS: usize = 2048;

/// Absolute webcam bitrate ceiling (§18.78): far above any sane webcam
/// stream; a rogue server must not turn the encoder into a CPU sink.
const MAX_CAM_BITRATE_KBPS: u32 = 20_000;

/// Client webcam endpoint: owns the capture+encode thread while streaming.
pub struct CamClient {
    /// The capture source; `None` while streaming (moved into the thread)
    /// or when no camera is available.
    source: Option<Box<dyn crate::integrations::webcam::WebcamSource>>,
    /// Receives fragmented datagram payloads once the pipeline is running.
    rx: Option<mpsc::Receiver<Vec<u8>>>,
    /// Server-chosen parameters, kept to restart after a stop/start cycle.
    spec: Option<CamSpec>,
    /// Keyframe request flag shared with the encode thread.
    keyframe: Arc<AtomicBool>,
    /// Capture thread handle (joined on stop, so the camera is released
    /// deterministically); the thread hands the source back on exit.
    thread: Option<JoinHandle<Box<dyn crate::integrations::webcam::WebcamSource>>>,
}

impl CamClient {
    /// Creates the endpoint with the probed source (or `None` when no
    /// camera/mock is available — webcam redirection is then not offered).
    #[must_use]
    pub fn new(source: Option<Box<dyn crate::integrations::webcam::WebcamSource>>) -> Self {
        Self {
            source,
            rx: None,
            spec: None,
            keyframe: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    /// True when a capture source exists (the client should offer
    /// `FLAG_WEBCAM` in the HELLO).
    #[must_use]
    pub fn available(&self) -> bool {
        self.source.is_some()
    }

    /// The capabilities message to send after the handshake, when a source
    /// exists. VP9 is the only offered codec (§18.57 decision).
    pub fn caps_message(&self) -> Option<ControlMessage> {
        let source = self.source.as_ref()?;
        let (max_width, max_height) = source.max_dimensions();
        info!(
            device = source.device_name(),
            max_width, max_height, "webcam source available"
        );
        Some(ControlMessage::WebcamCaps {
            max_width,
            max_height,
            max_fps: u8::try_from(source.max_fps().min(u32::from(u8::MAX))).unwrap_or(u8::MAX),
            codecs: vec![VideoCodecType::Vp9.wire_id()],
        })
    }

    /// Applies the server's webcam configuration (§18.57). The stream does
    /// not start until [`CamClient::start`] (the server's `WebcamStart`).
    ///
    /// §18.78: the honest server clamps its choice to the announced
    /// `WebcamCaps`, but a rogue one could request a giant geometry whose
    /// encoder init allocates gigabytes — re-clamp at the trust boundary
    /// against the source's real limits.
    pub fn configure(&mut self, spec: CamSpec) {
        if spec.codec != VideoCodecType::Vp9.wire_id() {
            warn!(
                codec = spec.codec,
                "unknown webcam codec id; webcam disabled"
            );
            return;
        }
        let spec = if let Some(source) = &self.source {
            let (max_width, max_height) = source.max_dimensions();
            let max_fps =
                u8::try_from(source.max_fps().min(u32::from(u8::MAX))).unwrap_or(u8::MAX);
            let clamped = CamSpec {
                width: spec.width.clamp(16, max_width.max(16)),
                height: spec.height.clamp(16, max_height.max(16)),
                fps: spec.fps.clamp(1, max_fps.max(1)),
                bitrate_kbps: spec.bitrate_kbps.clamp(16, MAX_CAM_BITRATE_KBPS),
                ..spec
            };
            if (clamped.width, clamped.height, clamped.fps, clamped.bitrate_kbps)
                != (spec.width, spec.height, spec.fps, spec.bitrate_kbps)
            {
                warn!(
                    requested = ?(spec.width, spec.height, spec.fps, spec.bitrate_kbps),
                    applied = ?(clamped.width, clamped.height, clamped.fps, clamped.bitrate_kbps),
                    "server webcam config exceeded the announced caps; clamped"
                );
            }
            clamped
        } else {
            spec
        };
        self.spec = Some(spec);
    }

    /// Starts capture+encode with the configured parameters.
    pub fn start(&mut self) {
        if self.rx.is_some() {
            return; // already streaming
        }
        let Some(spec) = self.spec else {
            return; // no server configuration yet
        };
        let Some(source) = self.source.take() else {
            return; // no camera
        };
        #[cfg(not(feature = "vp9"))]
        let _ = spec;
        #[cfg(feature = "vp9")]
        let encoder = match codec::Vp9Encoder::new(spec.width, spec.height, spec.bitrate_kbps) {
            Ok(enc) => enc,
            Err(err) => {
                warn!("webcam VP9 encoder unavailable: {err}; webcam disabled");
                self.source = Some(source);
                return;
            }
        };
        #[cfg(not(feature = "vp9"))]
        {
            warn!("webcam needs the `vp9` client feature; webcam disabled");
            self.source = Some(source);
            return;
        }
        #[cfg(feature = "vp9")]
        {
            let (tx, rx) = mpsc::channel(CAM_QUEUE_DGRAMS);
            let keyframe = Arc::clone(&self.keyframe);
            let handle = std::thread::spawn(move || run(source, encoder, spec, &tx, &keyframe));
            self.thread = Some(handle);
            self.rx = Some(rx);
            info!(
                width = spec.width,
                height = spec.height,
                fps = spec.fps,
                bitrate_kbps = spec.bitrate_kbps,
                "webcam stream started"
            );
        }
    }

    /// Stops the stream (server `WebcamStop`): the capture thread is joined
    /// and the source is recovered for a later start.
    pub fn stop(&mut self) {
        self.rx = None; // closes the channel; the thread exits on send failure
        if let Some(handle) = self.thread.take()
            && let Ok(source) = handle.join()
        {
            self.source = Some(source);
        }
        debug!("webcam stream stopped");
    }

    /// Receives the next datagram payload; pends forever while the webcam
    /// is off, so it can sit in a `select!` branch.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        match &mut self.rx {
            Some(rx) => rx.recv().await,
            None => std::future::pending().await,
        }
    }

    /// Handles the server's keyframe request (fragment loss on its side).
    /// Rate-limited to one per 500 ms like the display path (riesgos §2.1).
    pub fn request_keyframe(&mut self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    /// True while the pipeline is running.
    #[must_use]
    pub fn streaming(&self) -> bool {
        self.rx.is_some()
    }
}

impl Drop for CamClient {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Encode thread body: starts the source with the server-chosen geometry,
/// pulls raw frames, VP9-encodes them (forcing a keyframe when the flag is
/// set) and pushes the fragmented datagrams. Returns the source so the
/// camera can be restarted. Exits when the session stops consuming
/// (channel closed) or the source ends.
#[cfg(feature = "vp9")]
fn run(
    source: Box<dyn crate::integrations::webcam::WebcamSource>,
    mut encoder: codec::Vp9Encoder,
    spec: CamSpec,
    tx: &mpsc::Sender<Vec<u8>>,
    keyframe: &AtomicBool,
) -> Box<dyn crate::integrations::webcam::WebcamSource> {
    match source.start(spec.width, spec.height, u32::from(spec.fps)) {
        Ok(frames) => {
            let mut frame_id = 0u32;
            for frame in &frames {
                if keyframe.swap(false, Ordering::Relaxed) {
                    encoder.request_keyframe();
                }
                let bgra = match codec::color::rgb24_to_bgra(&frame.rgb, frame.width, frame.height)
                {
                    Ok(bgra) => bgra,
                    Err(err) => {
                        warn!("webcam frame conversion failed: {err}");
                        continue;
                    }
                };
                let view = VideoFrame {
                    width: frame.width,
                    height: frame.height,
                    stride: frame.width as usize * 4,
                    data: &bgra,
                };
                let packet = match encoder.encode(&view) {
                    Ok(packet) => packet,
                    Err(err) => {
                        warn!("webcam encode failed: {err}");
                        continue;
                    }
                };
                // §8.5: every frame opens with the codec wire id (u16 BE),
                // exactly like display frames; the server strips it before
                // decoding, so a config/race mismatch can never feed the
                // wrong decoder.
                let mut element = Vec::with_capacity(2 + packet.data.len());
                element.extend_from_slice(&VideoCodecType::Vp9.wire_id().to_be_bytes());
                element.extend_from_slice(&packet.data);
                frame_id = frame_id.wrapping_add(1);
                for dgram in
                    Fragmenter::split(wc::STREAM_ID, frame_id, frame.timestamp_ms, &element)
                {
                    if tx.try_send(dgram).is_err() {
                        // Channel full (consumer lagging) or closed (stop):
                        // dropping is correct on the lossy leg either way.
                        break;
                    }
                }
                if tx.is_closed() {
                    break;
                }
            }
        }
        Err(err) => warn!("webcam capture failed to start: {err:#}"),
    }
    source.stop();
    source
}
