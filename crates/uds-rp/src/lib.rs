// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! # uds-rp
//!
//! Public facade of the UDS remote desktop protocol framework.
//! Re-exports the internal crates for consumption by the launcher.

pub use client;
pub use codec;
pub use crypt;
pub use protocol as core;
pub use server;
pub use transport;
