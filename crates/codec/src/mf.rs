// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com
//! Media Foundation video encoder backend (Windows, §18.75 HW-encode).
//!
//! Runtime MFT enumeration plus [`MfEncoder`], a VP9/H264 encoder behind
//! the platform-neutral [`crate::VideoEncoder`] trait: drivers register
//! their hardware MFTs (Intel/NVIDIA/AMD) and the OS ships software MFTs,
//! so one backend covers every vendor. Two input paths:
//!
//! - **H1 (CPU)**: frames arrive in BGRA and are fed as NV12/I420.
//! - **H2 (zero-copy GPU)**: [`MfEncoder::encode_gpu`] takes an
//!   `ID3D11Texture2D` on the shared device, converts BGRA → NV12 with the
//!   D3D11 video processor and feeds the MFT through the DXGI device
//!   manager — no CPU readback anywhere.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;

use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Foundation::TRUE;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGISurface;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaEventGenerator, IMFSample,
    IMFTransform, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFShutdown, MFStartup,
    MFVideoFormat_H264, MFVideoFormat_I420, MFVideoFormat_NV12, MFVideoFormat_VP90,
    MFVideoInterlace_Progressive, MFSampleExtension_CleanPoint, MFSTARTUP_FULL, MF_VERSION,
    MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SAMPLE_SIZE, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE,
    MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_TRANSFORM_FLAGS_Attribute,
    MFTEnumEx,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG,
    MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SYNCMFT,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_INFO, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO,
    MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, METransformHaveOutput, METransformNeedInput,
};
use windows::Win32::Media::MediaFoundation::{
    eAVEncH264VProfile_High, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI,
    MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_MT_MPEG2_PROFILE,
};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4};
use windows::core::{GUID, Interface};

use crate::video::{EncodedPacket, VideoCodecType, VideoEncoder, VideoFrame};
use crate::{CodecError, Result};

/// Packs a SIZE attribute (MF_MT_FRAME_SIZE) the way MFSetAttributeSize did.
const fn pack_size(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

/// Builds a VT_UI4 variant (the crate ships no `From` helpers for VARIANT).
fn variant_u32(v: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { ulVal: v },
            }),
        },
    }
}

/// Builds a VT_BOOL variant.
fn variant_bool(v: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: windows::Win32::Foundation::VARIANT_BOOL(v.into()),
                },
            }),
        },
    }
}

/// One video-encoder MFT registered in the system.
#[derive(Debug, Clone)]
pub struct MftInfo {
    /// Friendly name registered by the driver.
    pub name: String,
    /// Output subtype as a short label (`VP9`, `H264` or the raw GUID string
    /// when unknown).
    pub subtype: String,
    /// True when the MFT is D3D11-aware (can consume GPU textures directly,
    /// the zero-copy door of §18.75 H2). Software MFTs never are.
    pub d3d11_aware: bool,
    /// True when the MFT is async (MF_TRANSFORM_ASYNC flag).
    pub async_mft: bool,
}

const SUBTYPES: [(&GUID, &str); 2] = [
    (&MFVideoFormat_VP90, "VP9"),
    (&MFVideoFormat_H264, "H264"),
];

/// Enumerates the registered VP9/H264 video-encoder MFTs (probe tool).
pub fn enumerate_encoders() -> Result<Vec<MftInfo>> {
    let mut out = Vec::new();
    for (subtype, label) in SUBTYPES {
        for act in enumerate_activates(subtype, false)? {
            out.push(read_info(&act, label));
        }
    }
    Ok(out)
}

/// Enumerates the activates of encoder MFTs producing `subtype`,
/// field-of-use filtered out. `hw_only` restricts the enumeration to
/// hardware transforms (the OS itself classifies them — the
/// `MF_SA_D3D11_AWARE` attribute is not reliable for this).
fn enumerate_activates(subtype: &GUID, hw_only: bool) -> Result<Vec<IMFActivate>> {
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: *subtype,
    };
    let flags = if hw_only {
        MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0)
    } else {
        MFT_ENUM_FLAG(
            MFT_ENUM_FLAG_SYNCMFT.0
                | MFT_ENUM_FLAG_ASYNCMFT.0
                | MFT_ENUM_FLAG_HARDWARE.0
                | MFT_ENUM_FLAG_LOCALMFT.0,
        )
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&raw const output),
            &raw mut activates,
            &raw mut count,
        )
    }
    .map_err(|e| CodecError::Backend(format!("MFTEnumEx: {e}")))?;
    let entries = if activates.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(activates, count as usize) }
    };
    let out: Vec<IMFActivate> = entries.iter().flatten().cloned().collect();
    if !activates.is_null() {
        unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(activates.cast())) };
    }
    Ok(out)
}

