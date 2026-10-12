//! `IMiniportWaveRT` and `IMiniportTopology` implementations.
//!
//! Hand-written COM vtables: each object starts with its vtable pointer (C++
//! COM layout), followed by an atomic reference count. PortCls owns the
//! objects through the port it wraps them in; when the port releases its
//! last reference our `Release` drops the Box.

use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use alloc::boxed::Box;

use crate::common::{
    STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL, STATUS_INSUFFICIENT_RESOURCES,
    STATUS_INVALID_PARAMETER, STATUS_NOINTERFACE, STATUS_NO_MATCH, STATUS_NOT_IMPLEMENTED,
    STATUS_SUCCESS, guid_eq,
};
use crate::dbg::dlog;
use crate::logic::{self, FilterKind};
use crate::portcls as pc;
use crate::stream::WaveRtStream;
use crate::tables;

/// Miniport object shared by both interfaces (wave + topology). The first
/// field is always the vtable pointer of the interface PortCls asked for.
#[repr(C)]
pub struct Miniport {
    vtbl: *const VtblHeader,
    ref_count: AtomicU32,
    kind: FilterKind,
    /// Back-pointer to the owning port (set in `Init`; wave miniports only
    /// use it today, kept for both for symmetry).
    port: pc::PPORT,
}

/// Marker for "a vtable pointer" — the real statics are the concrete
/// `IMiniportWaveRTVtbl` / `IMiniportTopologyVtbl`; only the address matters.
#[repr(C)]
struct VtblHeader {
    _private: [usize; 0],
}

impl Miniport {
    /// Creates a boxed wave miniport with one reference (the caller's).
    pub fn create_wave(kind: FilterKind) -> *mut Self {
        Self::create(kind, &raw const WAVE_VTBL as *const VtblHeader)
    }

    /// Creates a boxed topology miniport with one reference.
    pub fn create_topology(kind: FilterKind) -> *mut Self {
        Self::create(kind, &raw const TOPO_VTBL as *const VtblHeader)
    }

    fn create(kind: FilterKind, vtbl: *const VtblHeader) -> *mut Self {
        let miniport = Box::new(Self {
            vtbl,
            ref_count: AtomicU32::new(1),
            kind,
            port: ptr::null_mut(),
        });
        Box::into_raw(miniport)
    }

    /// Releases one reference to a miniport, freeing it when the count
    /// reaches zero. Used by the adapter for the creation reference after
    /// `IPort::Init` took over.
    ///
    /// SAFETY: `this` must be a live Miniport with a reference owned by the
    /// caller.
    pub unsafe fn release_reference(this: *mut Self) {
        // SAFETY: same body as the vtable Release; guaranteed by caller.
        unsafe {
            release(this as *mut pc::INTERFACE);
        }
    }
}

// --- shared IUnknown ---------------------------------------------------------

unsafe fn add_ref(this: *mut pc::INTERFACE) -> u32 {
    // SAFETY: `this` is a live Miniport.
    unsafe { &*(this as *const Miniport) }
        .ref_count
        .fetch_add(1, Ordering::Relaxed)
        + 1
}

unsafe fn release(this: *mut pc::INTERFACE) -> u32 {
    // SAFETY: `this` is a live Miniport owned by the caller's reference.
    let miniport = this as *mut Miniport;
    let prev = unsafe { &*miniport }
        .ref_count
        .fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        dlog!("miniport dropped");
        // SAFETY: refcount hit zero; the Box from `create` is dropped once.
        drop(unsafe { Box::from_raw(miniport) });
        0
    } else {
        prev - 1
    }
}

unsafe fn query_interface(
    this: *mut pc::INTERFACE,
    interface_id: *const pc::IID,
    interface: *mut pc::PVOID,
    specific: &pc::GUID,
) -> i32 {
    // SAFETY: pointers per the COM contract; `this` is a live Miniport.
    unsafe {
        if interface.is_null() || interface_id.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        *interface = ptr::null_mut();
        let iid = &*interface_id;
        if guid_eq(iid, &tables::IID_IUNKNOWN)
            || guid_eq(iid, &tables::IID_IMINIPORT)
            || guid_eq(iid, specific)
        {
            *interface = this.cast();
            add_ref(this);
            STATUS_SUCCESS
        } else {
            STATUS_NOINTERFACE
        }
    }
}

