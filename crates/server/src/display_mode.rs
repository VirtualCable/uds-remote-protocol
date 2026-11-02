// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Console display-mode switching (§18.36/§18.37): the agent applies the
//! session resolution on the mirrored console display and restores the
//! original mode when the session ends.
//!
//! Policy (Server > Client):
//! - A fixed `resolution` from the server config always wins.
//! - Otherwise the client's preferred size (HELLO / `DesktopResize`) is
//!   approached with the closest supported mode **not exceeding** it.
//! - If nothing applies, the console keeps its current mode — it is never
//!   left without a valid one.
//!
//! 32 bpp only (DXGI delivers BGRA; there is no reason for 16/24).

/// Policy input for one mode application: the server-fixed mode, if any,
/// and the client's current preference (`(0, 0)` = no preference).
#[derive(Debug, Clone, Copy, Default)]
pub struct ResolutionPolicy {
    /// Server-fixed resolution (Server > Client): when set, the client's
    /// preference is ignored.
    pub fixed: Option<(u32, u32)>,
    /// Client-preferred resolution (HELLO or `DesktopResize`).
    pub preferred: (u32, u32),
}

/// Picks the target mode for `policy` from `modes` (candidate `(w, h)`
/// pairs, already 32 bpp). Returns `None` when nothing should change.
#[must_use]
pub fn pick_mode(policy: &ResolutionPolicy, modes: &[(u32, u32)]) -> Option<(u32, u32)> {
    let target = policy.fixed.or(match policy.preferred {
        (0, 0) => None,
        p => Some(p),
    })?;
    // Closest supported mode not exceeding the target: maximize area among
    // modes that fit; ties go to the larger width.
    modes
        .iter()
        .copied()
        .filter(|&(w, h)| w <= target.0 && h <= target.1)
        .max_by_key(|&(w, h)| (w * h, w))
        // Nothing fits below the preference: take the smallest available
        // mode rather than leaving the console as-is at something bigger
        // the client cannot show without scrolling.
        .or_else(|| modes.iter().copied().min_by_key(|&(w, h)| w * h))
}

/// Restores the original display mode on drop (Windows).
#[cfg(all(windows, feature = "windows-capture"))]
pub use win::DisplayModeGuard;

/// No-op elsewhere: mode switching is only implemented on Windows.
#[cfg(not(all(windows, feature = "windows-capture")))]
pub use stub::DisplayModeGuard;

#[cfg(not(all(windows, feature = "windows-capture")))]
mod stub {
    /// No-op guard: keeps the agent code portable.
    #[derive(Debug, Default)]
    pub struct DisplayModeGuard;

    impl DisplayModeGuard {
        /// Creates a no-op guard.
        #[must_use]
        pub fn new() -> Self {
            Self
        }

        /// Does nothing and reports no change.
        pub fn apply(&mut self, _policy: &super::ResolutionPolicy) -> bool {
            false
        }
    }
}

#[cfg(all(windows, feature = "windows-capture"))]
mod win {
    use super::ResolutionPolicy;
    use windows::Win32::Graphics::Gdi::{
        CDS_FULLSCREEN, ChangeDisplaySettingsExW, DEVMODE_FIELD_FLAGS, DEVMODEW, DM_BITSPERPEL,
        DM_PELSHEIGHT, DM_PELSWIDTH, DISP_CHANGE_SUCCESSFUL, ENUM_CURRENT_SETTINGS,
        ENUM_DISPLAY_SETTINGS_FLAGS, ENUM_DISPLAY_SETTINGS_MODE, EnumDisplaySettingsExW,
    };
    use windows::core::PCWSTR;

    /// Applied display mode with the original one saved for restore.
    ///
    /// `CDS_FULLSCREEN` marks the change as temporary (not persisted to the
    /// registry), so a hard agent crash leaves the console at its boot mode
    /// on the next session — the watchdog story stays simple.
    pub struct DisplayModeGuard {
        original: Option<DEVMODEW>,
        applied: Option<(u32, u32)>,
    }

    fn new_devmode() -> DEVMODEW {
        DEVMODEW {
            dmSize: u16::try_from(std::mem::size_of::<DEVMODEW>()).unwrap_or(0),
            ..Default::default()
        }
    }

