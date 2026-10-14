//! Build script for the UDS Credential Provider DLL.
//!
//! Windows builds only — on other hosts this script is a no-op so the crate
//! compiles to its pure-Rust `logic` module and the workspace stays buildable
//! on Linux. On Windows it embeds the version info, the icon and the tile
//! bitmap (resource 101, loaded by `imp::credentials::credential`) using
//! `winres`, the same approach as the reference credential provider.

#[cfg(windows)]
mod win {
    /// Embeds icon, version info and the tile bitmap into the DLL.
    pub fn embed_resources() {
        println!("cargo:rerun-if-changed=img/uds.bmp");
        println!("cargo:rerun-if-changed=img/uds.ico");

        let mut res = winres::WindowsResource::new();
        res.set_icon("img/uds.ico");
        res.set("ProductName", "UDS Remote Protocol Credential Provider");
        res.set("FileDescription", "UDS auto-logon credential provider");
        res.set(
            "LegalCopyright",
            "Copyright © 2012-2026 Virtual Cable S.L.U.",
        );
        res.set("CompanyName", "Virtual Cable S.L.U.");
        res.append_rc_content(r##"101      BITMAP      DISCARDABLE "img/uds.bmp""##);
        res.compile().unwrap();
    }
}

fn main() {
    #[cfg(windows)]
    win::embed_resources();
}
