// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Session-scoped Windows shell visual-effects reduction (§18.54),
//! RDP-style: while a session runs, shell animations and effects that only
//! cost bandwidth are switched off — unconditionally, this is protocol
//! behaviour, not a policy. The desktop wallpaper is blanked too when the
//! session opts in (the only configurable piece: it is visually invasive).
//! The user's previous values are snapshot first and restored when the
//! session ends (the guard's `Drop`).
//!
//! Nothing is persisted (`SPIF_UPDATEINIFILE` is never passed), so a reboot
//! also restores everything even if the agent dies before its `Drop` runs.
//! Font smoothing is intentionally left alone: text readability is not an
//! effect we trade for bandwidth.

#[cfg(not(all(windows, feature = "windows-capture")))]
pub use stub::VisualEffectsGuard;
#[cfg(all(windows, feature = "windows-capture"))]
pub use win::VisualEffectsGuard;

#[cfg(not(all(windows, feature = "windows-capture")))]
mod stub {
    /// No-op guard off Windows.
    pub struct VisualEffectsGuard;

    impl VisualEffectsGuard {
        /// Applies the reduction (no-op). `wallpaper_off` blanks the
        /// desktop wallpaper too.
        #[must_use]
        pub fn apply(_wallpaper_off: bool) -> Self {
            Self
        }
    }
}

#[cfg(all(windows, feature = "windows-capture"))]
mod win {
    use std::ffi::c_void;

    use tracing::{info, warn};
    use windows::Win32::UI::WindowsAndMessaging::{
        ANIMATIONINFO, SPI_GETCOMBOBOXANIMATION, SPI_GETCURSORSHADOW, SPI_GETDESKWALLPAPER,
        SPI_GETDRAGFULLWINDOWS, SPI_GETLISTBOXSMOOTHSCROLLING, SPI_GETMENUANIMATION, SPI_GETMENUFADE,
        SPI_GETSELECTIONFADE, SPI_GETTOOLTIPANIMATION, SPI_GETTOOLTIPFADE, SPI_GETANIMATION,
        SPI_SETANIMATION,
        SPI_SETCOMBOBOXANIMATION, SPI_SETCURSORSHADOW, SPI_SETDESKWALLPAPER, SPI_SETDRAGFULLWINDOWS,
        SPI_SETLISTBOXSMOOTHSCROLLING, SPI_SETMENUANIMATION, SPI_SETMENUFADE, SPI_SETSELECTIONFADE,
        SPI_SETTOOLTIPANIMATION, SPI_SETTOOLTIPFADE, SPIF_SENDCHANGE, SystemParametersInfoW,
    };
    use windows::core::BOOL;

    /// One boolean shell setting: its GET and SET actions.
    struct BoolSetting {
        get: windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_ACTION,
        set: windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_ACTION,
        name: &'static str,
    }

    /// The effects we reduce while a session runs. Font smoothing is NOT
    /// here on purpose.
    const BOOL_SETTINGS: &[BoolSetting] = &[
        BoolSetting { get: SPI_GETMENUANIMATION, set: SPI_SETMENUANIMATION, name: "menu animation" },
        BoolSetting { get: SPI_GETCOMBOBOXANIMATION, set: SPI_SETCOMBOBOXANIMATION, name: "combobox animation" },
        BoolSetting {
            get: SPI_GETLISTBOXSMOOTHSCROLLING,
            set: SPI_SETLISTBOXSMOOTHSCROLLING,
            name: "listbox smooth scrolling",
        },
        BoolSetting { get: SPI_GETMENUFADE, set: SPI_SETMENUFADE, name: "menu fade" },
        BoolSetting { get: SPI_GETSELECTIONFADE, set: SPI_SETSELECTIONFADE, name: "selection fade" },
        BoolSetting { get: SPI_GETTOOLTIPANIMATION, set: SPI_SETTOOLTIPANIMATION, name: "tooltip animation" },
        BoolSetting { get: SPI_GETTOOLTIPFADE, set: SPI_SETTOOLTIPFADE, name: "tooltip fade" },
        BoolSetting { get: SPI_GETCURSORSHADOW, set: SPI_SETCURSORSHADOW, name: "cursor shadow" },
        BoolSetting { get: SPI_GETDRAGFULLWINDOWS, set: SPI_SETDRAGFULLWINDOWS, name: "drag full windows" },
    ];

    fn get_bool(setting: &BoolSetting) -> Option<bool> {
        let mut value = BOOL(0);
        // SAFETY: `value` is a valid BOOL out-pointer for these GET actions.
        unsafe {
            SystemParametersInfoW(setting.get, 0, Some(&raw mut value as *mut c_void), SPIF_SENDCHANGE).ok()?;
        }
        Some(value.as_bool())
    }

    fn set_bool(setting: &BoolSetting, on: bool) {
        // For these SET actions the BOOL travels as the pvParam VALUE
        // (not a pointer); uiParam stays 0. SPI_SETDRAGFULLWINDOWS is the
        // documented exception: its value goes in uiParam.
        let (ui_param, pv_param) = if setting.set == SPI_SETDRAGFULLWINDOWS {
            (u32::from(on), None)
        } else {
            (0, Some(usize::from(on) as *mut c_void))
        };
        // SAFETY: argument layout matches each action's documented contract.
        if let Err(err) = unsafe { SystemParametersInfoW(setting.set, ui_param, pv_param, SPIF_SENDCHANGE) } {
            warn!(setting = setting.name, error = %format!("{err:#}"), "visual-effects setting failed");
        }
    }

