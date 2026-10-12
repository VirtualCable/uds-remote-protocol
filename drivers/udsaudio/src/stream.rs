//! `IMiniportWaveRTStream` implementation: a virtual DMA stream. The render
//! sink discards whatever apps write; the capture/loopback sources return
//! silence (the mapped buffer is zeroed on allocation and nobody writes to
//! it). The position clock is simulated from the performance counter — the
//! pure math lives in [`crate::logic::PositionClock`].

use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use alloc::boxed::Box;

use crate::common::{
    STATUS_INVALID_PARAMETER, STATUS_NOINTERFACE, STATUS_NOT_IMPLEMENTED, STATUS_NOT_SUPPORTED,
    STATUS_SUCCESS, STATUS_UNSUCCESSFUL, guid_eq,
};
use crate::dbg::dlog;
use crate::logic::{self, FilterKind, PositionClock};
use crate::portcls as pc;
use crate::tables;

unsafe extern "C" {
    /// `KeQueryPerformanceCounter` (ntddk.h) — not in wdk-sys's base subset.
    /// Callable at any IRQL.
    fn KeQueryPerformanceCounter(performance_frequency: *mut i64) -> u64;
}

/// KSSTATE numeric values (ks.h).
const KSSTATE_STOP: i32 = 0;
const KSSTATE_ACQUIRE: i32 = 1;
const KSSTATE_PAUSE: i32 = 2;
const KSSTATE_RUN: i32 = 3;

/// A WaveRT stream. The first field is the vtable pointer so that the
/// struct address is directly usable as `PMINIPORTWAVERTSTREAM` /
/// `PUNKNOWN` (the C++ COM layout PortCls expects).
#[repr(C)]
pub struct WaveRtStream {
    vtbl: *const pc::IMiniportWaveRTStreamVtbl,
    ref_count: AtomicU32,
    kind: FilterKind,
    pin: u32,
    format: logic::PcmFormat,
    clock: PositionClock,
    state: i32,
    port_stream: pc::PPORTWAVERTSTREAM,
    /// MDL of the audio buffer, owned between Allocate/FreeAudioBuffer.
    audio_mdl: pc::PMDL,
    /// Kernel mapping of the audio buffer (we zero capture buffers).
    audio_buffer: *mut u8,
    audio_buffer_size: u32,
}

impl WaveRtStream {
    /// Creates a boxed stream with one reference (the caller's), ready to be
    /// handed to PortCls from `NewStream`.
    pub fn create(
        kind: FilterKind,
        pin: u32,
        port_stream: pc::PPORTWAVERTSTREAM,
    ) -> *mut Self {
        let Some(format) = kind.stream_format(pin) else {
            return ptr::null_mut();
        };
        let stream = Box::new(Self {
            vtbl: &raw const STREAM_VTBL,
            ref_count: AtomicU32::new(1),
            kind,
            pin,
            format,
            clock: PositionClock::new(format),
            state: KSSTATE_STOP,
            port_stream,
            audio_mdl: ptr::null_mut(),
            audio_buffer: ptr::null_mut(),
            audio_buffer_size: 0,
        });
        Box::into_raw(stream)
    }
}

unsafe extern "C" fn stream_query_interface(
    this: *mut pc::INTERFACE,
    interface_id: *const pc::IID,
    interface: *mut pc::PVOID,
) -> i32 {
    // SAFETY: `this` is one of our WaveRtStream (PortCls only calls us
    // through the pointer we handed out). `interface_id`/`interface` are
    // valid out-params per the COM contract.
    unsafe {
        if interface.is_null() || interface_id.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        *interface = ptr::null_mut();
        let iid = &*interface_id;
        if guid_eq(iid, &tables::IID_IUNKNOWN) || guid_eq(iid, &tables::IID_IMINIPORT_WAVE_RT_STREAM)
        {
            *interface = this.cast();
            stream_add_ref(this);
            STATUS_SUCCESS
        } else {
            STATUS_NOINTERFACE
        }
    }
}

unsafe extern "C" fn stream_add_ref(this: *mut pc::INTERFACE) -> u32 {
    // SAFETY: `this` is a live WaveRtStream.
    unsafe { &*(this as *const WaveRtStream) }
        .ref_count
        .fetch_add(1, Ordering::Relaxed)
        + 1
}

unsafe extern "C" fn stream_release(this: *mut pc::INTERFACE) -> u32 {
    // SAFETY: `this` is a live WaveRtStream owned by the caller's reference.
    let stream = this as *mut WaveRtStream;
    let prev = unsafe { &*stream }.ref_count.fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        dlog!("stream dropped");
        // SAFETY: refcount hit zero, no other reference exists; the Box was
        // created by `create` and is dropped exactly once here.
        drop(unsafe { Box::from_raw(stream) });
        0
    } else {
        prev - 1
    }
}

