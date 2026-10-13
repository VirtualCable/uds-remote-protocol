//! VP9/VP8 video codecs over libvpx (feature `codec-vp9` / `codec-vp8`).
//!
//! Realtime, low-latency configuration for interactive desktop: no lag in
//! frames, keyframes on demand (never periodic storms), bitrate changeable
//! hot. Input is BGRA (converted to I420 internally); output of the decoder
//! is tight BGRA.
//!
//! Safety: all `unsafe` is confined here. Invariants: the libvpx context is
//! created/destroyed symmetrically, the image wrapper never outlives the
//! frame buffers it points to (wrap per `encode` call), and packet data is
//! copied out before the next libvpx call.

// FFI boundary: implicit borrows as raw pointers and C-integer casts are
// idiomatic here; the values (ABI versions, flags, deadlines) are small
// constants.
#![allow(clippy::borrow_as_ptr, clippy::cast_sign_loss, clippy::cast_possible_wrap)]
// `c_long` is i64 on Linux but i32 on Windows: flag/deadline casts are
// lossless on one target and sign-changing on the other by C ABI design.
#![allow(clippy::cast_lossless)]

use std::ffi::CStr;

use vpx_sys as sys;

use crate::color::{I420Frame, bgra_to_i420, i420_to_bgra};
use crate::error::{CodecError, Result};
use crate::video::{
    DecodedFrame, EncodedPacket, VideoCodecType, VideoDecoder, VideoEncoder, VideoFrame,
};

fn err_str(err: sys::vpx_codec_err_t) -> String {
    // SAFETY: vpx_codec_err_to_string returns a static NUL-terminated string.
    unsafe { CStr::from_ptr(sys::vpx_codec_err_to_string(err)) }
        .to_string_lossy()
        .into_owned()
}

fn ctx_err(ctx: &sys::vpx_codec_ctx_t, prefix: &str) -> CodecError {
    // SAFETY: ctx is initialized; vpx_codec_error returns a valid string.
    let detail = unsafe { CStr::from_ptr(sys::vpx_codec_error(ctx)) }
        .to_string_lossy()
        .into_owned();
    CodecError::Backend(format!("{prefix}: {detail}"))
}

/// Which libvpx interface to use (VP9 or VP8, encode or decode).
#[derive(Clone, Copy)]
enum Iface {
    Vp9Enc,
    Vp9Dec,
    Vp8Enc,
    Vp8Dec,
}

impl Iface {
    fn ptr(self) -> *const sys::vpx_codec_iface_t {
        // SAFETY: these return static interface structs.
        unsafe {
            match self {
                Self::Vp9Enc => sys::vpx_codec_vp9_cx(),
                Self::Vp9Dec => sys::vpx_codec_vp9_dx(),
                Self::Vp8Enc => sys::vpx_codec_vp8_cx(),
                Self::Vp8Dec => sys::vpx_codec_vp8_dx(),
            }
        }
    }
}

struct VpxEncoder {
    ctx: sys::vpx_codec_ctx_t,
    cfg: sys::vpx_codec_enc_cfg_t,
    force_key: bool,
    pts: i64,
    codec: VideoCodecType,
    // Reusable scratch for the packed I420 image (no alloc per frame).
    img_buf: Vec<u8>,
    out: Vec<u8>,
}

impl VpxEncoder {
    fn new(codec: VideoCodecType, width: u32, height: u32, kbps: u32) -> Result<Self> {
        Self::with_options(codec, width, height, kbps, None)
    }

