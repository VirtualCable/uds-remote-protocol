// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Static KS descriptor tables (`PCFILTER_DESCRIPTOR` and friends) derived
//! from the pure descriptions in [`crate::logic`]. Windows-only: these
//! reference the generated PortCls bindings. PortCls only ever reads these
//! tables, so they live in read-only statics and are never freed.

use core::mem::size_of;
use core::ptr;

use crate::logic::{self, FilterKind};
use crate::portcls as pc;

/// Read-only static wrapper: the KS tables are written once at compile time
/// and only ever read by PortCls, so sharing them across threads is safe
/// even though they contain raw pointers (which are not `Sync` by default).
#[repr(transparent)]
pub struct SyncRo<T>(pub T);

// SAFETY: instances are only used for immutable statics; the pointees are
// read-only kernel tables.
unsafe impl<T> Sync for SyncRo<T> {}

/// Converts a pure [`logic::KsGuid`] into the bindgen GUID type (same
/// layout; used for statics, so it must be const).
const fn to_guid(g: &logic::KsGuid) -> pc::GUID {
    pc::GUID {
        Data1: g.data1,
        Data2: g.data2,
        Data3: g.data3,
        Data4: g.data4,
    }
}

// GUID statics referenced by the tables (portcls.h / ksmedia.h values; the
// equivalents in the bindings are `extern static`s that portcls.lib does not
// export, so we define our own).
pub static CLSID_PORT_WAVE_RT: pc::GUID = to_guid(&logic::CLSID_PORT_WAVE_RT);
pub static CLSID_PORT_TOPOLOGY: pc::GUID = to_guid(&logic::CLSID_PORT_TOPOLOGY);
pub static KSCATEGORY_AUDIO: pc::GUID = to_guid(&logic::KSCATEGORY_AUDIO);
pub static KSNODETYPE_SPEAKER: pc::GUID = to_guid(&logic::KSNODETYPE_SPEAKER);
pub static KSNODETYPE_MICROPHONE: pc::GUID = to_guid(&logic::KSNODETYPE_MICROPHONE);
pub static KSNODETYPE_AUDIO_LOOPBACK: pc::GUID = to_guid(&logic::KSNODETYPE_AUDIO_LOOPBACK);
pub static KSDATAFORMAT_TYPE_AUDIO: pc::GUID = to_guid(&logic::KSDATAFORMAT_TYPE_AUDIO);
pub static KSDATAFORMAT_SUBTYPE_PCM: pc::GUID = to_guid(&logic::KSDATAFORMAT_SUBTYPE_PCM);
pub static KSDATAFORMAT_SUBTYPE_ANALOG: pc::GUID = to_guid(&logic::KSDATAFORMAT_SUBTYPE_ANALOG);
pub static KSDATAFORMAT_SPECIFIER_WAVEFORMATEX: pc::GUID =
    to_guid(&logic::KSDATAFORMAT_SPECIFIER_WAVEFORMATEX);
pub static KSDATAFORMAT_SPECIFIER_NONE: pc::GUID = to_guid(&logic::KSDATAFORMAT_SPECIFIER_NONE);

// Interface GUIDs (portcls.h DEFINE_GUID values; not exported by
// portcls.lib, so defined locally).
/// `IID_IUnknown` ({00000000-0000-0000-C000-000000000046}).
pub static IID_IUNKNOWN: pc::GUID = to_guid(&logic::IID_IUNKNOWN);
/// `IID_IMiniport`.
pub static IID_IMINIPORT: pc::GUID = to_guid(&logic::IID_IMINIPORT);
/// `IID_IMiniportWaveRT`.
pub static IID_IMINIPORT_WAVE_RT: pc::GUID = to_guid(&logic::IID_IMINIPORT_WAVE_RT);
/// `IID_IMiniportWaveRTStream`.
pub static IID_IMINIPORT_WAVE_RT_STREAM: pc::GUID = to_guid(&logic::IID_IMINIPORT_WAVE_RT_STREAM);
/// `IID_IMiniportTopology`.
pub static IID_IMINIPORT_TOPOLOGY: pc::GUID = to_guid(&logic::IID_IMINIPORT_TOPOLOGY);
/// `IID_IMiniportAudioSignalProcessing`.
pub static IID_IMINIPORT_AUDIO_SIGNAL_PROCESSING: pc::GUID =
    to_guid(&logic::IID_IMINIPORT_AUDIO_SIGNAL_PROCESSING);

