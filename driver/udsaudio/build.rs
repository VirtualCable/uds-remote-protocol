//! Build script for the udsaudio kernel driver.
//!
//! Two jobs (Windows builds only — on other hosts this script is a no-op so
//! the pure logic in `src/logic.rs` stays testable everywhere):
//! 1. `wdk_build` — standard WDK configuration (include/lib paths, linking)
//!    for the WDM binary itself.
//! 2. Our own bindgen pass over `portcls.h` — `wdk-sys` does not ship an
//!    audio/PortCls API subset (checked 2026-10-12: Base, Wdf, Gpio, Hid,
//!    ParallelPorts, Spb, Storage, Usb only), so we generate the PortCls
//!    bindings ourselves and link against `portcls.lib`.

#[cfg(windows)]
mod win {
    use std::{env, fs, path::PathBuf};

    /// Root of the Windows Kits directory (WDK installs live here).
    const KITS_ROOT: &str = r"C:\Program Files (x86)\Windows Kits\10";

    /// Finds the newest kit version directory that actually contains
    /// kernel-mode headers (`km/`). wdk-build requires the SDK and WDK
    /// versions to match; this discovery follows the same rule
    /// (doc/plan/driver-audio-windows.md, M0 pitfall).
    pub fn newest_km_kit() -> (PathBuf, PathBuf) {
        let include_root = PathBuf::from(KITS_ROOT).join("Include");
        let lib_root = PathBuf::from(KITS_ROOT).join("Lib");
        let mut versions: Vec<_> = fs::read_dir(&include_root)
            .expect("Windows Kits include dir not found — is the WDK installed?")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| include_root.join(name).join("km").is_dir())
            .collect();
        versions.sort();
        let version = versions
            .last()
            .expect("no WDK kit version with kernel headers found");
        (include_root.join(version), lib_root.join(version))
    }

    /// Generates Rust bindings for PortCls (portcls.h + the KS headers it
    /// needs) into `OUT_DIR/portcls_bindings.rs` and links `portcls.lib`.
    pub fn generate_portcls_bindings(include: &std::path::Path, lib: &std::path::Path) {
        let out_dir =
            PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is always set by cargo"));

        let bindings = bindgen::Builder::default()
            .header(format!(r"{}\km\portcls.h", include.display()))
            .clang_arg(format!("-I{}", include.join("km").display()))
            .clang_arg(format!("-I{}", include.join("km").join("crt").display()))
            .clang_arg(format!("-I{}", include.join("shared").display()))
            .clang_arg("-D_KERNEL_MODE=1")
            .clang_arg("-DPOOL_NX_OPTIN=1")
            .clang_arg("--target=x86_64-pc-windows-msvc")
            // PortCls entry points and the miniport/port interface vtables.
            .allowlist_function("Pc.*")
            .allowlist_type("I(Miniport|Port|AdapterPower).*")
            .allowlist_var("CLSID_Miniport.*|IID_I(Miniport|Port).*")
            // KS types that portcls.h signatures reference.
            .allowlist_type("KSP.*|KSDataFormat.*|WAVEFORMATEX.*")
            .use_core()
            .ctypes_prefix("core::ffi")
            .layout_tests(false)
            .generate()
            .expect("portcls.h bindings failed to generate");

        bindings
            .write_to_file(out_dir.join("portcls_bindings.rs"))
            .expect("failed to write portcls bindings");

        // Link against the WDK's portcls.lib (kernel-mode library).
        println!(
            "cargo:rustc-link-search=native={}",
            lib.join("km").join("x64").display()
        );
        println!("cargo:rustc-link-lib=static=portcls");
    }
}

#[cfg(windows)]
fn main() -> Result<(), wdk_build::ConfigError> {
    let (include, lib) = win::newest_km_kit();
    win::generate_portcls_bindings(&include, &lib);
    wdk_build::configure_wdk_binary_build()
}

#[cfg(not(windows))]
fn main() {}
