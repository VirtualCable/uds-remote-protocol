// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Deterministic synthetic webcam source (no hardware needed).
//!
//! Generates RGB24 frames with a moving block and per-frame noise at the
//! requested rate, paced by an absolute schedule. Used for E2E tests and
//! on machines without a camera (`UDSRP_CAM_MOCK=1`).

// Synthetic-pattern math uses small fixed-range casts and short names.
#![allow(clippy::cast_possible_truncation, clippy::many_single_char_names)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::bail;
use tracing::{debug, info};

use super::{RawFrame, WebcamSource};

/// Synthetic source: moving-block frames at a paced rate.
#[derive(Debug, Default)]
pub struct MockSource {
    running: Arc<AtomicBool>,
    /// Join handle of the pacing thread, while running.
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl MockSource {
    /// Creates the source (nothing is started yet).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds one synthetic, fully describable frame:
    ///
    /// - Background: R grows left→right (0..255 across the width),
    ///   G grows top→bottom (0..255 across the height), B fixed at 96.
    ///   So the top-left corner is dark blue-ish, the right edge goes
    ///   red, the bottom edge goes green.
    /// - Foreground: a pure white square, side = width/8, sweeping
    ///   horizontally left→right across the vertical center, wrapping
    ///   around. With `fps` frames per sweep of ~4 s at 30 fps.
    ///
    /// Any conversion bug (channel swap, stride, row flip) shows up as
    /// an immediately describable distortion of this pattern.
    fn frame(width: u32, height: u32, tick: u32) -> RawFrame {
        let (w, h) = (width as usize, height as usize);
        let mut rgb = vec![0u8; w * h * 3];
        let side = (width / 8).max(2);
        // Sweep: one full left→right pass every 128 ticks (~4 s @30fps).
        let span = width + side;
        let x0 = ((tick % 128) as usize * span as usize / 128) as u32;
        let x0 = x0.saturating_sub(side); // starts fully off-screen at the left
        let y0 = height / 2 - side / 2;
        for y in 0..height {
            let g = (y * 255 / height.max(1)) as u8;
            for x in 0..width {
                let i = (y as usize * w + x as usize) * 3;
                let r = (x * 255 / width.max(1)) as u8;
                let in_square =
                    x >= x0 && x < x0 + side && y >= y0 && y < y0 + side;
                if in_square {
                    rgb[i] = 255;
                    rgb[i + 1] = 255;
                    rgb[i + 2] = 255;
                } else {
                    rgb[i] = r;
                    rgb[i + 1] = g;
                    rgb[i + 2] = 96;
                }
            }
        }
        RawFrame {
            width,
            height,
            timestamp_ms: 0, // Stamped by the pacing thread.
            rgb,
        }
    }
}

impl WebcamSource for MockSource {
    fn device_name(&self) -> String {
        "UDS Mock Camera".to_string()
    }

    fn max_dimensions(&self) -> (u32, u32) {
        (1920, 1080)
    }

    fn max_fps(&self) -> u32 {
        60
    }

    fn start(
        &self,
        width: u32,
        height: u32,
        fps: u32,
    ) -> anyhow::Result<flume::Receiver<RawFrame>> {
        if self.running.swap(true, Ordering::SeqCst) {
            bail!("webcam mock: already streaming");
        }
        let fps = fps.clamp(1, 60);
        let (tx, rx) = flume::bounded(4);
        let running = Arc::clone(&self.running);
        let epoch = Instant::now();
        let handle = std::thread::spawn(move || {
            info!(width, height, fps, "webcam mock: streaming started");
            let frame_dur = Duration::from_secs_f64(1.0 / f64::from(fps));
            let mut tick = 0u32;
            loop {
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                let mut frame = Self::frame(width, height, tick);
                frame.timestamp_ms = u32::try_from(epoch.elapsed().as_millis()).unwrap_or(u32::MAX);
                // Drop when the consumer lags: real cameras do not queue.
                let _ = tx.try_send(frame);
                tick = tick.wrapping_add(1);
                let next = epoch + frame_dur * tick;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                }
            }
            debug!("webcam mock: streaming stopped");
        });
        *self.thread.lock().expect("mock thread mutex") = Some(handle);
        Ok(rx)
    }

    fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread.lock().expect("mock thread mutex").take() {
            let _ = handle.join();
        }
    }
}

impl Drop for MockSource {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_streams_and_stops() {
        let source = MockSource::new();
        let rx = source.start(320, 240, 30).expect("start");
        let frame = rx.recv_timeout(Duration::from_secs(2)).expect("frame");
        assert_eq!(frame.width, 320);
        assert_eq!(frame.rgb.len(), 320 * 240 * 3);
        source.stop();
        // After stop the stream ends promptly.
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
    }

    #[test]
    fn double_start_fails() {
        let source = MockSource::new();
        let _rx = source.start(160, 120, 10).expect("start");
        assert!(source.start(160, 120, 10).is_err());
        source.stop();
    }
}
