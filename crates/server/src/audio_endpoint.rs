//! Default render-endpoint policy (design doc §9, driver-audio plan §8).
//!
//! While a remote session runs, the session's default render endpoint is
//! switched to our virtual "Speakers (UDS Virtual Audio Device)" so every
//! app renders into the driver, whose loopback pin feeds the WASAPI capture
//! that goes down the wire. The previous default is restored when the
//! session ends.
//!
//! The switch uses `IPolicyConfig::SetDefaultEndpoint`, an undocumented but
//! de-facto standard COM interface (same one the classic "sound switcher"
//! tools use). Only the `SetDefaultEndpoint` vtable slot is declared; the
//! rest of the vtable is opaque padding.
//!
//! Best-effort by design: with no UDS endpoint installed (dev machines, or
//! a host that keeps its physical audio) the session just captures whatever
//! the current default is, exactly as before.

use std::ffi::c_void;

use anyhow::{Context as _, Result};
use tracing::{info, warn};
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, ERole, IMMDevice, IMMDeviceCollection, IMMDeviceEnumerator,
    MMDeviceEnumerator, eCommunications, eConsole, eMultimedia, eRender,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::core::{GUID, HRESULT, IUnknown, Interface as _, PCWSTR, PWSTR};

/// `CLSID_PolicyConfig` (undocumented): {870AF99C-171D-4F9E-AF0D-E63DF40C2BC9}.
const CLSID_POLICY_CONFIG: GUID =
    GUID::from_values(0x870a_f99c, 0x171d, 0x4f9e, [0xaf, 0x0d, 0xe6, 0x3d, 0xf4, 0x0c, 0x2b, 0xc9]);

/// `IID_IPolicyConfig` (undocumented): {F8679F50-850A-41CF-9C72-430F290290C8}.
const IID_IPOLICY_CONFIG: GUID =
    GUID::from_values(0xf867_9f50, 0x850a, 0x41cf, [0x9c, 0x72, 0x43, 0x0f, 0x29, 0x02, 0x90, 0xc8]);

/// `PKEY_Device_FriendlyName`: {A45C254E-DF1C-4EFD-8020-67D146A850E0}, pid 14.
const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_values(
        0xa45c_254e,
        0xdf1c,
        0x4efd,
        [0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0],
    ),
    pid: 14,
};

/// VT_LPWSTR: the property store hands the friendly name as a wide string.
const VT_LPWSTR: windows::Win32::System::Variant::VARENUM =
    windows::Win32::System::Variant::VARENUM(31);

/// Substring matched against each active render endpoint's friendly name.
const UDS_SPEAKERS_NAME: &str = "UDS Virtual Audio Device";

/// Roles switched together so every app category lands on our device.
const ROLES: [ERole; 3] = [eConsole, eMultimedia, eCommunications];

/// `IUnknown` vtable slots we call manually. `QueryInterface` is required:
/// the `IPolicyConfig` interface pointer is NOT the same pointer as the
/// object's `IUnknown` (calling `SetDefaultEndpoint` through the `IUnknown`
/// pointer hits a wrong vtable and fails with RPC_X_NULL_REF_POINTER).
#[repr(C)]
struct IUnknownVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

/// Minimal `IPolicyConfig` vtable: only `SetDefaultEndpoint` (slot 13) is
/// real; the earlier slots keep the layout honest and are never called.
#[repr(C)]
struct IPolicyConfigVtbl {
    query_interface: *const c_void,      // 0
    add_ref: *const c_void,              // 1
    release: *const c_void,              // 2
    get_mix_format: *const c_void,       // 3
    get_device_format: *const c_void,    // 4
    reset_device_format: *const c_void,  // 5
    set_device_format: *const c_void,    // 6
    get_processing_period: *const c_void,// 7
    set_processing_period: *const c_void,// 8
    get_share_mode: *const c_void,       // 9
    set_share_mode: *const c_void,       // 10
    get_property_value: *const c_void,   // 11
    set_property_value: *const c_void,   // 12
    set_default_endpoint: unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT, // 13
}