/// Reads the interesting attributes of one enumerated transform.
fn read_info(act: &IMFActivate, subtype: &str) -> MftInfo {
    let name = unsafe {
        let mut raw = windows::core::PWSTR::null();
        let mut len = 0u32;
        match act.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &raw mut raw, &raw mut len) {
            Ok(()) => raw.to_string().unwrap_or_default(),
            Err(_) => String::new(),
        }
    };
    MftInfo {
        name,
        subtype: subtype.to_string(),
        d3d11_aware: unsafe { act.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) } != 0,
        async_mft: unsafe { act.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) } != 0,
    }
}

/// Keeps Media Foundation up; shuts it down on drop. Held by the encoder.
struct MfGuard {
    _private: (),
}

impl MfGuard {
    fn acquire() -> Result<Self> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .map_err(|e| CodecError::Backend(format!("MFStartup: {e}")))?;
        Ok(Self { _private: () })
    }
}

impl Drop for MfGuard {
    fn drop(&mut self) {
        unsafe { let _ = MFShutdown(); }
    }
}

/// One drained output packet.
struct PendingPacket {
    data: Vec<u8>,
    keyframe: bool,
}

/// Codec the MFT produces. H264 exists for lab validation of the GPU path
/// (NVIDIA ships an H264 hardware MFT but no VP9 one); the production
/// ladder only negotiates VP9.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MftCodec {
    /// VP9 (`MFVideoFormat_VP90`).
    Vp9,
    /// H264 (`MFVideoFormat_H264`).
    H264,
}

impl MftCodec {
    fn subtype(self) -> &'static GUID {
        match self {
            Self::Vp9 => &MFVideoFormat_VP90,
            Self::H264 => &MFVideoFormat_H264,
        }
    }

    fn codec_type(self) -> VideoCodecType {
        match self {
            Self::Vp9 => VideoCodecType::Vp9,
            Self::H264 => VideoCodecType::H264,
        }
    }
}

/// Zero-copy GPU input state (§18.75 H2): the DXGI device manager handed
/// to the MFT plus the video-processor chain that converts the captured
/// BGRA texture into the NV12 one the hardware encoder consumes.
struct GpuPipeline {
    context: ID3D11DeviceContext,
    video_context: ID3D11VideoContext,
    video_device: ID3D11VideoDevice,
    /// Kept alive: input/output views are created against the enumerator.
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// Persistent intermediate BGRA (the captured texture is copied here
    /// GPU-side: duplication surfaces cannot always back a VP input view).
    bgra: ID3D11Texture2D,
    /// Persistent NV12 surface fed to the MFT.
    nv12: ID3D11Texture2D,
    /// Plain NV12 (no bind flags) handed to the MFT: the video processor
    /// needs RENDER_TARGET on its output, but some hardware MFTs (NVIDIA)
    /// reject bound textures as encoder input — one extra GPU copy.
    nv12_feed: ID3D11Texture2D,
    /// Kept alive: the MFT borrows it for the session.
    _manager: IMFDXGIDeviceManager,
}

/// VP9/H264 video encoder through a Media Foundation transform (§18.75).
/// CPU input (BGRA → NV12/I420 internally) or zero-copy GPU input
/// ([`MfEncoder::encode_gpu`]) when built with a shared D3D11 device.
pub struct MfEncoder {
    transform: IMFTransform,
    codec_api: ICodecAPI,
    events: Option<IMFMediaEventGenerator>,
    in_stream: u32,
    out_stream: u32,
    out_cb_size: u32,
    out_provides_samples: bool,
    /// Input layout actually negotiated: NV12 when true, I420 when false.
    nv12: bool,
    codec: VideoCodecType,
    width: u32,
    height: u32,
    fps: u32,
    kbps: u32,
    frame_counter: u64,
    pending: VecDeque<PendingPacket>,
    gpu: Option<GpuPipeline>,
    /// Friendly name of the chosen MFT (for the "encoder ready" log line).
    mft_name: String,
    _guard: MfGuard,
}

