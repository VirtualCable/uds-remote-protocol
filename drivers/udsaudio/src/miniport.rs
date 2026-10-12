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
    STATUS_INVALID_DEVICE_REQUEST, STATUS_INVALID_PARAMETER, STATUS_NOINTERFACE, STATUS_NO_MATCH,
    STATUS_NOT_IMPLEMENTED, STATUS_NOT_SUPPORTED, STATUS_SUCCESS, guid_eq,
};
use crate::dbg::dlog;
use crate::logic::{self, FilterKind};
use crate::portcls as pc;
use crate::stream::WaveRtStream;
use crate::tables;

/// Miniport object shared by both interfaces (wave + topology). The first
/// field is always the vtable pointer of the interface PortCls asked for.
/// Wave miniports also expose `IMiniportAudioSignalProcessing` through a
/// secondary vtable pointer (`signal_vtbl`): its interface pointer is the
/// address of that field, like a C++ secondary base subobject.
#[repr(C)]
pub struct Miniport {
    vtbl: *const VtblHeader,
    /// `IMiniportAudioSignalProcessing` vtable (wave miniports only; null
    /// for topology miniports, which do not support the interface).
    signal_vtbl: *const VtblHeader,
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

/// Offset of the secondary vtable pointer: the interface pointer handed out
/// for `IMiniportAudioSignalProcessing`.
const SIGNAL_VTBL_OFFSET: usize = core::mem::offset_of!(Miniport, signal_vtbl);

impl Miniport {
    /// Creates a boxed wave miniport with one reference (the caller's).
    pub fn create_wave(kind: FilterKind) -> *mut Self {
        Self::create(
            kind,
            &raw const WAVE_VTBL as *const VtblHeader,
            &raw const SIGNAL_VTBL as *const VtblHeader,
        )
    }

    /// Creates a boxed topology miniport with one reference.
    pub fn create_topology(kind: FilterKind) -> *mut Self {
        Self::create(kind, &raw const TOPO_VTBL as *const VtblHeader, ptr::null())
    }

