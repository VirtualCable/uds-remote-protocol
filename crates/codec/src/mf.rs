// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com
//! Media Foundation video encoder backend (Windows, §18.75 HW-encode H1).
//!
//! Runtime MFT enumeration plus [`MfEncoder`], a VP9 encoder behind the
//! platform-neutral [`crate::VideoEncoder`] trait: drivers register their
//! hardware MFTs (Intel/NVIDIA/AMD) and the OS ships software MFTs, so one
//! backend covers every vendor. Frames arrive in BGRA (CPU) and are fed as
//! NV12/I420; the H2 step (zero-copy DXGI texture → MFT) plugs in later by
//! replacing only the input path of this module.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;

use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaBuffer, IMFMediaEventGenerator, IMFSample, IMFTransform,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFShutdown,
    MFStartup, MFVideoFormat_H264, MFVideoFormat_I420, MFVideoFormat_NV12, MFVideoFormat_VP90,
    MFVideoInterlace_Progressive, MFSampleExtension_CleanPoint, MFSTARTUP_FULL, MF_VERSION,
    MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SAMPLE_SIZE, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE,
    MF_TRANSFORM_FLAGS_Attribute, MFTEnumEx, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG,
    MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SYNCMFT,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_INFO,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO,
    MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, METransformHaveOutput, METransformNeedInput,
};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode,
    ICodecAPI, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
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
        for act in enumerate_activates(subtype)? {
            out.push(read_info(&act, label));
        }
    }
    Ok(out)
}

/// Enumerates the activates of encoder MFTs producing `subtype`, software
/// and hardware, field-of-use filtered out.
fn enumerate_activates(subtype: &GUID) -> Result<Vec<IMFActivate>> {
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: *subtype,
    };
    let flags = MFT_ENUM_FLAG(
        MFT_ENUM_FLAG_SYNCMFT.0
            | MFT_ENUM_FLAG_ASYNCMFT.0
            | MFT_ENUM_FLAG_HARDWARE.0
            | MFT_ENUM_FLAG_LOCALMFT.0,
    );
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
    let flags = unsafe { act.GetUINT32(&MF_TRANSFORM_FLAGS_Attribute).unwrap_or(0) };
    MftInfo {
        name,
        subtype: subtype.to_string(),
        d3d11_aware: unsafe { act.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) } != 0,
        // MF_TRANSFORM_ASYNC = 1 in the MF_TRANSFORM_FLAGS attribute.
        async_mft: flags & 1 != 0,
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

/// VP9 video encoder through a Media Foundation transform (§18.75 H1).
/// CPU input (BGRA → NV12/I420 internally); the backend is whichever MFT
/// the driver registered, hardware when available.
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
    width: u32,
    height: u32,
    fps: u32,
    kbps: u32,
    frame_counter: u64,
    pending: VecDeque<PendingPacket>,
    /// Friendly name of the chosen MFT (for the "encoder ready" log line).
    mft_name: String,
    _guard: MfGuard,
}

impl MfEncoder {
    /// Picks the best VP9 MFT (D3D11-aware first: hardware marks itself so)
    /// and configures it for `width`x`height` at `fps`, `kbps`, low latency.
    /// Returns `CodecError::Unsupported` when no VP9 MFT exists at all — the
    /// caller falls back to libvpx.
    pub fn new_vp9(width: u32, height: u32, fps: u32, kbps: u32) -> Result<Self> {
        let guard = MfGuard::acquire()?;
        let activates = enumerate_activates(&MFVideoFormat_VP90)?;
        if activates.is_empty() {
            return Err(CodecError::Unsupported("no VP9 encoder MFT registered"));
        }
        // Prefer D3D11-aware (hardware); among equals, the registry order.
        let act = activates
            .iter()
            .max_by_key(|a| unsafe { a.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) })
            .expect("non-empty")
            .clone();
        let info = read_info(&act, "VP9");

        let transform: IMFTransform = unsafe { act.ActivateObject() }
            .map_err(|e| CodecError::Backend(format!("MFT activate: {e}")))?;
        let codec_api: ICodecAPI = transform
            .cast()
            .map_err(|e| CodecError::Backend(format!("MFT has no ICodecAPI: {e}")))?;
        let events: Option<IMFMediaEventGenerator> = transform.cast().ok();

        // Non-default stream IDs are rare; try the query, default to 0/0.
        let (mut in_stream, mut out_stream) = (0u32, 0u32);
        unsafe {
            let _ = transform.GetStreamIDs(
                std::slice::from_mut(&mut in_stream),
                std::slice::from_mut(&mut out_stream),
            );
        }

        // Output type: video/VP9 with size, rate and average bitrate.
        let out_type = unsafe { MFCreateMediaType() }
            .map_err(|e| CodecError::Backend(format!("MFCreateMediaType: {e}")))?;
        unsafe {
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|()| out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_VP90))
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
        unsafe {
            let _ = codec_api.SetValue(&CODECAPI_AVLowLatencyMode, &variant_bool(true));
            codec_api
                .SetValue(&CODECAPI_AVEncCommonMeanBitRate, &variant_u32(kbps * 1000))
                .map_err(|e| CodecError::Backend(format!("set mean bitrate: {e}")))?;
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
            width,
            height,
            fps,
            kbps,
            frame_counter: 0,
            pending: VecDeque::new(),
            mft_name: info.name,
            _guard: guard,
        })
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
        let buffer = unsafe { MFCreateMemoryBuffer(data.len() as u32) }
            .map_err(|e| CodecError::Backend(format!("MFCreateMemoryBuffer: {e}")))?;
        unsafe {
            let mut ptr = std::ptr::null_mut();
            buffer
                .Lock(&raw mut ptr, None, None)
                .map_err(|e| CodecError::Backend(format!("buffer lock: {e}")))?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            buffer
                .Unlock()
                .and_then(|()| buffer.SetCurrentLength(data.len() as u32))
                .map_err(|e| CodecError::Backend(format!("buffer commit: {e}")))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| CodecError::Backend(format!("AddBuffer: {e}")))?;
            let dur = 10_000_000i64 / i64::from(self.fps.max(1));
            sample.SetSampleTime(self.frame_counter as i64 * dur).ok();
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
        self.frame_counter += 1;

        // Feed; when the MFT is momentarily full, pump events until it
        // wants input again (async MFTs) or fail.
        let mut feed_attempts = 0u32;
        loop {
            match unsafe { self.transform.ProcessInput(self.in_stream, &sample, 0) } {
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

        // Sync MFT: drain right away. Async MFT: pump events until at least
        // one output packet lands in the queue.
        if self.events.is_none() {
            self.drain_outputs()?;
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
        VideoCodecType::Vp9
    }
}