/// Raw, reference-counted `IPolicyConfig` pointer. Never stored across COM
/// apartment lifetimes: it is created, used and released inside a single
/// `CoInitializeEx`/`CoUninitialize` scope.
struct PolicyConfig {
    this: *mut c_void,
}

impl PolicyConfig {
    /// Activates the PolicyConfig object and queries its `IPolicyConfig`.
    fn create() -> Result<Self> {
        // SAFETY: standard coclass activation; we only need the IUnknown
        // pointer to QI for the real interface.
        let unknown: IUnknown = unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL) }
            .context("PolicyConfig activation failed")?;
        let mut this: *mut c_void = std::ptr::null_mut();
        // SAFETY: `unknown` is a live COM object; `this` is a valid out
        // param. On success `this` owns one reference released in Drop.
        let hr = unsafe {
            let vtbl = *(unknown.as_raw().cast::<*const IUnknownVtbl>());
            ((*vtbl).query_interface)(unknown.as_raw(), &IID_IPOLICY_CONFIG, &raw mut this)
        };
        hr.ok().context("QueryInterface(IPolicyConfig) failed")?;
        anyhow::ensure!(!this.is_null(), "QueryInterface(IPolicyConfig) returned null");
        Ok(Self { this })
    }

    fn set_default_endpoint(&self, device_id: &str, role: ERole) -> Result<()> {
        let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `this` is a live IPolicyConfig pointer whose first field
        // is the vtable pointer; `wide` is null-terminated and outlives the
        // call.
        let hr = unsafe {
            let vtbl = *(self.this.cast::<*const IPolicyConfigVtbl>());
            ((*vtbl).set_default_endpoint)(self.this, PCWSTR(wide.as_ptr()), role)
        };
        hr.ok().context("SetDefaultEndpoint")
    }
}

impl Drop for PolicyConfig {
    fn drop(&mut self) {
        // SAFETY: `this` owns one reference, released exactly once here.
        unsafe {
            let vtbl = *(self.this.cast::<*const IUnknownVtbl>());
            ((*vtbl).release)(self.this);
        }
    }
}

/// RAII guard: restores the pre-session default endpoints on drop.
///
/// Only the `(role, device id)` pairs are stored — NOT the COM object. The
/// PolicyConfig object lives in the MTA of whichever thread created it, and
/// that apartment is torn down (`CoUninitialize`) when the switch call
/// returns; calling the object later from a session-teardown thread derefs a
/// vtable that may point into an already-unloaded MMDevAPI (AV at the vtable
/// call). The drop therefore builds a fresh PolicyConfig inside its own COM
/// scope instead.
pub struct DefaultEndpointGuard {
    /// (role, device id) pairs captured before the switch.
    previous: Vec<(ERole, String)>,
}

impl Drop for DefaultEndpointGuard {
    fn drop(&mut self) {
        // The drop can run on a different thread than the creation (async
        // session task); make sure COM is up here. `S_FALSE` (already MTA)
        // is fine; the balancing uninit below is harmless either way.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        match PolicyConfig::create() {
            Ok(policy) => {
                for (role, id) in &self.previous {
                    if let Err(err) = policy.set_default_endpoint(id, *role) {
                        warn!(error = %format!("{err:#}"), role = role.0, "cannot restore the default render endpoint");
                    }
                }
                info!("default render endpoints restored");
            }
            Err(err) => {
                warn!(error = %format!("{err:#}"), "cannot restore the default render endpoints");
            }
        }
        unsafe { CoUninitialize() };
    }
}

/// Switches the session default render endpoint to the UDS virtual speakers
/// (all roles). Returns `Ok(None)` — and changes nothing — when no UDS
/// endpoint is installed; errors are for real API failures.
pub fn switch_default_to_uds() -> Result<Option<DefaultEndpointGuard>> {
    // SAFETY: standard per-thread COM init; S_FALSE (already MTA) is
    // accepted by `.ok()`. Balanced by CoUninitialize at the end.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .context("CoInitializeEx failed")?;
    let result = switch_default_to_uds_impl();
    unsafe { CoUninitialize() };
    result
}

