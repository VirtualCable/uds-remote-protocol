// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Real PC/SC backend: implements [`SmartcardIntegration`] against the
//! platform smartcard stack (pcsc-lite on Linux/macOS, WinSCard on
//! Windows) via the `pcsc` crate.
//!
//! Ported from uds-client's `channels/src/smartcard/native/`, dropping the
//! emulated-card helpers (container info / certificate caches) — phase 1
//! relays physical cards only, and the remote minidriver populates its own
//! cache via TRANSMIT.

mod helpers;
mod registry;

use std::time::Duration;

use helpers::{
    ERROR_NOT_SUPPORTED, dword_to_attribute, dword_to_disposition, dword_to_protocols,
    dword_to_share_mode, dword_to_state, pcsc_error_to_u32, pcsc_status_to_u32, protocol_to_u32,
    system_attribute,
};
use registry::NativeRegistry;

use crate::smartcard::{
    SCARD_E_INVALID_HANDLE, SCARD_E_INVALID_PARAMETER, SCARD_E_TIMEOUT, SCARD_S_SUCCESS,
    SCARD_TRANSMIT_MAX,
};
use crate::smartcard::{
    ConnectResult, ReaderStateIn, ReaderStateOut, ScardContext, ScardHandle, ScardIORequest,
    ScardStatus, TransmitResult,
};
use crate::smartcard::SmartcardIntegration;

/// [`SmartcardIntegration`] backed by the real PC/SC subsystem.
#[derive(Debug)]
pub struct PcscBackend {
    registry: NativeRegistry,
}

impl Default for PcscBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PcscBackend {
    /// Create a backend with an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: NativeRegistry::new(),
        }
    }
}

impl SmartcardIntegration for PcscBackend {
    fn establish_context(&self, _scope: u32) -> Result<ScardContext, u32> {
        let pcsc_ctx = pcsc::Context::establish(pcsc::Scope::System).map_err(pcsc_error_to_u32)?;
        let sc_ctx = ScardContext::new();
        self.registry
            .contexts
            .write()
            .unwrap()
            .insert(sc_ctx.raw(), pcsc_ctx);
        Ok(sc_ctx)
    }

    fn release_context(&self, ctx: &ScardContext) -> Result<(), u32> {
        let card_id = self.registry.ctx_cards.write().unwrap().remove(&ctx.raw());

        if let Some(card_id) = card_id
            && let Some((card, _)) = self.registry.cards.write().unwrap().remove(&card_id)
        {
            let _ = card.disconnect(pcsc::Disposition::LeaveCard);
        }

        self.registry
            .contexts
            .write()
            .unwrap()
            .remove(&ctx.raw())
            .ok_or(SCARD_E_INVALID_HANDLE)?;
        Ok(())
    }

    fn is_valid_context(&self, ctx: &ScardContext) -> bool {
        self.registry.contexts.read().unwrap().contains_key(&ctx.raw())
    }

    fn list_readers(
        &self,
        ctx: &ScardContext,
        _groups: Option<&[String]>,
    ) -> Result<Vec<String>, u32> {
        let contexts = self.registry.contexts.read().unwrap();
        let pcsc_ctx = contexts.get(&ctx.raw()).ok_or(SCARD_E_INVALID_HANDLE)?;

        let mut readers_buf = [0u8; 4096];
        let readers = match pcsc_ctx.list_readers(&mut readers_buf) {
            Ok(r) => r,
            Err(pcsc::Error::NoReadersAvailable) => return Ok(Vec::new()),
            Err(e) => return Err(pcsc_error_to_u32(e)),
        };

        let mut result = Vec::new();
        for reader in readers {
            if let Ok(s) = reader.to_str() {
                result.push(s.to_string());
            }
        }
        Ok(result)
    }

    fn connect(
        &self,
        ctx: &ScardContext,
        reader: &str,
        share_mode: u32,
        preferred_protocols: u32,
    ) -> Result<ConnectResult, u32> {
        let contexts = self.registry.contexts.read().unwrap();
        let pcsc_ctx = contexts.get(&ctx.raw()).ok_or(SCARD_E_INVALID_HANDLE)?;

        let share = dword_to_share_mode(share_mode)?;
        let protos = dword_to_protocols(preferred_protocols);

        let reader_c = std::ffi::CString::new(reader).map_err(|_| SCARD_E_INVALID_PARAMETER)?;
        let card = pcsc_ctx
            .connect(&reader_c, share, protos)
            .map_err(pcsc_error_to_u32)?;

        let mut r_buf = [0u8; 256];
        let mut a_buf = [0u8; 36];
        let active_proto = match card.status2(&mut r_buf, &mut a_buf) {
            Ok(status) => protocol_to_u32(status.protocol()),
            Err(err) => {
                let code = pcsc_error_to_u32(err);
                let _ = card.disconnect(pcsc::Disposition::LeaveCard);
                return Err(code);
            }
        };

        let handle = ScardHandle::new(active_proto);
        self.registry
            .cards
            .write()
            .unwrap()
            .insert(handle.raw(), (card, reader.to_string()));
        self.registry
            .ctx_cards
            .write()
            .unwrap()
            .insert(ctx.raw(), handle.raw());

        Ok(ConnectResult {
            handle,
            active_protocol: active_proto,
        })
    }

