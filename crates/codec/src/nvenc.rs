// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! H.264 hardware encoder backed by NVIDIA NVENC, loaded dynamically at
//! runtime from `nvEncodeAPI64.dll` (shipped with the NVIDIA driver — never
//! redistributed, no CUDA, no SDK on the build machine; §18.75.9).
//!
//! NVIDIA's own Media Foundation encoder is broken on current drivers
//! (`ProcessOutput` fails with 0x8000FFFF in every configuration, while
//! `ffmpeg -c:v h264_nvenc` works), so we bind the NVENC API directly like
//! ffmpeg does. Struct layouts are verified against `nvEncodeAPI.h`
//! (NVENC API 13.1) with a C offset checker; the test module asserts every
//! size we rely on.
//!
//! v1 feeds CPU-side NV12 buffers (converted from BGRA via
//! [`crate::color::bgra_to_nv12`]); zero-copy D3D11 input through
//! `nvEncRegisterResource` reusing the VideoProcessor chain of `mf.rs` is a
//! later iteration.

use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, OnceLock};

use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::core::Interface;

use crate::Result;
use crate::error::CodecError;
use crate::video::{EncodedPacket, VideoCodecType, VideoEncoder, VideoFrame};

type Ptr = *mut c_void;

// ---------------------------------------------------------------------------
// Constants (nvEncodeAPI.h, NVENC API 13.1 — values from the C checker)
// ---------------------------------------------------------------------------

const NVENCAPI_VERSION: u32 = 0x0100_000D;
const FUNCTION_LIST_VER: u32 = 0x7102_000D;
const OPEN_SESSION_EX_VER: u32 = 0x7101_000D;
const INITIALIZE_PARAMS_VER: u32 = 0xF107_000D;
const CONFIG_VER: u32 = 0xF109_000D;
const PIC_PARAMS_VER: u32 = 0xF107_000D;
const LOCK_BITSTREAM_VER: u32 = 0xF102_000D;
const RECONFIGURE_PARAMS_VER: u32 = 0xF102_000D;
const CREATE_INPUT_BUFFER_VER: u32 = 0x7102_000D;
const CREATE_BITSTREAM_BUFFER_VER: u32 = 0x7101_000D;
const LOCK_INPUT_BUFFER_VER: u32 = 0x7101_000D;
const RC_PARAMS_VER: u32 = 0x7101_000D;

const NV_ENC_SUCCESS: u32 = 0;
const INFINITE_GOPLENGTH: u32 = 0xFFFF_FFFF;

const DEVICE_TYPE_DIRECTX: u32 = 0; // also used for D3D11
const BUFFER_FORMAT_NV12: u32 = 1;
const PIC_TYPE_UNKNOWN: u32 = 255;
const PIC_TYPE_IDR: u32 = 3;
const PIC_STRUCT_FRAME: u32 = 1;
const RC_MODE_CBR: u32 = 2;
const PIC_FLAG_FORCEIDR: u32 = 2;
const TUNING_ULTRA_LOW_LATENCY: u32 = 3;

const H264_FLAG_REPEAT_SPSPPS: u32 = 1 << 12;
const RECONF_RESET_ENCODER: u32 = 1;
const RECONF_FORCE_IDR: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> Guid {
    Guid { d1, d2, d3, d4 }
}

