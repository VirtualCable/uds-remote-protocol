// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Small helpers: resource IDs, computer name and username/domain splitting.
//! Taken from the reference provider.

use windows::{
    Win32::System::WindowsProgramming::GetComputerNameW,
    core::{PWSTR, PCWSTR},
};

/// Builds a resource ID pointer (`MAKEINTRESOURCEW`).
#[inline]
pub const fn make_int_resource(id: u16) -> PCWSTR {
    PCWSTR(id as usize as *const u16)
}

/// Returns the local computer name, or `"."` if it cannot be retrieved.
pub fn get_computer_name() -> String {
    let mut buffer = [0u16; 64];
    let mut size = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    match unsafe { GetComputerNameW(Some(PWSTR(buffer.as_mut_ptr())), &mut size) } {
        Ok(()) => String::from_utf16_lossy(&buffer[..size as usize]),
        Err(_) => ".".into(),
    }
}

/// Splits `domain\username` / `username@domain` / bare `username` into
/// `(username, domain)`; a bare username gets the local computer name.
pub fn split_username_domain(username: &str) -> (String, String) {
    if username.contains('@') {
        let parts: Vec<&str> = username.split('@').collect();
        if parts.len() == 2 {
            return (parts[0].to_string(), parts[1].to_string());
        }
    } else if username.contains('\\') {
        let parts: Vec<&str> = username.split('\\').collect();
        if parts.len() == 2 {
            return (parts[1].to_string(), parts[0].to_string());
        }
    }
    (username.to_string(), get_computer_name())
}
