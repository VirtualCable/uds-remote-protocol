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
/// must appear here or it would be unreachable.
pub const TARGET_MODES: [Mode; 6] = [
    Mode::new(3840, 2160, 60),
    Mode::new(2560, 1440, 60),
    Mode::new(1920, 1080, 60),
    Mode::new(1600, 900, 60),
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
}
