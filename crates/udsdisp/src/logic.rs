//! Host-independent logic for the UDS Virtual Display driver: the mode lists
//! the virtual monitors expose and the video-signal math derived from them.
//!
//! Everything here is free of Windows types so it compiles and is unit-tested
//! on Linux; the Windows side (`imp`, cfg-gated) only translates these values
//! into IddCx/DISPLAYCONFIG structures.

/// A display mode: resolution plus vertical refresh rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    /// Horizontal pixels.
    pub width: u32,
    /// Vertical pixels.
    pub height: u32,
    /// Vertical refresh rate in Hz.
    pub vsync: u32,
}

impl Mode {
    /// Creates a mode.
    #[must_use]
    pub const fn new(width: u32, height: u32, vsync: u32) -> Self {
        Self {
            width,
            height,
            vsync,
        }
    }

    /// Approximate pixel clock (Hz), as used by Microsoft's IddSampleDriver:
    /// `vsync * width * height`. Blanking is not modeled — IddCx only needs
    /// the numbers to be internally consistent (the driver defines the units
    /// of its pipeline-rate accounting, and we cap nothing).
    #[must_use]
    pub const fn pixel_rate(&self) -> u64 {
        self.vsync as u64 * self.width as u64 * self.height as u64
    }

    /// Horizontal sync numerator (the rational denominator is always 1),
    /// following IddSampleDriver's `FillSignalInfo`.
    #[must_use]
    pub const fn hsync_numerator(&self) -> u32 {
        self.vsync * self.height
    }

    /// Whether the adapter can process this mode, i.e. it appears in
    /// [`TARGET_MODES`] (the OS only exposes the intersection of monitor and
    /// target modes, so a mode outside this list would be unreachable).
    #[must_use]
    pub fn is_supported(&self) -> bool {
        TARGET_MODES.contains(self)
    }
}

/// Maximum simultaneous virtual monitors the adapter advertises
/// (`IDDCX_ADAPTER_CAPS.MaxMonitorsSupported`). The product creates a single
/// monitor; the headroom exists so multi-monitor can be enabled later without
/// renegotiating adapter capabilities (see the spike plan, section 9).
pub const MAX_MONITORS: u32 = 4;

/// Default modes reported for a monitor with no EDID
/// (`EvtIddCxMonitorGetDefaultDescriptionModes`). The first entry is the
/// preferred mode the OS picks when the monitor connects.
pub const DEFAULT_MONITOR_MODES: [Mode; 3] = [
    Mode::new(1920, 1080, 60),
    Mode::new(1600, 900, 60),
    Mode::new(1024, 768, 75),
];

/// Index of the preferred mode within [`DEFAULT_MONITOR_MODES`].
pub const PREFERRED_DEFAULT_MODE: u32 = 0;

/// Target modes: what the adapter can process and scan out
/// (`EvtIddCxMonitorQueryTargetModes`). The OS exposes the intersection of
/// the monitor modes and these target modes, so every default monitor mode
/// must appear here or it would be unreachable. Modes accepted through the
/// control channel must be in this list too ([`Mode::is_supported`]).
pub const TARGET_MODES: [Mode; 7] = [
    Mode::new(3840, 2160, 60),
    Mode::new(2560, 1440, 60),
    Mode::new(1920, 1080, 60),
    Mode::new(1600, 900, 60),
    Mode::new(1280, 720, 60),
    Mode::new(1024, 768, 75),
    Mode::new(1024, 768, 60),
];

/// Packs `DISPLAYCONFIG_VIDEO_SIGNAL_INFO.AdditionalSignalInfo` into its
/// 32-bit storage: `videoStandard` in bits 0-15 (255 = unspecified) and
/// `vSyncFreqDivider` in bits 16-21. IddSampleDriver's `FillSignalInfo` uses
/// divider 0 for monitor modes and 1 for target modes (the divider scales the
/// desktop update rate below the monitor vsync; 1 = every vsync).
#[must_use]
pub const fn additional_signal_info(monitor_mode: bool) -> u32 {
    let divider = if monitor_mode { 0u32 } else { 1u32 };
    255 | (divider << 16)
}