    fn create(
        kind: FilterKind,
        vtbl: *const VtblHeader,
        signal_vtbl: *const VtblHeader,
    ) -> *mut Self {
        let miniport = Box::new(Self {
            vtbl,
            signal_vtbl,
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
    signal_processing: bool,
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
        } else if signal_processing
            && guid_eq(iid, &tables::IID_IMINIPORT_AUDIO_SIGNAL_PROCESSING)
        {
            // Secondary interface: the pointer is the vtable field address.
            *interface = (this as *mut u8).add(SIGNAL_VTBL_OFFSET).cast();
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
/// ourselves. When the client's range carries an attribute list (the
/// KSDATARANGE_ATTRIBUTES flag), the list is echoed after the format, as
/// attributed requests expect attributed answers.
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
        let client = &(*client_data_range).__bindgen_anon_1;
        // Packed: flags<<40 | fmtsize<<16 | kind<<8 | pin.
        dlog!(
            "DRI: 0x%I64x",
            (u64::from(client.Flags) << 40)
                | (u64::from(client.FormatSize) << 16)
                | ((kind as u64) << 8)
                | u64::from(pin_id & 0xff)
        );
        let Some(format) = kind.stream_format(pin_id) else {
            return STATUS_INVALID_PARAMETER;
        };
        if !guid_eq(&client.Specifier, &tables::KSDATAFORMAT_SPECIFIER_WAVEFORMATEX) {
            dlog!("DRI: specifier mismatch");
            return STATUS_NOT_IMPLEMENTED;
        }
        // KSDATARANGE_ATTRIBUTES: an attribute list (KSMULTIPLE_ITEM + the
        // mode attribute) follows the client's range in memory. When the
        // client sends one (PROPOSEDATAFORMAT2), the resultant format must
        // echo it or the audio engine discards the negotiated format.
        let mut attr_len = 0usize;
        let mut attr_src = ptr::null::<u8>();
        if client.Flags & tables::KSDATARANGE_ATTRIBUTES != 0 {
            // Sanity: the list header must fit and claim a bounded size.
            let list = (client_data_range as *const u8).add(client.FormatSize as usize);
            let list_len = (list as *const u32).read_unaligned() as usize;
            if (8..=4096).contains(&list_len) {
                attr_len = list_len;
                attr_src = list;
            }
        }
        let format_len = size_of::<pc::KSDATAFORMAT_WAVEFORMATEX>() as u32;
        let required = format_len + attr_len as u32;
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
        ptr::addr_of_mut!((*df).FormatSize).write_unaligned(format_len);
        ptr::addr_of_mut!((*df).Flags)
            .write_unaligned(if attr_len > 0 { tables::KSDATARANGE_ATTRIBUTES } else { 0 });
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
        // Echo the client's attribute list right after the format.
        if attr_len > 0 {
            ptr::copy_nonoverlapping(
                attr_src,
                (resultant_format as *mut u8).add(format_len as usize),
                attr_len,
            );
        }
        *resultant_format_length = required;
        STATUS_SUCCESS
    }
}

// --- filter property handlers (default device format) -----------------------------

/// `KSPROPERTY_PIN_PROPOSEDATAFORMAT` handler (wave filter automation).
/// Answers with the pin's single default PCM format as a
/// KSDATAFORMAT_WAVEFORMATEXTENSIBLE blob. The audio endpoint builder stores
/// the answer as the endpoint's device format (`GetMixFormat`).
///
/// SAFETY: called by PortCls with a valid property request.
pub unsafe extern "C" fn propose_format(request: pc::PPCPROPERTY_REQUEST) -> i32 {
    // SAFETY: forwards to the shared implementation.
    unsafe { propose_format_impl(request, false) }
}

/// `KSPROPERTY_PIN_PROPOSEDATAFORMAT2` handler (wave filter automation).
/// Same as [`propose_format`], but the request carries a
/// signal-processing-mode attribute list (a KSMULTIPLE_ITEM after the
/// KSP_PIN tail) which is validated and echoed after the output format, per
/// sysvad's `PropertyHandlerProposedFormat2`.
///
/// SAFETY: called by PortCls with a valid property request.
pub unsafe extern "C" fn propose_format2(request: pc::PPCPROPERTY_REQUEST) -> i32 {
    // SAFETY: forwards to the shared implementation.
    unsafe { propose_format_impl(request, true) }
}

/// Shared implementation of the two proposed-format property handlers,
/// mirroring sysvad's `PropertyHandlerProposedFormat[2]` semantics for a
/// driver with exactly one format per streaming pin and only the DEFAULT
/// signal processing mode.
unsafe fn propose_format_impl(request: pc::PPCPROPERTY_REQUEST, with_attributes: bool) -> i32 {
    // SAFETY: all pointers are validated before use; the request and its
    // buffers are owned by PortCls for the duration of the call.
    unsafe {
        if request.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        let req = &mut *request;
        // Packed: v2<<56 | verb<<48 | instance_size<<24 | value_size.
        dlog!(
            "propose_format enter: 0x%I64x",
            (u64::from(with_attributes as u8) << 56)
                | (u64::from(req.Verb) << 48)
                | (u64::from(req.InstanceSize & 0xFF_FFFF) << 24)
                | u64::from(req.ValueSize & 0xFF_FFFF)
        );
        let miniport = req.MajorTarget as *const Miniport;
        if miniport.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        let kind = (*miniport).kind;

        // The instance data holds the KSP_PIN fields past the KSPROPERTY
        // header (PinId + Reserved/Flags), then the attribute list for v2.
        let pin_tail = size_of::<pc::KSP_PIN>() - size_of::<pc::KSIDENTIFIER>();
        if (req.InstanceSize as usize) < pin_tail || req.Instance.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        let pin_id = (req.Instance as *const u32).read_unaligned();
        if pin_id as usize >= kind.desc().pins.len() {
            return STATUS_INVALID_PARAMETER;
        }
        // Only streaming pins have a default format (bridge pins are not
        // supported, as in sysvad).
        let Some(format) = kind.stream_format(pin_id) else {
            return STATUS_NOT_SUPPORTED;
        };

        // Basic support query: report KSPROPERTY_TYPE_GET (portcls does not
        // export PropertyHandler_BasicSupport; ks.lib is not linked).
        if req.Verb & logic::KSPROPERTY_TYPE_BASICSUPPORT != 0 {
            if req.ValueSize == 0 {
                req.ValueSize = size_of::<u32>() as u32;
                return STATUS_BUFFER_OVERFLOW;
            }
            if req.ValueSize < size_of::<u32>() as u32 || req.Value.is_null() {
                return STATUS_BUFFER_TOO_SMALL;
            }
            (req.Value as *mut u32).write_unaligned(logic::KSPROPERTY_TYPE_GET);
            req.ValueSize = size_of::<u32>() as u32;
            return STATUS_SUCCESS;
        }

        // v2: extract and validate the signal processing mode; only DEFAULT
        // is supported. The attribute list is echoed after the output format.
        let format_len = logic::DEFAULT_FORMAT_BLOB_SIZE;
        let mut attr_src = ptr::null::<u8>();
        let mut attr_len = 0usize;
        let mut total = format_len;
        if with_attributes {
            attr_src = (req.Instance as *const u8).add(pin_tail);
            attr_len = req.InstanceSize as usize - pin_tail;
            match extract_signal_processing_mode(attr_src, attr_len) {
                Ok(mode) if guid_eq(&mode, &tables::SIGNALPROCESSING_MODE_DEFAULT) => {}
                Ok(_) => return STATUS_NOT_SUPPORTED,
                Err(status) => return status,
            }
            total += attr_len;
        }

        if req.ValueSize == 0 {
            req.ValueSize = total as u32;
            return STATUS_BUFFER_OVERFLOW;
        }
        if (req.ValueSize as usize) < total {
            return STATUS_BUFFER_TOO_SMALL;
        }
        if req.Verb & logic::KSPROPERTY_TYPE_GET == 0 {
            return STATUS_INVALID_DEVICE_REQUEST;
        }
        if req.Value.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        // Packed: v2<<16 | kind<<8 | pin.
        dlog!(
            "propose_format: 0x%I64x",
            (u64::from(with_attributes as u8) << 16) | ((kind as u64) << 8) | u64::from(pin_id)
        );
        let blob = logic::default_format_blob(&format);
        ptr::copy_nonoverlapping(blob.as_ptr(), req.Value as *mut u8, format_len);
        if with_attributes {
            // The output format is followed by the echoed attribute list.
            ((req.Value as *mut u8).add(4) as *mut u32)
                .write_unaligned(tables::KSDATARANGE_ATTRIBUTES);
            ptr::copy_nonoverlapping(attr_src, (req.Value as *mut u8).add(format_len), attr_len);
        }
        req.ValueSize = total as u32;
        STATUS_SUCCESS
    }
}

/// Extracts the signal processing mode from a v2 request's attribute list
/// (a KSMULTIPLE_ITEM followed by QWORD-aligned KSATTRIBUTEs), mirroring
/// sysvad's `GetAttributesFromAttributeList`: unknown attributes are
/// NOT_SUPPORTED, and a list without a mode attribute means DEFAULT.
unsafe fn extract_signal_processing_mode(list: *const u8, size: usize) -> Result<pc::GUID, i32> {
    // SAFETY: `list`/`size` describe the tail of the PortCls instance
    // buffer, validated by the caller.
    unsafe {
        if list.is_null() || size < size_of::<pc::KSMULTIPLE_ITEM>() {
            return Err(STATUS_INVALID_PARAMETER);
        }
        let count = (list as *const u32).add(1).read_unaligned();
        let mut remaining = size - size_of::<pc::KSMULTIPLE_ITEM>();
        let mut attr = list.add(size_of::<pc::KSMULTIPLE_ITEM>());
        let mut mode = tables::SIGNALPROCESSING_MODE_DEFAULT;
        for _ in 0..count {
            if remaining < size_of::<pc::KSATTRIBUTE>() {
                return Err(STATUS_INVALID_PARAMETER);
            }
            let attr_size = (attr as *const u32).read_unaligned() as usize;
            let attr_guid = (attr.add(8) as *const pc::GUID).read_unaligned();
            if !guid_eq(&attr_guid, &tables::KSATTRIBUTEID_AUDIOSIGNALPROCESSING_MODE) {
                return Err(STATUS_NOT_SUPPORTED);
            }
            // The mode attribute carries the mode GUID as payload.
            let with_payload = size_of::<pc::KSATTRIBUTE>() + size_of::<pc::GUID>();
            if attr_size != with_payload || remaining < with_payload {
                return Err(STATUS_INVALID_PARAMETER);
            }
            mode = (attr.add(size_of::<pc::KSATTRIBUTE>()) as *const pc::GUID).read_unaligned();
            let stride = (attr_size + 7) & !7;
            attr = attr.add(stride);
            remaining = remaining.saturating_sub(stride);
        }
        Ok(mode)
    }
}

// --- IMiniportWaveRT ------------------------------------------------------------

unsafe extern "C" fn wave_query_interface(
    this: *mut pc::INTERFACE,
    interface_id: *const pc::IID,
    interface: *mut pc::PVOID,
) -> i32 {
    // SAFETY: forwards to the shared helper; pointers checked there. Wave
    // miniports also expose IMiniportAudioSignalProcessing.
    unsafe {
        query_interface(this, interface_id, interface, &tables::IID_IMINIPORT_WAVE_RT, true)
    }
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
            // tag << 48 | rate << 16 | channels << 8 | bits
            let tag = u64::from(ptr::read_unaligned(ptr::addr_of!((*wf_ptr).wFormatTag)));
            let detail = (tag << 48)
                | (u64::from(proposed.sample_rate) << 16)
                | (u64::from(proposed.channels) << 8)
                | u64::from(proposed.bits_per_sample);
            dlog!("NewStream: rejected fmt 0x%I64x", detail);
            dlog!(
                "NewStream: rejected pin 0x%I64x",
                ((kind as u64) << 8) | u64::from(pin)
            );
            return STATUS_INVALID_PARAMETER;
        }
        dlog!("NewStream: accepted");

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

// --- IMiniportAudioSignalProcessing (secondary interface of wave miniports) ---------
//
// PortCls queries the wave miniport for this interface when the audio
// endpoint builder asks for the pin's signal processing modes
// (KSPROPERTY_AUDIOSIGNALPROCESSING_MODES). Without it the capture endpoint
// never gets a device format. The interface pointer is the address of the
// `signal_vtbl` field; every method adjusts it back to the Miniport base.

/// Recovers the Miniport base pointer from the signal-processing interface
/// pointer (`this` points at the `signal_vtbl` field).
///
/// SAFETY: `this` must be a signal-processing interface pointer handed out
/// by `query_interface` for a live Miniport.
unsafe fn signal_base(this: *mut pc::INTERFACE) -> *mut Miniport {
    (this as *mut u8).wrapping_sub(SIGNAL_VTBL_OFFSET) as *mut Miniport
}

unsafe extern "C" fn signal_query_interface(
    this: *mut pc::INTERFACE,
    interface_id: *const pc::IID,
    interface: *mut pc::PVOID,
) -> i32 {
    // SAFETY: `this` is a signal interface pointer of a live Miniport;
    // forwards to the shared helper with the base pointer.
    unsafe {
        query_interface(
            signal_base(this) as *mut pc::INTERFACE,
            interface_id,
            interface,
            &tables::IID_IMINIPORT_WAVE_RT,
            true,
        )
    }
}

unsafe extern "C" fn signal_add_ref(this: *mut pc::INTERFACE) -> u32 {
    // SAFETY: `this` is a signal interface pointer of a live Miniport.
    unsafe { add_ref(signal_base(this) as *mut pc::INTERFACE) }
}

unsafe extern "C" fn signal_release(this: *mut pc::INTERFACE) -> u32 {
    // SAFETY: `this` is a signal interface pointer of a Miniport owned by
    // the caller's reference.
    unsafe { release(signal_base(this) as *mut pc::INTERFACE) }
}

/// `IMiniportAudioSignalProcessing::GetModes` — every streaming pin supports
/// exactly one mode, DEFAULT (sysvad semantics: invalid pin →
/// STATUS_INVALID_PARAMETER, bridge pin → STATUS_NOT_SUPPORTED).
unsafe extern "C" fn signal_get_modes(
    this: *mut pc::INTERFACE,
    pin: u32,
    modes: *mut pc::GUID,
    num_modes: *mut u32,
) -> i32 {
    // SAFETY: `this` is a signal interface pointer of a live Miniport;
    // out pointers per the PortCls contract.
    unsafe {
        if num_modes.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        let miniport = &*signal_base(this);
        if pin as usize >= miniport.kind.desc().pins.len() {
            return STATUS_INVALID_PARAMETER;
        }
        if miniport.kind.stream_format(pin).is_none() {
            return STATUS_NOT_SUPPORTED;
        }
        dlog!("GetModes: 0x%I64x", ((miniport.kind as u64) << 8) | u64::from(pin));
        if !modes.is_null() {
            if *num_modes < 1 {
                *num_modes = 1;
                return STATUS_BUFFER_TOO_SMALL;
            }
            *modes = tables::SIGNALPROCESSING_MODE_DEFAULT;
        }
        *num_modes = 1;
        STATUS_SUCCESS
    }
}

/// The `IMiniportAudioSignalProcessing` vtable (IUnknown + GetModes).
static SIGNAL_VTBL: pc::IMiniportAudioSignalProcessingVtbl = pc::IMiniportAudioSignalProcessingVtbl {
    QueryInterface: Some(signal_query_interface),
    AddRef: Some(signal_add_ref),
    Release: Some(signal_release),
    GetModes: Some(signal_get_modes),
};

// --- IMiniportTopology ----------------------------------------------------------

unsafe extern "C" fn topo_query_interface(
    this: *mut pc::INTERFACE,
    interface_id: *const pc::IID,
    interface: *mut pc::PVOID,
) -> i32 {
    // SAFETY: forwards to the shared helper; the topology miniport answers
    // IUnknown, IMiniport and IMiniportTopology.
    unsafe {
        query_interface(this, interface_id, interface, &tables::IID_IMINIPORT_TOPOLOGY, false)
    }
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
