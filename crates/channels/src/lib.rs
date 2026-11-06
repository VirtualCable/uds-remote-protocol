// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! # channels
//!
//! Device-side channel handles for the UDS remote protocol, modeled after
//! the `channels` crate of uds-client: every channel owns its OS device
//! access behind a handle (thread + command channel) and knows nothing
//! about the wire; the `client` crate wires these handles to the protocol
//! and knows nothing about the OS devices.
//!
//! The traits here are OURS, shaped for our protocol — close enough to
//! `rdp::integrations::*` that a future uds-client integration is a thin
//! wrapper in whichever direction is convenient per channel.

pub mod audio;
pub mod clipboard;
pub mod printer;
pub mod webcam;