    fn get_animation() -> Option<ANIMATIONINFO> {
        let mut info = ANIMATIONINFO {
            cbSize: u32::try_from(size_of::<ANIMATIONINFO>()).ok()?,
            iMinAnimate: 0,
        };
        // SAFETY: `info` is a valid ANIMATIONINFO out-pointer.
        unsafe {
            SystemParametersInfoW(SPI_GETANIMATION, info.cbSize, Some(&raw mut info as *mut c_void), SPIF_SENDCHANGE)
                .ok()?;
        }
        Some(info)
    }

    fn set_animation(on: bool) {
        let info = ANIMATIONINFO {
            cbSize: u32::try_from(size_of::<ANIMATIONINFO>()).unwrap_or_default(),
            iMinAnimate: i32::from(on),
        };
        // SAFETY: `info` is a valid ANIMATIONINFO in-pointer.
        if let Err(err) = unsafe {
            SystemParametersInfoW(
                SPI_SETANIMATION,
                info.cbSize,
                Some(&raw const info as *mut c_void),
                SPIF_SENDCHANGE,
            )
        } {
            warn!(error = %format!("{err:#}"), "window animation setting failed");
        }
    }

    /// Session visual-effects guard: snapshot on `apply`, restore on `Drop`.
    pub struct VisualEffectsGuard {
        /// Previous values of the boolean settings we turned off, in
        /// `BOOL_SETTINGS` order (`None` = the GET failed; nothing restored).
        saved: Vec<Option<bool>>,
        /// Previous window-animation setting.
        saved_animation: Option<ANIMATIONINFO>,
        /// Previous wallpaper path (`None` = untouched or GET failed).
        saved_wallpaper: Option<String>,
    }

    impl VisualEffectsGuard {
        /// Snapshots the current shell settings, reduces the animations
        /// (always), and blanks the wallpaper when `wallpaper_off` is set.
        /// Individual setting failures are logged and skipped (the session
        /// must not fail over cosmetics).
        #[must_use]
        pub fn apply(wallpaper_off: bool) -> Self {
            let mut guard = Self {
                saved: Vec::new(),
                saved_animation: None,
                saved_wallpaper: None,
            };
            info!("reducing shell visual effects for the session");
            for setting in BOOL_SETTINGS {
                let previous = get_bool(setting);
                info!(setting = setting.name, ?previous, "sessionfx get");
                if previous == Some(true) {
                    set_bool(setting, false);
                }
                guard.saved.push(previous);
            }
            guard.saved_animation = get_animation();
            if guard.saved_animation.is_some_and(|info| info.iMinAnimate != 0) {
                set_animation(false);
            }
            if wallpaper_off {
                let mut buffer = [0u16; 260];
                // SAFETY: `buffer` is a valid writable wide-char buffer of
                // the passed length.
                let got = unsafe {
                    SystemParametersInfoW(
                        SPI_GETDESKWALLPAPER,
                        u32::try_from(buffer.len()).unwrap_or_default(),
                        Some(buffer.as_mut_ptr().cast::<c_void>()),
                        SPIF_SENDCHANGE,
                    )
                };
                match got {
                    Ok(()) => {
                        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
                        guard.saved_wallpaper = Some(String::from_utf16_lossy(&buffer[..end]));
                        let empty: [u16; 1] = [0];
                        // SAFETY: `empty` is a valid NUL-terminated string.
                        if let Err(err) = unsafe {
                            SystemParametersInfoW(
                                SPI_SETDESKWALLPAPER,
                                0,
                                Some(empty.as_ptr() as *mut c_void),
                                SPIF_SENDCHANGE,
                            )
                        } {
                            warn!(error = %format!("{err:#}"), "wallpaper blanking failed");
                        }
                    }
                    Err(err) => {
                        warn!(error = %format!("{err:#}"), "wallpaper query failed; leaving it alone");
                    }
                }
            }
            guard
        }
    }

    impl Drop for VisualEffectsGuard {
        fn drop(&mut self) {
            for (setting, previous) in BOOL_SETTINGS.iter().zip(self.saved.drain(..)) {
                if let Some(on) = previous {
                    set_bool(setting, on);
                }
            }
            if let Some(info) = self.saved_animation.take() {
                set_animation(info.iMinAnimate != 0);
            }
            if let Some(wallpaper) = self.saved_wallpaper.take() {
                let wide: Vec<u16> = wallpaper.encode_utf16().chain(std::iter::once(0)).collect();
                // SAFETY: `wide` is a valid NUL-terminated string.
                if let Err(err) = unsafe {
                    SystemParametersInfoW(SPI_SETDESKWALLPAPER, 0, Some(wide.as_ptr() as *mut c_void), SPIF_SENDCHANGE)
                } {
                    warn!(error = %format!("{err:#}"), "wallpaper restore failed");
                }
            }
            debug!("shell visual effects restored");
        }
    }
}
