//! Pure, host-testable driver logic: KS GUID constants, PCM formats, pin /
//! connection tables for the four PortCls filters, format validation and the
//! simulated DMA position clock. No kernel APIs in here: this module must
//! compile and be tested on any platform (cargo test runs on Linux CI too).
//!
//! GUID values are copied from the WDK 10.0.22621 headers (portcls.h, ks.h,
//! ksmedia.h). They are ABI constants — they never change between releases.

/// PCM format the virtual microphone produces (48 kHz mono s16).
/// Matches the protocol's audio-in wire format.
pub const MIC_FORMAT: PcmFormat = PcmFormat {
    sample_rate: 48_000,
    channels: 1,
    bits_per_sample: 16,
};

/// PCM format the virtual speaker renders (48 kHz stereo s16), matching the
/// audio-out pipeline.
pub const SPEAKER_FORMAT: PcmFormat = PcmFormat {
    sample_rate: 48_000,
    channels: 2,
    bits_per_sample: 16,
};

/// A PCM format description (interleaved samples).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    /// Samples per second.
    pub sample_rate: u32,
    /// Interleaved channel count.
    pub channels: u16,
    /// Bits per sample (16 = s16).
    pub bits_per_sample: u16,
}

impl PcmFormat {
    /// Bytes per sample frame (all channels).
    #[must_use]
    pub const fn frame_bytes(&self) -> usize {
        (self.channels as usize) * (self.bits_per_sample as usize) / 8
    }

    /// Bytes per millisecond of audio. Used by the tests and by the future
    /// pacing logic (M4); the kernel path uses `bytes_per_sec`.
    #[allow(dead_code)]
    #[must_use]
    pub const fn bytes_per_ms(&self) -> usize {
        self.frame_bytes() * (self.sample_rate as usize) / 1000
    }

    /// Bytes per second of audio.
    #[must_use]
    pub const fn bytes_per_sec(&self) -> u64 {
        self.frame_bytes() as u64 * self.sample_rate as u64
    }
}

/// Kernel-streaming GUID (same layout as the WDK `GUID`/`_GUID` struct).
/// Keeping our own copy lets the tables stay host-testable; the Windows-only
/// side converts with [`KsGuid::as_bytes`]/plain transmutes.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KsGuid {
    /// First field (little-endian u32).
    pub data1: u32,
    /// Second field.
    pub data2: u16,
    /// Third field.
    pub data3: u16,
    /// Last 8 bytes, in wire order.
    pub data4: [u8; 8],
}

/// Builds a [`KsGuid`] from the parts as written in the WDK headers.
pub const fn ks_guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> KsGuid {
    KsGuid {
        data1: d1,
        data2: d2,
        data3: d3,
        data4: d4,
    }
}

// --- GUID constants (WDK 10.0.22621) ----------------------------------------

/// `CLSID_PortWaveRT` (portcls.h) — class id for `PcNewPort`.
pub const CLSID_PORT_WAVE_RT: KsGuid =
    ks_guid(0xcc9b_e57a, 0xeb9e, 0x42b4, [0x94, 0xfc, 0x0c, 0xad, 0x3d, 0xbc, 0xe7, 0xfa]);

/// `CLSID_PortTopology` (portcls.h) — class id for `PcNewPort`.
pub const CLSID_PORT_TOPOLOGY: KsGuid =
    ks_guid(0xb4c9_0a32, 0x5791, 0x11d0, [0x86, 0xf9, 0x00, 0xa0, 0xc9, 0x11, 0xb5, 0x44]);

/// `IID_IUnknown`.
pub const IID_IUNKNOWN: KsGuid =
    ks_guid(0x0000_0000, 0x0000, 0x0000, [0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46]);

/// `IID_IMiniport` (portcls.h).
pub const IID_IMINIPORT: KsGuid =
    ks_guid(0xb4c9_0a24, 0x5791, 0x11d0, [0x86, 0xf9, 0x00, 0xa0, 0xc9, 0x11, 0xb5, 0x44]);

/// `IID_IMiniportWaveRT` (portcls.h).
pub const IID_IMINIPORT_WAVE_RT: KsGuid =
    ks_guid(0x0f9f_c4d6, 0x6061, 0x4f3c, [0xb1, 0xfc, 0x07, 0x5e, 0x35, 0xf7, 0x96, 0x0a]);

/// `IID_IMiniportWaveRTStream` (portcls.h).
pub const IID_IMINIPORT_WAVE_RT_STREAM: KsGuid =
    ks_guid(0x000a_c9ab, 0xfaab, 0x4f3d, [0x94, 0x55, 0x6f, 0xf8, 0x30, 0x6a, 0x74, 0xa0]);

/// `IID_IMiniportAudioSignalProcessing` (portcls.h).
pub const IID_IMINIPORT_AUDIO_SIGNAL_PROCESSING: KsGuid =
    ks_guid(0xb532_678c, 0xbe50, 0x472d, [0x99, 0x73, 0x8a, 0x6f, 0x16, 0x59, 0x49, 0x89]);

/// `IID_IMiniportTopology` (portcls.h).
pub const IID_IMINIPORT_TOPOLOGY: KsGuid =
    ks_guid(0xb4c9_0a31, 0x5791, 0x11d0, [0x86, 0xf9, 0x00, 0xa0, 0xc9, 0x11, 0xb5, 0x44]);

/// `KSCATEGORY_AUDIO` (ksmedia.h).
pub const KSCATEGORY_AUDIO: KsGuid =
    ks_guid(0x6994_ad04, 0x93ef, 0x11d0, [0xa3, 0xcc, 0x00, 0xa0, 0xc9, 0x22, 0x31, 0x96]);

/// `KSNODETYPE_SPEAKER` (ksmedia.h).
pub const KSNODETYPE_SPEAKER: KsGuid =
    ks_guid(0xdff2_1ce1, 0xf70f, 0x11d0, [0xb9, 0x17, 0x00, 0xa0, 0xc9, 0x22, 0x31, 0x96]);

/// `KSNODETYPE_MICROPHONE` (ksmedia.h).
pub const KSNODETYPE_MICROPHONE: KsGuid =
    ks_guid(0xdff2_1be1, 0xf70f, 0x11d0, [0xb9, 0x17, 0x00, 0xa0, 0xc9, 0x22, 0x31, 0x96]);

