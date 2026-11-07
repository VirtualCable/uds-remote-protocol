// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! # labclient
//!
//! Laboratory clients for the uds-rp protocol — everything that exists only
//! to exercise the framework end to end and is **not** reusable by real
//! integrators (uds-client, future third parties):
//!
//! - `gui` module: reference winit + wgpu GUI (connect form, session view,
//!   input forwarding), used by the `uds-rp-client` binary.
//! - `testclient` binary: scripted/headless E2E client.
//! - `uds-rp-client` binary: interactive reference client.
//!
//! Reusable pieces (session, events/commands, channel contracts) live in
//! the `client` and `channels` crates, never here.

pub mod gui;