/// `KSATTRIBUTEID_AUDIOSIGNALPROCESSING_MODE` (ksmedia.h).
pub static KSATTRIBUTEID_AUDIOSIGNALPROCESSING_MODE: pc::GUID = pc::GUID {
    Data1: 0xe1f8_9eb5,
    Data2: 0x5f46,
    Data3: 0x419b,
    Data4: [0x96, 0x7b, 0xff, 0x67, 0x70, 0xb9, 0x84, 0x01],
};

/// `AUDIO_SIGNALPROCESSING_MODE_DEFAULT` (ksmedia.h).
pub static SIGNALPROCESSING_MODE_DEFAULT: pc::GUID = pc::GUID {
    Data1: 0xc18e_2f7e,
    Data2: 0x933d,
    Data3: 0x4965,
    Data4: [0xb7, 0xd1, 0x1e, 0xef, 0x22, 0x8d, 0x2a, 0xf3],
};

/// `KSPROPSETID_Pin` (ks.h) — property set of the pin properties.
pub static KSPROPSETID_PIN: pc::GUID = to_guid(&logic::KSPROPSETID_PIN);

// --- datarange attribute list (signal processing mode) ------------------------
//
// Every streaming pin advertises a KSATTRIBUTE list with the audio signal
// processing mode attribute, exactly like sysvad's simple.h: a bare
// KSATTRIBUTE (no mode payload — that side of the negotiation lives in the
// PROPOSEDATAFORMAT2 handler) and a KSATTRIBUTE_LIST holding an array of
// pointers to it. Without it the audio endpoint builder's format negotiation
// fails on capture endpoints and they never get a device format
// (IAudioClient::GetMixFormat then fails with AUDCLNT_E_UNSUPPORTED_FORMAT).

static MODE_ATTRIBUTE: SyncRo<pc::KSATTRIBUTE> = SyncRo(pc::KSATTRIBUTE {
    Size: size_of::<pc::KSATTRIBUTE>() as u32,
    Flags: 0,
    Attribute: KSATTRIBUTEID_AUDIOSIGNALPROCESSING_MODE,
});

static MODE_ATTRIBUTE_PTRS: SyncRo<[pc::PKSATTRIBUTE; 1]> =
    SyncRo([&raw const MODE_ATTRIBUTE.0 as pc::PKSATTRIBUTE]);

/// The mode attribute list referenced as the second "datarange" of every
/// streaming pin (KSDATARANGE_FLAGS_ATTRIBUTES set on the real range).
static MODE_ATTRIBUTE_LIST: SyncRo<pc::KSATTRIBUTE_LIST> = SyncRo(pc::KSATTRIBUTE_LIST {
    Count: 1,
    Attributes: MODE_ATTRIBUTE_PTRS.0.as_ptr() as *mut pc::PKSATTRIBUTE,
});

/// Maps a logic-table category GUID to its kernel-side static. Panics at
/// compile time if a table references an unmapped GUID.
const fn category_guid(g: &logic::KsGuid) -> &'static pc::GUID {
    if g.data1 == logic::KSCATEGORY_AUDIO.data1 {
        &KSCATEGORY_AUDIO
    } else if g.data1 == logic::KSNODETYPE_SPEAKER.data1 {
        &KSNODETYPE_SPEAKER
    } else if g.data1 == logic::KSNODETYPE_MICROPHONE.data1 {
        &KSNODETYPE_MICROPHONE
    } else if g.data1 == logic::KSNODETYPE_AUDIO_LOOPBACK.data1 {
        &KSNODETYPE_AUDIO_LOOPBACK
    } else {
        panic!("unmapped category GUID in logic tables")
    }
}

/// `KSDATARANGE_AUDIO` (ksmedia.h) — not generated by bindgen (it lives in
/// ksmedia.h, which we do not scan), so declared here with the exact C
/// layout: a `KSDATAFORMAT` followed by the min/max limits.
#[repr(C)]
pub struct DataRangeAudio {
    /// KS data format header (major/subtype/specifier GUIDs).
    pub data_range: pc::KSDATAFORMAT,
    /// Maximum channel count.
    pub maximum_channels: u32,
    /// Minimum bits per sample.
    pub minimum_bits_per_sample: u32,
    /// Maximum bits per sample.
    pub maximum_bits_per_sample: u32,
    /// Minimum sample rate.
    pub minimum_sample_frequency: u32,
    /// Maximum sample rate.
    pub maximum_sample_frequency: u32,
}