    fn disconnect(&self, handle: &ScardHandle, disposition: u32) -> Result<(), u32> {
        let disp = dword_to_disposition(disposition)?;
        let mut cards = self.registry.cards.write().unwrap();
        if let Some((card, _)) = cards.remove(&handle.raw()) {
            self.registry
                .ctx_cards
                .write()
                .unwrap()
                .retain(|_, card_id| *card_id != handle.raw());
            card.disconnect(disp)
                .map_err(|(_, err)| pcsc_error_to_u32(err))?;
            Ok(())
        } else {
            Err(SCARD_E_INVALID_HANDLE)
        }
    }

    fn reconnect(
        &self,
        handle: &ScardHandle,
        share_mode: u32,
        preferred_protocols: u32,
        initialization: u32,
    ) -> Result<u32, u32> {
        let share = dword_to_share_mode(share_mode)?;
        let protos = dword_to_protocols(preferred_protocols);
        let init = dword_to_disposition(initialization)?;

        let mut cards = self.registry.cards.write().unwrap();
        let (card, _) = cards.get_mut(&handle.raw()).ok_or(SCARD_E_INVALID_HANDLE)?;

        card.reconnect(share, protos, init)
            .map_err(pcsc_error_to_u32)?;

        let mut r_buf = [0u8; 256];
        let mut a_buf = [0u8; 36];
        card.status2(&mut r_buf, &mut a_buf)
            .map(|status| protocol_to_u32(status.protocol()))
            .map_err(pcsc_error_to_u32)
    }

    fn transmit(
        &self,
        handle: &ScardHandle,
        _send_pci: &ScardIORequest,
        data: &[u8],
    ) -> Result<TransmitResult, u32> {
        let cards = self.registry.cards.read().unwrap();
        let (card, _) = cards.get(&handle.raw()).ok_or(SCARD_E_INVALID_HANDLE)?;

        let mut recv_buf = vec![0u8; SCARD_TRANSMIT_MAX];
        let resp_slice = card
            .transmit(data, &mut recv_buf)
            .map_err(pcsc_error_to_u32)?;

        let recv_buffer = resp_slice.to_vec();
        let mut r_buf = [0u8; 256];
        let mut a_buf = [0u8; 36];
        let recv_pci = card
            .status2(&mut r_buf, &mut a_buf)
            .ok()
            .and_then(|status| status.protocol2())
            .map(|protocol| ScardIORequest {
                protocol: protocol_to_u32(protocol),
                extra_bytes: Vec::new(),
            });

        Ok(TransmitResult {
            recv_pci,
            recv_buffer,
        })
    }

    fn control(
        &self,
        handle: &ScardHandle,
        control_code: u32,
        in_data: &[u8],
    ) -> Result<Vec<u8>, u32> {
        let cards = self.registry.cards.read().unwrap();
        let (card, _) = cards.get(&handle.raw()).ok_or(SCARD_E_INVALID_HANDLE)?;

        let mut out_buf = vec![0u8; 4096];
        let resp_slice = card
            .control(control_code.into(), in_data, &mut out_buf)
            .map_err(pcsc_error_to_u32)?;

        Ok(resp_slice.to_vec())
    }

    fn status(&self, handle: &ScardHandle) -> Result<ScardStatus, u32> {
        let cards = self.registry.cards.read().unwrap();
        let (card, reader_name) = cards.get(&handle.raw()).ok_or(SCARD_E_INVALID_HANDLE)?;

        let mut r_buf = [0u8; 256];
        let mut a_buf = [0u8; 36];
        let status = card
            .status2(&mut r_buf, &mut a_buf)
            .map_err(pcsc_error_to_u32)?;

        let result = ScardStatus {
            reader_names: vec![reader_name.clone()],
            state: pcsc_status_to_u32(status.status()),
            protocol: protocol_to_u32(status.protocol()),
            atr: status.atr().to_vec(),
        };
        tracing::trace!(
            "pcsc backend: status 0x{:X}: pcsc={:?} -> state={} protocol=0x{:X}",
            handle.raw(),
            status.status(),
            result.state,
            result.protocol
        );
        Ok(result)
    }

