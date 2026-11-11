// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! H.264 decoder backed by Cisco's OpenH264 binary, loaded dynamically at
//! runtime (§18.75.8). The library is never redistributed nor built from
//! source — Cisco's own binary keeps the decode side royalty-free for us.
//! When the library is absent the client simply reports no H.264 decode
//! capability and the server stays on VP9.
//!
//! Loader mirrors the uds-client webcam pattern: probe next to the
//! executable first, then well-known install dirs, then the system search
//! path. Struct layouts verified against OpenH264 v2.6.0 headers
//! (`codec/api/wels/codec_{def,app_def,api}.h`).

use std::ffi::{c_char, c_int, c_long, c_uint, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use crate::Result;
use crate::color::{I420Frame, i420_to_bgra};
use crate::error::CodecError;
use crate::video::{DecodedFrame, EncodedPacket, VideoCodecType, VideoDecoder};

#[cfg(target_os = "windows")]
const LIB_NAMES: &[&str] = &[
    "openh264.dll",
    "openh264-2.6.0-win64.dll",
    "openh264-2.5.1-win64.dll",
];
#[cfg(target_os = "macos")]
const LIB_NAMES: &[&str] = &["libopenh264.8.dylib", "libopenh264.dylib"];
#[cfg(target_os = "linux")]
const LIB_NAMES: &[&str] = &["libopenh264.so", "libopenh264.so.8", "libopenh264.so.7"];

#[cfg(target_os = "macos")]
const SYSTEM_DIRS: &[&str] = &["/Library/Application Support/UDSLauncher/openh264"];
#[cfg(not(target_os = "macos"))]
const SYSTEM_DIRS: &[&str] = &[];

// ---------------------------------------------------------------------------
// FFI types (OpenH264 C API, v2.6.0 layouts)
// ---------------------------------------------------------------------------

const VIDEO_BITSTREAM_AVC: c_int = 0;
const ERROR_CON_SLICE_MV_COPY_CROSS_IDR_FREEZE_RES_CHANGE: c_int = 7;
const VIDEO_FORMAT_I420: c_int = 23;
const DS_ERROR_FREE: c_int = 0;

/// The decoder object handle: a pointer to a C++ object whose first field is
/// the vtable pointer (OpenH264 C API's `ISVCDecoder*` = object, `*object` =
/// vtable). Method calls therefore go through a double indirection.
type DecoderObj = *mut *const ISVCDecoderVtbl;

#[allow(non_snake_case)]
#[repr(C)]
struct SSysMEMBuffer {
    iWidth: c_int,
    iHeight: c_int,
    iFormat: c_int,
    iStride: [c_int; 2],
}

#[allow(non_snake_case)]
#[repr(C)]
struct SBufferInfo {
    iBufferStatus: c_int,
    uiInBsTimeStamp: u64,
    uiOutYuvTimeStamp: u64,
    // The C union holds only SSysMEMBuffer in the 2.x public header.
    UsrData: SSysMEMBuffer,
    pDst: [*mut u8; 3],
}

#[allow(non_snake_case)]
#[repr(C)]
struct SVideoProperty {
    size: c_uint,
    eVideoBsType: c_int,
}

#[allow(non_snake_case)]
#[repr(C)]
struct SDecodingParam {
    pFileNameRestructed: *mut c_char,
    uiCpuLoad: c_uint,
    uiTargetDqLayer: u8,
    eEcActiveIdc: c_int,
    bParseOnly: bool,
    sVideoProperty: SVideoProperty,
}

#[allow(non_snake_case)]
#[repr(C)]
struct ISVCDecoderVtbl {
    Initialize: unsafe extern "C" fn(DecoderObj, *const SDecodingParam) -> c_long,
    Uninitialize: unsafe extern "C" fn(DecoderObj) -> c_long,
    DecodeFrame:
        unsafe extern "C" fn(DecoderObj, *const u8, c_int, *mut *mut u8, *mut SBufferInfo) -> c_int,
    DecodeFrameNoDelay:
        unsafe extern "C" fn(DecoderObj, *const u8, c_int, *mut *mut u8, *mut SBufferInfo) -> c_int,
    DecodeFrame2:
        unsafe extern "C" fn(DecoderObj, *const u8, c_int, *mut *mut u8, *mut SBufferInfo) -> c_int,
    FlushFrame: unsafe extern "C" fn(DecoderObj, *mut *mut u8, *mut SBufferInfo) -> c_int,
    DecodeParser: unsafe extern "C" fn(DecoderObj, *const u8, c_int, *mut c_void) -> c_int,
    DecodeFrameEx: unsafe extern "C" fn(
        DecoderObj,
        *const u8,
        c_int,
        *mut *mut u8,
        c_int,
        *mut SBufferInfo,
    ) -> c_int,
    SetOption: unsafe extern "C" fn(DecoderObj, c_int, *mut c_void) -> c_long,
    GetOption: unsafe extern "C" fn(DecoderObj, c_int, *mut c_void) -> c_long,
}

type WelsCreateDecoderFn = unsafe extern "C" fn(*mut DecoderObj) -> c_int;
type WelsDestroyDecoderFn = unsafe extern "C" fn(DecoderObj);

struct OpenH264Api {
    create: WelsCreateDecoderFn,
    destroy: WelsDestroyDecoderFn,
    // Keeps the library mapped for the whole process lifetime.
    _lib: libloading::Library,
}

// Function pointers obtained from a shared library are safe to share.
unsafe impl Send for OpenH264Api {}
unsafe impl Sync for OpenH264Api {}

static API: OnceLock<Option<Arc<OpenH264Api>>> = OnceLock::new();
static AVAILABLE: AtomicBool = AtomicBool::new(false);

fn candidate_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        paths.extend(LIB_NAMES.iter().map(|name| dir.join(name)));
    }
    for dir in SYSTEM_DIRS {
        paths.extend(LIB_NAMES.iter().map(|name| PathBuf::from(dir).join(name)));
    }
    paths
}

