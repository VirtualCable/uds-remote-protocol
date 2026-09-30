//! # vpx-sys
//!
//! Internal FFI bindings to libvpx (VP8/VP9 encoder + decoder).
//!
//! The bindings are **pregenerated** (bindgen 0.73 against libvpx 1.15
//! headers, checked in per platform as `bindings_unix.rs` /
//! `bindings_windows.rs`) so building this crate needs neither bindgen nor
//! libclang. Per-platform files are required: the libvpx build config (and
//! even the header version, e.g. `VPX_EXT_RATECTRL_ABI_VERSION` 9→10 between
//! 1.15.0 and 1.15.2) changes the ABI version constants the runtime checks.
//! Regenerate with `scripts/regen-vpx-bindings.sh` when bumping libvpx.
//!
//! The crate is raw FFI: `unsafe`, C naming, no safety layer. The safe
//! wrappers implementing `codec::VideoEncoder`/`VideoDecoder` live in the
//! `codec` crate behind the `codec-vp9`/`codec-vp8` features.

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]
#![allow(clippy::all, clippy::pedantic)]

#[allow(missing_docs)]
#[cfg(not(windows))]
#[path = "bindings_unix.rs"]
mod bindings;

#[allow(missing_docs)]
#[cfg(windows)]
#[path = "bindings_windows.rs"]
mod bindings;

pub use bindings::*;

/// Linked libvpx version string, e.g. `"v1.15.0"`.
#[must_use]
pub fn version() -> String {
    // SAFETY: vpx_codec_version_str returns a static, NUL-terminated string.
    unsafe {
        let p = vpx_codec_version_str();
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_and_reports_version() {
        assert!(version().starts_with("v1."), "unexpected: {}", version());
    }
}