const fn data_format(
    major: &pc::GUID,
    sub: &pc::GUID,
    spec: &pc::GUID,
    format_size: u32,
    flags: u32,
) -> pc::KSDATAFORMAT {
    pc::KSDATAFORMAT {
        __bindgen_anon_1: pc::KSDATAFORMAT__bindgen_ty_1 {
            FormatSize: format_size,
            Flags: flags,
            SampleSize: 0,
            Reserved: 0,
            MajorFormat: *major,
            SubFormat: *sub,
            Specifier: *spec,
        },
    }
}

/// `KSDATARANGE_ATTRIBUTES` (ks.h): an attribute list follows this range in
/// the pin's datarange pointer array.
pub const KSDATARANGE_ATTRIBUTES: u32 = 0x2;

/// A streaming data range for a single fixed PCM format (min == max), with
/// the mode attribute list attached.
const fn pcm_range(format: &logic::PcmFormat) -> DataRangeAudio {
    DataRangeAudio {
        data_range: data_format(
            &KSDATAFORMAT_TYPE_AUDIO,
            &KSDATAFORMAT_SUBTYPE_PCM,
            &KSDATAFORMAT_SPECIFIER_WAVEFORMATEX,
            size_of::<DataRangeAudio>() as u32,
            KSDATARANGE_ATTRIBUTES,
        ),
        maximum_channels: format.channels as u32,
        minimum_bits_per_sample: format.bits_per_sample as u32,
        maximum_bits_per_sample: format.bits_per_sample as u32,
        minimum_sample_frequency: format.sample_rate,
        maximum_sample_frequency: format.sample_rate,
    }
}

/// The analog bridge range shared by every bridge pin.
const fn analog_range() -> pc::KSDATAFORMAT {
    data_format(
        &KSDATAFORMAT_TYPE_AUDIO,
        &KSDATAFORMAT_SUBTYPE_ANALOG,
        &KSDATAFORMAT_SPECIFIER_NONE,
        size_of::<pc::KSDATAFORMAT>() as u32,
        0,
    )
}

// --- data range tables (one per unique pin kind) --------------------------------

static DR_SPEAKER: DataRangeAudio = pcm_range(&logic::SPEAKER_FORMAT);
static DR_MIC: DataRangeAudio = pcm_range(&logic::MIC_FORMAT);
static DR_BRIDGE: pc::KSDATAFORMAT = analog_range();

static PTRS_SPEAKER: SyncRo<[pc::PKSDATARANGE; 2]> = SyncRo([
    &raw const DR_SPEAKER as pc::PKSDATARANGE,
    &raw const MODE_ATTRIBUTE_LIST.0 as pc::PKSDATARANGE,
]);
static PTRS_MIC: SyncRo<[pc::PKSDATARANGE; 2]> = SyncRo([
    &raw const DR_MIC as pc::PKSDATARANGE,
    &raw const MODE_ATTRIBUTE_LIST.0 as pc::PKSDATARANGE,
]);
static PTRS_BRIDGE: SyncRo<[pc::PKSDATARANGE; 1]> =
    SyncRo([&raw const DR_BRIDGE as pc::PKSDATARANGE]);

/// Maps a logic pin to its data-range pointer table.
const fn ranges_for(pin: &logic::PinDesc) -> &'static [pc::PKSDATARANGE] {
    match pin.range {
        logic::DataRange::AnalogBridge => &PTRS_BRIDGE.0,
        logic::DataRange::Pcm(format) => {
            if format.channels == logic::SPEAKER_FORMAT.channels {
                &PTRS_SPEAKER.0
            } else {
                &PTRS_MIC.0
            }
        }
    }
}

// --- pins -------------------------------------------------------------------------

