//! UDS Virtual Audio Device — PortCls miniport driver (spike).
//!
//! Provides two virtual audio endpoints for remote sessions:
//! - a virtual speaker (render), used as the audio-out capture source
//!   (WASAPI loopback works on it like on any render endpoint);
//! - a virtual microphone (capture), fed by the session agent via IOCTL
//!   (M4; currently produces silence).
//!
//! All policy lives in user mode; the driver only moves PCM buffers.
//! See doc/plan/driver-audio-windows.md for the spike plan.

#![cfg_attr(not(test), no_std)]

#[cfg(windows)]
mod adapter;
#[cfg(windows)]
mod common;
#[cfg(windows)]
mod dbg;
mod logic;
#[cfg(windows)]
mod miniport;
#[cfg(windows)]
mod portcls;
#[cfg(windows)]
mod stream;
#[cfg(windows)]
mod tables;

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
    driver: wdk_sys::PDRIVER_OBJECT,
    registry_path: wdk_sys::PCUNICODE_STRING,
) -> wdk_sys::NTSTATUS {
    // SAFETY: entry point contract — pointers valid for the call.
    unsafe { adapter::driver_entry(driver, registry_path) }
}
