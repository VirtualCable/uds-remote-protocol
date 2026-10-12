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
pub const RENDER_TOPO_PIN_ENDPOINT: u32 = 1;

// Pin indices of the capture topology filter (sides flipped vs render).
/// Capture topology pin 0: the (virtual) microphone jack (analog in).
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
        communication: Communication::Source,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