    fn get_status_change(
        &self,
        ctx: &ScardContext,
        timeout: Duration,
        reader_states: &[ReaderStateIn],
    ) -> Result<(Vec<ReaderStateOut>, u32), u32> {
        let mut pcsc_states = Vec::new();
        for rs in reader_states {
            let cstr = std::ffi::CString::new(rs.reader_name.as_str())
                .map_err(|_| SCARD_E_INVALID_PARAMETER)?;
            pcsc_states.push(pcsc::ReaderState::new(cstr, dword_to_state(rs.current_state)));
        }

        let pcsc_ctx = self
            .registry
            .contexts
            .read()
            .unwrap()
            .get(&ctx.raw())
            .ok_or(SCARD_E_INVALID_HANDLE)?
            .clone();

        let gsc_result = pcsc_ctx.get_status_change(timeout, &mut pcsc_states);

        let mut results = Vec::new();
        for (i, rs) in reader_states.iter().enumerate() {
            let out_state = &pcsc_states[i];
            // Read the raw SCARD_READERSTATE: the pcsc crate's event_state()
            // uses State::from_bits_truncate which drops the 0x0001_0000 bit
            // that Windows sets in reader states (e.g. for the
            // \\?PnP?\Notification pseudo-reader). The remote SCardSvr
            // expects these bits verbatim.
            #[expect(unsafe_code, reason = "no safe accessor for the raw dwEventState")]
            let raw: &pcsc::ffi::SCARD_READERSTATE =
                unsafe { &*std::ptr::from_ref(out_state).cast() };
            let atr_len = usize::try_from(raw.cbAtr).unwrap_or(0).min(raw.rgbAtr.len());
            results.push(ReaderStateOut {
                reader_name: rs.reader_name.clone(),
                current_state: rs.current_state,
                event_state: u32::try_from(raw.dwEventState).unwrap_or(u32::MAX),
                atr: raw.rgbAtr[..atr_len].to_vec(),
            });
        }

        match gsc_result {
            Ok(()) => Ok((results, SCARD_S_SUCCESS)),
            Err(pcsc::Error::Timeout) => Ok((results, SCARD_E_TIMEOUT)),
            Err(e) => Err(pcsc_error_to_u32(e)),
        }
    }

    fn cancel(&self, ctx: &ScardContext) -> Result<(), u32> {
        let contexts = self.registry.contexts.read().unwrap();
        let pcsc_ctx = contexts.get(&ctx.raw()).ok_or(SCARD_E_INVALID_HANDLE)?;
        pcsc_ctx.cancel().map_err(pcsc_error_to_u32)
    }

    fn begin_transaction(&self, _handle: &ScardHandle) -> Result<(), u32> {
        // Transactions are serialized by the dispatch thread (see
        // TransactionGate) and need no locks at the local PC/SC layer.
        Ok(())
    }

    fn end_transaction(&self, _handle: &ScardHandle, _disposition: u32) -> Result<(), u32> {
        // See begin_transaction.
        Ok(())
    }

    fn get_attrib(&self, handle: &ScardHandle, attr_id: u32) -> Result<Vec<u8>, u32> {
        let cards = self.registry.cards.read().unwrap();
        let Some((card, reader_name)) = cards.get(&handle.raw()) else {
            tracing::trace!(
                "pcsc backend: get_attrib 0x{attr_id:X}: unknown handle 0x{:X}",
                handle.raw()
            );
            return Err(SCARD_E_INVALID_HANDLE);
        };

        if let Some(value) = system_attribute(attr_id, reader_name) {
            return Ok(value);
        }
        let attribute = dword_to_attribute(attr_id)?;

        let mut buf = vec![0u8; 1024];
        // WinSCard reports a missing attribute as ERROR_NOT_SUPPORTED, a
        // Win32 code the pcsc crate can only surface as UnknownError.
        card.get_attribute(attribute, &mut buf)
            .map(<[u8]>::to_vec)
            .map_err(|err| match err {
                pcsc::Error::UnknownError => ERROR_NOT_SUPPORTED,
                other => pcsc_error_to_u32(other),
            })
    }

    fn set_attrib(&self, handle: &ScardHandle, attr_id: u32, data: &[u8]) -> Result<(), u32> {
        let attribute = dword_to_attribute(attr_id)?;
        let cards = self.registry.cards.read().unwrap();
        let (card, _) = cards.get(&handle.raw()).ok_or(SCARD_E_INVALID_HANDLE)?;
        card.set_attribute(attribute, data)
            .map_err(pcsc_error_to_u32)?;
        Ok(())
    }

    fn is_available(&self) -> bool {
        pcsc::Context::establish(pcsc::Scope::System).is_ok()
    }
}
