// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Integration contracts (device-side traits) for the client session,
//! modeled after freerdp-integration: the session speaks these traits and
//! the consumer (labclient today, the UDS launcher in production) injects
//! concrete implementations. Device implementations live in `channels`,
//! which depends on `client` — never the other way around.
//!
//! ```text
//! channels (devices) ──implements──▶ client::integrations ◀──uses── client::session
//! ```

pub mod audio_out;
pub mod mic;
pub mod printer;
pub mod smartcard;
pub mod webcam;

use std::sync::Arc;

use audio_out::AudioOutputFactory;
use mic::MicFactory;
use printer::PrinterSink;
use smartcard::SmartcardIntegration;
use webcam::WebcamSource;

/// Device implementations injected by the consumer (§15.3, freerdp-
/// integration model): each `Some` enables its channel — the presence of
/// the implementation IS the capability flag in the HELLO, so a channel
/// without a backend simply is not offered.
///
/// `labclient` builds these from `channels`; the UDS launcher will bridge
/// its own integrations here.
#[derive(Default)]
pub struct Integrations {
    /// Webcam capture source (§18.57); probed by the consumer.
    pub webcam: Option<Box<dyn WebcamSource>>,
    /// Printer job sink (§18.57 phase 0).
    pub printer: Option<Box<dyn PrinterSink>>,
    /// Mic capture factory (`MicMode::Device`); the protocol side (Opus,
    /// pacing, test tone) stays in `client`.
    pub mic: Option<MicFactory>,
    /// Audio output factory; `None` is the null sink (decode continues,
    /// A/V clock presents immediately).
    pub audio_out: Option<AudioOutputFactory>,
    /// Smartcard PC/SC backend (§17); `None` answers every request with
    /// `SCARD_E_NO_SERVICE` when the channel is enabled.
    pub smartcard: Option<Arc<dyn SmartcardIntegration>>,
}

impl std::fmt::Debug for Integrations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Integrations")
            .field("webcam", &self.webcam.is_some())
            .field("printer", &self.printer.is_some())
            .field("mic", &self.mic.is_some())
            .field("audio_out", &self.audio_out.is_some())
            .field("smartcard", &self.smartcard.is_some())
            .finish()
    }
}
