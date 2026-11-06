// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Real camera source via nokhwa (multiplatform), modeled on the proven
//! capture loop of uds-client (`channels/src/webcam`): closest-format
//! negotiation, paced read, drop-on-lag.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use nokhwa::utils::{ApiBackend, CameraIndex, RequestedFormat, RequestedFormatType};
use tracing::{debug, info, warn};

use super::{DEVICE_ENV, RawFrame, WebcamSource};

/// How long to wait before trying to reopen a camera that failed to open
/// or stopped delivering frames (uds-client capture_loop): Windows only
/// releases the hardware once the previous instance is dropped, so the
/// camera is dropped on failure and reacquired on this cadence for as long
/// as the stream lasts — whatever was holding it (another app, a USB
/// reset) usually lets go on its own.
const CAMERA_REOPEN_INTERVAL: Duration = Duration::from_secs(2);

/// Next paced send instant: previous + interval, but never in the past
/// (uds-client capture_loop) — after a stall we resume at the current
/// cadence instead of bursting catch-up frames.
fn next_deadline(previous: Instant, interval: Duration, now: Instant) -> Instant {
    (previous + interval).max(now)
}

/// Real camera source.
#[derive(Debug)]
pub struct NokhwaSource {
    index: CameraIndex,
    name: String,
    max_dimensions: (u32, u32),
    max_fps: u32,
    running: Arc<AtomicBool>,
    /// Join handle of the capture thread, while running.
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl NokhwaSource {
    /// Probes for a usable camera (device selected by `UDSRP_CAM_DEVICE` as
    /// an index or name substring, else index 0). Does not open the stream.
    pub fn probe() -> anyhow::Result<Self> {
        nokhwa::nokhwa_initialize(|granted| {
            if !granted {
                warn!("webcam: camera permission not granted by the OS");
            }
        });
        let devices = nokhwa::query(ApiBackend::Auto).context("webcam: camera query failed")?;
        if devices.is_empty() {
            bail!("webcam: no camera devices found");
        }
        let info = select_device(&devices);
        let index = info.index().clone();
        let name = info.human_name().clone();
        // Best-effort limits: open lazily to read compatible formats; on
        // failure report unknown (0) and let the server clamp blindly.
        let (max_dimensions, max_fps) = query_limits(&index).unwrap_or(((0, 0), 0));
        Ok(Self {
            index,
            name,
            max_dimensions,
            max_fps,
            running: Arc::new(AtomicBool::new(false)),
            thread: Mutex::new(None),
        })
    }
}

/// Picks the device indicated by `UDSRP_CAM_DEVICE` (index or name
/// substring), defaulting to the first one.
fn select_device(
    devices: &[nokhwa::utils::CameraInfo],
) -> nokhwa::utils::CameraInfo {
    if let Ok(val) = std::env::var(DEVICE_ENV) {
        if let Ok(idx) = val.parse::<u32>() {
            let wanted = CameraIndex::Index(idx);
            if let Some(info) = devices.iter().find(|d| d.index() == &wanted) {
                return info.clone();
            }
        }
        let val_lower = val.to_lowercase();
        if let Some(info) = devices
            .iter()
            .find(|d| d.human_name().to_lowercase().contains(&val_lower))
        {
            return info.clone();
        }
        warn!("webcam: {DEVICE_ENV}='{val}' matched no device, using the first one");
    }
    devices[0].clone()
}

/// Reads the device's maximum resolution and frame rate.
fn query_limits(index: &CameraIndex) -> Option<((u32, u32), u32)> {
    let requested = RequestedFormat::new::<nokhwa::pixel_format::RgbFormat>(
        RequestedFormatType::None,
    );
    let mut cam = nokhwa::Camera::new(index.clone(), requested).ok()?;
    let formats = cam.compatible_camera_formats().ok()?;
    let max_res = formats
        .iter()
        .map(nokhwa::utils::CameraFormat::resolution)
        .max_by_key(|r| u64::from(r.width()) * u64::from(r.height()))?;
    let max_fps = formats
        .iter()
        .map(nokhwa::utils::CameraFormat::frame_rate)
        .max()
        .unwrap_or(0);
    Some(((max_res.width(), max_res.height()), max_fps))
}

/// Opens the camera at the closest supported format to the request.
/// Returns the camera and the negotiated frame rate (like uds-client:
/// `Camera::frame_rate` is unreliable under Media Foundation, the value
/// returned when applying the format is the only trustworthy one).
fn open_camera(index: &CameraIndex, width: u32, height: u32, fps: u32) -> anyhow::Result<(nokhwa::Camera, u32)> {
    let requested_none = RequestedFormat::new::<nokhwa::pixel_format::RgbFormat>(
        RequestedFormatType::None,
    );
    let mut cam = nokhwa::Camera::new(index.clone(), requested_none)
        .context("webcam: failed to create camera")?;

    let mut negotiated_fps = 0;
    if let Ok(formats) = cam.compatible_camera_formats() {
        let best = formats.iter().min_by_key(|f| {
            let res_diff = (f.width().cast_signed() - width.cast_signed()).unsigned_abs()
                + (f.height().cast_signed() - height.cast_signed()).unsigned_abs();
            let fps_diff = (f.frame_rate().cast_signed() - fps.cast_signed()).unsigned_abs();
            (res_diff, fps_diff)
        });
        if let Some(&closest) = best {
            info!(?closest, "webcam: selected closest camera format");
            let requested_closest =
                RequestedFormat::new::<nokhwa::pixel_format::RgbFormat>(
                    RequestedFormatType::Exact(closest),
                );
            match cam.set_camera_requset(requested_closest) {
                Ok(applied) => negotiated_fps = applied.frame_rate(),
                Err(err) => warn!("webcam: camera rejected the selected format: {err}"),
            }
        }
    }

    cam.open_stream().context("webcam: failed to open camera stream")?;
    Ok((cam, negotiated_fps))
}

impl WebcamSource for NokhwaSource {
    fn device_name(&self) -> String {
        self.name.clone()
    }

