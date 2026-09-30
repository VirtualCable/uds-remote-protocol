//! DXGI Desktop Duplication capture backend (Windows only).
//!
//! Captures the desktop of one output (monitor) through the Desktop
//! Duplication API. GPU-side, delivers dirty rects, and survives desktop
//! changes (resolution switch, lock, UAC) by reinitializing on
//! `DXGI_ERROR_ACCESS_LOST` without breaking the session.

use std::time::Duration;

use anyhow::Context as _;
use tracing::{info, warn};
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, IDXGISurface, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_MAPPED_RECT, DXGI_MAP_READ, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::core::Interface;

use crate::capture::{copy_bgra_rows, CapturedFrame, FrameSource, Rect};

/// Desktop Duplication source for a single output.
pub struct DxgiSource {
    output_index: u32,
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
    dirty_buf: Vec<u8>,
}

impl DxgiSource {
    /// Captures `output_index` (0 = first enumerated output, usually primary).
    pub fn new(output_index: u32) -> anyhow::Result<Self> {
        let dup = init_duplication(output_index)?;
        Ok(Self {
            output_index,
            device: dup.device,
            context: dup.context,
            duplication: dup.duplication,
            width: dup.width,
            height: dup.height,
            staging: None,
            dirty_buf: Vec::with_capacity(64 * size_of::<RECT>()),
        })
    }

    /// Re-creates the duplication after `DXGI_ERROR_ACCESS_LOST`.
    fn reinit(&mut self) -> anyhow::Result<()> {
        info!(output = self.output_index, "DXGI access lost, reinitializing");
        let dup = init_duplication(self.output_index)?;
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
                return Ok(None);
            }
            if e.code() == DXGI_ERROR_ACCESS_LOST {
                self.reinit()?;
                return Ok(None);
            }
        }
        result.context("AcquireNextFrame failed")?;
        let resource = resource.context("AcquireNextFrame returned no resource")?;

        // Every acquired frame MUST be released, even on error paths.
        let result = self.extract(&resource, &info);
        // SAFETY: `resource` was acquired above and is released exactly once.
        unsafe { self.duplication.ReleaseFrame() }
            .ok()
            .context("ReleaseFrame failed")?;
        result.map(Some)
    }
}

impl DxgiSource {
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
        self.dirty_buf
            .resize(info.TotalMetadataBufferSize as usize, 0);
        let mut needed = 0u32;
        // SAFETY: `dirty_buf` is `TotalMetadataBufferSize` bytes long, as the
        // API requires; `needed` is a valid out-param.
        unsafe {
            self.duplication.GetFrameDirtyRects(
                info.TotalMetadataBufferSize,
                self.dirty_buf.as_mut_ptr().cast::<RECT>(),
                &mut needed,
            )
        }
        .ok()
        .context("GetFrameDirtyRects failed")?;

        let rects: Vec<Rect> = self
            .dirty_buf
            .as_chunks::<{ size_of::<RECT>() }>()
            .0
            .iter()
            .take((needed as usize) / size_of::<RECT>())
            .map(|chunk| {
                // SAFETY: `chunk` is exactly one `RECT` written by the API.
                let r = unsafe { &*chunk.as_ptr().cast::<RECT>() };
                Rect {
                    left: r.left,
                    top: r.top,
                    right: r.right,
                    bottom: r.bottom,
                }
            })
            .filter(|r| !r.is_empty())
            .collect();

        if rects.is_empty() {
            // Metadata present but no dirty rects: move-only update.
            Ok(vec![Rect {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            }])
        } else {
            Ok(rects)
        }
    }
}

/// Result of [`init_duplication`]: everything the source needs to capture.
struct Duplication {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    width: u32,
    height: u32,
}

/// Creates a D3D11 device and duplicates `output_index`.
fn init_duplication(output_index: u32) -> anyhow::Result<Duplication> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let feature_levels = [D3D_FEATURE_LEVEL_11_0];
    // SAFETY: standard device creation; all out-params are valid.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .context("D3D11CreateDevice failed")?;
    let device = device.context("no D3D11 device returned")?;

    // SAFETY: COM factory creation with valid out-params.
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1 failed")?;

    let mut adapter_idx = 0u32;
    loop {
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(adapter_idx) } {
            Ok(a) => a,
            Err(_) => break,
        };
        if let Ok(output) = unsafe { adapter.EnumOutputs(output_index) } {
            let output1: IDXGIOutput1 =
                output.cast().context("output does not expose IDXGIOutput1")?;
            let duplication = unsafe { output1.DuplicateOutput(&device) }
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
            info!(
                output = output_index,
                width = dup_desc.ModeDesc.Width,
                height = dup_desc.ModeDesc.Height,
                "DXGI duplication ready"
            );
            return Ok(Duplication {
                device: device.clone(),
                context: context.clone().context("no D3D11 device context returned")?,
                duplication,
                width: dup_desc.ModeDesc.Width,
                height: dup_desc.ModeDesc.Height,
            });
        }
        adapter_idx += 1;
    }
    anyhow::bail!("output index {output_index} not found on any adapter")
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
        let mut src = DxgiSource::new(0).expect("Desktop Duplication available");
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