const CODEC_H264: Guid = guid(
    0x6bc8_2762,
    0x4e63,
    0x4ca4,
    [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);
const PROFILE_HIGH: Guid = guid(
    0xe7cb_c309,
    0x4f7a,
    0x4b89,
    [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
);
// Balanced preset; quality/latency trade-off validated in the lab (H3.6).
const PRESET_P4: Guid = guid(
    0x90a7_b826,
    0xdf06,
    0x4862,
    [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
);

// ---------------------------------------------------------------------------
// FFI structs — only the fields we use are named; every struct is padded to
// the exact SDK size (asserted in tests) so the driver can safely scan the
// reserved tail it expects to find zeroed.
// ---------------------------------------------------------------------------

#[repr(C)]
struct FunctionList {
    version: u32,
    reserved: u32,
    fns: [usize; 43],
    reserved2: [usize; 275],
}

// Indices into FunctionList::fns (offsets verified: fn i sits at 8 + 8*i).
const FN_INITIALIZE_ENCODER: usize = 11;
const FN_CREATE_INPUT_BUFFER: usize = 12;
const FN_DESTROY_INPUT_BUFFER: usize = 13;
const FN_CREATE_BITSTREAM_BUFFER: usize = 14;
const FN_DESTROY_BITSTREAM_BUFFER: usize = 15;
const FN_ENCODE_PICTURE: usize = 16;
const FN_LOCK_BITSTREAM: usize = 17;
const FN_UNLOCK_BITSTREAM: usize = 18;
const FN_LOCK_INPUT_BUFFER: usize = 19;
const FN_UNLOCK_INPUT_BUFFER: usize = 20;
const FN_DESTROY_ENCODER: usize = 27;
const FN_OPEN_SESSION_EX: usize = 29;
const FN_RECONFIGURE_ENCODER: usize = 32;
const FN_GET_LAST_ERROR: usize = 37;

#[repr(C)]
struct OpenSessionExParams {
    version: u32,
    device_type: u32,
    device: Ptr,
    reserved: Ptr,
    api_version: u32,
    reserved1: [u32; 253],
    reserved2: [usize; 64],
}

#[repr(C)]
struct RcParams {
    version: u32,
    rate_control_mode: u32,
    const_qp: [u32; 3],
    average_bit_rate: u32,
    max_bit_rate: u32,
    vbv_buffer_size: u32,
    vbv_initial_delay: u32,
    pad: [u8; 128 - 36],
}

#[repr(C)]
struct ConfigH264 {
    bitfields: u32,
    level: u32,
    idr_period: u32,
    pad0: [u8; 192 - 12],
    chroma_format_idc: u32,
    pad1: [u8; 1792 - 196],
}

#[repr(C)]
struct Config {
    version: u32,
    profile_guid: Guid,
    gop_length: u32,
    frame_interval_p: i32,
    mono_chrome: u32,
    frame_field_mode: u32,
    mv_precision: u32,
    rc_params: RcParams,
    h264: ConfigH264,
    pad: [u8; 3584 - 1960],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InitializeParams {
    version: u32,
    encode_guid: Guid,
    preset_guid: Guid,
    encode_width: u32,
    encode_height: u32,
    dar_width: u32,
    dar_height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
    enable_encode_async: u32,
    enable_ptd: u32,
    bitfields: u32,
    priv_data_size: u32,
    reserved: u32,
    priv_data: Ptr,
    encode_config: *mut Config,
    max_encode_width: u32,
    max_encode_height: u32,
    max_me_hint_counts: [u32; 8],
    tuning_info: u32,
    pad: [u8; 1800 - 140],
}

#[repr(C)]
struct PicParams {
    version: u32,
    input_width: u32,
    input_height: u32,
    input_pitch: u32,
    encode_pic_flags: u32,
    frame_idx: u32,
    input_timestamp: u64,
    input_duration: u64,
    input_buffer: Ptr,
    output_bitstream: Ptr,
    completion_event: Ptr,
    buffer_fmt: u32,
    picture_struct: u32,
    picture_type: u32,
    pad: [u8; 3360 - 76],
}

#[repr(C)]
struct LockBitstream {
    version: u32,
    bitfields: u32,
    output_bitstream: Ptr,
    slice_offsets: Ptr,
    frame_idx: u32,
    hw_encode_status: u32,
    num_slices: u32,
    bitstream_size_in_bytes: u32,
    output_timestamp: u64,
    output_duration: u64,
    bitstream_buffer_ptr: Ptr,
    picture_type: u32,
    pad: [u8; 1544 - 68],
}

#[repr(C)]
struct CreateInputBuffer {
    version: u32,
    width: u32,
    height: u32,
    memory_heap: u32,
    buffer_fmt: u32,
    reserved: u32,
    input_buffer: Ptr,
    p_sys_mem_buffer: Ptr,
    pad: [u8; 776 - 40],
}

#[repr(C)]
struct CreateBitstreamBuffer {
    version: u32,
    size: u32,
    memory_heap: u32,
    reserved: u32,
    bitstream_buffer: Ptr,
    bitstream_buffer_ptr: Ptr,
    pad: [u8; 776 - 32],
}

#[repr(C)]
struct LockInputBuffer {
    version: u32,
    bitfields: u32,
    input_buffer: Ptr,
    buffer_data_ptr: Ptr,
    pitch: u32,
    pad: [u8; 1544 - 28],
}

#[repr(C)]
struct ReconfigureParams {
    version: u32,
    reserved: u32,
    re_init: InitializeParams,
    bitfields: u32,
    reserved2: u32,
}

// ---------------------------------------------------------------------------
// Runtime loader
// ---------------------------------------------------------------------------

type CreateInstanceFn = unsafe extern "C" fn(*mut FunctionList) -> u32;
type OpenSessionExFn = unsafe extern "C" fn(*mut OpenSessionExParams, *mut Ptr) -> u32;
type InitializeEncoderFn = unsafe extern "C" fn(Ptr, *mut InitializeParams) -> u32;
type CreateInputBufferFn = unsafe extern "C" fn(Ptr, *mut CreateInputBuffer) -> u32;
type CreateBitstreamBufferFn = unsafe extern "C" fn(Ptr, *mut CreateBitstreamBuffer) -> u32;
type EncodePictureFn = unsafe extern "C" fn(Ptr, *mut PicParams) -> u32;
type LockBitstreamFn = unsafe extern "C" fn(Ptr, *mut LockBitstream) -> u32;
type LockInputBufferFn = unsafe extern "C" fn(Ptr, *mut LockInputBuffer) -> u32;
type BufferFn = unsafe extern "C" fn(Ptr, Ptr) -> u32;
type DestroyEncoderFn = unsafe extern "C" fn(Ptr) -> u32;
type ReconfigureFn = unsafe extern "C" fn(Ptr, *mut ReconfigureParams) -> u32;
type GetLastErrorFn = unsafe extern "C" fn(Ptr) -> *const std::ffi::c_char;

struct NvencApi {
    list: Box<FunctionList>,
    _lib: libloading::Library,
}

// Function pointers obtained from a shared library are safe to share; the
// encoder handle itself is only touched by the owning encoder's thread.
unsafe impl Send for NvencApi {}
unsafe impl Sync for NvencApi {}

static API: OnceLock<Option<Arc<NvencApi>>> = OnceLock::new();

fn load_api() -> Option<Arc<NvencApi>> {
    let lib = match unsafe { libloading::Library::new("nvEncodeAPI64.dll") } {
        Ok(l) => l,
        Err(e) => {
            tracing::debug!("nvEncodeAPI64.dll not loadable: {e}");
            return None;
        }
    };
    let create = unsafe {
        *lib.get::<CreateInstanceFn>(b"NvEncodeAPICreateInstance")
            .map_err(|e| tracing::warn!("NvEncodeAPICreateInstance missing: {e}"))
            .ok()?
    };
    let mut list = Box::new(FunctionList {
        version: FUNCTION_LIST_VER,
        reserved: 0,
        fns: [0; 43],
        reserved2: [0; 275],
    });
    let status = unsafe { create(&raw mut *list) };
    if status != NV_ENC_SUCCESS {
        tracing::warn!("NvEncodeAPICreateInstance failed: status {status}");
        return None;
    }
    if list.fns[FN_OPEN_SESSION_EX] == 0 || list.fns[FN_INITIALIZE_ENCODER] == 0 {
        tracing::warn!("NVENC function list incomplete (old driver?)");
        return None;
    }
    tracing::info!("NVENC API loaded from nvEncodeAPI64.dll");
    Some(Arc::new(NvencApi { list, _lib: lib }))
}

impl NvencApi {
    fn get() -> Option<Arc<NvencApi>> {
        API.get_or_init(load_api).clone()
    }

    /// # Safety
    /// The slot must hold a valid function pointer of type `T` (guaranteed
    /// by the SDK layout for the indices we use).
    unsafe fn fptr<T: Copy>(&self, idx: usize) -> T {
        assert!(idx < self.list.fns.len() && self.list.fns[idx] != 0);
        unsafe { std::mem::transmute_copy(&self.list.fns[idx]) }
    }
}

/// True when an NVIDIA driver with NVENC is present (DLL loads and the
/// function table initializes). Does not prove an encoding session can be
/// opened — session limits are only known at [`NvencEncoder::new`].
#[must_use]
pub fn nvenc_available() -> bool {
    NvencApi::get().is_some()
}

fn nv_err(op: &str, status: u32) -> CodecError {
    CodecError::Backend(format!("nvenc {op} failed: status {status}"))
}

/// Driver-provided description of the last failure on this session, when
/// the function table exposes `nvEncGetLastErrorString` (it should).
fn last_error(api: &NvencApi, encoder: Ptr) -> String {
    if api.list.fns[FN_GET_LAST_ERROR] == 0 {
        return String::new();
    }
    unsafe {
        let f: GetLastErrorFn = api.fptr(FN_GET_LAST_ERROR);
        let p = f(encoder);
        if p.is_null() {
            return String::new();
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// Creates a D3D11 device on the NVIDIA adapter. Modern drivers reject a
/// null device in `nvEncOpenEncodeSessionEx` (INVALID_PTR), and the device
/// must live on the GPU that owns the encoder — on hybrid VMs adapter 0 is
/// the virtual display driver, so the adapter with "nvidia" in its name
/// wins; `NVENC_ADAPTER=<idx>` overrides the pick for the lab.
fn create_device() -> Result<ID3D11Device> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|e| CodecError::Backend(format!("CreateDXGIFactory1: {e}")))?;
    let mut adapters = Vec::new();
    for i in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        let desc = unsafe { adapter.GetDesc1() }
            .map_err(|e| CodecError::Backend(format!("adapter desc: {e}")))?;
        let end = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..end]);
        adapters.push((name, adapter));
    }
    let pick: usize = std::env::var("NVENC_ADAPTER")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            adapters
                .iter()
                .position(|(name, _)| name.to_lowercase().contains("nvidia"))
                .unwrap_or_else(|| adapters.len().saturating_sub(1))
        });
    let (name, adapter) = adapters
        .into_iter()
        .nth(pick)
        .ok_or_else(|| CodecError::Backend("nvenc: no DXGI adapter".into()))?;
    tracing::info!("NVENC using adapter {pick}: {name}");
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&raw mut device),
            None,
            None,
        )
    }
    .map_err(|e| CodecError::Backend(format!("D3D11CreateDevice on {name}: {e}")))?;
    device.ok_or_else(|| CodecError::Backend("nvenc: no D3D11 device returned".into()))
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// H.264 hardware encoder over NVENC (synchronous mode, CPU NV12 feed).
pub struct NvencEncoder {
    api: Arc<NvencApi>,
    encoder: Ptr,
    input: Ptr,
    output: Ptr,
    init: Box<InitializeParams>,
    config: Box<Config>,
    /// Keeps the D3D11 device alive for the session lifetime (the driver
    /// references it on every encode call).
    _device: ID3D11Device,
    width: u32,
    height: u32,
    frame_idx: u32,
    force_idr: bool,
}