/// Builds the monitor-mode list served for a monitor whose preferred mode is
/// `preferred`: the preferred mode first (preferred mode index is always 0),
/// then every other target mode. The full [`TARGET_MODES`] list is served
/// (not just the small defaults) because the OS caches the monitor mode list
/// at arrival — `IddCxMonitorUpdateModes` re-evaluates modes but does NOT
/// re-query `EvtIddCxMonitorGetDefaultDescriptionModes` (observed on Server
/// 2022), so a mode absent at arrival stays unreachable for
/// `ChangeDisplaySettingsEx`.
#[must_use]
pub fn modes_with_preferred(preferred: Mode) -> Vec<Mode> {
    let mut modes = vec![preferred];
    modes.extend(TARGET_MODES.iter().copied().filter(|m| *m != preferred));
    modes
}

// ---------------------------------------------------------------------------
// Control channel: the IOCTL protocol spoken between `udsdisp-ctl` (user
// mode) and the driver's `EvtIddCxDeviceIoControl`. All bodies are fixed-size
// little-endian; the driver validates lengths exactly.
// ---------------------------------------------------------------------------

/// Packs a Win32 `CTL_CODE` with `FILE_DEVICE_UNKNOWN` (0x22),
/// `METHOD_BUFFERED` and `FILE_ANY_ACCESS` — the value the `IoControlCode`
/// argument of `EvtIddCxDeviceIoControl` carries.
#[must_use]
pub const fn ctl_code(function: u16) -> u32 {
    (0x22 << 16) | ((function as u32) << 2)
}

/// IOCTL: report the state of every connector (no input; output is a
/// fixed-size [`LIST_RESPONSE_LEN`] buffer, see [`encode_list`]).
pub const IOCTL_LIST: u32 = ctl_code(0x800);
/// IOCTL: create and plug in a monitor (input: [`encode_create`] body).
pub const IOCTL_CREATE: u32 = ctl_code(0x801);
/// IOCTL: unplug and destroy a monitor (input: [`encode_connector`] body).
pub const IOCTL_REMOVE: u32 = ctl_code(0x802);
/// IOCTL: change the preferred mode of a live monitor (input: same body as
/// [`encode_create`]).
pub const IOCTL_SET_MODE: u32 = ctl_code(0x803);
/// IOCTL: report the hardware-cursor state of a connector (input:
/// [`encode_connector`] body; output: a fixed-size [`CURSOR_RESPONSE_LEN`]
/// buffer, see [`encode_cursor`]).
pub const IOCTL_GET_CURSOR: u32 = ctl_code(0x804);

/// Device interface GUID the driver registers (`WdfDeviceCreateDeviceInter-
/// face`) and the control tool enumerates (SetupAPI):
/// `{75647364-6973-7001-8000-554453445350}` ("udsd-is-p\0\x01..." — arbitrary
/// but stable). In memory-order bytes (GUID fields are little-endian).
pub const DEVICE_INTERFACE_GUID_BYTES: [u8; 16] = [
    0x64, 0x73, 0x64, 0x75, 0x73, 0x69, 0x01, 0x70, 0x80, 0x00, 0x55, 0x44, 0x53, 0x44, 0x53, 0x50,
];

/// Length of the create / set-mode request body.
pub const MODE_REQUEST_LEN: usize = 16;
/// Length of the remove request body (connector index only).
pub const CONNECTOR_REQUEST_LEN: usize = 4;

/// Per-connector state reported by [`IOCTL_LIST`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorStatus {
    /// Connector index (`IDDCX_MONITOR_INFO.ConnectorIndex`) of this slot.
    pub connector_index: u32,
    /// Whether a monitor is currently plugged into this connector.
    pub connected: bool,
    /// Last mode the OS committed on this connector; zeroed when the monitor
    /// has never committed (or when `connected` is false).
    pub mode: Mode,
}

/// Bytes per [`MonitorStatus`] entry in the list response.
const LIST_ENTRY_LEN: usize = 20;

/// Total length of the [`IOCTL_LIST`] response: a u32 entry count followed by
/// [`MAX_MONITORS`] fixed slots (unused slots report `connected: false`).
pub const LIST_RESPONSE_LEN: usize = 4 + MAX_MONITORS as usize * LIST_ENTRY_LEN;

