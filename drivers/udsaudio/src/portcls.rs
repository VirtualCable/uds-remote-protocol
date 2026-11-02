// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Generated PortCls bindings (bindgen over portcls.h, see build.rs).
//!
//! `wdk-sys` has no audio subset, so these are our own. Raw FFI: everything
//! here is unsafe by definition; the safe surface lives in higher modules.
#![allow(non_snake_case)]
#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]
#![allow(dead_code)]
#![allow(clippy::all)]
#![allow(unsafe_op_in_unsafe_fn)]
// bindgen 0.72 emits bitfield accessors with identity transmutes.
#![allow(unnecessary_transmutes)]

include!(concat!(env!("OUT_DIR"), "/portcls_bindings.rs"));