    /// `cpu_used`: libvpx speed/quality knob (VP9: 0 best/slowest .. 8
    /// fastest; VP8: +16 best .. -16 fastest). `None` uses the realtime
    /// defaults tuned for interactive desktop (VP9: 6, VP8: -10).
    fn with_options(
        codec: VideoCodecType,
        width: u32,
        height: u32,
        kbps: u32,
        cpu_used: Option<i32>,
    ) -> Result<Self> {
        let iface = match codec {
            VideoCodecType::Vp9 => Iface::Vp9Enc,
            VideoCodecType::Vp8 => Iface::Vp8Enc,
            other => {
                return Err(CodecError::Unsupported(match other {
                    VideoCodecType::Raw => "raw is not a libvpx codec",
                    VideoCodecType::H264 => "h264 is not a libvpx codec",
                    VideoCodecType::Av1 => "av1 is not a libvpx codec",
                    _ => "unknown codec",
                }));
            }
        };
        // SAFETY: zeroed ctx/cfg are valid starting points per libvpx docs;
        // all pointers are either null or valid for the call durations.
        unsafe {
            let mut cfg: sys::vpx_codec_enc_cfg_t = std::mem::zeroed();
            let e = sys::vpx_codec_enc_config_default(iface.ptr(), &mut cfg, 0);
            if e != sys::vpx_codec_err_t_VPX_CODEC_OK {
                return Err(CodecError::Backend(err_str(e)));
            }
            cfg.g_w = width;
            cfg.g_h = height;
            cfg.g_timebase.num = 1;
            cfg.g_timebase.den = 30;
            cfg.rc_target_bitrate = kbps;
            cfg.g_lag_in_frames = 0; // realtime: no frame buffering
            cfg.g_error_resilient = sys::VPX_ERROR_RESILIENT_DEFAULT;
            cfg.kf_mode = sys::vpx_kf_mode_VPX_KF_DISABLED; // keyframes on demand only
            cfg.g_threads = 4;

            let mut ctx: sys::vpx_codec_ctx_t = std::mem::zeroed();
            let e = sys::vpx_codec_enc_init_ver(
                &mut ctx,
                iface.ptr(),
                &cfg,
                0,
                sys::VPX_ENCODER_ABI_VERSION as i32,
            );
            if e != sys::vpx_codec_err_t_VPX_CODEC_OK {
                return Err(ctx_err(&ctx, "vpx_codec_enc_init"));
            }

            // Speed/quality knob (control id 13 is shared by VP8 and VP9 in
            // libvpx ≥ 1.15) plus VP9 desktop-content tuning.
            let cpu = cpu_used.unwrap_or(match codec {
                VideoCodecType::Vp9 => 6,
                _ => -10,
            });
            let e = sys::vpx_codec_control_(&mut ctx, sys::vp8e_enc_control_id_VP8E_SET_CPUUSED as i32, cpu);
            if e != sys::vpx_codec_err_t_VPX_CODEC_OK {
                return Err(ctx_err(&ctx, "vpx_codec_control(CPUUSED)"));
            }
            if codec == VideoCodecType::Vp9 {
                // Desktop screen content (sharp edges, flat areas) and
                // row-based multithreading for realtime throughput.
                let e = sys::vpx_codec_control_(
                    &mut ctx,
                    sys::vp8e_enc_control_id_VP9E_SET_TUNE_CONTENT as i32,
                    sys::vp9e_tune_content_VP9E_CONTENT_SCREEN as i32,
                );
                if e != sys::vpx_codec_err_t_VPX_CODEC_OK {
                    return Err(ctx_err(&ctx, "vpx_codec_control(TUNE_CONTENT)"));
                }
                let e = sys::vpx_codec_control_(
                    &mut ctx,
                    sys::vp8e_enc_control_id_VP9E_SET_ROW_MT as i32,
                    1,
                );
                if e != sys::vpx_codec_err_t_VPX_CODEC_OK {
                    return Err(ctx_err(&ctx, "vpx_codec_control(ROW_MT)"));
                }
            }

            Ok(Self {
                ctx,
                cfg,
                force_key: true, // first frame is always a keyframe
                pts: 0,
                codec,
                img_buf: Vec::new(),
                out: Vec::new(),
            })
        }
    }

