// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Small shared helpers for the COM-style kernel objects (status codes not
//! covered by wdk-sys, GUID comparison).

use crate::portcls as pc;

// NTSTATUS values (ntstatus.h) — wdk-sys does not export the full set, and
// the bindgen bindings only cover the allowlisted names, so the few we need
// are defined here as plain constants.
pub const STATUS_SUCCESS: i32 = 0;
pub const STATUS_UNSUCCESSFUL: i32 = 0xC000_0001_u32 as i32;
pub const STATUS_NOT_IMPLEMENTED: i32 = 0xC000_0002_u32 as i32;
pub const STATUS_INVALID_PARAMETER: i32 = 0xC000_000D_u32 as i32;
pub const STATUS_NO_MATCH: i32 = 0xC000_0272_u32 as i32;
pub const STATUS_INSUFFICIENT_RESOURCES: i32 = 0xC000_009A_u32 as i32;
pub const STATUS_NOT_SUPPORTED: i32 = 0xC000_00BB_u32 as i32;
pub const STATUS_NOINTERFACE: i32 = 0xC000_02B9_u32 as i32;
pub const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005_u32 as i32;
pub const STATUS_BUFFER_TOO_SMALL: i32 = 0xC000_0023_u32 as i32;
pub const STATUS_INVALID_DEVICE_REQUEST: i32 = 0xC000_0010_u32 as i32;
pub const STATUS_INVALID_BUFFER_SIZE: i32 = 0xC000_0206_u32 as i32;

/// Byte-wise GUID comparison (the bindgen GUID derives no PartialEq).
pub fn guid_eq(a: &pc::GUID, b: &pc::GUID) -> bool {
    a.Data1 == b.Data1 && a.Data2 == b.Data2 && a.Data3 == b.Data3 && a.Data4 == b.Data4
}
