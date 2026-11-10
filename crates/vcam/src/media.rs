// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! DirectShow media type construction for the offered formats.
//!
//! All `AM_MEDIA_TYPE`/`VIDEOINFOHEADER` blocks handed out here are
//! allocated with `CoTaskMemAlloc` — ownership passes to the caller per the
//! DirectShow contract (`DeleteMediaType` / `CoTaskMemFree`).

// DShow structures mix i32/u32 sizes by design; casts are bounded by the
// fixed maximum geometry (1920x1080).
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    // CoTaskMemAlloc buffers are sufficiently aligned for the structs we write.
    clippy::cast_ptr_alignment
)]

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::Media::DirectShow::VIDEO_STREAM_CONFIG_CAPS;
use windows::Win32::Media::MediaFoundation::{
    AM_MEDIA_TYPE, FORMAT_VideoInfo, MEDIASUBTYPE_NV12, MEDIASUBTYPE_RGB24, MEDIATYPE_Video,
    VIDEOINFOHEADER,
};
use windows::Win32::System::Com::CoTaskMemAlloc;
use windows::core::GUID;

use crate::pixel;

/// Standard geometries the filter offers, in ascending order. Consumers
/// (browsers) typically SetFormat one of these before connecting.
pub const GEOMETRIES: [(u32, u32); 6] = [
    (320, 240),
    (640, 480),
    (800, 600),
    (1280, 720),
    (1600, 900),
    (1920, 1080),
];

/// One enumerated offer: format + geometry.
#[must_use]
pub fn offer(index: usize) -> Option<(CamFormat, u32, u32)> {
    let (fi, gi) = (index % CamFormat::ALL.len(), index / CamFormat::ALL.len());
    let &(w, h) = GEOMETRIES.get(gi)?;
    Some((CamFormat::ALL[fi], w, h))
}

/// Total number of offers (formats × geometries).
#[must_use]
pub fn offer_count() -> usize {
    CamFormat::ALL.len() * GEOMETRIES.len()
}

/// Acceptable negotiated geometry: even dimensions inside sane bounds
/// (NV12 chroma subsampling requires even sizes).
#[must_use]
pub fn valid_geometry(w: u32, h: u32) -> bool {
    w % 2 == 0 && h % 2 == 0 && (160..=1920).contains(&w) && (120..=1080).contains(&h)
}

/// FourCC "NV12" for `biCompression`.
const BI_NV12: u32 = 0x3256_564E;

/// Pixel formats the filter can deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CamFormat {
    /// NV12 (Y + interleaved UV), the format most camera consumers prefer.
    Nv12,
    /// RGB24 bottom-up DIB, the classic DShow fallback.
    Rgb24,
}

impl CamFormat {
    /// All offered formats, in preference order.
    pub const ALL: [CamFormat; 2] = [CamFormat::Nv12, CamFormat::Rgb24];

    fn subtype(self) -> GUID {
        match self {
            CamFormat::Nv12 => MEDIASUBTYPE_NV12,
            CamFormat::Rgb24 => MEDIASUBTYPE_RGB24,
        }
    }

    /// Frame size in bytes for this format.
    pub fn frame_bytes(self, width: u32, height: u32) -> usize {
        match self {
            CamFormat::Nv12 => pixel::nv12_len(width, height),
            CamFormat::Rgb24 => pixel::rgb24_len(width, height),
        }
    }
}

/// Builds a fully-allocated `AM_MEDIA_TYPE` (with `VIDEOINFOHEADER`) for a
/// format and geometry at the given fps.
///
/// # Safety
/// The returned struct owns two COM task-memory blocks; the caller must
/// eventually hand it to DirectShow or free them with
/// [`free_media_type`].
pub unsafe fn make_media_type(
    format: CamFormat,
    width: u32,
    height: u32,
    fps: u32,
) -> Option<*mut AM_MEDIA_TYPE> {
    unsafe {
        let frame = format.frame_bytes(width, height);
        let mt = CoTaskMemAlloc(size_of::<AM_MEDIA_TYPE>()).cast::<AM_MEDIA_TYPE>();
        if mt.is_null() {
            return None;
        }
        let vih = CoTaskMemAlloc(size_of::<VIDEOINFOHEADER>()).cast::<VIDEOINFOHEADER>();
        if vih.is_null() {
            windows::Win32::System::Com::CoTaskMemFree(Some(mt.cast()));
            return None;
        }
        std::ptr::write_bytes(mt, 0, 1);
        std::ptr::write_bytes(vih, 0, 1);

        let rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        (*vih).rcSource = rect;
        (*vih).rcTarget = rect;
        (*vih).dwBitRate = (frame * fps as usize) as u32 * 8;
        (*vih).AvgTimePerFrame = 10_000_000 / i64::from(fps.max(1));
        (*vih).bmiHeader = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: height as i32, // positive: bottom-up for RGB; NV12 top-down by convention
            biPlanes: 1,
            biBitCount: match format {
                CamFormat::Nv12 => 12,
                CamFormat::Rgb24 => 24,
            },
            biCompression: match format {
                CamFormat::Nv12 => BI_NV12,
                CamFormat::Rgb24 => 0, // BI_RGB
            },
            biSizeImage: frame as u32,
            ..Default::default()
        };

        (*mt).majortype = MEDIATYPE_Video;
        (*mt).subtype = format.subtype();
        (*mt).bFixedSizeSamples = true.into();
        (*mt).bTemporalCompression = false.into();
        (*mt).lSampleSize = frame as u32;
        (*mt).formattype = FORMAT_VideoInfo;
        (*mt).cbFormat = size_of::<VIDEOINFOHEADER>() as u32;
        (*mt).pbFormat = vih.cast();
        Some(mt)
    }
}