/// `KSNODETYPE_AUDIO_LOOPBACK` (ksmedia.h) — category of the loopback pin.
pub const KSNODETYPE_AUDIO_LOOPBACK: KsGuid =
    ks_guid(0x8f42_c0b2, 0x91ce, 0x4bcf, [0x9c, 0xcd, 0x0e, 0x59, 0x90, 0x37, 0xab, 0x35]);

/// `KSDATAFORMAT_TYPE_AUDIO` (ksmedia.h).
pub const KSDATAFORMAT_TYPE_AUDIO: KsGuid =
    ks_guid(0x7364_7561, 0x0000, 0x0010, [0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71]);

/// `KSDATAFORMAT_SUBTYPE_PCM` (ksmedia.h; `DEFINE_WAVEFORMATEX_GUID(WAVE_FORMAT_PCM)`).
pub const KSDATAFORMAT_SUBTYPE_PCM: KsGuid =
    ks_guid(0x0000_0001, 0x0000, 0x0010, [0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71]);

/// `KSDATAFORMAT_SUBTYPE_ANALOG` (ksmedia.h) — used on bridge pins.
pub const KSDATAFORMAT_SUBTYPE_ANALOG: KsGuid =
    ks_guid(0x6dba_3190, 0x67bd, 0x11cf, [0xa0, 0xf7, 0x00, 0x20, 0xaf, 0xd1, 0x56, 0xe4]);

/// `KSDATAFORMAT_SPECIFIER_WAVEFORMATEX` (ksmedia.h).
pub const KSDATAFORMAT_SPECIFIER_WAVEFORMATEX: KsGuid =
    ks_guid(0x0558_9f81, 0xc356, 0x11ce, [0xbf, 0x01, 0x00, 0xaa, 0x00, 0x55, 0x59, 0x5a]);

/// `KSDATAFORMAT_SPECIFIER_NONE` (ksmedia.h) — used on bridge pins.
pub const KSDATAFORMAT_SPECIFIER_NONE: KsGuid =
    ks_guid(0x0f64_17d6, 0xc318, 0x11d0, [0xa4, 0x3f, 0x00, 0xa0, 0xc9, 0x22, 0x31, 0x96]);

/// `KSPROPSETID_Pin` (ks.h) — property set of the pin properties.
pub const KSPROPSETID_PIN: KsGuid =
    ks_guid(0x8c13_4960, 0x51ad, 0x11cf, [0x87, 0x8a, 0x94, 0xf8, 0x01, 0xc1, 0x00, 0x00]);

// --- Default device format blob ------------------------------------------------

/// `KSPROPERTY_TYPE_GET` (ks.h).
pub const KSPROPERTY_TYPE_GET: u32 = 0x1;

/// `KSPROPERTY_TYPE_BASICSUPPORT` (ks.h).
pub const KSPROPERTY_TYPE_BASICSUPPORT: u32 = 0x200;

/// `WAVE_FORMAT_EXTENSIBLE` (mmreg.h).
pub const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// `SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT` channel mask (ksmedia.h).
pub const SPEAKER_STEREO_MASK: u32 = 0x3;

/// `KSAUDIO_SPEAKER_MONO` (`SPEAKER_FRONT_CENTER`) channel mask (ksmedia.h).
pub const SPEAKER_MONO_MASK: u32 = 0x4;

/// Byte size of the default device format blob: `KSDATAFORMAT` (64) +
/// `WAVEFORMATEXTENSIBLE` (40).
pub const DEFAULT_FORMAT_BLOB_SIZE: usize = 104;

/// Builds the `KSDATAFORMAT_WAVEFORMATEXTENSIBLE` blob the driver reports as
/// the default device format of a streaming pin — the
/// `KSPROPERTY_PIN_PROPOSEDATAFORMAT[2]` answer the audio endpoint builder
/// stores as `PKEY_AudioEngine_DeviceFormat` (what `IAudioClient::GetMixFormat`
/// returns). Byte layout matches the packed C structs; GUIDs are encoded
/// field by field, little-endian.
#[must_use]
pub const fn default_format_blob(format: &PcmFormat) -> [u8; DEFAULT_FORMAT_BLOB_SIZE] {
    let mut b = [0u8; DEFAULT_FORMAT_BLOB_SIZE];
    let frame = format.frame_bytes() as u32;
    // KSDATAFORMAT header. Flags stays 0: the PROPOSEDATAFORMAT2 handler
    // sets KSDATAFORMAT_ATTRIBUTES on the output copy itself.
    put_u32(&mut b, 0, DEFAULT_FORMAT_BLOB_SIZE as u32); // FormatSize
    put_u32(&mut b, 8, frame); // SampleSize
    put_guid(&mut b, 16, &KSDATAFORMAT_TYPE_AUDIO);
    put_guid(&mut b, 32, &KSDATAFORMAT_SUBTYPE_PCM);
    put_guid(&mut b, 48, &KSDATAFORMAT_SPECIFIER_WAVEFORMATEX);
    // WAVEFORMATEXTENSIBLE.
    put_u16(&mut b, 64, WAVE_FORMAT_EXTENSIBLE);
    put_u16(&mut b, 66, format.channels);
    put_u32(&mut b, 68, format.sample_rate);
    put_u32(&mut b, 72, format.bytes_per_sec() as u32);
    put_u16(&mut b, 76, frame as u16);
    put_u16(&mut b, 78, format.bits_per_sample);
    put_u16(&mut b, 80, 22); // cbSize
    put_u16(&mut b, 82, format.bits_per_sample); // wValidBitsPerSample
    put_u32(
        &mut b,
        84,
        if format.channels == 1 {
            SPEAKER_MONO_MASK
        } else {
            SPEAKER_STEREO_MASK
        },
    );
    put_guid(&mut b, 88, &KSDATAFORMAT_SUBTYPE_PCM);
    b
}

const fn put_u16(b: &mut [u8], off: usize, v: u16) {
    let le = v.to_le_bytes();
    b[off] = le[0];
    b[off + 1] = le[1];
}

const fn put_u32(b: &mut [u8], off: usize, v: u32) {
    let le = v.to_le_bytes();
    b[off] = le[0];
    b[off + 1] = le[1];
    b[off + 2] = le[2];
    b[off + 3] = le[3];
}

const fn put_guid(b: &mut [u8], off: usize, g: &KsGuid) {
    put_u32(b, off, g.data1);
    put_u16(b, off + 4, g.data2);
    put_u16(b, off + 6, g.data3);
    let mut i = 0;
    while i < 8 {
        b[off + 8 + i] = g.data4[i];
        i += 1;
    }
}

