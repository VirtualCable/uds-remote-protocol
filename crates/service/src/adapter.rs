//! Enable/disable of the udsdisp display adapter device (PnP state change).
//!
//! Rationale (doc/plan/iddcx-pantalla-virtual.md §12.4): vdagent ≤ 0.10.0 —
//! the version shipped by virtio-win / spice-guest-tools — discards all mouse
//! input whenever a non-QXL display adapter is merely *present* (its
//! `consistent_displays` check rejects the mixed adapter list). The same
//! pattern affects other console agents. We therefore keep the adapter
//! **disabled by default** and enable it only for the duration of a remote
//! session; the state change is hot (no reboot) and reversible.

/// Pure predicate: does this PnP device (by instance id + hardware ids) look
/// like our virtual display adapter? The instance id of the root-enumerated
/// devnode is `ROOT\DISPLAY\NNNN`; the hardware id declared by our INF is
/// `ROOT\udsdisp`. Case-insensitive on both.
#[must_use]
#[cfg_attr(not(windows), allow(dead_code))] // used only by the Windows imp
pub fn matches_udsdisp(instance_id: &str, hardware_ids: &str) -> bool {
    let id = instance_id.to_ascii_uppercase();
    id.starts_with(r"ROOT\DISPLAY\") && hardware_ids.to_ascii_uppercase().contains(r"ROOT\UDSDISP")
}

#[cfg(windows)]
mod imp {
    use anyhow::{Context, Result};
    use tracing::{info, warn};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        DICS_DISABLE, DICS_ENABLE, DICS_FLAG_GLOBAL, DIF_PROPERTYCHANGE,
        DIGCF_ALLCLASSES, DIGCF_PRESENT, HDEVINFO, SPDRP_HARDWAREID, SP_CLASSINSTALL_HEADER,
        SP_DEVINFO_DATA, SP_PROPCHANGE_PARAMS, SetupDiChangeState, SetupDiDestroyDeviceInfoList,
        SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW,
        SetupDiGetDeviceRegistryPropertyW, SetupDiSetClassInstallParamsW,
    };
    use windows::core::PCWSTR;

    /// RAII wrapper for a device info set handle.
    struct DevInfoSet(HDEVINFO);

    impl Drop for DevInfoSet {
        fn drop(&mut self) {
            // SAFETY: the handle is ours and valid.
            unsafe {
                let _ = SetupDiDestroyDeviceInfoList(self.0);
            }
        }
    }

    /// Enables or disables every present device matching [`super::matches_udsdisp`].
    /// Returns `Ok(false)` when the adapter is not installed at all.
    ///
    /// # Errors
    /// SetupAPI failures (requires elevation; the service runs as SYSTEM).
    pub fn set_enabled(enable: bool) -> Result<bool> {
        // SAFETY: enumerating all present devices of every class.
        let set = unsafe {
            SetupDiGetClassDevsW(None, PCWSTR::null(), None, DIGCF_ALLCLASSES | DIGCF_PRESENT)
        }
        .context("SetupDiGetClassDevs")?;
        let set = DevInfoSet(set);

        let mut found = false;
        for index in 0u32.. {
            let mut data = SP_DEVINFO_DATA {
                cbSize: u32::try_from(size_of::<SP_DEVINFO_DATA>()).expect("struct fits in u32"),
                ..Default::default()
            };
            // SAFETY: `data` is correctly sized; error = no more items.
            if unsafe { SetupDiEnumDeviceInfo(set.0, index, &raw mut data) }.is_err() {
                break;
            }
            if !is_ours(set.0, &data) {
                continue;
            }
            found = true;
            change_state(set.0, &mut data, enable)
                .with_context(|| format!("SetupDiChangeState (enable={enable})"))?;
        }
        if found {
            info!(enable, "udsdisp adapter PnP state changed");
        } else {
            warn!("udsdisp adapter device not found (driver not installed?)");
        }
        Ok(found)
    }

    /// Reads the instance id and hardware ids of `data` and applies the pure
    /// predicate. Errors reading properties are treated as "not ours".
    fn is_ours(set: HDEVINFO, data: &SP_DEVINFO_DATA) -> bool {
        let mut id_buf = [0u16; 256];
        // SAFETY: buffer is valid for its length; returns error if too small.
        let instance_id = unsafe {
            SetupDiGetDeviceInstanceIdW(set, data, Some(&mut id_buf), None)
                .ok()
                .map(|()| {
                    let len = id_buf.iter().position(|&c| c == 0).unwrap_or(id_buf.len());
                    String::from_utf16_lossy(&id_buf[..len])
                })
        };
        let Some(instance_id) = instance_id else { return false };

        // Hardware ids are a REG_MULTI_SZ; concatenate with spaces for the
        // substring check in the pure predicate.
        let mut hw_buf = [0u8; 512];
        // SAFETY: buffer is valid for its length; error = treated as empty.
        let hardware_ids = unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set,
                data,
                SPDRP_HARDWAREID,
                None,
                Some(&mut hw_buf),
                None,
            )
            .ok()
            .map(|()| {
                let wide: Vec<u16> = hw_buf
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| u16::from_le_bytes(*b))
                    .collect();
                let mut out = String::new();
                for chunk in wide.split(|&c| c == 0) {
                    if !chunk.is_empty() {
                        out.push_str(&String::from_utf16_lossy(chunk));
                        out.push(' ');
                    }
                }
                out
            })
        }
        .unwrap_or_default();

        super::matches_udsdisp(&instance_id, &hardware_ids)
    }

    /// Issues a `DIF_PROPERTYCHANGE` class-installer call (`DICS_ENABLE` /
    /// `DICS_DISABLE`, global scope) on the device — the same mechanism
    /// `devcon enable/disable` and `pnputil /enable-device` use.
    fn change_state(set: HDEVINFO, data: &mut SP_DEVINFO_DATA, enable: bool) -> Result<()> {
        let params = SP_PROPCHANGE_PARAMS {
            ClassInstallHeader: SP_CLASSINSTALL_HEADER {
                cbSize: u32::try_from(size_of::<SP_CLASSINSTALL_HEADER>()).expect("struct fits in u32"),
                InstallFunction: DIF_PROPERTYCHANGE,
            },
            StateChange: if enable { DICS_ENABLE } else { DICS_DISABLE },
            Scope: DICS_FLAG_GLOBAL,
            HwProfile: 0,
        };
        // SAFETY: `data` and `params` are correctly sized and outlive the
        // call; the params buffer covers the whole SP_PROPCHANGE_PARAMS.
        unsafe {
            SetupDiSetClassInstallParamsW(
                set,
                Some(data),
                Some(&raw const params.ClassInstallHeader),
                u32::try_from(size_of::<SP_PROPCHANGE_PARAMS>()).expect("struct fits in u32"),
            )
            .context("SetupDiSetClassInstallParams")?;
            SetupDiChangeState(set, data)?;
        }
        Ok(())
    }
}

#[cfg(windows)]
pub use imp::set_enabled;

#[cfg(test)]
mod tests {
    use super::matches_udsdisp;

    #[test]
    fn matches_our_adapter() {
        assert!(matches_udsdisp(r"ROOT\DISPLAY\0000", r"ROOT\udsdisp"));
        assert!(matches_udsdisp(r"root\display\0007", r"ROOT\UDSDISP"));
    }

    #[test]
    fn rejects_other_devices() {
        assert!(!matches_udsdisp(r"PCI\VEN_1B36&DEV_0100\3&11583659&0&08", r"PCI\VEN_1B36"));
        assert!(!matches_udsdisp(r"ROOT\DISPLAY\0000", r"ROOT\BasicDisplay"));
        assert!(!matches_udsdisp(r"SWD\REMOTEPDISPLAY\...", r"ROOT\udsdisp"));
    }
}
