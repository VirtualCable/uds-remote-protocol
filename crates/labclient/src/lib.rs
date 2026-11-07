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

/// Builds the [`Integrations`](client::integrations::Integrations) the lab
/// clients inject into the session: every device backend comes from
/// `channels` (real devices, cpal audio, PC/SC smartcard), which is exactly
/// the wiring a real integrator replaces with its own implementations.
#[allow(clippy::fn_params_excessive_bools)] // flags mirror the CLI/form one to one
pub fn build_integrations(
    webcam: bool,
    printer_dir: Option<std::path::PathBuf>,
    mic_device: bool,
    audio: bool,
    smartcard: bool,
) -> client::integrations::Integrations {
    client::integrations::Integrations {
        webcam: webcam.then(channels::webcam::default_source).flatten(),
        printer: printer_dir.and_then(|dir| {
            match channels::printer::PdfToDirSink::new(dir.clone(), protocol::MAX_JOB_BYTES) {
                Ok(sink) => Some(Box::new(sink) as Box<dyn client::integrations::printer::PrinterSink>),
                Err(e) => {
                    tracing::warn!(dir = %dir.display(), "printer save dir unavailable: {e}");
                    None
                }
            }
        }),
        mic: mic_device.then(channels::audio::input::default_mic_factory),
        audio_out: audio.then(channels::audio::output::default_output_factory),
        smartcard: smartcard.then(channels::smartcard::default_backend).flatten(),
    }
}