// --- Pin / filter tables -----------------------------------------------------

/// `KSPIN_DATAFLOW` values (ks.h).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataFlow {
    /// `KSPIN_DATAFLOW_IN` (1).
    In,
    /// `KSPIN_DATAFLOW_OUT` (2).
    Out,
}

impl DataFlow {
    /// The numeric `KSPIN_DATAFLOW` value.
    #[must_use]
    pub const fn ks(self) -> i32 {
        match self {
            Self::In => 1,
            Self::Out => 2,
        }
    }
}

/// `KSPIN_COMMUNICATION` values (ks.h).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Communication {
    /// `KSPIN_COMMUNICATION_NONE` (0).
    None,
    /// `KSPIN_COMMUNICATION_SINK` (1).
    Sink,
    /// `KSPIN_COMMUNICATION_SOURCE` (2).
    Source,
}

impl Communication {
    /// The numeric `KSPIN_COMMUNICATION` value.
    #[must_use]
    pub const fn ks(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Sink => 1,
            Self::Source => 2,
        }
    }
}

/// What a pin carries: a PCM stream range or an analog bridge range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataRange {
    /// Streaming pin: a single PCM format (major=audio, subtype=PCM,
    /// specifier=WAVEFORMATEX).
    Pcm(PcmFormat),
    /// Bridge pin: major=audio, subtype=analog, specifier=none.
    AnalogBridge,
}

/// A filter pin (pure description; converted to `PCPIN_DESCRIPTOR` on
/// Windows by the `tables` module).
#[derive(Debug, Clone, Copy)]
pub struct PinDesc {
    /// KS data flow direction.
    pub data_flow: DataFlow,
    /// KS communication kind.
    pub communication: Communication,
    /// Category GUID (`&KSPIN_DESCRIPTOR.Category`).
    pub category: &'static KsGuid,
    /// What the pin carries.
    pub range: DataRange,
    /// Maximum simultaneous instances of this pin.
    pub max_instances: u32,
}

/// A node-less connection (`PCFILTER_NODE` is 0xFFFF_FFFF).
pub const PCFILTER_NODE: u32 = u32::MAX;

/// A filter connection (`PCCONNECTION_DESCRIPTOR` / `KSTOPOLOGY_CONNECTION`).
#[derive(Debug, Clone, Copy)]
pub struct Connection {
    /// Source node id, or [`PCFILTER_NODE`] for a filter pin.
    pub from_node: u32,
    /// Source pin (of the node or of the filter).
    pub from_pin: u32,
    /// Destination node id, or [`PCFILTER_NODE`] for a filter pin.
    pub to_node: u32,
    /// Destination pin (of the node or of the filter).
    pub to_pin: u32,
}

/// A complete KS filter description (no nodes — our topology is direct
/// pin-to-pin).
#[derive(Debug, Clone, Copy)]
pub struct FilterDesc {
    /// Pins of the filter.
    pub pins: &'static [PinDesc],
    /// Connections between them.
    pub connections: &'static [Connection],
}

// Pin indices of the render (speaker) wave filter.
/// Render wave pin 0: system stream sink (apps render here).
pub const RENDER_WAVE_PIN_SINK: u32 = 0;
/// Render wave pin 1: loopback source (WASAPI loopback reads here).
pub const RENDER_WAVE_PIN_LOOPBACK: u32 = 1;
/// Render wave pin 2: bridge to the topology filter.
pub const RENDER_WAVE_PIN_BRIDGE: u32 = 2;

// Pin indices of the capture (mic) wave filter.
/// Capture wave pin 0: bridge from the topology filter.
pub const CAPTURE_WAVE_PIN_BRIDGE: u32 = 0;
/// Capture wave pin 1: system stream source (apps capture here).
pub const CAPTURE_WAVE_PIN_SOURCE: u32 = 1;

// Pin indices of the render topology filter.
/// Render topology pin 0: wired towards the wave filter (analog in).
pub const RENDER_TOPO_PIN_WAVE: u32 = 0;
/// Render topology pin 1: the (virtual) speaker jack (analog out).
/// Not referenced by the kernel glue today (only the wave side is), kept
/// for table readability and tests.
#[allow(dead_code)]
pub const RENDER_TOPO_PIN_ENDPOINT: u32 = 1;

// Pin indices of the capture topology filter (sides flipped vs render).
/// Capture topology pin 0: the (virtual) microphone jack (analog in).
/// See [`RENDER_TOPO_PIN_ENDPOINT`] for why this is `allow(dead_code)`.
#[allow(dead_code)]
pub const CAPTURE_TOPO_PIN_ENDPOINT: u32 = 0;
/// Capture topology pin 1: wired towards the wave filter (analog out).
pub const CAPTURE_TOPO_PIN_WAVE: u32 = 1;

/// Maximum simultaneous render streams on the speaker sink pin.
pub const MAX_RENDER_STREAMS: u32 = 6;
/// Maximum simultaneous loopback streams.
pub const MAX_LOOPBACK_STREAMS: u32 = 2;
/// Maximum simultaneous capture streams on the mic source pin.
pub const MAX_CAPTURE_STREAMS: u32 = 2;

/// Render wave filter: sink (apps) + loopback + bridge.
pub static RENDER_WAVE_PINS: [PinDesc; 3] = [
    PinDesc {
        data_flow: DataFlow::In,
        communication: Communication::Sink,
        category: &KSCATEGORY_AUDIO,
        range: DataRange::Pcm(SPEAKER_FORMAT),
        max_instances: MAX_RENDER_STREAMS,
    },
    PinDesc {
        data_flow: DataFlow::Out,
        communication: Communication::Sink,
        category: &KSNODETYPE_AUDIO_LOOPBACK,
        range: DataRange::Pcm(SPEAKER_FORMAT),
        max_instances: MAX_LOOPBACK_STREAMS,
    },
    PinDesc {
        data_flow: DataFlow::Out,
        communication: Communication::None,
        category: &KSCATEGORY_AUDIO,
        range: DataRange::AnalogBridge,
        max_instances: 0,
    },
];

/// Render wave filter connections: sink -> loopback, sink -> bridge.
pub static RENDER_WAVE_CONNECTIONS: [Connection; 2] = [
    Connection {
        from_node: PCFILTER_NODE,
        from_pin: RENDER_WAVE_PIN_SINK,
        to_node: PCFILTER_NODE,
        to_pin: RENDER_WAVE_PIN_LOOPBACK,
    },
    Connection {
        from_node: PCFILTER_NODE,
        from_pin: RENDER_WAVE_PIN_SINK,
        to_node: PCFILTER_NODE,
        to_pin: RENDER_WAVE_PIN_BRIDGE,
    },
];

