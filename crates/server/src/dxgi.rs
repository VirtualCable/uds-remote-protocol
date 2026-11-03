// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! DXGI Desktop Duplication capture backend (Windows only).
//!
//! Captures the desktop of one output (monitor) through the Desktop
//! Duplication API. GPU-side, delivers dirty rects, and survives desktop
//! changes (resolution switch, lock, UAC) by reinitializing on
//! `DXGI_ERROR_ACCESS_LOST` without breaking the session.

// The `windows` crate FFI methods take raw pointers; implicit borrows are
// the idiomatic call style for that API.
#![allow(clippy::borrow_as_ptr)]

use std::time::Duration;

use anyhow::Context as _;
use tracing::{info, warn};
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, IDXGISurface, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_INVALID_CALL,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_MAPPED_RECT, DXGI_MAP_READ, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};
use windows::core::Interface;
use windows::Win32::Graphics::Gdi::{EnumDisplayDevicesW, DISPLAY_DEVICEW};

use crate::capture::{
    copy_bgra_rows, mask_shape_to_rgba, select_output, CapturedFrame, CursorUpdate, FrameSource,
    MaskShapeKind, OutputCandidate, OutputSelector, Rect,
};

/// How long a reinit may keep retrying before it is declared fatal (see
/// [`DxgiSource::reinit`]). Generous on purpose: secure-desktop transitions
/// (lock, UAC) can hold `DuplicateOutput` denied for seconds.
const REINIT_BUDGET: Duration = Duration::from_secs(30);
/// Delay between reinit attempts while the desktop settles.
const REINIT_RETRY_DELAY: Duration = Duration::from_millis(250);
/// Retry budget for the FIRST duplication of a source (§18.37): the capture
/// thread is restarted around console mode changes, so its initial
/// `DuplicateOutput` can land inside the settle window of the mode switch
/// (denied while the display stack is mid-transition). Much shorter than
/// [`REINIT_BUDGET`]: a genuinely locked console at session start must fail
/// fast, not hang the connect.
const INITIAL_CREATE_BUDGET: Duration = Duration::from_secs(3);

/// First delay between storm re-creations (see [`STORM_BREAKER`]).
const STORM_DELAY_MIN: Duration = Duration::from_millis(100);
/// Max delay between storm re-creations.
const STORM_DELAY_MAX: Duration = Duration::from_secs(2);
/// How long an `ACCESS_LOST` storm may last before the capture is declared
/// broken. A poisoned output (seen on QXL after driver churn) invalidates
/// every new duplication instantly; spinning unthrottled there hung a whole
/// VM once (≈500 factory+device+duplication creations/s → display-stack
/// resource exhaustion → qemu watchdog reset). Failing the session releases
/// everything and lets the next connection retry from scratch.
const STORM_BREAKER: Duration = Duration::from_secs(120);

