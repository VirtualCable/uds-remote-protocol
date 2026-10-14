//! Control/verification utility for the UDS virtual display driver
//! (`udsdisp`, IddCx UMDF). Windows-only.
//!
//! Subcommands:
//!
//! - `list`: dump every connector slot of the virtual adapter (connected
//!   flag + last committed mode);
//! - `create <connector> <WxH[@HZ]>`: hot-plug a monitor on `connector`
//!   (0-3) with the given preferred mode;
//! - `set-mode <connector> <WxH[@HZ]>`: change the preferred mode of a live
//!   monitor (`IddCxMonitorUpdateModes`) and best-effort apply it to the
//!   desktop with `ChangeDisplaySettingsEx` (the latter needs an interactive
//!   session; the IOCTL works from anywhere);
//! - `remove <connector>`: unplug and destroy the monitor.

#[cfg(windows)]
mod imp;

fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        imp::run()
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("udsdisp-ctl only runs on Windows: it talks to the udsdisp UMDF driver")
    }
}