/// Primes an MFT for zero-copy GPU input on `device` (§18.75 H2): hands
/// the DXGI device manager to the transform and builds the video-processor
/// chain that converts the captured BGRA texture into the NV12 surface the
/// hardware encoder consumes. Any failure here means "no GPU path" — the
/// caller retries with CPU input or falls back to libvpx.
fn init_gpu(transform: &IMFTransform, device: &ID3D11Device, width: u32, height: u32) -> Result<GpuPipeline> {
    unsafe {
        let mut token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&raw mut token, &raw mut manager)
            .map_err(|e| CodecError::Backend(format!("MFCreateDXGIDeviceManager: {e}")))?;
        let manager =
            manager.ok_or_else(|| CodecError::Backend("DXGI device manager not created".into()))?;
        manager
            .ResetDevice(device, token)
            .map_err(|e| CodecError::Backend(format!("DXGI manager ResetDevice: {e}")))?;
        transform
            .ProcessMessage(
                MFT_MESSAGE_SET_D3D_MANAGER,
                manager.as_raw() as usize,
            )
            .map_err(|e| CodecError::Backend(format!("MFT rejected the D3D11 manager: {e}")))?;

        let video_device: ID3D11VideoDevice = device
            .cast()
            .map_err(|e| CodecError::Backend(format!("no ID3D11VideoDevice: {e}")))?;
        let context = device
            .GetImmediateContext()
            .map_err(|e| CodecError::Backend(format!("no immediate D3D11 context: {e}")))?;
        let video_context: ID3D11VideoContext = context
            .cast()
            .map_err(|e| CodecError::Backend(format!("no ID3D11VideoContext: {e}")))?;

        let vp_content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: 30, Denominator: 1 },
            InputWidth: width,
            InputHeight: height,
            OutputWidth: width,
            OutputHeight: height,
            OutputFrameRate: DXGI_RATIONAL { Numerator: 30, Denominator: 1 },
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = video_device
            .CreateVideoProcessorEnumerator(&raw const vp_content)
            .map_err(|e| CodecError::Backend(format!("VP enumerator: {e}")))?;
        let in_support = enumerator
            .CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)
            .map_err(|e| CodecError::Backend(format!("VP input format check: {e}")))?;
        let out_support = enumerator
            .CheckVideoProcessorFormat(DXGI_FORMAT_NV12)
            .map_err(|e| CodecError::Backend(format!("VP output format check: {e}")))?;
        if in_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0
            || out_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0
        {
            return Err(CodecError::Backend(
                "video processor cannot convert BGRA to NV12 on this device".into(),
            ));
        }
        let processor = video_device
            .CreateVideoProcessor(&enumerator, 0)
            .map_err(|e| CodecError::Backend(format!("CreateVideoProcessor: {e}")))?;

        let mut desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut bgra = None;
        device
            .CreateTexture2D(&raw const desc, None, Some(&raw mut bgra))
            .map_err(|e| CodecError::Backend(format!("BGRA intermediate texture: {e}")))?;
        desc.Format = DXGI_FORMAT_NV12;
        desc.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
        let mut nv12 = None;
        device
            .CreateTexture2D(&raw const desc, None, Some(&raw mut nv12))
            .map_err(|e| CodecError::Backend(format!("NV12 target texture: {e}")))?;
        desc.BindFlags = 0;
        let mut nv12_feed = None;
        device
            .CreateTexture2D(&raw const desc, None, Some(&raw mut nv12_feed))
            .map_err(|e| CodecError::Backend(format!("NV12 feed texture: {e}")))?;

        Ok(GpuPipeline {
            context,
            video_context,
            video_device,
            enumerator,
            processor,
            bgra: bgra.ok_or_else(|| CodecError::Backend("BGRA texture not created".into()))?,
            nv12: nv12.ok_or_else(|| CodecError::Backend("NV12 texture not created".into()))?,
            nv12_feed: nv12_feed
                .ok_or_else(|| CodecError::Backend("NV12 feed texture not created".into()))?,
            _manager: manager,
        })
    }
}

