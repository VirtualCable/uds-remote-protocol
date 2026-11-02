// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Build script: locates a system libvpx and links it.
//!
//! Resolution order:
//! 1. `VPX_LIB_DIR` (+ `VPX_STATIC=1` for static) — manual override.
//! 2. Windows: vcpkg (`vcpkg install libvpx:x64-windows-static-md`).
//! 3. Unix: pkg-config (`apt install libvpx-dev` / `brew install libvpx`).
//!
//! Bindings are pregenerated and committed per platform (`bindings_unix.rs`,
//! `bindings_windows.rs`); bindgen is NOT required to build. To regenerate
//! after a libvpx version bump, run `scripts/regen-vpx-bindings.sh` on a
//! machine with bindgen + libclang.

use std::env;

fn main() {
    // 1. Manual override.
    if let Ok(lib_dir) = env::var("VPX_LIB_DIR") {
        let mode = if env::var("VPX_STATIC").is_ok() {
            "static"
        } else {
            "dylib"
        };
        println!("cargo:rustc-link-search=native={lib_dir}");
        println!("cargo:rustc-link-lib={mode}=vpx");
        return;
    }

    // 2. Windows: vcpkg.
    #[cfg(target_os = "windows")]
    {
        match vcpkg::find_package("libvpx") {
            Ok(_) => return,
            Err(e) => {
                eprintln!("vcpkg could not find libvpx: {e}");
                eprintln!("install it with: vcpkg install libvpx:x64-windows-static-md");
                std::process::exit(1);
            }
        }
    }

    // 3. Unix: pkg-config.
    #[cfg(not(target_os = "windows"))]
    {
        match pkg_config::Config::new().atleast_version("1.10").probe("vpx") {
            Ok(_) => {}
            Err(e) => {
                eprintln!("pkg-config could not find libvpx >= 1.10: {e}");
                eprintln!("install it with: apt install libvpx-dev (or set VPX_LIB_DIR)");
                std::process::exit(1);
            }
        }
    }
}