    fn max_dimensions(&self) -> (u32, u32) {
        self.max_dimensions
    }

    fn max_fps(&self) -> u32 {
        self.max_fps
    }

    fn start(
        &self,
        width: u32,
        height: u32,
        fps: u32,
    ) -> anyhow::Result<flume::Receiver<RawFrame>> {
        if self.running.swap(true, Ordering::SeqCst) {
            bail!("webcam: already streaming");
        }
        let (tx, rx) = flume::bounded(4);
        let index = self.index.clone();
        let running = Arc::clone(&self.running);
        let epoch = Instant::now();
        let handle = std::thread::spawn(move || {
            let mut cam: Option<nokhwa::Camera> = None;
            let mut reopen_at = Instant::now();
            let mut fps = fps.max(1);
            let mut frame_dur = Duration::from_secs_f64(1.0 / f64::from(fps));
            let mut next = Instant::now();
            while running.load(Ordering::Relaxed) {
                if cam.is_none() {
                    let now = Instant::now();
                    if now < reopen_at {
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    match open_camera(&index, width, height, fps) {
                        Ok((opened, negotiated_fps)) => {
                            if negotiated_fps > 0 {
                                fps = negotiated_fps;
                                frame_dur = Duration::from_secs_f64(1.0 / f64::from(fps));
                            }
                            info!(width, height, fps, "webcam: capture started");
                            next = Instant::now();
                            cam = Some(opened);
                        }
                        Err(err) => {
                            warn!("webcam: capture not started: {err:#}; retrying");
                            reopen_at = Instant::now() + CAMERA_REOPEN_INTERVAL;
                        }
                    }
                    continue;
                }
                match cam.as_mut().expect("camera present").frame() {
                    Ok(buffer) => {
                        match buffer.decode_image::<nokhwa::pixel_format::RgbFormat>() {
                            Ok(image) => {
                                let frame = RawFrame {
                                    width: image.width(),
                                    height: image.height(),
                                    timestamp_ms: u32::try_from(epoch.elapsed().as_millis()).unwrap_or(u32::MAX),
                                    rgb: image.into_vec(),
                                };
                                // Drop when the consumer lags: real cameras
                                // do not queue.
                                let _ = tx.try_send(frame);
                            }
                            Err(err) => warn!("webcam: frame decode failed: {err}"),
                        }
                    }
                    Err(err) => {
                        // Camera lost (USB reset, another app took it):
                        // drop it and reacquire on the reopen cadence.
                        warn!("webcam: capture failed: {err}; reopening the camera");
                        drop(cam.take());
                        reopen_at = Instant::now() + CAMERA_REOPEN_INTERVAL;
                        continue;
                    }
                }
                next = next_deadline(next, frame_dur, Instant::now());
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                }
            }
            if let Some(mut c) = cam.take() {
                let _ = c.stop_stream();
            }
            debug!("webcam: capture stopped");
        });
        *self.thread.lock().expect("capture thread mutex") = Some(handle);
        Ok(rx)
    }

    fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread.lock().expect("capture thread mutex").take() {
            let _ = handle.join();
        }
    }
}

impl Drop for NokhwaSource {
    fn drop(&mut self) {
        self.stop();
    }
}
