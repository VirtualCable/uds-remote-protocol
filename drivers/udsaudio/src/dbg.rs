//! Minimal `DbgPrint` wrapper for kernel debug output (visible with
//! Sysinternals DebugView on the target). Logging is best-effort and must
//! never fail: no allocation, no panics.

use core::ffi::c_char;

unsafe extern "C" {
    /// `DbgPrint` from ntoskrnl (kdprint.h).
    fn DbgPrint(format: *const c_char, ...) -> u32;
}

/// Emits one static log line. `msg` must be a null-terminated byte string
/// (use the [`log`] macro, which appends the terminator).
///
/// SAFETY: `DbgPrint` accepts a null-terminated format string at any IRQL.
pub unsafe fn raw(msg: *const c_char) {
    // SAFETY: caller guarantees `msg` is null-terminated and static.
    unsafe {
        DbgPrint(msg);
    }
}

/// Logs a static string with the driver prefix.
macro_rules! dlog {
    ($msg:literal) => {{
        // SAFETY: concat! of string literals is a 'static null-terminated C
        // string.
        unsafe { $crate::dbg::raw(concat!("udsaudio: ", $msg, "\0").as_ptr().cast()) }
    }};
    ($msg:literal, $val:expr) => {{
        // SAFETY: as above; single %x / %lx style argument passed varargs-style.
        unsafe {
            $crate::dbg::raw_int(concat!("udsaudio: ", $msg, "\0").as_ptr().cast(), $val)
        }
    }};
}

/// Emits one log line with a single `0x%lx` argument.
///
/// SAFETY: `fmt` must be a null-terminated static string containing exactly
/// one `%lx`-compatible specifier.
pub unsafe fn raw_int(fmt: *const c_char, val: u64) {
    // SAFETY: caller guarantees format/argument pairing.
    unsafe {
        DbgPrint(fmt, val);
    }
}

pub(crate) use dlog;
