//! Build script for the udsdisp UMDF driver.
//!
//! Windows builds only — on other hosts this script is a no-op so the crate
//! compiles to an empty library and the workspace stays buildable on Linux.
//!
//! Two jobs on Windows:
//! 1. `wdk_build` — standard WDK configuration (include/lib paths, linking)
//!    for the UMDF binary itself.
//! 2. Our own bindgen pass over `IddCx.h` — `wdk-sys` does not ship an IddCx
//!    API subset (checked 2026-10-13: Base, Wdf, Gpio, Hid, ParallelPorts,
//!    Spb, Storage, Usb only), so we generate the IddCx bindings ourselves,
//!    same approach as the PortCls bindings in `drivers/udsaudio`.
//!
//! We target the IddCx 1.7 headers/libraries: 1.7 is the IddCx version
//! shipped by Windows Server 2022 (our minimum target OS), and a client built
//! against 1.7 also loads on any newer framework (Windows 11 ships 1.8+).

#[cfg(windows)]
mod win {
    use std::{env, fs, path::PathBuf};

    /// Root of the Windows Kits directory (WDK installs live here).
    const KITS_ROOT: &str = r"C:\Program Files (x86)\Windows Kits\10";

    /// IddCx version we build against (see module docs).
    const IDDCX_VERSION_DIR: &str = "1.7";

    /// UMDF version we build against; must match `target-umdf-version-minor`
    /// in Cargo.toml.
    const UMDF_WDF_HEADERS_DIR: &str = "2.33";

    /// Finds the newest kit version directory that contains user-mode
    /// headers (`um/`).
    pub fn newest_um_kit() -> (PathBuf, PathBuf) {
        let include_root = PathBuf::from(KITS_ROOT).join("Include");
        let lib_root = PathBuf::from(KITS_ROOT).join("Lib");
        let mut versions: Vec<_> = fs::read_dir(&include_root)
            .expect("Windows Kits include dir not found — is the WDK installed?")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| include_root.join(name).join("um").is_dir())
            .collect();
        versions.sort();
        let version = versions
            .last()
            .expect("no WDK kit version with user-mode headers found");
        (include_root.join(version), lib_root.join(version))
    }

    /// Generates Rust bindings for IddCx (IddCx.h + IddCxFuncEnum.h) into
    /// `OUT_DIR/iddcx_bindings.rs` and links `iddcxstub.lib` (the stub library
    /// that owns the `IddFunctions`/`IddDriverGlobals` globals the IddCx class
    /// extension patches at load time).
    pub fn generate_iddcx_bindings(include: &std::path::Path, lib: &std::path::Path) {
        let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is always set by cargo"));

        // IddCx.h refuses to compile unless WDFAPI is defined, which the UMDF
        // wdf.h provides; it also pulls in Opmapi.h, Dxgi.h and d3d11_4.h
        // from the um/ directory. The versioned path must match
        // IDDCX_VERSION_DIR (and the -DIDDCX_VERSION_* defines below).
        let wrapper = format!(
            "#include <windows.h>\n#include <wdf.h>\n#include <iddcx/{IDDCX_VERSION_DIR}/IddCx.h>\n"
        );

        let wdf_umdf_include = PathBuf::from(KITS_ROOT)
            .join("Include")
            .join("wdf")
            .join("umdf")
            .join(UMDF_WDF_HEADERS_DIR);
        assert!(
            wdf_umdf_include.join("wdf.h").is_file(),
            "wdf.h for UMDF {UMDF_WDF_HEADERS_DIR} not found at {}",
            wdf_umdf_include.display()
        );

        let bindings = bindgen::Builder::default()
            .header_contents("iddcx_wrapper.h", &wrapper)
            .clang_arg(format!("-I{}", include.join("um").display()))
            .clang_arg(format!("-I{}", include.join("shared").display()))
            .clang_arg(format!("-I{}", include.join("ucrt").display()))
            .clang_arg(format!("-I{}", wdf_umdf_include.display()))
            // WDK headers select target-arch code via _AMD64_ (defining the
            // clang --target is not enough: ntdef.h errors with "No Target
            // Architecture" otherwise).
            .clang_arg("-D_AMD64_=1")
            // IddCx.h is C++ (it uses IDARG_* names without the struct
            // keyword) and IddCxFuncEnum.h requires the client to declare
            // the IddCx version it builds against; both must match the
            // versioned header dir in the wrapper include.
            .clang_args(["-x", "c++", "-std=c++17"])
            .clang_arg("-DIDDCX_VERSION_MAJOR=1")
            .clang_arg(format!(
                "-DIDDCX_VERSION_MINOR={}",
                IDDCX_VERSION_DIR.split('.').next_back().unwrap()
            ))
            .clang_arg("-fms-extensions")
            .clang_arg("--target=x86_64-pc-windows-msvc")
            // IddCx entry points are FORCEINLINE shims over the IddFunctions[]
            // dispatch table, so bindgen cannot export them as functions; what
            // we need are the PFN_* typedefs, the IDARG_*/IDD*/IDDCX_* types,
            // the EVT callback typedefs and the dispatch-table index enum.
            .allowlist_type("IDDCX.*|IDARG.*|IDD_.*|PFN_IDD.*|EVT_IDD.*")
            .allowlist_var("IDDCX_VERSION.*|IDD_ALWAYS_AVAILABLE_FUNCTION_COUNT|.*TableIndex")
            .ctypes_prefix("std::ffi")
            .layout_tests(false)
            .generate()
            .expect("IddCx.h bindings failed to generate");

        bindings
            .write_to_file(out_dir.join("iddcx_bindings.rs"))
            .expect("failed to write iddcx bindings");

        // Link against the versioned IddCx stub library (um\x64\iddcx\1.7).
        println!(
            "cargo:rustc-link-search=native={}",
            lib.join("um").join("x64").join("iddcx").join(IDDCX_VERSION_DIR).display()
        );
        println!("cargo:rustc-link-lib=static=iddcxstub");
    }
}

#[cfg(windows)]
fn main() -> Result<(), wdk_build::ConfigError> {
    let (include, lib) = win::newest_um_kit();
    win::generate_iddcx_bindings(&include, &lib);
    wdk_build::configure_wdk_binary_build()
}

#[cfg(not(windows))]
fn main() {}