/// `IMiniport::GetDescription` — identical for both interfaces.
unsafe fn get_description(this: *mut pc::INTERFACE, description: *mut pc::PPCFILTER_DESCRIPTOR) -> i32 {
    // SAFETY: `this` is a live Miniport; `description` is a valid out param.
    unsafe {
        if description.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        let kind = (*(this as *const Miniport)).kind;
        *description = tables::filter_descriptor(kind) as pc::PPCFILTER_DESCRIPTOR;
        STATUS_SUCCESS
    }
}

/// `IMiniport::DataRangeIntersection` — single-format semantics: after
/// validating the specifier and channel count, the intersection is always
/// our one advertised PCM format, which we write into the resultant buffer
/// ourselves. Deferring to the PortCls class handler (sysvad style, with
/// STATUS_NOT_IMPLEMENTED) works for the stereo speaker but leaves the
/// mono mic without a negotiated device format (the engine's proposal is
/// stereo, the class handler does not downgrade channels, and
/// `IAudioClient::GetMixFormat` then fails with
/// AUDCLNT_E_UNSUPPORTED_FORMAT).
unsafe fn data_range_intersection(
    this: *mut pc::INTERFACE,
    pin_id: u32,
    client_data_range: pc::PKSDATARANGE,
    _my_data_range: pc::PKSDATARANGE,
    output_buffer_length: u32,
    resultant_format: pc::PVOID,
    resultant_format_length: *mut u32,
) -> i32 {
    // SAFETY: all pointers are caller-owned; we only read the range and
    // write the resultant format within the advertised output buffer size.
    unsafe {
        if client_data_range.is_null() || resultant_format_length.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        let kind = (*(this as *const Miniport)).kind;
        dlog!(
            "DRI: kind 0x%I64x",
            ((kind as u64) << 32) | u64::from(pin_id)
        );
        let Some(format) = kind.stream_format(pin_id) else {
            return STATUS_INVALID_PARAMETER;
        };
        let client = &(*client_data_range).__bindgen_anon_1;
        if !guid_eq(&client.Specifier, &tables::KSDATAFORMAT_SPECIFIER_WAVEFORMATEX) {
            dlog!("DRI: specifier mismatch");
            return STATUS_NOT_IMPLEMENTED;
        }
        let required = size_of::<pc::KSDATAFORMAT_WAVEFORMATEX>() as u32;
        if output_buffer_length == 0 {
            *resultant_format_length = required;
            return STATUS_BUFFER_OVERFLOW;
        }
        if output_buffer_length < required {
            return STATUS_BUFFER_TOO_SMALL;
        }
        if resultant_format.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        // The client's range must allow our channel count.
        let client_audio = &*(client_data_range as *const tables::DataRangeAudio);
        if client_audio.maximum_channels < format.channels as u32 {
            dlog!("DRI: no channel match");
            return STATUS_NO_MATCH;
        }
        dlog!("DRI: answering with our format");
        // The intersection of "anything reasonable" with a single-format
        // range is that format. KSDATAFORMAT_WAVEFORMATEX is packed: write
        // field by field with unaligned stores.
        let out = resultant_format as *mut pc::KSDATAFORMAT_WAVEFORMATEX;
        let df = ptr::addr_of_mut!((*out).DataFormat.__bindgen_anon_1);
        ptr::addr_of_mut!((*df).FormatSize).write_unaligned(required);
        ptr::addr_of_mut!((*df).Flags).write_unaligned(0);
        ptr::addr_of_mut!((*df).SampleSize).write_unaligned(format.frame_bytes() as u32);
        ptr::addr_of_mut!((*df).Reserved).write_unaligned(0);
        ptr::addr_of_mut!((*df).MajorFormat).write_unaligned(tables::KSDATAFORMAT_TYPE_AUDIO);
        ptr::addr_of_mut!((*df).SubFormat).write_unaligned(tables::KSDATAFORMAT_SUBTYPE_PCM);
        ptr::addr_of_mut!((*df).Specifier)
            .write_unaligned(tables::KSDATAFORMAT_SPECIFIER_WAVEFORMATEX);
        let wf = ptr::addr_of_mut!((*out).WaveFormatEx);
        let frame = format.frame_bytes() as u16;
        ptr::addr_of_mut!((*wf).wFormatTag).write_unaligned(1); // WAVE_FORMAT_PCM
        ptr::addr_of_mut!((*wf).nChannels).write_unaligned(format.channels);
        ptr::addr_of_mut!((*wf).nSamplesPerSec).write_unaligned(format.sample_rate);
        ptr::addr_of_mut!((*wf).nAvgBytesPerSec)
            .write_unaligned(format.bytes_per_sec() as u32);
        ptr::addr_of_mut!((*wf).nBlockAlign).write_unaligned(frame);
        ptr::addr_of_mut!((*wf).wBitsPerSample).write_unaligned(format.bits_per_sample);
        ptr::addr_of_mut!((*wf).cbSize).write_unaligned(0);
        *resultant_format_length = required;
        STATUS_SUCCESS
    }
}

