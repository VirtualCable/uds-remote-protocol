//! UDS Virtual Audio Device — PortCls miniport driver (spike).
//!
//! Provides two virtual audio endpoints for remote sessions:
//! - a virtual speaker (render), used as the audio-out capture source
//!   (WASAPI loopback works on it like on any render endpoint);
//! - a virtual microphone (capture), fed by the session agent via IOCTL.
//!
//! All policy lives in user mode; the driver only moves PCM buffers.
//! See doc/plan/driver-audio-windows.md for the spike plan.

#![cfg_attr(not(test), no_std)]

mod logic;
#[cfg(windows)]
mod portcls;

#[cfg(all(windows, not(test)))]
extern crate wdk_panic;

#[cfg(all(windows, not(test)))]
use wdk_alloc::WdkAllocator;

#[cfg(all(windows, not(test)))]
#[global_allocator]
static GLOBAL_ALLOCATOR: WdkAllocator = WdkAllocator;

// SAFETY: "DriverEntry" is the required symbol name for Windows driver entry
// points. No other function in this compilation unit exports this name,
// preventing symbol conflicts.
#[cfg(windows)]
#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    _driver: wdk_sys::PDRIVER_OBJECT,
    _registry_path: wdk_sys::PCUNICODE_STRING,
) -> wdk_sys::NTSTATUS {
    // M1 milestone: prove the crate links (wdk-sys + portcls.lib) and loads.
    // M2 will register the PortCls adapter here.
    wdk_sys::STATUS_SUCCESS
}