/// Capture wave filter: bridge + source (apps).
///
/// The streaming pin's communication kind is SINK — not SOURCE — matching
/// sysvad's capture pin (micinwavtable.h): all host pins are sinks from the
/// filter's point of view. With SOURCE the audio endpoint builder never
/// finishes the capture endpoint (no device format, GetMixFormat fails with
/// AUDCLNT_E_UNSUPPORTED_FORMAT).
pub static CAPTURE_WAVE_PINS: [PinDesc; 2] = [
    PinDesc {
        data_flow: DataFlow::In,
        communication: Communication::None,
        category: &KSCATEGORY_AUDIO,
        range: DataRange::AnalogBridge,
        max_instances: 0,
    },
    PinDesc {
        data_flow: DataFlow::Out,
        communication: Communication::Sink,
        category: &KSCATEGORY_AUDIO,
        range: DataRange::Pcm(MIC_FORMAT),
        max_instances: MAX_CAPTURE_STREAMS,
    },
];

/// Capture wave filter connections: bridge -> source.
pub static CAPTURE_WAVE_CONNECTIONS: [Connection; 1] = [Connection {
    from_node: PCFILTER_NODE,
    from_pin: CAPTURE_WAVE_PIN_BRIDGE,
    to_node: PCFILTER_NODE,
    to_pin: CAPTURE_WAVE_PIN_SOURCE,
}];

/// Render topology filter pins: wave side in, speaker out.
pub static RENDER_TOPO_PINS: [PinDesc; 2] = [
    PinDesc {
        data_flow: DataFlow::In,
        communication: Communication::None,
        category: &KSCATEGORY_AUDIO,
        range: DataRange::AnalogBridge,
        max_instances: 0,
    },
    PinDesc {
        data_flow: DataFlow::Out,
        communication: Communication::None,
        category: &KSNODETYPE_SPEAKER,
        range: DataRange::AnalogBridge,
        max_instances: 0,
    },
];

/// Capture topology filter pins: microphone in, wave side out.
pub static CAPTURE_TOPO_PINS: [PinDesc; 2] = [
    PinDesc {
        data_flow: DataFlow::In,
        communication: Communication::None,
        category: &KSNODETYPE_MICROPHONE,
        range: DataRange::AnalogBridge,
        max_instances: 0,
    },
    PinDesc {
        data_flow: DataFlow::Out,
        communication: Communication::None,
        category: &KSCATEGORY_AUDIO,
        range: DataRange::AnalogBridge,
        max_instances: 0,
    },
];

/// Topology filter connections (identical for both): pin 0 -> pin 1.
/// Render: wave-side in -> speaker out. Capture: mic in -> wave-side out.
pub static TOPO_CONNECTIONS: [Connection; 1] = [Connection {
    from_node: PCFILTER_NODE,
    from_pin: 0,
    to_node: PCFILTER_NODE,
    to_pin: 1,
}];

/// Render wave filter descriptor.
pub static RENDER_WAVE_FILTER: FilterDesc = FilterDesc {
    pins: &RENDER_WAVE_PINS,
    connections: &RENDER_WAVE_CONNECTIONS,
};

/// Capture wave filter descriptor.
pub static CAPTURE_WAVE_FILTER: FilterDesc = FilterDesc {
    pins: &CAPTURE_WAVE_PINS,
    connections: &CAPTURE_WAVE_CONNECTIONS,
};

/// Render topology filter descriptor.
pub static RENDER_TOPO_FILTER: FilterDesc = FilterDesc {
    pins: &RENDER_TOPO_PINS,
    connections: &TOPO_CONNECTIONS,
};

/// Capture topology filter descriptor.
pub static CAPTURE_TOPO_FILTER: FilterDesc = FilterDesc {
    pins: &CAPTURE_TOPO_PINS,
    connections: &TOPO_CONNECTIONS,
};

/// Which filter a miniport/stream instance belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterKind {
    /// Speaker wave filter (render + loopback pins).
    RenderWave,
    /// Mic wave filter (capture pin).
    CaptureWave,
    /// Speaker topology filter.
    RenderTopo,
    /// Mic topology filter.
    CaptureTopo,
}

impl FilterKind {
    /// The filter description for this kind.
    #[must_use]
    pub fn desc(&self) -> &'static FilterDesc {
        match self {
            Self::RenderWave => &RENDER_WAVE_FILTER,
            Self::CaptureWave => &CAPTURE_WAVE_FILTER,
            Self::RenderTopo => &RENDER_TOPO_FILTER,
            Self::CaptureTopo => &CAPTURE_TOPO_FILTER,
        }
    }

    /// The PCM format a streaming pin of this filter carries, if any.
    /// Loopback streams share the render format.
    #[must_use]
    pub fn stream_format(&self, pin: u32) -> Option<PcmFormat> {
        let pin_desc = self.desc().pins.get(pin as usize)?;
        match pin_desc.range {
            DataRange::Pcm(fmt) => Some(fmt),
            DataRange::AnalogBridge => None,
        }
    }
}

/// Checks whether a proposed WAVEFORMATEX-style PCM format is supported by a
/// filter's streaming pin. We accept exactly one format per endpoint
/// (48 kHz s16; stereo for speaker, mono for mic).
#[must_use]
pub fn is_format_supported(kind: FilterKind, pin: u32, format: PcmFormat) -> bool {
    kind.stream_format(pin) == Some(format)
}

// --- Simulated DMA position clock ---------------------------------------------

/// Simulates a DMA engine advancing through the audio buffer at the PCM
/// byte rate while the stream is running. WaveRT render/capture clients
/// poll `GetPosition`; the position must advance monotonically in real time
/// or the audio stack stalls.
#[derive(Debug, Clone, Copy)]
pub struct PositionClock {
    bytes_per_sec: u64,
    /// Byte position at `start_qpc` (sum of whole elapsed runs).
    base_byte: u64,
    /// QPC timestamp of the last transition to RUN (0 when stopped).
    start_qpc: u64,
    /// QPC frequency captured at start (Hz; 0 when stopped).
    qpc_freq: u64,
}

impl PositionClock {
    /// A stopped clock at position 0.
    #[must_use]
    pub const fn new(format: PcmFormat) -> Self {
        Self {
            bytes_per_sec: format.bytes_per_sec(),
            base_byte: 0,
            start_qpc: 0,
            qpc_freq: 0,
        }
    }