impl MfEncoder {
    /// Picks the best MFT for `codec` (D3D11-aware first: hardware marks
    /// itself so) and configures it for `width`x`height` at `fps`, `kbps`,
    /// low latency. With `hw_only` only hardware transforms are considered
    /// (the Microsoft Store software MFT is skipped). With `gpu_device`
    /// the encoder is primed for zero-copy input on that device
    /// ([`MfEncoder::encode_gpu`]); a failure to negotiate the D3D11
    /// handshake with the MFT fails the whole constructor — the caller
    /// retries without the device or falls back to libvpx. Returns
    /// `CodecError::Unsupported` when no matching MFT exists at all.
    #[allow(clippy::too_many_lines)] // MFT negotiation is one long COM sequence; splitting hurts readability.
    pub fn new_mft(
        codec: MftCodec,
        width: u32,
        height: u32,
        fps: u32,
        kbps: u32,
        hw_only: bool,
        gpu_device: Option<ID3D11Device>,
    ) -> Result<Self> {
        let guard = MfGuard::acquire()?;
        let activates = enumerate_activates(codec.subtype(), hw_only)?;
        if activates.is_empty() {
            return Err(CodecError::Unsupported(if hw_only {
                "no hardware encoder MFT registered for the codec"
            } else {
                "no encoder MFT registered for the codec"
            }));
        }
        // Prefer D3D11-aware (hardware); among equals, the registry order.
        // Lab override: MF_PICK=<substring> forces a specific MFT by name.
        let pick = std::env::var("MF_PICK").ok();
        let act = if let Some(pick) = pick.as_deref() {
            activates
                .iter()
                .find(|a| {
                    read_info(a, "")
                        .name
                        .to_lowercase()
                        .contains(&pick.to_lowercase())
                })
                .ok_or(CodecError::Unsupported("no MFT matches the MF_PICK filter"))?
                .clone()
        } else {
            activates
                .iter()
                .max_by_key(|a| unsafe { a.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) })
                .expect("non-empty")
                .clone()
        };
        let info = read_info(
            &act,
            match codec {
                MftCodec::Vp9 => "VP9",
                MftCodec::H264 => "H264",
            },
        );

        let transform: IMFTransform = unsafe { act.ActivateObject() }
            .map_err(|e| CodecError::Backend(format!("MFT activate: {e}")))?;
        let codec_api: ICodecAPI = transform
            .cast()
            .map_err(|e| CodecError::Backend(format!("MFT has no ICodecAPI: {e}")))?;

        // Async MFTs refuse SetInputType/ProcessMessage from a synchronous
        // caller until explicitly unlocked (MF_E_ASYNC 0xC00D6D77
        // otherwise); unlock before querying the event generator too.
        if let Ok(attrs) = unsafe { transform.GetAttributes() } {
            unsafe {
                let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
            }
        }
        let events: Option<IMFMediaEventGenerator> = transform.cast().ok();
        if std::env::var_os("MF_DEBUG").is_some() {
            let flags = unsafe {
                transform
                    .GetAttributes()
                    .and_then(|a| a.GetUINT32(&MF_TRANSFORM_FLAGS_Attribute))
                    .unwrap_or(0)
            };
            eprintln!("[mf] post-activate flags=0x{flags:x} events={}", events.is_some());
        }

        // Zero-copy GPU input (H2): hand the DXGI device manager to the
        // MFT and build the BGRA→NV12 video-processor chain. Done before
        // any type negotiation: several hardware MFTs (NVIDIA) only enable
        // their D3D11 input path when the manager arrives first.
        let gpu = match gpu_device {
            Some(device) => Some(init_gpu(&transform, &device, width, height)?),
            None => None,
        };

        // Non-default stream IDs are rare; try the query, default to 0/0.
        let (mut in_stream, mut out_stream) = (0u32, 0u32);
        unsafe {
            let _ = transform.GetStreamIDs(
                std::slice::from_mut(&mut in_stream),
                std::slice::from_mut(&mut out_stream),
            );
        }