// The NVENC session handle is only used from the thread that owns the
// encoder (server capture thread); the struct is moved across threads but
// never shared.
unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
    /// Opens an NVENC session and initializes an H.264 encoder for
    /// `width`x`height` at `fps`, targeting `kbps` (CBR). Fails when no
    /// NVENC-capable GPU/driver is present — callers fall back to VP9.
    pub fn new(width: u32, height: u32, fps: u32, kbps: u32) -> Result<Self> {
        let api = NvencApi::get()
            .ok_or_else(|| CodecError::Backend("nvenc: API not available".into()))?;
        let device = create_device()?;

        let mut encoder: Ptr = ptr::null_mut();
        let mut session = OpenSessionExParams {
            version: OPEN_SESSION_EX_VER,
            device_type: DEVICE_TYPE_DIRECTX,
            device: device.as_raw().cast(),
            reserved: ptr::null_mut(),
            api_version: NVENCAPI_VERSION,
            reserved1: [0; 253],
            reserved2: [0; 64],
        };
        let open: OpenSessionExFn = unsafe { api.fptr(FN_OPEN_SESSION_EX) };
        let status = unsafe { open(&raw mut session, &raw mut encoder) };
        if status != NV_ENC_SUCCESS || encoder.is_null() {
            return Err(nv_err("open session", status));
        }

        let mut config = Box::new(Config {
            version: CONFIG_VER,
            profile_guid: PROFILE_HIGH,
            gop_length: INFINITE_GOPLENGTH,
            frame_interval_p: 1, // IPP, no B-frames (low latency)
            mono_chrome: 0,
            // NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME (0 is invalid; init
            // coerces it but reconfigure then rejects the mismatch).
            frame_field_mode: 1,
            mv_precision: 0,
            rc_params: RcParams {
                version: RC_PARAMS_VER,
                rate_control_mode: RC_MODE_CBR,
                const_qp: [0; 3],
                average_bit_rate: kbps.saturating_mul(1000),
                max_bit_rate: kbps.saturating_mul(1000),
                vbv_buffer_size: kbps.saturating_mul(1000),
                vbv_initial_delay: 0,
                pad: [0; 128 - 36],
            },
            h264: ConfigH264 {
                // SPS/PPS on every IDR so a client can (re)join mid-stream.
                bitfields: H264_FLAG_REPEAT_SPSPPS,
                level: 0, // autoselect
                idr_period: INFINITE_GOPLENGTH,
                pad0: [0; 192 - 12],
                chroma_format_idc: 1, // YUV 4:2:0
                pad1: [0; 1792 - 196],
            },
            pad: [0; 3584 - 1960],
        });

        let mut init = Box::new(InitializeParams {
            version: INITIALIZE_PARAMS_VER,
            encode_guid: CODEC_H264,
            preset_guid: PRESET_P4,
            encode_width: width,
            encode_height: height,
            dar_width: width,
            dar_height: height,
            frame_rate_num: fps.max(1),
            frame_rate_den: 1,
            enable_encode_async: 0,
            enable_ptd: 1, // picture-type decision by NVENC; required for FORCEIDR
            bitfields: 0,
            priv_data_size: 0,
            reserved: 0,
            priv_data: ptr::null_mut(),
            encode_config: &raw mut *config,
            max_encode_width: 0,
            max_encode_height: 0,
            max_me_hint_counts: [0; 8],
            tuning_info: TUNING_ULTRA_LOW_LATENCY,
            pad: [0; 1800 - 140],
        });

        let initialize: InitializeEncoderFn = unsafe { api.fptr(FN_INITIALIZE_ENCODER) };
        let status = unsafe { initialize(encoder, &raw mut *init) };
        if status != NV_ENC_SUCCESS {
            unsafe { (api.fptr::<DestroyEncoderFn>(FN_DESTROY_ENCODER))(encoder) };
            return Err(nv_err("initialize", status));
        }

        let (input, output) = match create_buffers(&api, encoder, width, height) {
            Ok(io) => io,
            Err(e) => {
                unsafe { (api.fptr::<DestroyEncoderFn>(FN_DESTROY_ENCODER))(encoder) };
                return Err(e);
            }
        };

        tracing::info!("NVENC H.264 encoder ready: {width}x{height} @{fps}fps, {kbps} kbps CBR");
        Ok(Self {
            api,
            encoder,
            input,
            output,
            init,
            config,
            _device: device,
            width,
            height,
            frame_idx: 0,
            force_idr: true, // first frame must be an IDR
        })
    }
}