const fn ks_pin(pin: &logic::PinDesc) -> pc::KSPIN_DESCRIPTOR {
    let ranges = ranges_for(pin);
    pc::KSPIN_DESCRIPTOR {
        InterfacesCount: 0,
        Interfaces: ptr::null(),
        MediumsCount: 0,
        Mediums: ptr::null(),
        DataRangesCount: ranges.len() as u32,
        DataRanges: ranges.as_ptr(),
        DataFlow: pin.data_flow.ks(),
        Communication: pin.communication.ks(),
        Category: category_guid(pin.category),
        Name: ptr::null(),
        __bindgen_anon_1: pc::KSPIN_DESCRIPTOR__bindgen_ty_1 { Reserved: 0 },
    }
}

const fn pc_pin(pin: &logic::PinDesc) -> pc::PCPIN_DESCRIPTOR {
    pc::PCPIN_DESCRIPTOR {
        MaxGlobalInstanceCount: pin.max_instances,
        MaxFilterInstanceCount: pin.max_instances,
        MinFilterInstanceCount: 0,
        AutomationTable: ptr::null(),
        KsPinDescriptor: ks_pin(pin),
    }
}

const fn connection(c: &logic::Connection) -> pc::KSTOPOLOGY_CONNECTION {
    pc::KSTOPOLOGY_CONNECTION {
        FromNode: c.from_node,
        FromNodePin: c.from_pin,
        ToNode: c.to_node,
        ToNodePin: c.to_pin,
    }
}

// --- render wave filter (sink + loopback + bridge) --------------------------------

static RENDER_WAVE_PINS: SyncRo<[pc::PCPIN_DESCRIPTOR; 3]> = SyncRo([
    pc_pin(&logic::RENDER_WAVE_PINS[0]),
    pc_pin(&logic::RENDER_WAVE_PINS[1]),
    pc_pin(&logic::RENDER_WAVE_PINS[2]),
]);

static RENDER_WAVE_CONNECTIONS: [pc::KSTOPOLOGY_CONNECTION; 2] = [
    connection(&logic::RENDER_WAVE_FILTER.connections[0]),
    connection(&logic::RENDER_WAVE_FILTER.connections[1]),
];

// --- capture wave filter (bridge + source) -----------------------------------------

static CAPTURE_WAVE_PINS: SyncRo<[pc::PCPIN_DESCRIPTOR; 2]> = SyncRo([
    pc_pin(&logic::CAPTURE_WAVE_PINS[0]),
    pc_pin(&logic::CAPTURE_WAVE_PINS[1]),
]);

static CAPTURE_WAVE_CONNECTIONS: [pc::KSTOPOLOGY_CONNECTION; 1] =
    [connection(&logic::CAPTURE_WAVE_CONNECTIONS[0])];

// --- topology filters -----------------------------------------------------------------

static RENDER_TOPO_PINS: SyncRo<[pc::PCPIN_DESCRIPTOR; 2]> = SyncRo([
    pc_pin(&logic::RENDER_TOPO_PINS[0]),
    pc_pin(&logic::RENDER_TOPO_PINS[1]),
]);

static CAPTURE_TOPO_PINS: SyncRo<[pc::PCPIN_DESCRIPTOR; 2]> = SyncRo([
    pc_pin(&logic::CAPTURE_TOPO_PINS[0]),
    pc_pin(&logic::CAPTURE_TOPO_PINS[1]),
]);

static TOPO_CONNECTIONS: [pc::KSTOPOLOGY_CONNECTION; 1] =
    [connection(&logic::TOPO_CONNECTIONS[0])];

// --- wave filter automation (default device format properties) ----------------
//
// The audio endpoint builder queries KSPROPERTY_PIN_PROPOSEDATAFORMAT[2] on
// the wave FILTER to learn the default device format, and stores the answer
// as PKEY_AudioEngine_DeviceFormat (what IAudioClient::GetMixFormat
// returns). PortCls routes filter properties through the filter descriptor's
// automation table; with AutomationTable = NULL the request never reaches
// the miniport and the capture endpoint ends up without a device format
// (GetMixFormat fails with AUDCLNT_E_UNSUPPORTED_FORMAT). sysvad registers
// the same two items (minwavert.cpp, PropertyHandler_WaveFilter).