    /// Whether the simulated DMA is currently running.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.qpc_freq != 0
    }

    /// Starts advancing from `now` (QPC ticks, `freq` Hz). No-op if already
    /// running.
    pub fn start(&mut self, now: u64, freq: u64) {
        if !self.is_running() && freq != 0 {
            self.start_qpc = now;
            self.qpc_freq = freq;
        }
    }

    /// Stops advancing at `now`, folding the elapsed bytes into the base.
    /// No-op if already stopped.
    pub fn stop(&mut self, now: u64) {
        if self.is_running() {
            self.base_byte = self.position_at(now);
            self.qpc_freq = 0;
        }
    }

    /// Resets to position 0, stopped.
    pub fn reset(&mut self) {
        self.base_byte = 0;
        self.qpc_freq = 0;
    }

    /// Byte position at time `now` (QPC ticks).
    #[must_use]
    pub fn position_at(&self, now: u64) -> u64 {
        if !self.is_running() {
            return self.base_byte;
        }
        let elapsed = now.saturating_sub(self.start_qpc);
        // bytes = elapsed_ticks * bytes_per_sec / freq. Keep precision by
        // dividing last; u64 overflow is impossible for realistic runtimes
        // (elapsed * 192_000 overflows only after ~3 million years).
        self.base_byte + elapsed * self.bytes_per_sec / self.qpc_freq
    }
}

// --- SPSC PCM ring ------------------------------------------------------------

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

/// `CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS)`:
/// the single control-channel command, pushes PCM into the mic ring.
/// The user-mode tool recomputes this with the same formula (Win32
/// `CTL_CODE`); keep both in sync.
pub const IOCTL_PUSH_MIC: u32 = ctl_code(34, 0x800, 0, 0);

/// Win32/WDK `CTL_CODE` macro as a const fn (host-testable).
#[must_use]
pub const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

/// Capacity of the mic feed ring in bytes (~680 ms at 96 kB/s).
pub const MIC_RING_CAPACITY: usize = 64 * 1024;

/// Capacity of the render→loopback ring in bytes (~680 ms at 192 kB/s).
pub const LOOPBACK_RING_CAPACITY: usize = 128 * 1024;

/// Lock-free single-producer/single-consumer PCM byte ring.
///
/// Used in two places:
/// - mic feed: producer is the control-channel IOCTL (PASSIVE_LEVEL),
///   consumer is the capture stream's `GetPosition` (up to DISPATCH_LEVEL);
/// - render→loopback: producer is the render sink stream's `GetPosition`,
///   consumer is the loopback stream's `GetPosition` (both up to DISPATCH).
///
/// Only atomics and plain byte copies — no locks, no allocation — so every
/// method is safe at any IRQL. Positions are monotonic counters; the buffer
/// offset is `position % N`, so a wrap is just a two-chunk copy. Torn audio
/// on an overwrite race is accepted (a glitch, never UB); the counters are
/// always consistent because each side only stores its own index.
///
/// Not `Sync`-safe beyond the SPSC contract: exactly one producer thread
/// calls `push*` and exactly one consumer thread calls `pop*` at a time.
#[repr(C)]
pub struct SpscRing<const N: usize> {
    buffer: UnsafeCell<[u8; N]>,
    /// Total bytes ever pushed (write index). Owned by the producer.
    head: AtomicUsize,
    /// Total bytes ever popped (read index). Owned by the consumer, except
    /// that `push_overwrite` may advance it when discarding old data.
    tail: AtomicUsize,
}

// SAFETY: sound under the SPSC contract documented on the type: the producer
// writes `head` and reads `tail`, the consumer writes `tail` and reads
// `head`; buffer regions touched by each side are disjoint while the counter
// invariant (head - tail <= N) holds, which both push paths maintain.
unsafe impl<const N: usize> Sync for SpscRing<N> {}

impl<const N: usize> SpscRing<N> {
    /// An empty ring.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buffer: UnsafeCell::new([0; N]),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Bytes currently buffered. Used by the tests today; kept public for
    /// the future pacing/telemetry IOCTL.
    #[allow(dead_code)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.head.load(Ordering::Acquire) - self.tail.load(Ordering::Acquire)
    }

    /// Whether the ring holds no data. See [`SpscRing::len`].
    #[allow(dead_code)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Pushes as much of `data` as fits, dropping the rest (mic feed
    /// policy: better a gap than smearing old samples into the present).
    /// Returns the number of bytes accepted.
    pub fn push(&self, data: &[u8]) -> usize {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);
        let free = N - (head - tail);
        let n = data.len().min(free);
        self.write_at(head, &data[..n]);
        // Release: the buffer writes above must be visible before the new
        // head is.
        self.head.store(head + n, Ordering::Release);
        n
    }

    /// Pushes all of `data`, discarding the oldest bytes when full
    /// (loopback policy: loopback must track "what is playing now", and the
    /// render engine never stops producing). Returns `data.len()`.
    pub fn push_overwrite(&self, data: &[u8]) -> usize {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);
        let free = N - (head - tail);
        if data.len() > free {
            // Advance the read index past the bytes we are about to
            // clobber. A racing consumer may read torn audio for one
            // period; counters stay consistent.
            self.tail
                .store(tail + (data.len() - free), Ordering::Release);
        }
        self.write_at(head, data);
        self.head.store(head + data.len(), Ordering::Release);
        data.len()
    }

    /// Pops up to `dst.len()` bytes; returns how many were actually read
    /// (fewer on underrun). The remainder of `dst` is left untouched.
    pub fn pop(&self, dst: &mut [u8]) -> usize {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Acquire);
        let n = dst.len().min(head - tail);
        self.read_at(tail, &mut dst[..n]);
        // Release: the buffer reads above must complete before the new tail
        // lets the producer reuse the space.
        self.tail.store(tail + n, Ordering::Release);
        n
    }

    /// Fills `dst` completely: pops what the ring has and zero-fills the
    /// rest (silence on underrun — a capture stream must always deliver a
    /// full period).
    pub fn pop_or_silence(&self, dst: &mut [u8]) {
        let n = self.pop(dst);
        dst[n..].fill(0);
    }

    /// Discards all buffered data, keeping the counters monotonic (the read
    /// index jumps to the current write index). Safe against a concurrent
    /// producer: at worst the bytes of an in-flight push are skipped, the
    /// `head >= tail` invariant can never break.
    pub fn drain(&self) {
        let head = self.head.load(Ordering::Acquire);
        self.tail.store(head, Ordering::Release);
    }

    /// Copies `src` into the ring starting at absolute position `pos`.
    fn write_at(&self, pos: usize, src: &[u8]) {
        if src.is_empty() {
            return;
        }
        let off = pos % N;
        let first = (N - off).min(src.len());
        // SAFETY: single producer; the region [pos, pos+src.len()) mod N is
        // inside the ring's owned space by the counter invariant.
        unsafe {
            let base = self.buffer.get();
            core::ptr::copy_nonoverlapping(src.as_ptr(), (*base).as_mut_ptr().add(off), first);
            core::ptr::copy_nonoverlapping(
                src.as_ptr().add(first),
                (*base).as_mut_ptr(),
                src.len() - first,
            );
        }
    }

    /// Copies `dst.len()` bytes out of the ring starting at absolute
    /// position `pos`.
    fn read_at(&self, pos: usize, dst: &mut [u8]) {
        if dst.is_empty() {
            return;
        }
        let off = pos % N;
        let first = (N - off).min(dst.len());
        // SAFETY: single consumer; reads stay inside the buffer.
        unsafe {
            let base = self.buffer.get();
            core::ptr::copy_nonoverlapping((*base).as_ptr().add(off), dst.as_mut_ptr(), first);
            core::ptr::copy_nonoverlapping(
                (*base).as_ptr(),
                dst.as_mut_ptr().add(first),
                dst.len() - first,
            );
        }
    }
}