/// Encodes a create / set-mode request body: connector, width, height, vsync.
#[must_use]
pub fn encode_mode_request(connector_index: u32, mode: Mode) -> [u8; MODE_REQUEST_LEN] {
    let mut out = [0u8; MODE_REQUEST_LEN];
    out[0..4].copy_from_slice(&connector_index.to_le_bytes());
    out[4..8].copy_from_slice(&mode.width.to_le_bytes());
    out[8..12].copy_from_slice(&mode.height.to_le_bytes());
    out[12..16].copy_from_slice(&mode.vsync.to_le_bytes());
    out
}

/// Decodes a create / set-mode request body; `None` on wrong length.
#[must_use]
pub fn decode_mode_request(buf: &[u8]) -> Option<(u32, Mode)> {
    if buf.len() != MODE_REQUEST_LEN {
        return None;
    }
    let read = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(buf[at..at + 4].try_into().ok()?))
    };
    Some((read(0)?, Mode::new(read(4)?, read(8)?, read(12)?)))
}

/// Encodes a remove request body (connector index).
#[must_use]
pub fn encode_connector(connector_index: u32) -> [u8; CONNECTOR_REQUEST_LEN] {
    connector_index.to_le_bytes()
}

/// Decodes a remove request body; `None` on wrong length.
#[must_use]
pub fn decode_connector(buf: &[u8]) -> Option<u32> {
    if buf.len() != CONNECTOR_REQUEST_LEN {
        return None;
    }
    Some(u32::from_le_bytes(buf.try_into().ok()?))
}

/// Encodes the [`IOCTL_LIST`] response: always [`MAX_MONITORS`] slots.
///
/// # Panics
/// Never panics for inputs produced by the driver (at most [`MAX_MONITORS`]
/// entries); extra entries are silently truncated.
#[must_use]
pub fn encode_list(statuses: &[MonitorStatus]) -> [u8; LIST_RESPONSE_LEN] {
    let mut out = [0u8; LIST_RESPONSE_LEN];
    let count = statuses.len().min(MAX_MONITORS as usize);
    let count32 = u32::try_from(count).unwrap_or(0);
    out[0..4].copy_from_slice(&count32.to_le_bytes());
    for (i, status) in statuses.iter().take(count).enumerate() {
        let base = 4 + i * LIST_ENTRY_LEN;
        out[base..base + 4].copy_from_slice(&status.connector_index.to_le_bytes());
        out[base + 4..base + 8].copy_from_slice(&u32::from(status.connected).to_le_bytes());
        out[base + 8..base + 12].copy_from_slice(&status.mode.width.to_le_bytes());
        out[base + 12..base + 16].copy_from_slice(&status.mode.height.to_le_bytes());
        out[base + 16..base + 20].copy_from_slice(&status.mode.vsync.to_le_bytes());
    }
    out
}

/// Decodes an [`IOCTL_LIST`] response; `None` on wrong length or a count that
/// does not match the buffer.
#[must_use]
pub fn decode_list(buf: &[u8]) -> Option<Vec<MonitorStatus>> {
    if buf.len() != LIST_RESPONSE_LEN {
        return None;
    }
    let count = u32::from_le_bytes(buf[0..4].try_into().ok()?) as usize;
    if count > MAX_MONITORS as usize {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let base = 4 + i * LIST_ENTRY_LEN;
        let entry = &buf[base..base + LIST_ENTRY_LEN];
        let read = |at: usize| -> Option<u32> {
            Some(u32::from_le_bytes(entry[at..at + 4].try_into().ok()?))
        };
        out.push(MonitorStatus {
            connector_index: read(0)?,
            connected: read(4)? != 0,
            mode: Mode::new(read(8)?, read(12)?, read(16)?),
        });
    }
    Some(out)
}

/// Hardware-cursor state of a connector, as tracked by the driver through
/// `IddCxMonitorQueryHardwareCursor` (the cursor update events the OS posts
/// once hardware cursor is enabled on the path).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CursorStatus {
    /// Whether the driver has queried the cursor at least once for this
    /// connector. When false the remaining fields are meaningless (zeroed).
    pub known: bool,
    /// Whether the cursor is currently visible on this path.
    pub visible: bool,
    /// Cursor position in desktop coordinates.
    pub x: i32,
    /// Cursor position in desktop coordinates.
    pub y: i32,
    /// Identifier of the current cursor shape (increases with every shape
    /// change; same shape reuses the id).
    pub shape_id: u32,
    /// Shape width in pixels.
    pub width: u32,
    /// Shape height in pixels.
    pub height: u32,
    /// Shape row pitch in bytes.
    pub pitch: u32,
    /// Shape type: `IDDCX_CURSOR_SHAPE_TYPE` (1 = masked color, 2 = alpha).
    pub shape_type: u32,
}