static WAVE_FILTER_PROPERTIES: SyncRo<[pc::PCPROPERTY_ITEM; 2]> = SyncRo([
    pc::PCPROPERTY_ITEM {
        Set: &raw const KSPROPSETID_PIN,
        Id: pc::KSPROPERTY_PIN_KSPROPERTY_PIN_PROPOSEDATAFORMAT as u32,
        Flags: logic::KSPROPERTY_TYPE_GET | logic::KSPROPERTY_TYPE_BASICSUPPORT,
        Handler: Some(crate::miniport::propose_format),
    },
    pc::PCPROPERTY_ITEM {
        Set: &raw const KSPROPSETID_PIN,
        Id: pc::KSPROPERTY_PIN_KSPROPERTY_PIN_PROPOSEDATAFORMAT2 as u32,
        Flags: logic::KSPROPERTY_TYPE_GET | logic::KSPROPERTY_TYPE_BASICSUPPORT,
        Handler: Some(crate::miniport::propose_format2),
    },
]);

static WAVE_FILTER_AUTOMATION: SyncRo<pc::PCAUTOMATION_TABLE> = SyncRo(pc::PCAUTOMATION_TABLE {
    PropertyItemSize: size_of::<pc::PCPROPERTY_ITEM>() as u32,
    PropertyCount: WAVE_FILTER_PROPERTIES.0.len() as u32,
    Properties: WAVE_FILTER_PROPERTIES.0.as_ptr(),
    MethodItemSize: 0,
    MethodCount: 0,
    Methods: ptr::null(),
    EventItemSize: 0,
    EventCount: 0,
    Events: ptr::null(),
    Reserved: 0,
});

// --- filter descriptors -----------------------------------------------------------------

const fn filter_desc(
    pins: &'static [pc::PCPIN_DESCRIPTOR],
    connections: &'static [pc::KSTOPOLOGY_CONNECTION],
    automation: *const pc::PCAUTOMATION_TABLE,
) -> pc::PCFILTER_DESCRIPTOR {
    pc::PCFILTER_DESCRIPTOR {
        Version: 0,
        AutomationTable: automation,
        PinSize: size_of::<pc::PCPIN_DESCRIPTOR>() as u32,
        PinCount: pins.len() as u32,
        Pins: pins.as_ptr(),
        NodeSize: size_of::<pc::PCNODE_DESCRIPTOR>() as u32,
        NodeCount: 0,
        Nodes: ptr::null(),
        ConnectionCount: connections.len() as u32,
        Connections: connections.as_ptr(),
        CategoryCount: 0,
        Categories: ptr::null(),
    }
}

static RENDER_WAVE_FILTER: SyncRo<pc::PCFILTER_DESCRIPTOR> = SyncRo(filter_desc(
    &RENDER_WAVE_PINS.0,
    &RENDER_WAVE_CONNECTIONS,
    &raw const WAVE_FILTER_AUTOMATION.0,
));
static CAPTURE_WAVE_FILTER: SyncRo<pc::PCFILTER_DESCRIPTOR> = SyncRo(filter_desc(
    &CAPTURE_WAVE_PINS.0,
    &CAPTURE_WAVE_CONNECTIONS,
    &raw const WAVE_FILTER_AUTOMATION.0,
));
static RENDER_TOPO_FILTER: SyncRo<pc::PCFILTER_DESCRIPTOR> = SyncRo(filter_desc(
    &RENDER_TOPO_PINS.0,
    &TOPO_CONNECTIONS,
    ptr::null(),
));
static CAPTURE_TOPO_FILTER: SyncRo<pc::PCFILTER_DESCRIPTOR> = SyncRo(filter_desc(
    &CAPTURE_TOPO_PINS.0,
    &TOPO_CONNECTIONS,
    ptr::null(),
));

/// The static filter descriptor for a miniport kind. Returned from
/// `IMiniport::GetDescription`; PortCls reads it, never frees it.
pub fn filter_descriptor(kind: FilterKind) -> *const pc::PCFILTER_DESCRIPTOR {
    match kind {
        FilterKind::RenderWave => &raw const RENDER_WAVE_FILTER.0,
        FilterKind::CaptureWave => &raw const CAPTURE_WAVE_FILTER.0,
        FilterKind::RenderTopo => &raw const RENDER_TOPO_FILTER.0,
        FilterKind::CaptureTopo => &raw const CAPTURE_TOPO_FILTER.0,
    }
}