fn switch_default_to_uds_impl() -> Result<Option<DefaultEndpointGuard>> {
    // SAFETY: coclass activation with valid out-params.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .context("MMDeviceEnumerator creation failed (is Audiosrv running?)")?;

    let Some(uds) = find_uds_render_endpoint(&enumerator)? else {
        info!("no UDS virtual audio endpoint installed; keeping the current default");
        return Ok(None);
    };
    let uds_id = device_id(&uds)?;
    let uds_name = device_friendly_name(&uds).unwrap_or_else(|| UDS_SPEAKERS_NAME.to_string());

    // The undocumented policy class, with a real IPolicyConfig pointer
    // (NOT the IUnknown pointer — see `PolicyConfig::create`).
    let policy = PolicyConfig::create()?;

    // Capture the current defaults per role (best-effort per role).
    let mut previous = Vec::new();
    for role in ROLES {
        if let Ok(device) = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, role) } {
            match device_id(&device) {
                Ok(id) => previous.push((role, id)),
                Err(err) => warn!(error = %format!("{err:#}"), role = role.0, "cannot read the current default endpoint id"),
            }
        } else {
            info!(role = role.0, "no current default render endpoint for this role");
        }
    }

    for role in ROLES {
        policy
            .set_default_endpoint(&uds_id, role)
            .with_context(|| format!("SetDefaultEndpoint failed for role {}", role.0))?;
    }
    info!(endpoint = %uds_name, "default render endpoint switched to UDS virtual speakers");
    // The PolicyConfig object dies with this COM scope on purpose; the guard
    // re-creates it at restore time (see DefaultEndpointGuard).
    Ok(Some(DefaultEndpointGuard { previous }))
}

/// Finds the first active render endpoint whose friendly name contains
/// [`UDS_SPEAKERS_NAME`].
fn find_uds_render_endpoint(enumerator: &IMMDeviceEnumerator) -> Result<Option<IMMDevice>> {
    let collection: IMMDeviceCollection =
        unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }
            .context("EnumAudioEndpoints failed")?;
    let count = unsafe { collection.GetCount() }.context("IMMDeviceCollection::GetCount failed")?;
    for index in 0..count {
        let device = unsafe { collection.Item(index) }
            .context("IMMDeviceCollection::Item failed")?;
        if let Some(name) = device_friendly_name(&device)
            && name.contains(UDS_SPEAKERS_NAME)
        {
            return Ok(Some(device));
        }
    }
    Ok(None)
}

/// The endpoint's WMI device interface id (`IMMDevice::GetId`).
fn device_id(device: &IMMDevice) -> Result<String> {
    // SAFETY: the returned PWSTR is CoTaskMem-owned by us and freed below.
    let id: PWSTR = unsafe { device.GetId() }.context("IMMDevice::GetId failed")?;
    let text = unsafe { id.to_string() }.context("device id is not valid UTF-16")?;
    // SAFETY: `id` came from GetId and is freed exactly once.
    unsafe { CoTaskMemFree(Some(id.as_ptr().cast())) };
    Ok(text)
}

/// The endpoint's friendly name from its property store (None when unreadable).
fn device_friendly_name(device: &IMMDevice) -> Option<String> {
    // SAFETY: standard property-store read; the PROPVARIANT is cleared
    // exactly once before returning.
    let store: IPropertyStore = unsafe { device.OpenPropertyStore(STGM_READ) }.ok()?;
    let mut value: PROPVARIANT = unsafe { store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME) }.ok()?;
    let name = unsafe {
        let outer = &value.Anonymous.Anonymous;
        if outer.vt == VT_LPWSTR {
            let pwsz: PWSTR = outer.Anonymous.pwszVal;
            pwsz.to_string().ok()
        } else {
            None
        }
    };
    let _ = unsafe { PropVariantClear(&raw mut value) };
    name
}