/// Length of the [`IOCTL_GET_CURSOR`] response body.
pub const CURSOR_RESPONSE_LEN: usize = 36;

/// Encodes the [`IOCTL_GET_CURSOR`] response.
#[must_use]
pub fn encode_cursor(status: &CursorStatus) -> [u8; CURSOR_RESPONSE_LEN] {
    let mut out = [0u8; CURSOR_RESPONSE_LEN];
    out[0..4].copy_from_slice(&u32::from(status.known).to_le_bytes());
    out[4..8].copy_from_slice(&u32::from(status.visible).to_le_bytes());
    out[8..12].copy_from_slice(&status.x.to_le_bytes());
    out[12..16].copy_from_slice(&status.y.to_le_bytes());
    out[16..20].copy_from_slice(&status.shape_id.to_le_bytes());
    out[20..24].copy_from_slice(&status.width.to_le_bytes());
    out[24..28].copy_from_slice(&status.height.to_le_bytes());
    out[28..32].copy_from_slice(&status.pitch.to_le_bytes());
    out[32..36].copy_from_slice(&status.shape_type.to_le_bytes());
    out
}

/// Decodes an [`IOCTL_GET_CURSOR`] response; `None` on wrong length.
#[must_use]
pub fn decode_cursor(buf: &[u8]) -> Option<CursorStatus> {
    if buf.len() != CURSOR_RESPONSE_LEN {
        return None;
    }
    let read = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(buf[at..at + 4].try_into().ok()?))
    };
    Some(CursorStatus {
        known: read(0)? != 0,
        visible: read(4)? != 0,
        x: read(8)?.cast_signed(),
        y: read(12)?.cast_signed(),
        shape_id: read(16)?,
        width: read(20)?,
        height: read(24)?,
        pitch: read(28)?,
        shape_type: read(32)?,
    })
}