unsafe extern "C" fn stream_set_format(
    this: *mut pc::INTERFACE,
    data_format: pc::PKSDATAFORMAT,
) -> i32 {
    // SAFETY: `this` is a live WaveRtStream; `data_format` points to a
    // caller-owned KSDATAFORMAT we only read.
    unsafe {
        let stream = &mut *(this as *mut WaveRtStream);
        if data_format.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        // Only accept the exact advertised format. Parse WAVEFORMATEX if the
        // specifier matches.
        let df = &*data_format;
        let fields = &df.__bindgen_anon_1;
        if !guid_eq(&fields.Specifier, &tables::KSDATAFORMAT_SPECIFIER_WAVEFORMATEX) {
            dlog!("SetFormat: unsupported specifier");
            return STATUS_NOT_SUPPORTED;
        }
        if fields.FormatSize < size_of::<pc::KSDATAFORMAT_WAVEFORMATEX>() as u32 {
            return STATUS_INVALID_PARAMETER;
        }
        // KSDATAFORMAT_WAVEFORMATEX is #[repr(C, packed)]: the WAVEFORMATEX
        // member sits right after the KSDATAFORMAT union and may be
        // unaligned, so read fields via read_unaligned.
        let wf_ptr = (data_format as *const u8)
            .add(size_of::<pc::KSDATAFORMAT>())
            .cast::<pc::WAVEFORMATEX>();
        let proposed = logic::PcmFormat {
            sample_rate: ptr::read_unaligned(ptr::addr_of!((*wf_ptr).nSamplesPerSec)),
            channels: ptr::read_unaligned(ptr::addr_of!((*wf_ptr).nChannels)),
            bits_per_sample: ptr::read_unaligned(ptr::addr_of!((*wf_ptr).wBitsPerSample)),
        };
        if !logic::is_format_supported(stream.kind, stream.pin, proposed) {
            dlog!("SetFormat: format rejected");
            return STATUS_NOT_SUPPORTED;
        }
        stream.format = proposed;
        stream.clock = PositionClock::new(proposed);
        STATUS_SUCCESS
    }
}


/// Current QPC time and frequency.
fn qpc() -> (u64, u64) {
    let mut freq: i64 = 0;
    // SAFETY: KeQueryPerformanceCounter is callable at any IRQL; the out
    // param is a valid stack pointer.
    let ticks = unsafe { KeQueryPerformanceCounter(&raw mut freq) };
    (ticks, freq as u64)
}

unsafe extern "C" fn stream_set_state(this: *mut pc::INTERFACE, state: i32) -> i32 {
    // SAFETY: `this` is a live WaveRtStream.
    unsafe {
        let stream = &mut *(this as *mut WaveRtStream);
        let (now, freq) = qpc();
        match state {
            KSSTATE_RUN => {
                if !stream.clock.is_running() {
                    stream.clock.start(now, freq);
                }
            }
            KSSTATE_PAUSE | KSSTATE_ACQUIRE => {
                if stream.state == KSSTATE_RUN {
                    stream.clock.stop(now);
                }
            }
            KSSTATE_STOP => {
                stream.clock.stop(now);
                stream.clock.reset();
            }
            _ => {
                dlog!("SetState: unknown state", state as u64);
                return STATUS_INVALID_PARAMETER;
            }
        }
        stream.state = state;
        STATUS_SUCCESS
    }
}

unsafe extern "C" fn stream_get_position(
    this: *mut pc::INTERFACE,
    position: *mut pc::KSAUDIO_POSITION,
) -> i32 {
    // SAFETY: `this` is a live WaveRtStream; `position` is a valid out
    // pointer per the PortCls contract.
    unsafe {
        if position.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        let stream = &mut *(this as *mut WaveRtStream);
        let (now, _freq) = qpc();
        let byte_pos = stream.clock.position_at(now);
        let pos = &mut *position;
        pos.PlayOffset = byte_pos;
        pos.WriteOffset = byte_pos;
        STATUS_SUCCESS
    }
}

