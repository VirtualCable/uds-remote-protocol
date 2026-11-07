// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! The [`SmartcardIntegration`] backend trait: the contract a consumer
//! implements against a real PC/SC subsystem (pcsc-lite / WinSCard).
//!
//! Ported from freerdp-integration (`rdp/src/integrations/smartcard/` and
//! `rdp/src/addins/smartcard/transactions.rs`), dropping everything tied to
//! MS-RDPESC/NDR and the emulated-card line (container-info/certificate
//! caches): phase 1 is physical card relay only. Synchronous and pure: no
//! networking, no async — the channel layer owns threading.

mod consts;
mod types;

pub use consts::*;
pub use types::*;

use std::time::Duration;

/// Backend-agnostic trait for smartcard access.
///
/// The client implements this against the real backend (pcsc-lite via the
/// `pcsc` crate on Linux/macOS, WinSCard on Windows); tests use a scripted
/// dummy. All methods are **synchronous**: the channel layer runs them on
/// dedicated threads, so implementations never see async.
///
/// Errors are raw SCARD codes (`u32`, see `consts::SCARD_*`) — that is what
/// the wire protocol carries.
///
/// Implementations must be `Send + Sync`: the same instance is shared by
/// the request-dispatch thread and the status-watch thread.
pub trait SmartcardIntegration: Send + Sync + std::fmt::Debug {
    // === Context management ===

    /// Establish a new resource manager context (`scope`: `SCARD_SCOPE_*`).
    fn establish_context(&self, scope: u32) -> Result<ScardContext, u32>;

    /// Release a previously established context, cancelling its watches
    /// and connections.
    fn release_context(&self, ctx: &ScardContext) -> Result<(), u32>;

    /// Whether a context handle is still valid.
    fn is_valid_context(&self, ctx: &ScardContext) -> bool;

    // === Reader discovery ===

    /// List all readers currently available (`groups` = `None` for all).
    fn list_readers(
        &self,
        ctx: &ScardContext,
        groups: Option<&[String]>,
    ) -> Result<Vec<String>, u32>;

    // === Card connection ===

    /// Connect to the card in `reader` (`SCARD_SHARE_*`,
    /// `SCARD_PROTOCOL_*` bitmask).
    fn connect(
        &self,
        ctx: &ScardContext,
        reader: &str,
        share_mode: u32,
        preferred_protocols: u32,
    ) -> Result<ConnectResult, u32>;

    /// Disconnect a card (`SCARD_LEAVE/RESET/UNPOWER/EJECT`).
    fn disconnect(&self, handle: &ScardHandle, disposition: u32) -> Result<(), u32>;

    /// Re-establish a previously connected card. Returns the active
    /// protocol.
    fn reconnect(
        &self,
        handle: &ScardHandle,
        share_mode: u32,
        preferred_protocols: u32,
        initialization: u32,
    ) -> Result<u32, u32>;

    // === Card communication ===

    /// Exchange one APDU. The response is returned **as-is** — callers
    /// (the remote minidriver) chain GET RESPONSE themselves.
    fn transmit(
        &self,
        handle: &ScardHandle,
        send_pci: &ScardIORequest,
        data: &[u8],
    ) -> Result<TransmitResult, u32>;

    /// Send a control command to the reader.
    fn control(
        &self,
        handle: &ScardHandle,
        control_code: u32,
        in_data: &[u8],
    ) -> Result<Vec<u8>, u32>;

    // === Status & state ===

    /// Query the status of a connected card.
    fn status(&self, handle: &ScardHandle) -> Result<ScardStatus, u32>;

    /// Block until one of the watched readers changes state or the timeout
    /// expires.
    ///
    /// Returns the new reader states plus the SCARD return code
    /// (`SCARD_S_SUCCESS` on change, `SCARD_E_TIMEOUT` on expiry). The
    /// states are populated **even on timeout** — an empty body makes
    /// SCardSvr re-enumerate in a flood (RDPESC lesson).
    fn get_status_change(
        &self,
        ctx: &ScardContext,
        timeout: Duration,
        reader_states: &[ReaderStateIn],
    ) -> Result<(Vec<ReaderStateOut>, u32), u32>;

    // === Transactions ===

    /// Begin an exclusive transaction on the card.
    fn begin_transaction(&self, handle: &ScardHandle) -> Result<(), u32>;

    /// End an exclusive transaction (`SCARD_*` disposition).
    fn end_transaction(&self, handle: &ScardHandle, disposition: u32) -> Result<(), u32>;

    // === Attributes ===

    /// Get a card/reader attribute.
    fn get_attrib(&self, handle: &ScardHandle, attr_id: u32) -> Result<Vec<u8>, u32>;

    /// Set a card/reader attribute.
    fn set_attrib(&self, handle: &ScardHandle, attr_id: u32, data: &[u8]) -> Result<(), u32>;

    // === Cancel ===

    /// Cancel all pending operations on the context (signal any blocked
    /// `get_status_change` to return immediately).
    fn cancel(&self, ctx: &ScardContext) -> Result<(), u32> {
        let _ = ctx;
        Ok(())
    }

    // === Meta ===

    /// Whether the smartcard subsystem is available and ready to serve.
    fn is_available(&self) -> bool;
}