fn load_api() -> Option<Arc<OpenH264Api>> {
    let mut lib = None;
    for path in candidate_paths() {
        if path.exists() {
            match unsafe { libloading::Library::new(&path) } {
                Ok(l) => {
                    tracing::info!("OpenH264 loaded from {}", path.display());
                    lib = Some(l);
                    break;
                }
                Err(e) => tracing::warn!("OpenH264 at {} failed to load: {e}", path.display()),
            }
        }
    }
    if lib.is_none() {
        for &name in LIB_NAMES {
            match unsafe { libloading::Library::new(name) } {
                Ok(l) => {
                    tracing::info!("OpenH264 loaded from system path: {name}");
                    lib = Some(l);
                    break;
                }
                Err(e) => tracing::debug!("OpenH264 {name}: {e}"),
            }
        }
    }
    let lib = lib?;

    let api = unsafe {
        let create = *lib
            .get::<WelsCreateDecoderFn>(b"WelsCreateDecoder")
            .map_err(|e| tracing::warn!("WelsCreateDecoder symbol missing: {e}"))
            .ok()?;
        let destroy = *lib
            .get::<WelsDestroyDecoderFn>(b"WelsDestroyDecoder")
            .map_err(|e| tracing::warn!("WelsDestroyDecoder symbol missing: {e}"))
            .ok()?;
        OpenH264Api {
            create,
            destroy,
            _lib: lib,
        }
    };
    Some(Arc::new(api))
}

/// True when the Cisco OpenH264 binary is present and loadable. Probed once.
pub fn h264_available() -> bool {
    let api = API.get_or_init(|| {
        let api = load_api();
        AVAILABLE.store(api.is_some(), Ordering::Relaxed);
        if api.is_none() {
            tracing::info!("OpenH264 not found: H.264 decode unavailable (VP9 fallback)");
        }
        api
    });
    AVAILABLE.load(Ordering::Relaxed) && api.is_some()
}

/// H.264 decoder over the OpenH264 binary. Single-threaded use, like every
/// other `VideoDecoder` implementation.
pub struct OpenH264Decoder {
    api: Arc<OpenH264Api>,
    obj: DecoderObj,
}