impl<const N: usize> Default for SpscRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// The mic feed ring (control IOCTL pushes, capture stream pops).
pub type MicRing = SpscRing<MIC_RING_CAPACITY>;

/// The render→loopback ring (render sink pushes, loopback stream pops).
pub type LoopbackRing = SpscRing<LOOPBACK_RING_CAPACITY>;

/// A statically dispatched reference to one of the driver rings, so a
/// stream can carry "its ring" without const-generic sprawl.
#[derive(Clone, Copy)]
pub enum RingRef {
    /// The mic feed ring.
    Mic(&'static MicRing),
    /// The render→loopback ring.
    Loopback(&'static LoopbackRing),
}

impl RingRef {
    /// Forwards to the ring's [`SpscRing::pop_or_silence`].
    pub fn pop_or_silence(self, dst: &mut [u8]) {
        match self {
            Self::Mic(ring) => ring.pop_or_silence(dst),
            Self::Loopback(ring) => ring.pop_or_silence(dst),
        }
    }

    /// Forwards to the ring's [`SpscRing::push_overwrite`].
    pub fn push_overwrite(self, data: &[u8]) -> usize {
        match self {
            Self::Mic(ring) => ring.push_overwrite(data),
            Self::Loopback(ring) => ring.push_overwrite(data),
        }
    }

    /// Forwards to the ring's [`SpscRing::drain`].
    pub fn drain(self) {
        match self {
            Self::Mic(ring) => ring.drain(),
            Self::Loopback(ring) => ring.drain(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::Wrapping;

    #[test]
    fn mic_format_matches_wire_format() {
        assert_eq!(MIC_FORMAT.frame_bytes(), 2);
        assert_eq!(MIC_FORMAT.bytes_per_ms(), 96); // 48k mono s16: 96 B/ms
    }

    #[test]
    fn speaker_format_is_stereo_48k() {
        assert_eq!(SPEAKER_FORMAT.frame_bytes(), 4);
        assert_eq!(SPEAKER_FORMAT.bytes_per_ms(), 192);
    }

    #[test]
    fn twenty_ms_mic_frame_is_1920_bytes() {
        // One Opus 20 ms frame at 48 kHz mono s16 = 960 samples = 1920 bytes.
        assert_eq!(MIC_FORMAT.bytes_per_ms() * 20, 1920);
    }

    #[test]
    fn guid_values_match_wdk_headers() {
        // Spot-checked against WDK 10.0.22621 portcls.h / ksmedia.h.
        assert_eq!(CLSID_PORT_WAVE_RT.data1, 0xcc9b_e57a);
        assert_eq!(KSCATEGORY_AUDIO.data1, 0x6994_ad04);
        assert_eq!(KSNODETYPE_SPEAKER.data1, 0xdff2_1ce1);
        assert_eq!(KSNODETYPE_MICROPHONE.data1, 0xdff2_1be1);
        assert_eq!(KSNODETYPE_AUDIO_LOOPBACK.data1, 0x8f42_c0b2);
        assert_eq!(KSDATAFORMAT_SUBTYPE_PCM.data1, 1);
        assert_eq!(KSDATAFORMAT_SUBTYPE_ANALOG.data1, 0x6dba_3190);
        // The 00AA00389B71 family (mmreg-derived audio format GUIDs).
        for g in [KSDATAFORMAT_TYPE_AUDIO, KSDATAFORMAT_SUBTYPE_PCM] {
            assert_eq!(&g.data4[2..], &[0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71]);
        }
        // The 00A0C9223196 family (KS node types / categories).
        for g in [KSNODETYPE_SPEAKER, KSNODETYPE_MICROPHONE, KSCATEGORY_AUDIO] {
            assert_eq!(&g.data4[2..], &[0x00, 0xa0, 0xc9, 0x22, 0x31, 0x96]);
        }
    }

    /// Every connection must reference existing filter pins, and the data
    /// flow must move forward (IN pins are sources of nothing except for
    /// bridge/loopback cases — the structural rule we enforce: the `from`
    /// pin of a connection is a pin that data leaves, i.e. a render-sink
    /// pin feeds loopback/bridge).
    #[test]
    fn connections_reference_valid_pins() {
        for filter in [
            &RENDER_WAVE_FILTER,
            &CAPTURE_WAVE_FILTER,
            &RENDER_TOPO_FILTER,
            &CAPTURE_TOPO_FILTER,
        ] {
            let pin_count = filter.pins.len() as u32;
            for conn in filter.connections {
                assert_eq!(conn.from_node, PCFILTER_NODE);
                assert_eq!(conn.to_node, PCFILTER_NODE);
                assert!(conn.from_pin < pin_count);
                assert!(conn.to_pin < pin_count);
                assert_ne!(conn.from_pin, conn.to_pin);
            }
        }
    }

    #[test]
    fn every_filter_has_exactly_one_pcm_format_per_streaming_pin() {
        for kind in [
            FilterKind::RenderWave,
            FilterKind::CaptureWave,
            FilterKind::RenderTopo,
            FilterKind::CaptureTopo,
        ] {
            for (i, pin) in kind.desc().pins.iter().enumerate() {
                let fmt = kind.stream_format(i as u32);
                match pin.range {
                    DataRange::Pcm(expected) => assert_eq!(fmt, Some(expected)),
                    DataRange::AnalogBridge => assert_eq!(fmt, None),
                }
            }
        }
    }

    #[test]
    fn speaker_and_mic_formats_are_advertised_on_the_right_pins() {
        assert_eq!(
            FilterKind::RenderWave.stream_format(RENDER_WAVE_PIN_SINK),
            Some(SPEAKER_FORMAT)
        );
        assert_eq!(
            FilterKind::RenderWave.stream_format(RENDER_WAVE_PIN_LOOPBACK),
            Some(SPEAKER_FORMAT)
        );
        assert_eq!(
            FilterKind::CaptureWave.stream_format(CAPTURE_WAVE_PIN_SOURCE),
            Some(MIC_FORMAT)
        );
        // Bridge pins have no PCM format.
        assert_eq!(
            FilterKind::RenderWave.stream_format(RENDER_WAVE_PIN_BRIDGE),
            None
        );
        assert_eq!(
            FilterKind::CaptureWave.stream_format(CAPTURE_WAVE_PIN_BRIDGE),
            None
        );
    }

    #[test]
    fn format_validation_accepts_exactly_our_formats() {
        assert!(is_format_supported(
            FilterKind::RenderWave,
            RENDER_WAVE_PIN_SINK,
            SPEAKER_FORMAT
        ));
        assert!(is_format_supported(
            FilterKind::CaptureWave,
            CAPTURE_WAVE_PIN_SOURCE,
            MIC_FORMAT
        ));
        // Wrong channel count / rate rejected.
        assert!(!is_format_supported(
            FilterKind::RenderWave,
            RENDER_WAVE_PIN_SINK,
            MIC_FORMAT
        ));
        assert!(!is_format_supported(
            FilterKind::CaptureWave,
            CAPTURE_WAVE_PIN_SOURCE,
            PcmFormat {
                sample_rate: 44_100,
                channels: 1,
                bits_per_sample: 16,
            }
        ));
    }

    #[test]
    fn bridge_pins_reject_stream_creation_formats() {
        // The port driver must not be able to open a stream on a bridge pin.
        assert!(!is_format_supported(
            FilterKind::RenderWave,
            RENDER_WAVE_PIN_BRIDGE,
            SPEAKER_FORMAT
        ));
        assert!(!is_format_supported(
            FilterKind::CaptureWave,
            CAPTURE_WAVE_PIN_BRIDGE,
            MIC_FORMAT
        ));
    }

    #[test]
    fn position_clock_advances_at_byte_rate_while_running() {
        // Speaker: 192_000 bytes/sec. Fake QPC at 10 MHz (100 ns ticks).
        let mut clock = PositionClock::new(SPEAKER_FORMAT);
        assert_eq!(clock.position_at(0), 0);
        clock.start(1_000_000, 10_000_000);
        // 10 ms later: 1920 bytes.
        assert_eq!(clock.position_at(1_100_000), 1920);
        // 1 s later: 192_000 bytes.
        assert_eq!(clock.position_at(11_000_000), 192_000);
        // Stop and check the position freezes.
        clock.stop(21_000_000); // 2 s total -> 384_000
        assert_eq!(clock.position_at(999_000_000), 384_000);
        // Restart: continues from the frozen position.
        clock.start(30_000_000, 10_000_000);
        assert_eq!(clock.position_at(30_500_000), 384_000 + 9600);
    }

    #[test]
    fn position_clock_monotonic_across_stop_start() {
        let mut clock = PositionClock::new(MIC_FORMAT); // 96_000 B/s
        clock.start(0, 10_000_000);
        let p1 = clock.position_at(50_000);
        clock.stop(50_000);
        clock.start(60_000, 10_000_000);
        let p2 = clock.position_at(60_000);
        assert_eq!(p1, p2);
        assert!(clock.position_at(70_000) > p2);
    }

    #[test]
    fn position_clock_reset_zeroes_everything() {
        let mut clock = PositionClock::new(SPEAKER_FORMAT);
        clock.start(0, 10_000_000);
        clock.stop(1_000_000);
        clock.reset();
        assert_eq!(clock.position_at(5_000_000), 0);
        assert!(!clock.is_running());
    }

    #[test]
    fn position_clock_ignores_time_going_backwards() {
        let mut clock = PositionClock::new(SPEAKER_FORMAT);
        clock.start(1_000_000, 10_000_000);
        assert_eq!(clock.position_at(500_000), 0); // saturating_sub
    }

    #[test]
    fn ioctl_push_mic_matches_ctl_code_layout() {
        // CTL_CODE(FILE_DEVICE_UNKNOWN=34, 0x800, METHOD_BUFFERED=0,
        // FILE_ANY_ACCESS=0) = 0x22_2000. The user-mode tool must compute
        // the same value with the Win32 CTL_CODE macro.
        assert_eq!(IOCTL_PUSH_MIC, 0x0022_2000);
    }

    #[test]
    fn default_format_blob_mic_layout() {
        let blob = default_format_blob(&MIC_FORMAT);
        assert_eq!(blob.len(), 104);
        // KSDATAFORMAT: FormatSize, Flags, SampleSize, Reserved.
        assert_eq!(u32::from_le_bytes(blob[0..4].try_into().unwrap()), 104);
        assert_eq!(u32::from_le_bytes(blob[4..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(blob[8..12].try_into().unwrap()), 2); // frame bytes
        assert_eq!(u32::from_le_bytes(blob[12..16].try_into().unwrap()), 0);
        // MajorFormat = KSDATAFORMAT_TYPE_AUDIO (LE field by field).
        assert_eq!(&blob[16..20], &0x7364_7561u32.to_le_bytes());
        assert_eq!(&blob[32..36], &1u32.to_le_bytes()); // SubFormat = PCM
        assert_eq!(&blob[48..52], &0x0558_9f81u32.to_le_bytes()); // Specifier
        // WAVEFORMATEXTENSIBLE.
        assert_eq!(u16::from_le_bytes(blob[64..66].try_into().unwrap()), 0xFFFE);
        assert_eq!(u16::from_le_bytes(blob[66..68].try_into().unwrap()), 1); // mono
        assert_eq!(u32::from_le_bytes(blob[68..72].try_into().unwrap()), 48_000);
        assert_eq!(u32::from_le_bytes(blob[72..76].try_into().unwrap()), 96_000);
        assert_eq!(u16::from_le_bytes(blob[76..78].try_into().unwrap()), 2); // align
        assert_eq!(u16::from_le_bytes(blob[78..80].try_into().unwrap()), 16); // bits
        assert_eq!(u16::from_le_bytes(blob[80..82].try_into().unwrap()), 22); // cbSize
        assert_eq!(u16::from_le_bytes(blob[82..84].try_into().unwrap()), 16); // valid bits
        assert_eq!(u32::from_le_bytes(blob[84..88].try_into().unwrap()), 0x4); // mono mask
        assert_eq!(&blob[88..92], &1u32.to_le_bytes()); // SubFormat GUID = PCM
    }

    #[test]
    fn default_format_blob_speaker_layout() {
        let blob = default_format_blob(&SPEAKER_FORMAT);
        assert_eq!(u32::from_le_bytes(blob[8..12].try_into().unwrap()), 4); // frame bytes
        assert_eq!(u16::from_le_bytes(blob[66..68].try_into().unwrap()), 2); // stereo
        assert_eq!(u32::from_le_bytes(blob[72..76].try_into().unwrap()), 192_000);
        assert_eq!(u32::from_le_bytes(blob[84..88].try_into().unwrap()), 0x3); // stereo mask
    }

    #[test]
    fn pin_property_set_guid_matches_ks_h() {
        // KSPROPSETID_Pin = {8C134960-51AD-11CF-878A-94F801C10000} (ks.h).
        assert_eq!(KSPROPSETID_PIN.data1, 0x8c13_4960);
        assert_eq!(KSPROPSETID_PIN.data2, 0x51ad);
        assert_eq!(KSPROPSETID_PIN.data3, 0x11cf);
        assert_eq!(
            KSPROPSETID_PIN.data4,
            [0x87, 0x8a, 0x94, 0xf8, 0x01, 0xc1, 0x00, 0x00]
        );
    }

    #[test]
    fn ring_push_pop_roundtrip() {
        let ring = SpscRing::<64>::new();
        assert!(ring.is_empty());
        let data: Vec<u8> = (0..32u8).collect();
        assert_eq!(ring.push(&data), 32);
        assert_eq!(ring.len(), 32);
        let mut out = [0u8; 32];
        assert_eq!(ring.pop(&mut out), 32);
        assert_eq!(&out, &data[..]);
        assert!(ring.is_empty());
    }

    #[test]
    fn ring_wraps_around() {
        let ring = SpscRing::<16>::new();
        let a: Vec<u8> = (0..12u8).collect();
        let b: Vec<u8> = (100..112u8).collect();
        assert_eq!(ring.push(&a), 12);
        let mut sink = [0u8; 8];
        assert_eq!(ring.pop(&mut sink), 8); // head=12, tail=8
        // This write wraps the buffer end (offset 12, 12 bytes > 16-12=4).
        assert_eq!(ring.push(&b), 12);
        assert_eq!(ring.len(), 16);
        let mut out = [0u8; 16];
        assert_eq!(ring.pop(&mut out), 16);
        assert_eq!(&out[..4], &a[8..]);
        assert_eq!(&out[4..], &b[..]);
    }

    #[test]
    fn ring_push_drops_new_data_when_full() {
        let ring = SpscRing::<8>::new();
        let data = [7u8; 16];
        assert_eq!(ring.push(&data), 8); // only what fits
        assert_eq!(ring.push(&data), 0); // full: nothing accepted
        let mut out = [0u8; 4];
        assert_eq!(ring.pop(&mut out), 4);
        assert_eq!(ring.push(&[1, 2, 3, 4, 5]), 4); // 4 free again
        let mut all = [0u8; 8];
        assert_eq!(ring.pop(&mut all), 8);
        assert_eq!(&all[..4], &[7, 7, 7, 7]);
        assert_eq!(&all[4..], &[1, 2, 3, 4]);
    }

    #[test]
    fn ring_push_overwrite_discards_oldest() {
        let ring = SpscRing::<8>::new();
        assert_eq!(ring.push(&[1, 2, 3, 4, 5, 6]), 6);
        assert_eq!(ring.push_overwrite(&[10, 11, 12, 13]), 4);
        // 6+4 = 10 total pushed into 8 slots: oldest 2 bytes (1,2) gone.
        assert_eq!(ring.len(), 8);
        let mut out = [0u8; 8];
        assert_eq!(ring.pop(&mut out), 8);
        assert_eq!(&out, &[3, 4, 5, 6, 10, 11, 12, 13]);
    }

    #[test]
    fn ring_pop_or_silence_zero_fills_underrun() {
        let ring = SpscRing::<16>::new();
        ring.push(&[1, 2, 3]);
        let mut out = [0xAAu8; 8];
        ring.pop_or_silence(&mut out);
        assert_eq!(&out[..3], &[1, 2, 3]);
        assert_eq!(&out[3..], &[0, 0, 0, 0, 0]);
        // Fully empty ring: pure silence.
        ring.pop_or_silence(&mut out);
        assert_eq!(&out, &[0u8; 8]);
    }

    #[test]
    fn ring_drain_drops_pending_data() {
        let ring = SpscRing::<16>::new();
        ring.push(&[9u8; 10]);
        ring.drain();
        assert!(ring.is_empty());
        assert_eq!(ring.len(), 0);
        // Counters stay monotonic: the ring keeps working across the drain.
        assert_eq!(ring.push(&[1, 2]), 2);
        let mut out = [0u8; 2];
        assert_eq!(ring.pop(&mut out), 2);
        assert_eq!(&out, &[1, 2]);
    }

    #[test]
    fn ring_survives_alternating_load() {
        // Long alternating push/pop session crossing many wraps; every byte
        // must come out in order.
        let ring = SpscRing::<256>::new();
        let mut counter = Wrapping(0u8);
        let mut expected = Wrapping(0u8);
        for _ in 0..1000 {
            let chunk: Vec<u8> = (0..37).map(|_| { counter += 1; counter.0 }).collect();
            assert_eq!(ring.push(&chunk), 37);
            let mut out = [0u8; 37];
            assert_eq!(ring.pop(&mut out), 37);
            for b in out {
                expected += 1;
                assert_eq!(b, expected.0);
            }
        }
        assert!(ring.is_empty());
    }
}