    /// Enumerates the supported 32-bpp modes of the primary display as
    /// `(width, height)` pairs (deduplicated; refresh rate ignored — the
    /// console mirror does not care).
    fn supported_modes() -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        let mut index = 0u32;
        loop {
            let mut mode = new_devmode();
            // SAFETY: `mode` is a valid, correctly sized out struct; the null
            // device name selects the primary display.
            let ok = unsafe {
                EnumDisplaySettingsExW(
                    PCWSTR::null(),
                    ENUM_DISPLAY_SETTINGS_MODE(index),
                    &mut mode,
                    ENUM_DISPLAY_SETTINGS_FLAGS(0),
                )
            };
            if !ok.as_bool() {
                break;
            }
            index += 1;
            if mode.dmBitsPerPel == 32 {
                let pair = (mode.dmPelsWidth, mode.dmPelsHeight);
                if !out.contains(&pair) {
                    out.push(pair);
                }
            }
        }
        out
    }

    /// Applies `mode` to the primary display as a temporary change.
    fn set_mode(mode: &DEVMODEW) -> bool {
        // SAFETY: `mode` is a valid, correctly sized struct.
        let result = unsafe {
            ChangeDisplaySettingsExW(PCWSTR::null(), Some(&raw const *mode), None, CDS_FULLSCREEN, None)
        };
        result == DISP_CHANGE_SUCCESSFUL
    }

    impl DisplayModeGuard {
        /// Creates a guard with nothing applied yet.
        #[must_use]
        pub fn new() -> Self {
            Self { original: None, applied: None }
        }

        /// Applies the policy to the primary display. Returns true when the
        /// mode actually changed (the caller should expect a capture
        /// re-initialization via DXGI access-lost).
        pub fn apply(&mut self, policy: &ResolutionPolicy) -> bool {
            let modes = supported_modes();
            let Some((w, h)) = super::pick_mode(policy, &modes) else {
                return false;
            };
            if self.applied == Some((w, h)) {
                return false;
            }
            // Save the original mode once, before the first change.
            if self.original.is_none() {
                let mut current = new_devmode();
                // SAFETY: `current` is a valid, correctly sized out struct.
                let ok = unsafe {
                    EnumDisplaySettingsExW(
                        PCWSTR::null(),
                        ENUM_CURRENT_SETTINGS,
                        &mut current,
                        ENUM_DISPLAY_SETTINGS_FLAGS(0),
                    )
                };
                if ok.as_bool() {
                    self.original = Some(current);
                }
            }
            let mut mode = new_devmode();
            mode.dmPelsWidth = w;
            mode.dmPelsHeight = h;
            mode.dmBitsPerPel = 32;
            mode.dmFields = DEVMODE_FIELD_FLAGS(DM_PELSWIDTH.0 | DM_PELSHEIGHT.0 | DM_BITSPERPEL.0);
            if !set_mode(&mode) {
                tracing::warn!(w, h, "display mode change failed; keeping current mode");
                return false;
            }
            tracing::info!(w, h, "console display mode applied");
            self.applied = Some((w, h));
            true
        }
    }

    impl Drop for DisplayModeGuard {
        fn drop(&mut self) {
            let Some(original) = self.original.take() else {
                return;
            };
            if set_mode(&original) {
                tracing::info!("console display mode restored");
            } else {
                tracing::warn!("failed to restore the console display mode");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODES: &[(u32, u32)] = &[
        (640, 480),
        (800, 600),
        (1024, 768),
        (1280, 720),
        (1280, 1024),
        (1600, 900),
        (1680, 1050),
        (1920, 1080),
        (2560, 1440),
    ];

    #[test]
    fn fixed_wins_over_preference() {
        let policy = ResolutionPolicy {
            fixed: Some((1280, 720)),
            preferred: (2560, 1440),
        };
        assert_eq!(pick_mode(&policy, MODES), Some((1280, 720)));
    }

    #[test]
    fn preference_is_never_exceeded() {
        let policy = ResolutionPolicy {
            fixed: None,
            preferred: (1700, 1000),
        };
        // 1680x1050 exceeds height; the best fitting is 1600x900.
        assert_eq!(pick_mode(&policy, MODES), Some((1600, 900)));
    }

    #[test]
    fn exact_match_is_used() {
        let policy = ResolutionPolicy {
            fixed: None,
            preferred: (1920, 1080),
        };
        assert_eq!(pick_mode(&policy, MODES), Some((1920, 1080)));
    }

    #[test]
    fn no_preference_means_no_change() {
        let policy = ResolutionPolicy::default();
        assert_eq!(pick_mode(&policy, MODES), None);
    }

    #[test]
    fn below_everything_takes_smallest() {
        let policy = ResolutionPolicy {
            fixed: None,
            preferred: (320, 200),
        };
        assert_eq!(pick_mode(&policy, MODES), Some((640, 480)));
    }
}