// --- IMiniportWaveRT ------------------------------------------------------------

unsafe extern "C" fn wave_query_interface(
    this: *mut pc::INTERFACE,
    interface_id: *const pc::IID,
    interface: *mut pc::PVOID,
) -> i32 {
    // SAFETY: forwards to the shared helper; pointers checked there.
    unsafe { query_interface(this, interface_id, interface, &tables::IID_IMINIPORT_WAVE_RT) }
}

unsafe extern "C" fn wave_add_ref(this: *mut pc::INTERFACE) -> u32 {
    // SAFETY: `this` is a live Miniport.
    unsafe { add_ref(this) }
}

unsafe extern "C" fn wave_release(this: *mut pc::INTERFACE) -> u32 {
    // SAFETY: `this` is a live Miniport owned by the caller's reference.
    unsafe { release(this) }
}

unsafe extern "C" fn wave_get_description(
    this: *mut pc::INTERFACE,
    description: *mut pc::PPCFILTER_DESCRIPTOR,
) -> i32 {
    // SAFETY: forwards to the shared helper.
    unsafe { get_description(this, description) }
}

unsafe extern "C" fn wave_data_range_intersection(
    this: *mut pc::INTERFACE,
    pin_id: u32,
    client_data_range: pc::PKSDATARANGE,
    my_data_range: pc::PKSDATARANGE,
    output_buffer_length: u32,
    resultant_format: pc::PVOID,
    resultant_format_length: *mut u32,
) -> i32 {
    // SAFETY: forwards to the shared helper.
    unsafe {
        data_range_intersection(
            this,
            pin_id,
            client_data_range,
            my_data_range,
            output_buffer_length,
            resultant_format,
            resultant_format_length,
        )
    }
}

unsafe extern "C" fn wave_init(
    this: *mut pc::INTERFACE,
    _unknown_adapter: pc::PUNKNOWN,
    _resource_list: pc::PRESOURCELIST,
    port: pc::PPORTWAVERT,
) -> i32 {
    // SAFETY: `this` is a live Miniport; `port` is owned by PortCls and
    // outlives the miniport (the port holds the reference to us).
    unsafe {
        let miniport = &mut *(this as *mut Miniport);
        miniport.port = port as pc::PPORT;
        STATUS_SUCCESS
    }
}

unsafe extern "C" fn wave_new_stream(
    this: *mut pc::INTERFACE,
    stream: *mut pc::PMINIPORTWAVERTSTREAM,
    port_stream: pc::PPORTWAVERTSTREAM,
    pin: u32,
    _capture: pc::BOOLEAN,
    data_format: pc::PKSDATAFORMAT,
) -> i32 {
    // SAFETY: `this` is a live Miniport; out params per the PortCls
    // contract; the format is validated before use.
    unsafe {
        if stream.is_null() || data_format.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        *stream = ptr::null_mut();
        let miniport = &mut *(this as *mut Miniport);
        let kind = miniport.kind;

        // Parse the proposed format (WAVEFORMATEX specifier).
        let fields = &(*data_format).__bindgen_anon_1;
        if !guid_eq(&fields.Specifier, &tables::KSDATAFORMAT_SPECIFIER_WAVEFORMATEX)
            || fields.FormatSize < size_of::<pc::KSDATAFORMAT_WAVEFORMATEX>() as u32
        {
            dlog!("NewStream: bad specifier/size");
            return STATUS_INVALID_PARAMETER;
        }
        let wf_ptr = (data_format as *const u8)
            .add(size_of::<pc::KSDATAFORMAT>())
            .cast::<pc::WAVEFORMATEX>();
        let proposed = logic::PcmFormat {
            sample_rate: ptr::read_unaligned(ptr::addr_of!((*wf_ptr).nSamplesPerSec)),
            channels: ptr::read_unaligned(ptr::addr_of!((*wf_ptr).nChannels)),
            bits_per_sample: ptr::read_unaligned(ptr::addr_of!((*wf_ptr).wBitsPerSample)),
        };
        if !logic::is_format_supported(kind, pin, proposed) {
            dlog!("NewStream: format not supported");
            return STATUS_INVALID_PARAMETER;
        }

        let new_stream = WaveRtStream::create(kind, pin, port_stream);
        if new_stream.is_null() {
            return STATUS_INSUFFICIENT_RESOURCES;
        }
        // The Box owns one reference, which passes to PortCls together with
        // the pointer.
        *stream = new_stream as pc::PMINIPORTWAVERTSTREAM;
        dlog!("NewStream: created");
        STATUS_SUCCESS
    }
}