        // Output type: video/<codec> with size, rate and average bitrate.
        let out_type = unsafe { MFCreateMediaType() }
            .map_err(|e| CodecError::Backend(format!("MFCreateMediaType: {e}")))?;
        unsafe {
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|()| out_type.SetGUID(&MF_MT_SUBTYPE, codec.subtype()))
                .and_then(|()| {
                    // H264: the profile is a required part of the type for
                    // strict MFTs (NVIDIA rejects the output type without it).
                    if matches!(codec, MftCodec::H264) {
                        let profile = std::env::var("MF_PROFILE")
                            .ok()
                            .and_then(|v| v.parse::<u32>().ok())
                            .unwrap_or(eAVEncH264VProfile_High.0 as u32);
                        out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)
                    } else {
                        Ok(())
                    }
                })
                .and_then(|()| out_type.SetUINT32(&MF_MT_AVG_BITRATE, kbps * 1000))
                .and_then(|()| {
                    out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height))
                })
                .and_then(|()| out_type.SetUINT64(&MF_MT_FRAME_RATE, pack_size(fps, 1)))
                .and_then(|()| {
                    out_type.SetUINT32(
                        &MF_MT_INTERLACE_MODE,
                        MFVideoInterlace_Progressive.0 as u32,
                    )
                })
                .map_err(|e| CodecError::Backend(format!("output media type: {e}")))?;
            transform
                .SetOutputType(out_stream, &out_type, 0)
                .map_err(|e| CodecError::Backend(format!("SetOutputType VP9: {e}")))?;
        }

        // Input type: NV12 first, I420 as fallback.
        let mut nv12 = true;
        let input_result = (|| -> Result<()> {
            let in_type = unsafe { MFCreateMediaType() }
                .map_err(|e| CodecError::Backend(format!("MFCreateMediaType: {e}")))?;
            unsafe {
                in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                    .and_then(|()| in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12))
                    .and_then(|()| {
                        in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height))
                    })
                    .and_then(|()| in_type.SetUINT64(&MF_MT_FRAME_RATE, pack_size(fps, 1)))
                    .and_then(|()| {
                        in_type.SetUINT32(
                            &MF_MT_INTERLACE_MODE,
                            MFVideoInterlace_Progressive.0 as u32,
                        )
                    })
                    .and_then(|()| in_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, width))
                    .and_then(|()| {
                        in_type.SetUINT32(&MF_MT_SAMPLE_SIZE, width * height * 3 / 2)
                    })
                    .map_err(|e| CodecError::Backend(format!("input media type: {e}")))?;
                transform
                    .SetInputType(in_stream, &in_type, 0)
                    .map_err(|e| CodecError::Backend(format!("SetInputType NV12: {e}")))?;
            }
            Ok(())
        })();
        if input_result.is_err() {
            nv12 = false;
            let in_type = unsafe { MFCreateMediaType() }
                .map_err(|e| CodecError::Backend(format!("MFCreateMediaType: {e}")))?;
            unsafe {
                in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                    .and_then(|()| in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_I420))
                    .and_then(|()| {
                        in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height))
                    })
                    .and_then(|()| in_type.SetUINT64(&MF_MT_FRAME_RATE, pack_size(fps, 1)))
                    .and_then(|()| {
                        in_type.SetUINT32(
                            &MF_MT_INTERLACE_MODE,
                            MFVideoInterlace_Progressive.0 as u32,
                        )
                    })
                    .map_err(|e| CodecError::Backend(format!("input media type: {e}")))?;
                transform
                    .SetInputType(in_stream, &in_type, 0)
                    .map_err(|e| CodecError::Backend(format!("SetInputType I420: {e}")))?;
            }
        }

        // Codec API knobs (each is best-effort except the bitrate): low
        // latency for interactive desktop, average bitrate as the target.
        // Lab override: MF_KNOBS=0 skips the optional knobs (some MFTs
        // misbehave with low-latency / rate-control settings).
        let knobs = std::env::var("MF_KNOBS").ok().as_deref() != Some("0");
        unsafe {
            if knobs {
                let _ = codec_api.SetValue(&CODECAPI_AVLowLatencyMode, &variant_bool(true));
                // CBR rate control; some hardware MFTs reject streams
                // without an explicit rate-control mode.
                let _ = codec_api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &variant_u32(0));
                if matches!(codec, MftCodec::H264) {
                    // No B-frames: interactive desktop needs decode order
                    // == presentation order.
                    let _ = codec_api
                        .SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &variant_u32(0));
                }
            }
            codec_api
                .SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(kbps * 1000))
                .map_err(|e| CodecError::Backend(format!("set mean bitrate: {e}")))?;
        }

        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .and_then(|()| transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0))
                .map_err(|e| CodecError::Backend(format!("start streaming: {e}")))?;
        }

        let out_info: MFT_OUTPUT_STREAM_INFO = unsafe {
            transform
                .GetOutputStreamInfo(out_stream)
                .map_err(|e| CodecError::Backend(format!("GetOutputStreamInfo: {e}")))?
        };
        if std::env::var_os("MF_DEBUG").is_some() {
            eprintln!(
                "[mf] out_info cbSize={} flags=0x{:x} provides={} input={}",
                out_info.cbSize,
                out_info.dwFlags,
                out_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0,
                if nv12 { "NV12" } else { "I420" }
            );
        }

        Ok(Self {
            transform,
            codec_api,
            events,
            in_stream,
            out_stream,
            out_cb_size: out_info.cbSize.max(width * height),
            out_provides_samples: out_info.dwFlags
                & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                != 0,
            nv12,
            codec: codec.codec_type(),
            width,
            height,
            fps,
            kbps,
            frame_counter: 0,
            pending: VecDeque::new(),
            gpu,
            mft_name: info.name,
            _guard: guard,
        })
    }

    /// VP9 convenience wrapper over [`MfEncoder::new_mft`] (CPU input).
    ///
    /// # Errors
    /// Same as [`MfEncoder::new_mft`].
    pub fn new_vp9(width: u32, height: u32, fps: u32, kbps: u32, hw_only: bool) -> Result<Self> {
        Self::new_mft(MftCodec::Vp9, width, height, fps, kbps, hw_only, None)
    }

    /// True when this encoder was primed for zero-copy GPU input.
    #[must_use]
    pub fn gpu_capable(&self) -> bool {
        self.gpu.is_some()
    }

    /// Friendly name of the chosen MFT (log line).
    #[must_use]
    pub fn mft_name(&self) -> &str {
        &self.mft_name
    }

    /// Converts the BGRA frame to the negotiated input layout (NV12/I420).
    fn convert_input(&self, frame: &VideoFrame<'_>) -> Result<Vec<u8>> {
        let i420 = crate::color::bgra_to_i420(frame)?;
        if !self.nv12 {
            let mut data = i420.y;
            data.extend_from_slice(&i420.u);
            data.extend_from_slice(&i420.v);
            return Ok(data);
        }
        // NV12: Y plane, then U/V interleaved.
        let mut data = i420.y;
        data.reserve(i420.u.len() * 2);
        for (u, v) in i420.u.iter().zip(i420.v.iter()) {
            data.push(*u);
            data.push(*v);
        }
        Ok(data)
    }

    /// Builds an MF sample around a converted frame.
    fn make_sample(&self, data: &[u8]) -> Result<IMFSample> {
        let sample = unsafe { MFCreateSample() }
            .map_err(|e| CodecError::Backend(format!("MFCreateSample: {e}")))?;
        let buffer = unsafe { MFCreateMemoryBuffer(u32::try_from(data.len()).unwrap_or(u32::MAX)) }
            .map_err(|e| CodecError::Backend(format!("MFCreateMemoryBuffer: {e}")))?;
        unsafe {
            let mut ptr = std::ptr::null_mut();
            buffer
                .Lock(&raw mut ptr, None, None)
                .map_err(|e| CodecError::Backend(format!("buffer lock: {e}")))?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            buffer
                .Unlock()
                .and_then(|()| buffer.SetCurrentLength(u32::try_from(data.len()).unwrap_or(u32::MAX)))
                .map_err(|e| CodecError::Backend(format!("buffer commit: {e}")))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| CodecError::Backend(format!("AddBuffer: {e}")))?;
            let dur = 10_000_000i64 / i64::from(self.fps.max(1));
            sample.SetSampleTime(self.frame_counter.cast_signed() * dur).ok();
            sample.SetSampleDuration(dur).ok();
        }
        Ok(sample)
    }

    /// One `ProcessOutput` round; appends whatever it yields to the pending
    /// queue. Returns false when the MFT asks for more input instead.
    fn drain_outputs(&mut self) -> Result<()> {
        loop {
            let out_sample: Option<IMFSample> = if self.out_provides_samples {
                None
            } else {
                let s = unsafe { MFCreateSample() }
                    .map_err(|e| CodecError::Backend(format!("MFCreateSample: {e}")))?;
                let b = unsafe { MFCreateMemoryBuffer(self.out_cb_size) }
                    .map_err(|e| CodecError::Backend(format!("MFCreateMemoryBuffer: {e}")))?;
                unsafe {
                    s.AddBuffer(&b)
                        .map_err(|e| CodecError::Backend(format!("AddBuffer: {e}")))?;
                }
                Some(s)
            };
            let mut bufs = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.out_stream,
                pSample: ManuallyDrop::new(out_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, &mut bufs, &raw mut status)
            };
            match result {
                Ok(()) => {
                    let sample = unsafe { ManuallyDrop::take(&mut bufs[0].pSample) }
                        .ok_or_else(|| CodecError::Backend("output without sample".into()))?;
                    let contiguous: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer() }
                        .map_err(|e| CodecError::Backend(format!("contiguous: {e}")))?;
                    let (data, keyframe) = unsafe {
                        let mut ptr = std::ptr::null_mut();
                        let mut len = 0u32;
                        contiguous
                            .Lock(&raw mut ptr, None, Some(&raw mut len))
                            .map_err(|e| CodecError::Backend(format!("out lock: {e}")))?;
                        let data =
                            std::slice::from_raw_parts(ptr, len as usize).to_vec();
                        let _ = contiguous.Unlock();
                        let keyframe =
                            sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
                        (data, keyframe)
                    };
                    if !data.is_empty() {
                        self.pending.push_back(PendingPacket { data, keyframe });
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(CodecError::Backend(format!("ProcessOutput: {e}"))),
            }
        }
    }

    /// Waits for one async-MFT event and drains output when it is a
    /// have-output one. Returns true when more input is wanted.
    fn pump_event(&mut self) -> Result<bool> {
        let events = self
            .events
            .as_ref()
            .ok_or_else(|| CodecError::Backend("async MFT without event generator".into()))?;
        let ev = unsafe { events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) }
            .map_err(|e| CodecError::Backend(format!("GetEvent: {e}")))?;
        let kind = unsafe { ev.GetType() }
            .map_err(|e| CodecError::Backend(format!("event type: {e}")))?;
        if std::env::var_os("MF_DEBUG").is_some() {
            let what = match kind as i32 {
                k if k == METransformNeedInput.0 => "NeedInput",
                k if k == METransformHaveOutput.0 => "HaveOutput",
                _ => "other",
            };
            eprintln!("[mf] event {kind} ({what})");
        }
        if kind == METransformHaveOutput.0 as u32 {
            self.drain_outputs()?;
            Ok(false)
        } else {
            Ok(kind == METransformNeedInput.0 as u32)
        }
    }
}