/// Frees a media type produced by [`make_media_type`] (DirectShow style).
///
/// # Safety
/// `mt` must come from [`make_media_type`] or be null.
pub unsafe fn free_media_type(mt: *mut AM_MEDIA_TYPE) {
    unsafe {
        if mt.is_null() {
            return;
        }
        if !(*mt).pbFormat.is_null() {
            windows::Win32::System::Com::CoTaskMemFree(Some((*mt).pbFormat.cast()));
        }
        windows::Win32::System::Com::CoTaskMemFree(Some(mt.cast()));
    }
}

/// Parses an `AM_MEDIA_TYPE`: format, width, height, fps — if it is a video
/// type we offer.
///
/// # Safety
/// `mt` must be a valid DirectShow media type pointer (may be null).
pub unsafe fn parse_media_type(mt: *const AM_MEDIA_TYPE) -> Option<(CamFormat, u32, u32, u32)> {
    unsafe {
        if mt.is_null()
            || (*mt).majortype != MEDIATYPE_Video
            || (*mt).formattype != FORMAT_VideoInfo
            || (*mt).cbFormat < size_of::<VIDEOINFOHEADER>() as u32
            || (*mt).pbFormat.is_null()
        {
            return None;
        }
        let format = if (*mt).subtype == MEDIASUBTYPE_NV12 {
            CamFormat::Nv12
        } else if (*mt).subtype == MEDIASUBTYPE_RGB24 {
            CamFormat::Rgb24
        } else {
            return None;
        };
        let vih = &*((*mt).pbFormat.cast::<VIDEOINFOHEADER>());
        let fps = if vih.AvgTimePerFrame > 0 {
            (10_000_000 / vih.AvgTimePerFrame) as u32
        } else {
            30
        };
        Some((
            format,
            vih.bmiHeader.biWidth.max(0) as u32,
            vih.bmiHeader.biHeight.unsigned_abs(),
            fps.max(1),
        ))
    }
}

/// Fills a `VIDEO_STREAM_CONFIG_CAPS` declaring a single fixed geometry
/// (min == max) for the current dimensions.
///
/// # Safety
/// `caps` must point to at least `size_of::<VIDEO_STREAM_CONFIG_CAPS>()`
/// writable bytes.
pub unsafe fn fill_stream_caps(caps: *mut u8, width: u32, height: u32, fps: u32) {
    unsafe {
        let c = caps.cast::<VIDEO_STREAM_CONFIG_CAPS>();
        std::ptr::write_bytes(c, 0, 1);
        let frame_dur = 10_000_000 / i64::from(fps.max(1));
        let size = windows::Win32::Foundation::SIZE {
            cx: width as i32,
            cy: height as i32,
        };
        (*c).guid = windows::Win32::Media::MediaFoundation::PIN_CATEGORY_CAPTURE;
        (*c).InputSize = size;
        (*c).MinFrameInterval = frame_dur;
        (*c).MaxFrameInterval = frame_dur;
        (*c).CropGranularityX = 1;
        (*c).CropGranularityY = 1;
        (*c).MinOutputSize = size;
        (*c).MaxOutputSize = size;
        (*c).OutputGranularityX = 1;
        (*c).OutputGranularityY = 1;
        (*c).MinBitsPerSecond = 1;
        (*c).MaxBitsPerSecond = (width * height * 24 * fps).max(1) as i32;
        (*c).MinCroppingSize = size;
        (*c).MaxCroppingSize = size;
        (*c).StretchTapsX = 1;
        (*c).StretchTapsY = 1;
        (*c).ShrinkTapsX = 1;
        (*c).ShrinkTapsY = 1;
    }
}
