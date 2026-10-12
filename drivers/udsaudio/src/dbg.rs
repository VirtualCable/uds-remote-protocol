//! Minimal `DbgPrint` wrapper for kernel debug output (visible with
//! Sysinternals DebugView on the target). Logging is best-effort and must
//! never fail: no allocation, no panics.

use core::ffi::c_char;

unsafe extern "C" {
    /// `DbgPrint` from ntoskrnl (kdprint.h).
    fn DbgPrint(format: *const c_char, ...) -> u32;
}

/// Emits one static log line. Only callable via the [`dlog`] macro, which
/// guarantees a static null-terminated string.
pub fn log_str(msg: &'static [u8]) {
    // SAFETY: the macro only passes `concat!(...).as_bytes()` of string
    // literals ending in "\0", so the pointer is a valid null-terminated
    // static C string. DbgPrint is callable at any IRQL.
    unsafe {
        DbgPrint(msg.as_ptr().cast());
    }
}

/// Same as [`log_str`] with a single `%I64x` argument.
pub fn log_str_int(msg: &'static [u8], val: u64) {
    // SAFETY: as above; the format strings we use take one u64.
    unsafe {
        DbgPrint(msg.as_ptr().cast(), val);
    }
}

/// Logs a static string with the driver prefix.
macro_rules! dlog {
    ($msg:literal) => {{
        $crate::dbg::log_str(concat!("udsaudio: ", $msg, "\0").as_bytes())
    }};
    ($msg:literal, $val:expr) => {{
        $crate::dbg::log_str_int(concat!("udsaudio: ", $msg, "\0").as_bytes(), $val)
    }};
}

pub(crate) use dlog;