// COM interface pointers are not `Send` in the 0.62 crate; MFTs are
// free-threaded and every method here goes through `&mut self`, so moving
// the encoder between threads is safe.
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    /// Stamps, feeds and drains one prepared sample; shared by the CPU and
    /// GPU input paths.
    fn submit_and_collect(&mut self, sample: &IMFSample) -> Result<EncodedPacket> {
        unsafe {
            let dur = 10_000_000i64 / i64::from(self.fps.max(1));
            sample.SetSampleTime(self.frame_counter.cast_signed() * dur).ok();
            sample.SetSampleDuration(dur).ok();
        }
        self.frame_counter += 1;

        // Feed; when the MFT is momentarily full, pump events until it
        // wants input again (async MFTs) or fail.
        let mut feed_attempts = 0u32;
        loop {
            match unsafe { self.transform.ProcessInput(self.in_stream, sample, 0) } {
                Ok(()) => break,
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    if self.events.is_none() {
                        return Err(CodecError::Backend(
                            "sync MFT refused input (MF_E_NOTACCEPTING)".into(),
                        ));
                    }
                    self.pump_event()?;
                    feed_attempts += 1;
                    if feed_attempts > 100 {
                        return Err(CodecError::Backend("MFT feed stall".into()));
                    }
                }
                Err(e) => return Err(CodecError::Backend(format!("ProcessInput: {e}"))),
            }
        }

        // Sync MFT: drain right away. Sync MFTs with internal lookahead
        // (B-frames) legitimately yield nothing for some inputs — the
        // server already treats an empty packet as "no output this frame".
        if self.events.is_none() {
            self.drain_outputs()?;
            if self.pending.is_empty() {
                return Ok(EncodedPacket {
                    data: Vec::new(),
                    keyframe: false,
                });
            }
        }
        let mut stalls = 0u32;
        while self.pending.is_empty() {
            self.pump_event()?;
            stalls += 1;
            if stalls > 300 {
                return Err(CodecError::Backend("MFT output stall".into()));
            }
        }
        let packet = self.pending.pop_front().expect("non-empty");
        Ok(EncodedPacket {
            data: packet.data,
            keyframe: packet.keyframe,
        })
    }

    /// Wraps the NV12 GPU surface in an MF sample (zero-copy).
    fn make_gpu_sample(&self) -> Result<IMFSample> {
        let gpu = self
            .gpu
            .as_ref()
            .ok_or_else(|| CodecError::Backend("encoder is not GPU-primed".into()))?;
        let sample = unsafe { MFCreateSample() }
            .map_err(|e| CodecError::Backend(format!("MFCreateSample: {e}")))?;
        let surface: IDXGISurface = gpu
            .nv12_feed
            .cast()
            .map_err(|e| CodecError::Backend(format!("NV12 texture is no DXGI surface: {e}")))?;
        let buffer = unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &surface, 0, false) }
            .map_err(|e| CodecError::Backend(format!("MFCreateDXGISurfaceBuffer: {e}")))?;
        unsafe {
            sample
                .AddBuffer(&buffer)
                .map_err(|e| CodecError::Backend(format!("AddBuffer: {e}")))?;
        }
        Ok(sample)
    }

    /// Zero-copy GPU input (§18.75 H2): `src` is a BGRA texture on the
    /// shared device (e.g. the DXGI duplication output). The frame is
    /// copied GPU-side into the intermediate BGRA surface, converted to
    /// NV12 by the video processor and fed to the MFT — nothing touches
    /// the CPU.
    ///
    /// # Errors
    /// [`CodecError::Backend`] when the encoder was not built with a GPU
    /// device, or on any D3D11/MFT failure.
    pub fn encode_gpu(&mut self, src: &ID3D11Texture2D) -> Result<EncodedPacket> {
        let gpu = self
            .gpu
            .as_ref()
            .ok_or_else(|| CodecError::Backend("encoder is not GPU-primed".into()))?;
        let mut src_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&raw mut src_desc) };
        if src_desc.Width != self.width || src_desc.Height != self.height {
            return Err(CodecError::BadDimensions {
                width: src_desc.Width,
                height: src_desc.Height,
                stride: 0,
                data_len: 0,
            });
        }
        unsafe {
            gpu.context.CopyResource(&gpu.bgra, src);
            let in_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let mut in_view: Option<ID3D11VideoProcessorInputView> = None;
            gpu.video_device
                .CreateVideoProcessorInputView(
                    &gpu.bgra,
                    &gpu.enumerator,
                    &raw const in_view_desc,
                    Some(&raw mut in_view),
                )
                .map_err(|e| CodecError::Backend(format!("VP input view: {e}")))?;
            let in_view =
                in_view.ok_or_else(|| CodecError::Backend("VP input view not created".into()))?;
            let out_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut out_view: Option<ID3D11VideoProcessorOutputView> = None;
            gpu.video_device
                .CreateVideoProcessorOutputView(
                    &gpu.nv12,
                    &gpu.enumerator,
                    &raw const out_view_desc,
                    Some(&raw mut out_view),
                )
                .map_err(|e| CodecError::Backend(format!("VP output view: {e}")))?;
            let out_view =
                out_view.ok_or_else(|| CodecError::Backend("VP output view not created".into()))?;
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: TRUE,
                pInputSurface: ManuallyDrop::new(Some(in_view)),
                ..Default::default()
            };
            let blt = gpu
                .video_context
                .VideoProcessorBlt(&gpu.processor, &out_view, 0, std::slice::from_ref(&stream));
            // Reclaim the input view wrapped in ManuallyDrop so its COM
            // reference is released with the frame, not leaked.
            drop(ManuallyDrop::take(&mut stream.pInputSurface));
            blt.map_err(|e| CodecError::Backend(format!("VideoProcessorBlt: {e}")))?;
            gpu.context.CopyResource(&gpu.nv12_feed, &gpu.nv12);
        }
        let sample = self.make_gpu_sample()?;
        self.submit_and_collect(&sample)
    }
}

impl VideoEncoder for MfEncoder {
    fn encode(&mut self, frame: &VideoFrame<'_>) -> Result<EncodedPacket> {
        if frame.width != self.width || frame.height != self.height {
            return Err(CodecError::BadDimensions {
                width: frame.width,
                height: frame.height,
                stride: frame.stride,
                data_len: frame.data.len(),
            });
        }
        let data = self.convert_input(frame)?;
        let sample = self.make_sample(&data)?;
        self.submit_and_collect(&sample)
    }

    fn request_keyframe(&mut self) {
        unsafe {
            let _ = self
                .codec_api
                .SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &variant_u32(1));
        }
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        unsafe {
            self.codec_api
                .SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(kbps * 1000))
                .map_err(|e| CodecError::Backend(format!("set mean bitrate: {e}")))?;
        }
        self.kbps = kbps;
        Ok(())
    }

    fn codec_type(&self) -> VideoCodecType {
        self.codec
    }
}