/// Allocates the NV12 input buffer and the output bitstream buffer.
fn create_buffers(api: &NvencApi, encoder: Ptr, width: u32, height: u32) -> Result<(Ptr, Ptr)> {
    let create_input: CreateInputBufferFn = unsafe { api.fptr(FN_CREATE_INPUT_BUFFER) };
    let mut input_buf = CreateInputBuffer {
        version: CREATE_INPUT_BUFFER_VER,
        width,
        height,
        memory_heap: 1, // NV_ENC_MEMORY_HEAP_SYSMEM_CACHED
        buffer_fmt: BUFFER_FORMAT_NV12,
        reserved: 0,
        input_buffer: ptr::null_mut(),
        p_sys_mem_buffer: ptr::null_mut(),
        pad: [0; 776 - 40],
    };
    let status = unsafe { create_input(encoder, &raw mut input_buf) };
    if status != NV_ENC_SUCCESS || input_buf.input_buffer.is_null() {
        return Err(nv_err("create input buffer", status));
    }

    let create_output: CreateBitstreamBufferFn = unsafe { api.fptr(FN_CREATE_BITSTREAM_BUFFER) };
    let mut output_buf = CreateBitstreamBuffer {
        version: CREATE_BITSTREAM_BUFFER_VER,
        size: 0,
        memory_heap: 1,
        reserved: 0,
        bitstream_buffer: ptr::null_mut(),
        bitstream_buffer_ptr: ptr::null_mut(),
        pad: [0; 776 - 32],
    };
    let status = unsafe { create_output(encoder, &raw mut output_buf) };
    if status != NV_ENC_SUCCESS || output_buf.bitstream_buffer.is_null() {
        unsafe { (api.fptr::<BufferFn>(FN_DESTROY_INPUT_BUFFER))(encoder, input_buf.input_buffer) };
        return Err(nv_err("create bitstream buffer", status));
    }
    Ok((input_buf.input_buffer, output_buf.bitstream_buffer))
}