    /// Packs the I420 planes contiguously and wraps them in a
    /// `vpx_image_t` valid for the duration of the call.
    fn wrap_image(&mut self, i420: &I420Frame) -> sys::vpx_image_t {
        let (w, h) = (i420.width as usize, i420.height as usize);
        self.img_buf.clear();
        self.img_buf.extend_from_slice(&i420.y);
        self.img_buf.extend_from_slice(&i420.u);
        self.img_buf.extend_from_slice(&i420.v);
        // SAFETY: img_buf outlives the returned wrapper use (single encode
        // call); libvpx reads exactly w*h + 2*(w/2*h/2) bytes.
        unsafe {
            let mut img: sys::vpx_image_t = std::mem::zeroed();
            let p = sys::vpx_img_wrap(
                &mut img,
                sys::vpx_img_fmt_VPX_IMG_FMT_I420,
                i420.width,
                i420.height,
                1,
                self.img_buf.as_mut_ptr(),
            );
            debug_assert!(!p.is_null());
            let _ = (w, h);
            img
        }
    }

    fn encode_impl(&mut self, frame: &VideoFrame<'_>) -> Result<EncodedPacket> {
        let i420 = bgra_to_i420(frame)?;
        let img = self.wrap_image(&i420);
        let flags = if self.force_key {
            self.force_key = false;
            sys::VPX_EFLAG_FORCE_KF
        } else {
            0
        };
        // SAFETY: ctx initialized; img valid for this call.
        unsafe {
            let e = sys::vpx_codec_encode(
                &mut self.ctx,
                &img,
                self.pts,
                1,
                flags as sys::vpx_enc_frame_flags_t,
                sys::VPX_DL_REALTIME as sys::vpx_enc_deadline_t,
            );
            if e != sys::vpx_codec_err_t_VPX_CODEC_OK {
                return Err(ctx_err(&self.ctx, "vpx_codec_encode"));
            }
        }
        self.pts += 1;

        self.out.clear();
        let mut keyframe = false;
        // SAFETY: packet iteration per libvpx contract; data copied out.
        unsafe {
            let mut iter: sys::vpx_codec_iter_t = std::ptr::null();
            loop {
                let pkt = sys::vpx_codec_get_cx_data(&mut self.ctx, &mut iter);
                if pkt.is_null() {
                    break;
                }
                let pkt = &*pkt;
                if pkt.kind == sys::vpx_codec_cx_pkt_kind_VPX_CODEC_CX_FRAME_PKT {
                    let f = pkt.data.frame;
                    let slice =
                        std::slice::from_raw_parts(f.buf.cast::<u8>(), f.sz);
                    self.out.extend_from_slice(slice);
                    keyframe |= f.flags & sys::VPX_FRAME_IS_KEY != 0;
                }
            }
        }
        Ok(EncodedPacket {
            data: self.out.clone(),
            keyframe,
        })
    }
}

impl Drop for VpxEncoder {
    fn drop(&mut self) {
        // SAFETY: ctx was initialized in new().
        unsafe {
            sys::vpx_codec_destroy(&mut self.ctx);
        }
    }
}

impl VideoEncoder for VpxEncoder {
    fn encode(&mut self, frame: &VideoFrame<'_>) -> Result<EncodedPacket> {
        self.encode_impl(frame)
    }

    fn request_keyframe(&mut self) {
        self.force_key = true;
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        self.cfg.rc_target_bitrate = kbps;
        // SAFETY: ctx and cfg are consistent (cfg is the live config).
        unsafe {
            let e = sys::vpx_codec_enc_config_set(&mut self.ctx, &self.cfg);
            if e != sys::vpx_codec_err_t_VPX_CODEC_OK {
                return Err(ctx_err(&self.ctx, "vpx_codec_enc_config_set"));
            }
        }
        Ok(())
    }

    fn codec_type(&self) -> VideoCodecType {
        self.codec
    }
}