unsafe extern "C" fn wave_get_device_description(
    _this: *mut pc::INTERFACE,
    device_description: *mut pc::DEVICE_DESCRIPTION,
) -> i32 {
    // SAFETY: `device_description` is a valid out pointer per the PortCls
    // contract.
    unsafe {
        if device_description.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        let desc = &mut *device_description;
        // Same shape as sysvad: virtual "master" DMA with 32-bit addresses.
        desc.Version = 2; // DEVICE_DESCRIPTION_VERSION2
        desc.Master = 1;
        desc.ScatterGather = 1;
        desc.Dma32BitAddresses = 1;
        desc.InterfaceType = pc::_INTERFACE_TYPE_PCIBus;
        desc.MaximumLength = u32::MAX;
        STATUS_SUCCESS
    }
}

/// The `IMiniportWaveRT` vtable. Field order matches portcls.h exactly.
static WAVE_VTBL: pc::IMiniportWaveRTVtbl = pc::IMiniportWaveRTVtbl {
    QueryInterface: Some(wave_query_interface),
    AddRef: Some(wave_add_ref),
    Release: Some(wave_release),
    GetDescription: Some(wave_get_description),
    DataRangeIntersection: Some(wave_data_range_intersection),
    Init: Some(wave_init),
    NewStream: Some(wave_new_stream),
    GetDeviceDescription: Some(wave_get_device_description),
};

// --- IMiniportTopology ----------------------------------------------------------

unsafe extern "C" fn topo_query_interface(
    this: *mut pc::INTERFACE,
    interface_id: *const pc::IID,
    interface: *mut pc::PVOID,
) -> i32 {
    // SAFETY: forwards to the shared helper; the topology miniport answers
    // IUnknown, IMiniport and IMiniportTopology.
    unsafe { query_interface(this, interface_id, interface, &tables::IID_IMINIPORT_TOPOLOGY) }
}

unsafe extern "C" fn topo_add_ref(this: *mut pc::INTERFACE) -> u32 {
    // SAFETY: `this` is a live Miniport.
    unsafe { add_ref(this) }
}

unsafe extern "C" fn topo_release(this: *mut pc::INTERFACE) -> u32 {
    // SAFETY: `this` is a live Miniport owned by the caller's reference.
    unsafe { release(this) }
}

unsafe extern "C" fn topo_get_description(
    this: *mut pc::INTERFACE,
    description: *mut pc::PPCFILTER_DESCRIPTOR,
) -> i32 {
    // SAFETY: forwards to the shared helper.
    unsafe { get_description(this, description) }
}

unsafe extern "C" fn topo_data_range_intersection(
    this: *mut pc::INTERFACE,
    pin_id: u32,
    client_data_range: pc::PKSDATARANGE,
    my_data_range: pc::PKSDATARANGE,
    output_buffer_length: u32,
    resultant_format: pc::PVOID,
    resultant_format_length: *mut u32,
) -> i32 {
    // SAFETY: forwards to the shared helper.
    unsafe {
        data_range_intersection(
            this,
            pin_id,
            client_data_range,
            my_data_range,
            output_buffer_length,
            resultant_format,
            resultant_format_length,
        )
    }
}

unsafe extern "C" fn topo_init(
    this: *mut pc::INTERFACE,
    _unknown_adapter: pc::PUNKNOWN,
    _resource_list: pc::PRESOURCELIST,
    port: pc::PPORTTOPOLOGY,
) -> i32 {
    // SAFETY: `this` is a live Miniport.
    unsafe {
        let miniport = &mut *(this as *mut Miniport);
        miniport.port = port as pc::PPORT;
        STATUS_SUCCESS
    }
}

/// The `IMiniportTopology` vtable (IUnknown + GetDescription +
/// DataRangeIntersection + Init).
static TOPO_VTBL: pc::IMiniportTopologyVtbl = pc::IMiniportTopologyVtbl {
    QueryInterface: Some(topo_query_interface),
    AddRef: Some(topo_add_ref),
    Release: Some(topo_release),
    GetDescription: Some(topo_get_description),
    DataRangeIntersection: Some(topo_data_range_intersection),
    Init: Some(topo_init),
};