impl VideoEncoder for NvencEncoder {
    fn encode(&mut self, frame: &VideoFrame<'_>) -> Result<EncodedPacket> {
        let nv12 = crate::color::bgra_to_nv12(frame)?;
        let api = self.api.clone();

        let lock_input: LockInputBufferFn = unsafe { api.fptr(FN_LOCK_INPUT_BUFFER) };
        let mut lock = LockInputBuffer {
            version: LOCK_INPUT_BUFFER_VER,
            bitfields: 0,
            input_buffer: self.input,
            buffer_data_ptr: ptr::null_mut(),
            pitch: 0,
            pad: [0; 1544 - 28],
        };
        let status = unsafe { lock_input(self.encoder, &raw mut lock) };
        if status != NV_ENC_SUCCESS {
            return Err(nv_err("lock input", status));
        }
        // Copy Y (height rows) and interleaved UV (height/2 rows), both with
        // `width` bytes per row in the tight NV12 buffer.
        let row_bytes = self.width as usize;
        let rows = self.height as usize * 3 / 2;
        unsafe {
            for row in 0..rows {
                ptr::copy_nonoverlapping(
                    nv12.as_ptr().add(row * row_bytes),
                    lock.buffer_data_ptr
                        .cast::<u8>()
                        .add(row * lock.pitch as usize),
                    row_bytes,
                );
            }
            let unlock_input: BufferFn = api.fptr(FN_UNLOCK_INPUT_BUFFER);
            unlock_input(self.encoder, self.input);
        }

        let flags = if self.force_idr { PIC_FLAG_FORCEIDR } else { 0 };
        let mut pic = PicParams {
            version: PIC_PARAMS_VER,
            input_width: self.width,
            input_height: self.height,
            input_pitch: self.width,
            encode_pic_flags: flags,
            frame_idx: self.frame_idx,
            input_timestamp: u64::from(self.frame_idx),
            input_duration: 1,
            input_buffer: self.input,
            output_bitstream: self.output,
            completion_event: ptr::null_mut(),
            buffer_fmt: BUFFER_FORMAT_NV12,
            picture_struct: PIC_STRUCT_FRAME,
            picture_type: PIC_TYPE_UNKNOWN,
            pad: [0; 3360 - 76],
        };
        let encode_picture: EncodePictureFn = unsafe { api.fptr(FN_ENCODE_PICTURE) };
        let status = unsafe { encode_picture(self.encoder, &raw mut pic) };
        if status != NV_ENC_SUCCESS {
            return Err(nv_err("encode picture", status));
        }
        self.frame_idx = self.frame_idx.wrapping_add(1);
        self.force_idr = false;

        let lock_bitstream: LockBitstreamFn = unsafe { api.fptr(FN_LOCK_BITSTREAM) };
        let mut bs = LockBitstream {
            version: LOCK_BITSTREAM_VER,
            bitfields: 0,
            output_bitstream: self.output,
            slice_offsets: ptr::null_mut(),
            frame_idx: 0,
            hw_encode_status: 0,
            num_slices: 0,
            bitstream_size_in_bytes: 0,
            output_timestamp: 0,
            output_duration: 0,
            bitstream_buffer_ptr: ptr::null_mut(),
            picture_type: 0,
            pad: [0; 1544 - 68],
        };
        let status = unsafe { lock_bitstream(self.encoder, &raw mut bs) };
        if status != NV_ENC_SUCCESS {
            return Err(nv_err("lock bitstream", status));
        }
        let size = bs.bitstream_size_in_bytes as usize;
        let mut data = vec![0u8; size];
        unsafe {
            ptr::copy_nonoverlapping(
                bs.bitstream_buffer_ptr.cast::<u8>(),
                data.as_mut_ptr(),
                size,
            );
            let unlock_bitstream: BufferFn = api.fptr(FN_UNLOCK_BITSTREAM);
            unlock_bitstream(self.encoder, self.output);
        }
        Ok(EncodedPacket {
            data,
            keyframe: bs.picture_type == PIC_TYPE_IDR,
        })
    }

    fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        self.config.rc_params.average_bit_rate = kbps.saturating_mul(1000);
        self.config.rc_params.max_bit_rate = kbps.saturating_mul(1000);
        self.config.rc_params.vbv_buffer_size = kbps.saturating_mul(1000);
        let mut reconf = ReconfigureParams {
            version: RECONFIGURE_PARAMS_VER,
            reserved: 0,
            re_init: *self.init,
            bitfields: RECONF_RESET_ENCODER | RECONF_FORCE_IDR,
            reserved2: 0,
        };
        reconf.re_init.encode_config = &raw mut *self.config;
        let reconfigure: ReconfigureFn = unsafe { self.api.fptr(FN_RECONFIGURE_ENCODER) };
        let status = unsafe { reconfigure(self.encoder, &raw mut reconf) };
        if status != NV_ENC_SUCCESS {
            let detail = last_error(&self.api, self.encoder);
            return Err(CodecError::Backend(format!(
                "nvenc reconfigure failed: status {status} ({detail})"
            )));
        }
        self.force_idr = true;
        Ok(())
    }

    fn codec_type(&self) -> VideoCodecType {
        VideoCodecType::H264
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        unsafe {
            (self.api.fptr::<BufferFn>(FN_DESTROY_INPUT_BUFFER))(self.encoder, self.input);
            (self.api.fptr::<BufferFn>(FN_DESTROY_BITSTREAM_BUFFER))(self.encoder, self.output);
            (self.api.fptr::<DestroyEncoderFn>(FN_DESTROY_ENCODER))(self.encoder);
        }
    }
}