struct VpxDecoder {
    ctx: sys::vpx_codec_ctx_t,
    codec: VideoCodecType,
}

impl VpxDecoder {
    fn new(codec: VideoCodecType) -> Result<Self> {
        let iface = match codec {
            VideoCodecType::Vp9 => Iface::Vp9Dec,
            VideoCodecType::Vp8 => Iface::Vp8Dec,
            _ => return Err(CodecError::Unsupported("not a libvpx decoder")),
        };
        // SAFETY: zeroed ctx is valid; null cfg uses decoder defaults.
        unsafe {
            let mut ctx: sys::vpx_codec_ctx_t = std::mem::zeroed();
            let e = sys::vpx_codec_dec_init_ver(
                &mut ctx,
                iface.ptr(),
                std::ptr::null(),
                0,
                sys::VPX_DECODER_ABI_VERSION as i32,
            );
            if e != sys::vpx_codec_err_t_VPX_CODEC_OK {
                return Err(CodecError::Backend(err_str(e)));
            }
            Ok(Self { ctx, codec })
        }
    }
}

impl Drop for VpxDecoder {
    fn drop(&mut self) {
        // SAFETY: ctx was initialized in new().
        unsafe {
            sys::vpx_codec_destroy(&mut self.ctx);
        }
    }
}

impl VideoDecoder for VpxDecoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<DecodedFrame> {
        // SAFETY: ctx initialized; packet data valid for the call; frame
        // iteration per libvpx contract.
        unsafe {
            let e = sys::vpx_codec_decode(
                &mut self.ctx,
                packet.data.as_ptr(),
                u32::try_from(packet.data.len()).map_err(|_| CodecError::Malformed)?,
                std::ptr::null_mut(),
                0,
            );
            if e != sys::vpx_codec_err_t_VPX_CODEC_OK {
                return Err(ctx_err(&self.ctx, "vpx_codec_decode"));
            }

            let mut iter: sys::vpx_codec_iter_t = std::ptr::null();
            let img = sys::vpx_codec_get_frame(&mut self.ctx, &mut iter);
            if img.is_null() {
                return Err(CodecError::Malformed);
            }
            let img = &*img;
            let (w, h) = (img.d_w, img.d_h);
            let y_stride = usize::try_from(img.stride[0]).map_err(|_| CodecError::Malformed)?;
            let uv_stride = usize::try_from(img.stride[1]).map_err(|_| CodecError::Malformed)?;
            let uv_h = (h as usize).div_ceil(2);
            if img.planes[0].is_null() || img.planes[1].is_null() || img.planes[2].is_null() {
                return Err(CodecError::Malformed);
            }
            let plane = |p: *const u8, stride: usize, rows: usize| -> Vec<u8> {
                let mut out = Vec::with_capacity(stride * rows);
                for row in 0..rows {
                    out.extend_from_slice(std::slice::from_raw_parts(p.add(row * stride), stride));
                }
                out
            };
            let i420 = I420Frame {
                width: w,
                height: h,
                y_stride,
                uv_stride,
                y: plane(img.planes[0], y_stride, h as usize),
                u: plane(img.planes[1], uv_stride, uv_h),
                v: plane(img.planes[2], uv_stride, uv_h),
            };
            i420_to_bgra(&i420)
        }
    }

    fn codec_type(&self) -> VideoCodecType {
        self.codec
    }
}

/// VP9 encoder (libvpx, realtime profile).
pub struct Vp9Encoder(VpxEncoder);

// SAFETY: the libvpx codec context is !Send only because it holds raw
// pointers; libvpx contexts are single-threaded but movable — every access
// goes through `&mut self` trait methods, so ownership transfer across
// threads can never alias a live use (the trait requires `Send`, §15.3).
unsafe impl Send for VpxEncoder {}
// SAFETY: same argument as `VpxEncoder` above.
unsafe impl Send for VpxDecoder {}