/// Desktop Duplication source for a single output.
pub struct DxgiSource {
    selector: OutputSelector,
    /// GDI device name of the captured output (e.g. `\\.\DISPLAY2`); the
    /// input injector needs it to translate display-local pointer
    /// coordinates into virtual-desktop ones.
    output_device: String,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    width: u32,
    height: u32,
    /// Staging texture for the GPU-only path (created lazily): some display
    /// stacks (VM synthetic display adapters on the physical console, RDP
    /// indirect display, hybrid GPUs) hand out desktop images that cannot
    /// be mapped directly. On VMs this is the normal path, not a corner case.
    staging: Option<ID3D11Texture2D>,
    /// Reused buffer for `GetFrameDirtyRects` (avoids per-frame allocation).
    dirty_buf: Vec<RECT>,
    /// Reused buffer for `GetFramePointerShape`.
    shape_buf: Vec<u8>,
    /// Cursor update accumulated since the last `take_cursor_update`.
    pending_cursor: CursorUpdate,
    /// Last cursor visibility reported by DXGI (for change detection).
    cursor_visible: bool,
    /// Last time `AcquireNextFrame` proved the duplication healthy (a frame
    /// or a clean timeout). Drives the storm throttle/breaker.
    last_healthy: std::time::Instant,
    /// Current backoff between storm re-creations; doubles per iteration up
    /// to [`STORM_DELAY_MAX`], reset on a healthy acquire.
    storm_delay: Duration,
    /// Cooperative shutdown shared with the capture thread's owner: when set,
    /// reinit retry loops bail out immediately instead of blocking the join
    /// in `stop_capture` for the whole [`REINIT_BUDGET`] (§18.41: entering a
    /// secure desktop denies `DuplicateOutput` for good, so the full budget
    /// would always be burnt with the session loop frozen behind the join).
    stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl DxgiSource {
    /// Captures the output picked by `selector` (see [`OutputSelector`]).
    pub fn new(selector: &OutputSelector) -> anyhow::Result<Self> {
        // Retry like `reinit` does: this creation can land inside the
        // settle window of a console mode change applied moments ago (the
        // capture thread is restarted around mode switches, §18.37), where
        // `DuplicateOutput` is transiently denied.
        let deadline = std::time::Instant::now() + INITIAL_CREATE_BUDGET;
        let mut attempt = 0u32;
        let dup = loop {
            attempt += 1;
            match init_duplication(selector, false) {
                Ok(dup) => break dup,
                Err(e) if std::time::Instant::now() < deadline => {
                    if attempt == 1 {
                        info!(error = %format!("{e:#}"), "duplication not possible yet; retrying");
                    }
                    std::thread::sleep(REINIT_RETRY_DELAY);
                }
                Err(e) => {
                    return Err(e.context(format!(
                        "duplication could not be created after {attempt} attempts over {INITIAL_CREATE_BUDGET:?}"
                    )));
                }
            }
        };
        Ok(Self {
            selector: selector.clone(),
            output_device: dup.output_name,
            device: dup.device,
            context: dup.context,
            duplication: dup.duplication,
            width: dup.width,
            height: dup.height,
            staging: None,
            dirty_buf: Vec::with_capacity(64),
            shape_buf: Vec::new(),
            pending_cursor: CursorUpdate::default(),
            cursor_visible: true,
            last_healthy: std::time::Instant::now(),
            storm_delay: STORM_DELAY_MIN,
            stop: None,
        })
    }

    /// Arms the cooperative shutdown flag (see the `stop` field).
    pub fn set_stop(&mut self, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.stop = Some(stop);
    }

    /// True once the owner asked us to shut down.
    fn stop_requested(&self) -> bool {
        self.stop
            .as_ref()
            .is_some_and(|s| s.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Handles `DXGI_ERROR_ACCESS_LOST` from `AcquireNextFrame`.
    ///
    /// Every re-creation is throttled with exponential backoff — even the
    /// first one right after healthy traffic. Each reinit costs a DXGI
    /// factory, a D3D11 device and a duplication; an output that keeps
    /// invalidating them (desktop switch still in flight after a secure
    /// desktop is released, §18.41) would otherwise spin at ~500
    /// re-creations per second and exhaust the VM's
    /// display stack into a state where every future duplication fails
    /// forever (seen live: returning from a UAC prompt blinded the session
    /// permanently). Broken off entirely after [`STORM_BREAKER`].
    fn on_access_lost(&mut self) -> anyhow::Result<()> {
        // A secure desktop (lock, UAC, Winlogon) invalidates the duplication,
        // and the secure branch of the session loop owns the restart once
        // the transition settles. Re-creating duplications mid-transition
        // poisons this process's display stack on the Basic Render Driver
        // (every later duplication is invalidated, §18.52), so bail fast and
        // let the capture-thread grace window hand over to the helper.
        // The lock curtain is NOT a secure desktop (§18.52.3): gate on the
        // WTS lock state too, or a lock cycle recreates duplications while
        // the console is still switching (§18.52.4).
        if crate::secure_helper::secure_desktop_active() || crate::secure_helper::console_locked()
        {
            anyhow::bail!("access lost to a secure desktop; the secure transition owns the restart");
        }
        let since_healthy = self.last_healthy.elapsed();
        anyhow::ensure!(
            since_healthy < STORM_BREAKER,
            "the output has invalidated every new desktop duplication for {STORM_BREAKER:?}; \
             the display stack looks poisoned — giving up instead of spinning"
        );
        // Both gates above have a flip window (~500 ms between a lock request
        // and the WTS flags moving) where neither sees the transition, and a
        // duplication recreated inside it poisons the display stack
        // (§18.52.4). Spend the factory/device/duplication only once the
        // desktop has been continuously stable for a second.
        let mut clear_since: Option<std::time::Instant> = None;
        loop {
            if self.stop_requested() {
                anyhow::bail!("capture shutdown requested; abandoning the stability wait");
            }
            if crate::secure_helper::secure_desktop_active()
                || crate::secure_helper::console_locked()
            {
                clear_since = None;
            } else {
                let since = clear_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= Duration::from_secs(1) {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Sleep in chunks so a shutdown request is honored promptly even
        // from inside the storm backoff.
        let mut remaining = self.storm_delay;
        while remaining > Duration::ZERO {
            if self.stop_requested() {
                anyhow::bail!("capture shutdown requested; abandoning the storm backoff");
            }
            let step = remaining.min(Duration::from_millis(100));
            std::thread::sleep(step);
            remaining -= step;
        }
        self.storm_delay = (self.storm_delay * 2).min(STORM_DELAY_MAX);
        self.reinit(true)
    }

    /// Re-creates the duplication after `DXGI_ERROR_ACCESS_LOST`.
    ///
    /// `DuplicateOutput` keeps failing for a short window while Windows
    /// applies a desktop topology change (mode switch, display attach,
    /// secure desktop transition): `ACCESS_DENIED`/`E_ACCESSDENIED` or a
    /// transiently missing output. Dying on the first failure would kill
    /// the session on every topology change, so the reinit is retried with
    /// backoff for up to [`REINIT_BUDGET`] before giving up. In `storm`
    /// mode (caller already throttles) logging drops to `debug` to avoid
    /// spamming per iteration.
    fn reinit(&mut self, storm: bool) -> anyhow::Result<()> {
        if storm {
            tracing::debug!(selector = ?self.selector, "DXGI access lost, reinitializing (storm)");
        } else {
            info!(selector = ?self.selector, "DXGI access lost, reinitializing");
        }
        let deadline = std::time::Instant::now() + REINIT_BUDGET;
        let mut attempt = 0u32;
        let dup = loop {
            attempt += 1;
            if self.stop_requested() {
                anyhow::bail!("capture shutdown requested; abandoning the duplication reinit");
            }
            match init_duplication(&self.selector, storm) {
                Ok(dup) => break dup,
                Err(e) if std::time::Instant::now() < deadline => {
                    // The desktop may have gone secure AFTER the access-lost
                    // that brought us here (the switch lags the invalidation
                    // by a few ms): stop retrying and hand over, same as the
                    // up-front check in `on_access_lost` (§18.52).
                    if crate::secure_helper::secure_desktop_active() {
                        return Err(e.context(
                            "secure desktop active; the secure transition owns the restart",
                        ));
                    }
                    if attempt == 1 {
                        info!(error = %format!("{e:#}"), "reinit not possible yet; retrying");
                    }
                    std::thread::sleep(REINIT_RETRY_DELAY);
                }
                Err(e) => {
                    return Err(e.context(format!(
                        "duplication could not be re-created after {attempt} attempts over {REINIT_BUDGET:?}"
                    )));
                }
            }
        };
        if attempt > 1 {
            info!(attempts = attempt, "DXGI duplication re-acquired");
        }
        self.output_device.clone_from(&dup.output_name);
        if (dup.width, dup.height) != (self.width, self.height) {
            info!(
                old = ?(self.width, self.height),
                new = ?(dup.width, dup.height),
                "desktop resolution changed"
            );
        }
        self.device = dup.device;
        self.context = dup.context;
        self.duplication = dup.duplication;
        self.width = dup.width;
        self.height = dup.height;
        self.staging = None;
        Ok(())
    }
}

impl FrameSource for DxgiSource {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn next_frame(&mut self, timeout: Duration) -> anyhow::Result<Option<CapturedFrame>> {
        let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        // SAFETY: `info` and `resource` are valid out-params; the duplication
        // object is alive for the whole call.
        let result = unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        };
        if let Err(e) = &result {
            if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                // A clean timeout also proves the duplication is alive.
                self.last_healthy = std::time::Instant::now();
                self.storm_delay = STORM_DELAY_MIN;
                return Ok(None);
            }
            if e.code() == DXGI_ERROR_ACCESS_LOST {
                self.on_access_lost()?;
                return Ok(None);
            }
            if e.code() == DXGI_ERROR_INVALID_CALL {
                // Rapid secure-desktop round-trips (lock bursts, §18.52.4)
                // leave the duplication in a state where the next acquire
                // reports INVALID_CALL instead of ACCESS_LOST. The recovery
                // is the same rebuild, so route it through the access-lost
                // path instead of killing the capture thread and session.
                tracing::warn!("AcquireNextFrame: INVALID_CALL; rebuilding the duplication");
                self.on_access_lost()?;
                return Ok(None);
            }
        }
        result.context("AcquireNextFrame failed")?;
        self.last_healthy = std::time::Instant::now();
        self.storm_delay = STORM_DELAY_MIN;
        let resource = resource.context("AcquireNextFrame returned no resource")?;

        // Every acquired frame MUST be released, even on error paths.
        let result = self.extract(&resource, &info);
        self.read_cursor(&info);
        // SAFETY: `resource` was acquired above and is released exactly once.
        if let Err(e) = unsafe { self.duplication.ReleaseFrame() } {
            // The duplication died while the frame was held (a mode change
            // lands exactly here): the extracted frame is still valid, but
            // the duplication needs a rebuild before the next acquire —
            // same path as an ACCESS_LOST on acquire. Propagating the error
            // instead used to kill the capture thread and the whole session.
            tracing::debug!("ReleaseFrame failed ({e:#}); reinitializing the duplication");
            self.on_access_lost()?;
        }
        match result {
            Ok(frame) => Ok(Some(frame)),
            Err(e) => {
                // A desktop switch can land mid-extract (lab: lock bursts make
                // `GetFrameDirtyRects` fail right after a rebuild, §18.52.4):
                // the frame is unusable but the fix is the same rebuild as
                // ACCESS_LOST, not a dead capture thread.
                let code = e.downcast_ref::<windows::core::Error>().map(|w| w.code());
                if code == Some(DXGI_ERROR_ACCESS_LOST) || code == Some(DXGI_ERROR_INVALID_CALL) {
                    tracing::warn!(
                        "frame extract failed with {code:?} mid-transition; rebuilding the duplication"
                    );
                    self.on_access_lost()?;
                    return Ok(None);
                }
                Err(e)
            }
        }
    }

    fn take_cursor_update(&mut self) -> Option<CursorUpdate> {
        let update = std::mem::take(&mut self.pending_cursor);
        (update.visible.is_some() || update.shape.is_some()).then_some(update)
    }

    fn output_device(&self) -> Option<&str> {
        Some(&self.output_device)
    }
}

impl DxgiSource {
    /// Reads cursor visibility and (when present) the new pointer shape from
    /// the acquired frame. Must run before `ReleaseFrame`: the shape buffer
    /// is owned by the frame. Errors are logged, not propagated — a cursor
    /// failure must never break video.
    fn read_cursor(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO) {
        // A non-zero LastMouseUpdateTime means PointerPosition is valid.
        if info.LastMouseUpdateTime != 0 {
            let visible = info.PointerPosition.Visible.as_bool();
            if visible != self.cursor_visible {
                self.cursor_visible = visible;
                self.pending_cursor.visible = Some(visible);
            }
        }
        if info.PointerShapeBufferSize == 0 {
            return;
        }
        self.shape_buf.resize(info.PointerShapeBufferSize as usize, 0);
        let mut shape_info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        let mut needed = 0u32;
        // SAFETY: `shape_buf` is `PointerShapeBufferSize` bytes long and the
        // out-params are valid; the frame is still acquired.
        if let Err(e) = unsafe {
            self.duplication.GetFramePointerShape(
                info.PointerShapeBufferSize,
                self.shape_buf.as_mut_ptr().cast(),
                &mut needed,
                &mut shape_info,
            )
        } {
            warn!(error = %e, "GetFramePointerShape failed");
            return;
        }
        let kind = match shape_info.Type {
            t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0.cast_unsigned() => {
                MaskShapeKind::Monochrome
            }
            t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0.cast_unsigned() => MaskShapeKind::Color,
            t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0.cast_unsigned() => {
                MaskShapeKind::MaskedColor
            }
            other => {
                warn!(r#type = other, "unknown pointer shape type");
                return;
            }
        };
        // For monochrome shapes DXGI reports the FULL buffer height (AND and
        // XOR masks stacked); the conversion expects the logical height.
        let height = if kind == MaskShapeKind::Monochrome {
            shape_info.Height / 2
        } else {
            shape_info.Height
        };
        let shape = mask_shape_to_rgba(
            kind,
            u16::try_from(shape_info.Width).unwrap_or(u16::MAX),
            u16::try_from(height).unwrap_or(u16::MAX),
            shape_info.Pitch as usize,
            u16::try_from(shape_info.HotSpot.x).unwrap_or(0),
            u16::try_from(shape_info.HotSpot.y).unwrap_or(0),
            &self.shape_buf,
        );
        if let Some(shape) = shape {
            info!(
                width = shape.width,
                height = shape.height,
                "cursor shape changed"
            );
            self.pending_cursor.shape = Some(shape);
        } else {
            warn!(
                kind = ?kind,
                width = shape_info.Width,
                height = shape_info.Height,
                pitch = shape_info.Pitch,
                buf_len = self.shape_buf.len(),
                "pointer shape buffer rejected by conversion"
            );
        }
    }
    /// Copies the desktop image out of the acquired `resource` and reads the
    /// dirty rects described by `info`.
    fn extract(
        &mut self,
        resource: &IDXGIResource,
        info: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> anyhow::Result<CapturedFrame> {
        let width = self.width;
        let height = self.height;
        let mut data = vec![0u8; width as usize * height as usize * 4];

        // Fast path: the desktop image is a CPU-mappable surface.
        let surface_map = (|| {
            let surface: IDXGISurface = resource.cast().ok()?;
            let mut mapped = DXGI_MAPPED_RECT::default();
            // SAFETY: `mapped` is a valid out-param; the surface is alive and
            // the returned pointer is used only until `Unmap`.
            unsafe { surface.Map(&mut mapped, DXGI_MAP_READ) }.ok()?;
            let result = (|| {
                anyhow::ensure!(!mapped.pBits.is_null(), "mapped pointer is null");
                let pitch = usize::try_from(mapped.Pitch).context("negative RowPitch")?;
                // SAFETY: `pBits` points to `Pitch * height` readable bytes
                // while the surface is mapped.
                let src =
                    unsafe { std::slice::from_raw_parts(mapped.pBits, pitch * height as usize) };
                copy_bgra_rows(src, pitch, width, height, &mut data)
            })();
            // SAFETY: the surface was mapped above and is unmapped exactly once.
            if let Err(e) = unsafe { surface.Unmap() }.ok().context("surface Unmap failed") {
                return Some(Err(e));
            }
            Some(result)
        })();

        match surface_map {
            Some(result) => result?,
            None => self.copy_via_staging(resource, &mut data)?,
        }

        let dirty_rects = self.read_dirty_rects(info, width, height)?;
        Ok(CapturedFrame {
            width,
            height,
            stride: width as usize * 4,
            data,
            dirty_rects,
        })
    }

    /// Slow-but-portable path: copies the desktop texture into a staging
    /// texture and maps that (needed on display stacks whose desktop image
    /// is GPU-only, e.g. the RDP indirect display driver).
    fn copy_via_staging(
        &mut self,
        resource: &IDXGIResource,
        data: &mut [u8],
    ) -> anyhow::Result<()> {
        let texture: ID3D11Texture2D = resource
            .cast()
            .context("frame resource is neither surface nor texture")?;

        if self.staging.is_none() {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.width,
                Height: self.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            // SAFETY: `desc` is fully initialized; `staging` is a valid out-param.
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) }
                .context("CreateTexture2D (staging) failed")?;
            self.staging = staging;
            info!("using staging-texture copy path (desktop image not mappable)");
        }
        let staging = self.staging.as_ref().expect("staging created above");

        // SAFETY: both textures live for the whole call.
        unsafe { self.context.CopyResource(staging, &texture) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `mapped` is a valid out-param; the staging texture is alive
        // and the returned pointer is used only until `Unmap`.
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .context("staging Map failed")?;
        let result = (|| {
            anyhow::ensure!(!mapped.pData.is_null(), "mapped pointer is null");
            let pitch = usize::try_from(mapped.RowPitch).context("negative RowPitch")?;
            // SAFETY: `pData` points to `RowPitch * height` readable bytes
            // while the staging texture is mapped.
            let src =
                unsafe { std::slice::from_raw_parts(mapped.pData.cast(), pitch * self.height as usize) };
            copy_bgra_rows(src, pitch, self.width, self.height, data)
        })();
        // SAFETY: the staging texture was mapped above and is unmapped once.
        unsafe { self.context.Unmap(staging, 0) };
        result
    }

    /// Reads the frame's dirty rects into `self.dirty_buf` (reused).
    ///
    /// A frame with metadata but no dirty rects (move-only updates) is
    /// reported as fully dirty — the encoder needs the new pixels regardless.
    fn read_dirty_rects(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Vec<Rect>> {
        if info.TotalMetadataBufferSize == 0 {
            return Ok(Vec::new());
        }
        let count = info.TotalMetadataBufferSize as usize / size_of::<RECT>();
        self.dirty_buf.resize(count, RECT::default());
        let mut needed = 0u32;
        // SAFETY: `dirty_buf` holds `TotalMetadataBufferSize` bytes worth of
        // properly aligned `RECT`s, as the API requires; `needed` is a valid
        // out-param.
        unsafe {
            self.duplication.GetFrameDirtyRects(
                info.TotalMetadataBufferSize,
                self.dirty_buf.as_mut_ptr(),
                &mut needed,
            )
        }
        .ok()
        .context("GetFrameDirtyRects failed")?;

        let rects: Vec<Rect> = self
            .dirty_buf
            .iter()
            .take(needed as usize / size_of::<RECT>())
            .map(|r| Rect {
                left: r.left,
                top: r.top,
                right: r.right,
                bottom: r.bottom,
            })
            .filter(|r| !r.is_empty())
            .collect();

        if rects.is_empty() {
            // Metadata present but no dirty rects: move-only update.
            Ok(vec![Rect {
                left: 0,
                top: 0,
                right: i32::try_from(width).unwrap_or(i32::MAX),
                bottom: i32::try_from(height).unwrap_or(i32::MAX),
            }])
        } else {
            Ok(rects)
        }
    }
}

/// Result of [`init_duplication`]: everything the source needs to capture.
struct DupInit {
    /// GDI device name of the chosen output (e.g. `\\.\DISPLAY2`).
    output_name: String,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    width: u32,
    height: u32,
}

/// One enumerated output with everything needed to duplicate it.
struct EnumeratedOutput {
    adapter: IDXGIAdapter1,
    output: IDXGIOutput1,
    candidate: OutputCandidate,
}

/// Converts a fixed-size NUL-terminated UTF-16 buffer (as DXGI uses for its
/// description fields) to a `String`.
fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Resolves GDI identity strings for a DXGI output device name
/// (`\\.\DISPLAYn`): the display adapter's DeviceString and the first
/// connected monitor's DeviceString. DXGI may report an output under a
/// generic adapter ("Microsoft Basic Render Driver"), so these GDI
/// strings are the reliable identity of the monitor. Returns an empty
/// string when nothing can be resolved.
fn gdi_device_string(device_name: &str) -> String {
    let name_wide: Vec<u16> = device_name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut parts: Vec<String> = Vec::new();
    for dev_num in 0..=1u32 {
        let mut dd = DISPLAY_DEVICEW {
            cb: u32::try_from(std::mem::size_of::<DISPLAY_DEVICEW>()).expect("struct fits in u32"),
            ..DISPLAY_DEVICEW::default()
        };
        // SAFETY: `dd` is a valid, correctly sized out buffer and
        // `name_wide` is NUL-terminated for the whole call.
        let ok = unsafe {
            EnumDisplayDevicesW(
                windows::core::PCWSTR(name_wide.as_ptr()),
                dev_num,
                &mut dd,
                0,
            )
        };
        if !ok.as_bool() {
            break;
        }
        let s = wide_to_string(&dd.DeviceString);
        if !s.is_empty() {
            parts.push(s);
        }
        let id = wide_to_string(&dd.DeviceID);
        if !id.is_empty() {
            parts.push(id);
        }
    }
    parts.join(" | ")
}

/// Enumerates every output of every adapter, adapter-major (the order
/// `IDXGIFactory1::EnumAdapters1`/`EnumOutputs` reports them).
fn enumerate_outputs(factory: &IDXGIFactory1) -> Vec<EnumeratedOutput> {
    let mut out = Vec::new();
    for adapter_idx in 0.. {
        // SAFETY: standard enumeration; ends with DXGI_ERROR_NOT_FOUND.
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(adapter_idx) } {
            Ok(a) => a,
            Err(_) => break,
        };
        // SAFETY: plain getter on a live COM object.
        let adapter_desc = unsafe { adapter.GetDesc1() }
            .map(|d| wide_to_string(&d.Description))
            .unwrap_or_default();
        for output_idx in 0.. {
            // SAFETY: standard enumeration; ends with DXGI_ERROR_NOT_FOUND.
            let Ok(output) = (unsafe { adapter.EnumOutputs(output_idx) }) else {
                break;
            };
            // SAFETY: plain getter on a live COM object.
            let name = unsafe { output.GetDesc() }
                .map(|d| wide_to_string(&d.DeviceName))
                .unwrap_or_default();
            let Ok(output1) = output.cast::<IDXGIOutput1>() else {
                continue;
            };
            let gdi_name = gdi_device_string(&name);
            out.push(EnumeratedOutput {
                adapter: adapter.clone(),
                output: output1,
                candidate: OutputCandidate {
                    adapter: adapter_desc.clone(),
                    name,
                    gdi_name,
                    index_in_adapter: output_idx,
                },
            });
        }
    }
    out
}

/// Selects the output matching `selector`, creates a D3D11 device on the
/// adapter owning it (a hard requirement of `DuplicateOutput`) and
/// duplicates it.
fn init_duplication(selector: &OutputSelector, quiet: bool) -> anyhow::Result<DupInit> {
    // SAFETY: COM factory creation with a valid out-param.
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1 failed")?;

    let enumerated = enumerate_outputs(&factory);
    let candidates: Vec<OutputCandidate> =
        enumerated.iter().map(|e| e.candidate.clone()).collect();
    let pos = select_output(&candidates, selector).with_context(|| {
        let available = candidates
            .iter()
            .map(|c| format!("{} on {} [{}]", c.name, c.adapter, c.gdi_name))
            .collect::<Vec<_>>()
            .join(", ");
        format!("no output matches {selector:?} (available: {available})")
    })?;
    let chosen = &enumerated[pos];

    // DuplicateOutput rejects a D3D11 device created on a different adapter
    // than the output's, so the device is created here, after the selection
    // (an explicit adapter requires D3D_DRIVER_TYPE_UNKNOWN).
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let feature_levels = [D3D_FEATURE_LEVEL_11_0];
    // SAFETY: standard device creation; all out-params are valid.
    unsafe {
        D3D11CreateDevice(
            &chosen.adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .with_context(|| {
        format!("D3D11CreateDevice failed on adapter {}", chosen.candidate.adapter)
    })?;
    let device = device.context("no D3D11 device returned")?;

    // SAFETY: the output and device are alive and share the same adapter.
    let duplication = unsafe { chosen.output.DuplicateOutput(&device) }
        .context("DuplicateOutput failed (needs an interactive desktop session)")?;
    let dup_desc = unsafe { duplication.GetDesc() };
    if dup_desc.Rotation != DXGI_MODE_ROTATION_IDENTITY {
        warn!(
            rotation = ?dup_desc.Rotation,
            "output is rotated; capture assumes identity rotation (not yet supported)"
        );
    }
    anyhow::ensure!(
        dup_desc.ModeDesc.Format == DXGI_FORMAT_B8G8R8A8_UNORM,
        "unexpected desktop format {:?}",
        dup_desc.ModeDesc.Format
    );
    if quiet {
        tracing::debug!(
            adapter = %chosen.candidate.adapter,
            output = %chosen.candidate.name,
            "DXGI duplication ready (storm)"
        );
    } else {
        info!(
            adapter = %chosen.candidate.adapter,
            output = %chosen.candidate.name,
            width = dup_desc.ModeDesc.Width,
            height = dup_desc.ModeDesc.Height,
            "DXGI duplication ready"
        );
    }
    Ok(DupInit {
        output_name: chosen.candidate.name.clone(),
        device,
        context: context.context("no D3D11 device context returned")?,
        duplication,
        width: dup_desc.ModeDesc.Width,
        height: dup_desc.ModeDesc.Height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Manual capture benchmark: `cargo test -p server --features
    /// windows-capture -- dxgi --ignored --nocapture` on a Windows machine
    /// with an interactive desktop.
    #[test]
    #[ignore = "requires an interactive Windows desktop session"]
    fn dxgi_capture_bench() {
        let mut src =
            DxgiSource::new(&OutputSelector::Index(0)).expect("Desktop Duplication available");
        let (w, h) = src.dimensions();
        eprintln!("output 0: {w}x{h}");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut frames = 0u32;
        let mut no_change = 0u32;
        let mut acquire_total = Duration::ZERO;
        let mut dirty_total = 0usize;
        let mut full_dirty = 0u32;
        while Instant::now() < deadline {
            let t0 = Instant::now();
            match src.next_frame(Duration::from_millis(100)) {
                Ok(Some(f)) => {
                    acquire_total += t0.elapsed();
                    frames += 1;
                    dirty_total += f.dirty_rects.len();
                    let full = w as i64 * h as i64;
                    if Rect::bounding(&f.dirty_rects)
                        .is_some_and(|r| i64::from(r.right - r.left) * i64::from(r.bottom - r.top) >= full)
                    {
                        full_dirty += 1;
                    }
                }
                Ok(None) => no_change += 1,
                Err(e) => panic!("capture failed: {e:#}"),
            }
        }
        eprintln!(
            "5s: {frames} frames captured ({:.1} fps), {no_change} empty polls, \
             avg acquire+copy {:.2} ms, avg dirty rects {:.1}, full-dirty {full_dirty}/{frames}",
            f64::from(frames) / 5.0,
            acquire_total.as_secs_f64() * 1000.0 / f64::from(frames.max(1)),
            dirty_total as f64 / f64::from(frames.max(1)),
        );
        assert!(frames > 0, "no frames captured in 5s (idle desktop is fine, but zero frames + zero dirty means the duplication is not delivering)");
    }
}