// OpenH264 decoder instances are not thread-safe but are movable; the
// channel layer uses each decoder from a single thread at a time.
unsafe impl Send for OpenH264Decoder {}

impl OpenH264Decoder {
    /// Creates and initializes a decoder; fails when the library is absent.
    pub fn new() -> Result<Self> {
        if !h264_available() {
            return Err(CodecError::Unsupported(
                "OpenH264 binary not available on this system",
            ));
        }
        let api = API
            .get()
            .and_then(Option::as_ref)
            .cloned()
            .ok_or(CodecError::Unsupported("OpenH264 not loaded"))?;

        let mut obj: DecoderObj = ptr::null_mut();
        let rc = unsafe { (api.create)(&raw mut obj) };
        if rc != 0 || obj.is_null() {
            return Err(CodecError::Backend(format!(
                "WelsCreateDecoder failed: {rc}"
            )));
        }

        let param = SDecodingParam {
            pFileNameRestructed: ptr::null_mut(),
            uiCpuLoad: 0,
            uiTargetDqLayer: 0xFF,
            // Most aggressive error concealment: our video path is lossy UDP
            // and a concealed frame is always better than a dropped one.
            eEcActiveIdc: ERROR_CON_SLICE_MV_COPY_CROSS_IDR_FREEZE_RES_CHANGE,
            bParseOnly: false,
            sVideoProperty: SVideoProperty {
                size: c_uint::try_from(std::mem::size_of::<SVideoProperty>())
                    .expect("SVideoProperty fits in c_uint"),
                eVideoBsType: VIDEO_BITSTREAM_AVC,
            },
        };
        let rc = unsafe { ((**obj).Initialize)(obj, &raw const param) };
        if rc != 0 {
            unsafe { (api.destroy)(obj) };
            return Err(CodecError::Backend(format!(
                "OpenH264 Initialize failed: {rc}"
            )));
        }
        Ok(Self { api, obj })
    }
}

impl VideoDecoder for OpenH264Decoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<Option<DecodedFrame>> {
        if packet.data.is_empty() {
            return Err(CodecError::Malformed);
        }
        let mut dst: [*mut u8; 3] = [ptr::null_mut(); 3];
        let mut info = SBufferInfo {
            iBufferStatus: 0,
            uiInBsTimeStamp: 0,
            uiOutYuvTimeStamp: 0,
            UsrData: SSysMEMBuffer {
                iWidth: 0,
                iHeight: 0,
                iFormat: 0,
                iStride: [0; 2],
            },
            pDst: [ptr::null_mut(); 3],
        };
        let len = c_int::try_from(packet.data.len()).map_err(|_| CodecError::Malformed)?;
        // DecodeFrameNoDelay: screen content has no B-frames, so every input
        // frame must yield its output immediately (§18.75.8).
        let state = unsafe {
            ((**self.obj).DecodeFrameNoDelay)(
                self.obj,
                packet.data.as_ptr(),
                len,
                dst.as_mut_ptr(),
                &raw mut info,
            )
        };
        if info.iBufferStatus == 0 || dst[0].is_null() {
            // Corrupt/undecodable packets are expected on the lossy path;
            // the channel layer already requested a keyframe for recovery.
            return Err(CodecError::Backend(format!(
                "OpenH264 produced no frame (state 0x{state:x})"
            )));
        }
        if state != DS_ERROR_FREE {
            // Error concealment already patched the frame; keep it.
            tracing::debug!("OpenH264 decode flags: 0x{state:x}");
        }
        let buf = &info.UsrData;
        if buf.iFormat != VIDEO_FORMAT_I420 {
            return Err(CodecError::Unsupported(
                "OpenH264 returned a non-I420 frame",
            ));
        }
        if buf.iWidth <= 0 || buf.iHeight <= 0 {
            return Err(CodecError::Malformed);
        }
        // §18.78: hard ceiling on decoded geometry before any plane copy.
        if buf.iWidth as u32 > crate::video::MAX_DECODE_DIM
            || buf.iHeight as u32 > crate::video::MAX_DECODE_DIM
        {
            return Err(CodecError::Malformed);
        }
        let w = usize::try_from(buf.iWidth).map_err(|_| CodecError::Malformed)?;
        let h = usize::try_from(buf.iHeight).map_err(|_| CodecError::Malformed)?;
        let y_stride = usize::try_from(buf.iStride[0]).map_err(|_| CodecError::Malformed)?;
        let uv_stride = usize::try_from(buf.iStride[1]).map_err(|_| CodecError::Malformed)?;
        if y_stride < w || uv_stride < w.div_ceil(2) {
            return Err(CodecError::Malformed);
        }
        let copy_plane = |src: *const u8, src_stride: usize, rows: usize, cols: usize| -> Vec<u8> {
            let mut out = Vec::with_capacity(rows * cols);
            for row in 0..rows {
                let line = unsafe { std::slice::from_raw_parts(src.add(row * src_stride), cols) };
                out.extend_from_slice(line);
            }
            out
        };
        let frame = I420Frame {
            width: u32::try_from(buf.iWidth).map_err(|_| CodecError::Malformed)?,
            height: u32::try_from(buf.iHeight).map_err(|_| CodecError::Malformed)?,
            y_stride: w,
            uv_stride: w.div_ceil(2),
            y: copy_plane(dst[0], y_stride, h, w),
            u: copy_plane(dst[1], uv_stride, h.div_ceil(2), w.div_ceil(2)),
            v: copy_plane(dst[2], uv_stride, h.div_ceil(2), w.div_ceil(2)),
        };
        i420_to_bgra(&frame).map(Some)
    }

    fn codec_type(&self) -> VideoCodecType {
        VideoCodecType::H264
    }
}