unsafe extern "C" fn stream_allocate_audio_buffer(
    this: *mut pc::INTERFACE,
    requested_size: u32,
    audio_buffer_mdl: *mut pc::PMDL,
    actual_size: *mut u32,
    offset_from_first_page: *mut u32,
    cache_type: *mut pc::MEMORY_CACHING_TYPE,
) -> i32 {
    // SAFETY: `this` is a live WaveRtStream; the out pointers are valid per
    // the PortCls contract; the port stream vtable is owned by PortCls and
    // outlives us.
    unsafe {
        let stream = &mut *(this as *mut WaveRtStream);
        if audio_buffer_mdl.is_null()
            || actual_size.is_null()
            || offset_from_first_page.is_null()
            || cache_type.is_null()
        {
            return STATUS_INVALID_PARAMETER;
        }
        if stream.port_stream.is_null() {
            return STATUS_UNSUCCESSFUL;
        }
        // HighAddress = 0xFFFFFFFF (anywhere below 4 GB).
        let high_address = pc::PHYSICAL_ADDRESS {
            QuadPart: 0xFFFF_FFFF,
        };
        let vtbl = (*stream.port_stream).lpVtbl;
        let Some(allocate) = (*vtbl).AllocatePagesForMdl else {
            return STATUS_UNSUCCESSFUL;
        };
        let mdl = allocate(
            stream.port_stream as *mut pc::INTERFACE,
            high_address,
            requested_size as u64,
        );
        if mdl.is_null() {
            dlog!("AllocateAudioBuffer: AllocatePagesForMdl failed");
            return STATUS_UNSUCCESSFUL;
        }
        // Map the pages so we can zero capture buffers (silence).
        let Some(map) = (*vtbl).MapAllocatedPages else {
            return STATUS_UNSUCCESSFUL;
        };
        let mapping = map(
            stream.port_stream as *mut pc::INTERFACE,
            mdl,
            pc::_MEMORY_CACHING_TYPE_MmCached,
        );
        if !mapping.is_null() {
            // Capture/loopback buffers start as silence. Render sink
            // contents are discarded, so zeroing is harmless there too.
            ptr::write_bytes(mapping as *mut u8, 0, requested_size as usize);
        }
        stream.audio_mdl = mdl;
        stream.audio_buffer = mapping as *mut u8;
        stream.audio_buffer_size = requested_size;
        *audio_buffer_mdl = mdl;
        *actual_size = requested_size;
        *offset_from_first_page = 0;
        *cache_type = pc::_MEMORY_CACHING_TYPE_MmCached;
        STATUS_SUCCESS
    }
}

unsafe extern "C" fn stream_free_audio_buffer(
    this: *mut pc::INTERFACE,
    audio_buffer_mdl: pc::PMDL,
    _buffer_size: u32,
) {
    // SAFETY: `this` is a live WaveRtStream; the MDL belongs to us.
    unsafe {
        let stream = &mut *(this as *mut WaveRtStream);
        if stream.port_stream.is_null() || audio_buffer_mdl.is_null() {
            return;
        }
        let vtbl = (*stream.port_stream).lpVtbl;
        if !stream.audio_buffer.is_null() {
            if let Some(unmap) = (*vtbl).UnmapAllocatedPages {
                unmap(
                    stream.port_stream as *mut pc::INTERFACE,
                    stream.audio_buffer.cast(),
                    audio_buffer_mdl,
                );
            }
            stream.audio_buffer = ptr::null_mut();
        }
        if let Some(free) = (*vtbl).FreePagesFromMdl {
            free(stream.port_stream as *mut pc::INTERFACE, audio_buffer_mdl);
        }
        stream.audio_mdl = ptr::null_mut();
        stream.audio_buffer_size = 0;
    }
}

unsafe extern "C" fn stream_get_hw_latency(
    _this: *mut pc::INTERFACE,
    hw_latency: *mut pc::KSRTAUDIO_HWLATENCY,
) {
    // SAFETY: `hw_latency` is a valid out pointer per the PortCls contract.
    unsafe {
        if !hw_latency.is_null() {
            let latency = &mut *hw_latency;
            latency.FifoSize = 0;
            latency.ChipsetDelay = 0;
            latency.CodecDelay = 0;
        }
    }
}

unsafe extern "C" fn stream_get_position_register(
    _this: *mut pc::INTERFACE,
    _register: *mut pc::KSRTAUDIO_HWREGISTER,
) -> i32 {
    // No hardware position register: the audio stack falls back to polling
    // GetPosition (same as sysvad).
    STATUS_NOT_IMPLEMENTED
}

unsafe extern "C" fn stream_get_clock_register(
    _this: *mut pc::INTERFACE,
    _register: *mut pc::KSRTAUDIO_HWREGISTER,
) -> i32 {
    STATUS_NOT_IMPLEMENTED
}

/// The stream vtable. Field order matches `IMiniportWaveRTStreamVtbl` from
/// portcls.h exactly (IUnknown, then the stream methods).
static STREAM_VTBL: pc::IMiniportWaveRTStreamVtbl = pc::IMiniportWaveRTStreamVtbl {
    QueryInterface: Some(stream_query_interface),
    AddRef: Some(stream_add_ref),
    Release: Some(stream_release),
    SetFormat: Some(stream_set_format),
    SetState: Some(stream_set_state),
    GetPosition: Some(stream_get_position),
    AllocateAudioBuffer: Some(stream_allocate_audio_buffer),
    FreeAudioBuffer: Some(stream_free_audio_buffer),
    GetHWLatency: Some(stream_get_hw_latency),
    GetPositionRegister: Some(stream_get_position_register),
    GetClockRegister: Some(stream_get_clock_register),
};