impl Vp9Encoder {
    /// Creates a VP9 encoder for `width`x`height` at `kbps` target bitrate.
    pub fn new(width: u32, height: u32, kbps: u32) -> Result<Self> {
        Ok(Self(VpxEncoder::new(VideoCodecType::Vp9, width, height, kbps)?))
    }

    /// Creates a VP9 encoder with an explicit `cpu_used` (0 best/slowest ..
    /// 8 fastest; realtime default is 6).
    pub fn with_cpu_used(width: u32, height: u32, kbps: u32, cpu_used: i32) -> Result<Self> {
        Ok(Self(VpxEncoder::with_options(
            VideoCodecType::Vp9,
            width,
            height,
            kbps,
            Some(cpu_used),
        )?))
    }
}

impl VideoEncoder for Vp9Encoder {
    fn encode(&mut self, frame: &VideoFrame<'_>) -> Result<EncodedPacket> {
        self.0.encode(frame)
    }

    fn request_keyframe(&mut self) {
        self.0.request_keyframe();
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        self.0.set_bitrate(kbps)
    }

    fn codec_type(&self) -> VideoCodecType {
        self.0.codec_type()
    }
}

/// VP9 decoder (libvpx).
pub struct Vp9Decoder(VpxDecoder);

impl Vp9Decoder {
    /// Creates a VP9 decoder.
    pub fn new() -> Result<Self> {
        Ok(Self(VpxDecoder::new(VideoCodecType::Vp9)?))
    }
}

impl VideoDecoder for Vp9Decoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<DecodedFrame> {
        self.0.decode(packet)
    }

    fn codec_type(&self) -> VideoCodecType {
        self.0.codec_type()
    }
}

/// VP8 encoder (libvpx, realtime profile; the low-CPU option — see the
/// design doc §8: worse ratio than VP9, ~2-3x cheaper encode).
#[cfg(feature = "codec-vp8")]
pub struct Vp8Encoder(VpxEncoder);

#[cfg(feature = "codec-vp8")]
impl Vp8Encoder {
    /// Creates a VP8 encoder for `width`x`height` at `kbps` target bitrate.
    pub fn new(width: u32, height: u32, kbps: u32) -> Result<Self> {
        Ok(Self(VpxEncoder::new(VideoCodecType::Vp8, width, height, kbps)?))
    }

    /// Creates a VP8 encoder with an explicit `cpu_used` (+16 best .. -16
    /// fastest; realtime default is -10).
    pub fn with_cpu_used(width: u32, height: u32, kbps: u32, cpu_used: i32) -> Result<Self> {
        Ok(Self(VpxEncoder::with_options(
            VideoCodecType::Vp8,
            width,
            height,
            kbps,
            Some(cpu_used),
        )?))
    }
}

#[cfg(feature = "codec-vp8")]
impl VideoEncoder for Vp8Encoder {
    fn encode(&mut self, frame: &VideoFrame<'_>) -> Result<EncodedPacket> {
        self.0.encode(frame)
    }

    fn request_keyframe(&mut self) {
        self.0.request_keyframe();
    }

    fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
        self.0.set_bitrate(kbps)
    }

    fn codec_type(&self) -> VideoCodecType {
        self.0.codec_type()
    }
}

/// VP8 decoder (libvpx).
#[cfg(feature = "codec-vp8")]
pub struct Vp8Decoder(VpxDecoder);

#[cfg(feature = "codec-vp8")]
impl Vp8Decoder {
    /// Creates a VP8 decoder.
    pub fn new() -> Result<Self> {
        Ok(Self(VpxDecoder::new(VideoCodecType::Vp8)?))
    }
}

#[cfg(feature = "codec-vp8")]
impl VideoDecoder for Vp8Decoder {
    fn decode(&mut self, packet: &EncodedPacket) -> Result<DecodedFrame> {
        self.0.decode(packet)
    }

    fn codec_type(&self) -> VideoCodecType {
        self.0.codec_type()
    }
}