/// Parses a mode in `WxH@HZ` form; the `@HZ` part is optional and defaults to
/// 60 Hz. This is the CLI syntax of `udsdisp-ctl`.
#[must_use]
pub fn parse_mode(text: &str) -> Option<Mode> {
    let (res, vsync) = match text.split_once('@') {
        Some((res, vsync)) => (res, vsync.parse().ok()?),
        None => (text, 60),
    };
    let (w, h) = res.split_once(['x', 'X'])?;
    Some(Mode::new(w.parse().ok()?, h.parse().ok()?, vsync))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_rate_matches_sample_formula() {
        assert_eq!(Mode::new(1920, 1080, 60).pixel_rate(), 124_416_000);
    }

    #[test]
    fn hsync_follows_sample_formula() {
        assert_eq!(Mode::new(1920, 1080, 60).hsync_numerator(), 64_800);
    }

    #[test]
    fn signal_info_packs_divider_like_the_sample() {
        assert_eq!(additional_signal_info(true), 255);
        assert_eq!(additional_signal_info(false), 255 + 65536);
    }

    #[test]
    fn preferred_default_mode_is_1080p60() {
        assert_eq!(
            DEFAULT_MONITOR_MODES[PREFERRED_DEFAULT_MODE as usize],
            Mode::new(1920, 1080, 60)
        );
    }

    #[test]
    fn every_default_monitor_mode_has_a_target_mode() {
        // The OS exposes the intersection of both lists; a default mode
        // missing from TARGET_MODES would be unreachable for users.
        for default in DEFAULT_MONITOR_MODES {
            assert!(
                TARGET_MODES.contains(&default),
                "{default:?} missing from target modes"
            );
        }
    }

    #[test]
    fn ctl_codes_match_ctl_code_layout() {
        // FILE_DEVICE_UNKNOWN (0x22) << 16 | function << 2, METHOD_BUFFERED,
        // FILE_ANY_ACCESS — same values as the audio driver's control codes.
        assert_eq!(IOCTL_LIST, 0x0022_2000);
        assert_eq!(IOCTL_CREATE, 0x0022_2004);
        assert_eq!(IOCTL_REMOVE, 0x0022_2008);
        assert_eq!(IOCTL_SET_MODE, 0x0022_200C);
        assert_eq!(IOCTL_GET_CURSOR, 0x0022_2010);
    }

    #[test]
    fn mode_request_roundtrips() {
        let mode = Mode::new(1280, 720, 60);
        let body = encode_mode_request(3, mode);
        assert_eq!(decode_mode_request(&body), Some((3, mode)));
        assert_eq!(decode_mode_request(&body[..8]), None);
        assert_eq!(decode_mode_request(&[]), None);
    }

    #[test]
    fn connector_request_roundtrips() {
        let body = encode_connector(2);
        assert_eq!(decode_connector(&body), Some(2));
        assert_eq!(decode_connector(&body[..3]), None);
    }

    #[test]
    fn list_response_roundtrips() {
        let statuses = vec![
            MonitorStatus {
                connector_index: 0,
                connected: true,
                mode: Mode::new(1920, 1080, 60),
            },
            MonitorStatus {
                connector_index: 1,
                connected: true,
                mode: Mode::new(1280, 720, 60),
            },
            MonitorStatus {
                connector_index: 2,
                connected: false,
                mode: Mode::new(0, 0, 0),
            },
            MonitorStatus {
                connector_index: 3,
                connected: false,
                mode: Mode::new(0, 0, 0),
            },
        ];
        let bytes = encode_list(&statuses);
        assert_eq!(bytes.len(), LIST_RESPONSE_LEN);
        assert_eq!(decode_list(&bytes), Some(statuses));
        assert_eq!(decode_list(&bytes[..LIST_RESPONSE_LEN - 1]), None);
    }

    #[test]
    fn list_response_rejects_bad_count() {
        let mut bytes = encode_list(&[]);
        bytes[0..4].copy_from_slice(&99u32.to_le_bytes());
        assert_eq!(decode_list(&bytes), None);
    }

    #[test]
    fn modes_with_preferred_puts_it_first_without_duplicates() {
        let modes = modes_with_preferred(Mode::new(2560, 1440, 60));
        assert_eq!(modes[0], Mode::new(2560, 1440, 60));
        assert_eq!(modes.len(), TARGET_MODES.len());

        // A preferred mode that is already a target mode must not duplicate.
        let modes = modes_with_preferred(TARGET_MODES[2]);
        assert_eq!(modes[0], TARGET_MODES[2]);
        assert_eq!(modes.len(), TARGET_MODES.len());
    }

    #[test]
    fn parse_mode_accepts_full_and_short_forms() {
        assert_eq!(parse_mode("1280x720@60"), Some(Mode::new(1280, 720, 60)));
        assert_eq!(parse_mode("1280x720"), Some(Mode::new(1280, 720, 60)));
        assert_eq!(parse_mode("2560X1440@75"), Some(Mode::new(2560, 1440, 75)));
        assert_eq!(parse_mode("1280x"), None);
        assert_eq!(parse_mode("1280x720@hz"), None);
        assert_eq!(parse_mode(""), None);
    }

    #[test]
    fn cursor_response_roundtrips() {
        let status = CursorStatus {
            known: true,
            visible: true,
            x: -32,
            y: 1080,
            shape_id: 7,
            width: 32,
            height: 32,
            pitch: 128,
            shape_type: 2,
        };
        let bytes = encode_cursor(&status);
        assert_eq!(bytes.len(), CURSOR_RESPONSE_LEN);
        assert_eq!(decode_cursor(&bytes), Some(status));
        assert!(!decode_cursor(&encode_cursor(&CursorStatus::default()))
            .unwrap_or_default()
            .known);
        assert_eq!(decode_cursor(&bytes[..CURSOR_RESPONSE_LEN - 1]), None);
    }

    #[test]
    fn supported_modes_cover_control_requests() {
        assert!(Mode::new(1280, 720, 60).is_supported());
        assert!(Mode::new(2560, 1440, 60).is_supported());
        assert!(!Mode::new(640, 480, 60).is_supported());
    }
}