// Layouts verified at compile time against the C checker's numbers
// (nvEncodeAPI.h, NVENC API 13.1). A driver-side struct mismatch breaks the
// build here instead of corrupting memory at runtime.
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<Guid>() == 16);
    assert!(size_of::<FunctionList>() == 2552);
    assert!(size_of::<OpenSessionExParams>() == 1552);
    assert!(offset_of!(OpenSessionExParams, api_version) == 24);
    assert!(size_of::<RcParams>() == 128);
    assert!(offset_of!(RcParams, average_bit_rate) == 20);
    assert!(size_of::<ConfigH264>() == 1792);
    assert!(offset_of!(ConfigH264, chroma_format_idc) == 192);
    assert!(size_of::<Config>() == 3584);
    assert!(offset_of!(Config, gop_length) == 20);
    assert!(offset_of!(Config, rc_params) == 40);
    assert!(offset_of!(Config, h264) == 168);
    assert!(size_of::<InitializeParams>() == 1800);
    assert!(offset_of!(InitializeParams, encode_config) == 88);
    assert!(offset_of!(InitializeParams, tuning_info) == 136);
    assert!(size_of::<PicParams>() == 3360);
    assert!(offset_of!(PicParams, input_buffer) == 40);
    assert!(offset_of!(PicParams, picture_type) == 72);
    assert!(size_of::<LockBitstream>() == 1544);
    assert!(offset_of!(LockBitstream, bitstream_size_in_bytes) == 36);
    assert!(offset_of!(LockBitstream, bitstream_buffer_ptr) == 56);
    assert!(offset_of!(LockBitstream, picture_type) == 64);
    assert!(size_of::<CreateInputBuffer>() == 776);
    assert!(offset_of!(CreateInputBuffer, input_buffer) == 24);
    assert!(size_of::<CreateBitstreamBuffer>() == 776);
    assert!(offset_of!(CreateBitstreamBuffer, bitstream_buffer) == 16);
    assert!(size_of::<LockInputBuffer>() == 1544);
    assert!(offset_of!(LockInputBuffer, pitch) == 24);
    assert!(size_of::<ReconfigureParams>() == 1816);
    assert!(offset_of!(ReconfigureParams, re_init) == 8);
};