impl Drop for OpenH264Decoder {
    fn drop(&mut self) {
        unsafe {
            ((**self.obj).Uninitialize)(self.obj);
            (self.api.destroy)(self.obj);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_paths_cover_every_library_name() {
        let names: Vec<String> = candidate_paths()
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_owned))
            .collect();
        for expected in LIB_NAMES {
            // Non-macOS builds only probe the exe dir; on macOS the system
            // dir adds the soname variants. Either way every name must be
            // reachable somewhere.
            assert!(
                names.iter().any(|n| n == expected),
                "{expected} is never searched for"
            );
        }
    }

    #[test]
    #[ignore = "requires the Cisco OpenH264 binary installed on the host"]
    fn manual_openh264_load() {
        assert!(h264_available(), "OpenH264 binary not found");
    }

    #[test]
    #[ignore = "requires the Cisco OpenH264 binary and OPENH264_TEST_FRAME"]
    fn manual_openh264_decode_smoke() {
        // Needs a real H.264 Annex-B stream with several frames; generate one
        // with:
        //   ffmpeg -f lavfi -i testsrc=size=320x240:rate=10:duration=1 \
        //     -pix_fmt yuv420p -c:v libx264 -bf 0 -f h264 /tmp/ten.h264
        // and point OPENH264_TEST_FRAME at it. OpenH264 is 4:2:0-only.
        let path = std::env::var("OPENH264_TEST_FRAME").expect("OPENH264_TEST_FRAME not set");
        let data = std::fs::read(path).expect("test stream unreadable");
        let mut dec = OpenH264Decoder::new().expect("decoder init failed");
        // Feed the stream NAL unit by NAL unit, like the wire does.
        let mut starts: Vec<usize> = Vec::new();
        let mut i = 0;
        while let Some(pos) = data[i..]
            .windows(4)
            .position(|w| w == [0, 0, 0, 1])
            .map(|p| p + i)
        {
            starts.push(pos);
            i = pos + 1;
        }
        starts.push(data.len());
        let mut decoded = 0;
        for w in starts.windows(2) {
            let nal = data[w[0]..w[1]].to_vec();
            if let Ok(Some(frame)) = dec.decode(&EncodedPacket {
                data: nal,
                keyframe: decoded == 0,
            }) {
                assert_eq!(frame.width, 320);
                assert_eq!(frame.height, 240);
                assert_eq!(frame.data.len(), 320 * 240 * 4);
                decoded += 1;
            }
        }
        assert!(decoded > 5, "only {decoded} frames decoded out of 10");
    }
}
