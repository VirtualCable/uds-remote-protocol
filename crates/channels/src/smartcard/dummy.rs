// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Scripted [`SmartcardIntegration`] for tests: one fixed reader, one fixed
//! ATR, connects always succeed, `transmit` answers a programmed map (or
//! echoes the APDU with `90 00` appended) and `get_status_change` parks
//! until its timeout (or a `cancel`) reporting unchanged states.
//!
//! Always compiled (no feature gate): it is tiny, has no dependencies and
//! is handy for any crate's integration tests.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use crate::smartcard::SmartcardIntegration;
use crate::smartcard::consts::{
    SCARD_E_CANCELLED, SCARD_E_TIMEOUT, SCARD_E_UNSUPPORTED_FEATURE, SCARD_PROTOCOL_T1,
    SCARD_STATE_PRESENT,
};
use crate::smartcard::types::{
    ConnectResult, ReaderStateIn, ReaderStateOut, ScardContext, ScardHandle, ScardIORequest,
    ScardStatus, TransmitResult,
};

/// Name of the single reader the dummy exposes.
pub const DUMMY_READER: &str = "UDS Test Reader 0";

/// ATR reported by the dummy (historical bytes spell "UDS").
pub const DUMMY_ATR: [u8; 8] = [0x3B, 0x85, 0x80, 0x01, 0x55, 0x44, 0x53, 0x00];

/// Poll cadence of the dummy's `get_status_change` wait loop.
const GSC_POLL: Duration = Duration::from_millis(10);

/// Scripted backend, see the module docs. `Send + Sync` via interior
/// mutability, like a real backend must be.
#[derive(Debug, Default)]
pub struct DummyBackend {
    contexts: Mutex<HashSet<u64>>,
    cancelled: Mutex<HashSet<u64>>,
    transmit_map: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
}

impl DummyBackend {
    /// Create a backend with an empty transmit map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Program the exact response `transmit` returns for `apdu` (including
    /// its status word — the reply goes out as-is, RDPESC lesson).
    pub fn program_transmit(&self, apdu: &[u8], response: &[u8]) {
        self.transmit_map
            .lock()
            .expect("transmit map poisoned")
            .insert(apdu.to_vec(), response.to_vec());
    }
}

impl SmartcardIntegration for DummyBackend {
    fn establish_context(&self, _scope: u32) -> Result<ScardContext, u32> {
        let ctx = ScardContext::new();
        self.contexts
            .lock()
            .expect("contexts poisoned")
            .insert(ctx.raw());
        Ok(ctx)
    }

    fn release_context(&self, ctx: &ScardContext) -> Result<(), u32> {
        self.contexts
            .lock()
            .expect("contexts poisoned")
            .remove(&ctx.raw());
        Ok(())
    }

    fn is_valid_context(&self, ctx: &ScardContext) -> bool {
        self.contexts
            .lock()
            .expect("contexts poisoned")
            .contains(&ctx.raw())
    }

    fn list_readers(
        &self,
        _ctx: &ScardContext,
        _groups: Option<&[String]>,
    ) -> Result<Vec<String>, u32> {
        Ok(vec![DUMMY_READER.to_string()])
    }

    fn connect(
        &self,
        _ctx: &ScardContext,
        _reader: &str,
        _share_mode: u32,
        _preferred_protocols: u32,
    ) -> Result<ConnectResult, u32> {
        Ok(ConnectResult {
            handle: ScardHandle::new(SCARD_PROTOCOL_T1),
            active_protocol: SCARD_PROTOCOL_T1,
        })
    }

    fn disconnect(&self, _handle: &ScardHandle, _disposition: u32) -> Result<(), u32> {
        Ok(())
    }

    fn reconnect(
        &self,
        handle: &ScardHandle,
        _share_mode: u32,
        _preferred_protocols: u32,
        _initialization: u32,
    ) -> Result<u32, u32> {
        Ok(handle.active_protocol())
    }

    fn transmit(
        &self,
        handle: &ScardHandle,
        _send_pci: &ScardIORequest,
        data: &[u8],
    ) -> Result<TransmitResult, u32> {
        let mapped = self
            .transmit_map
            .lock()
            .expect("transmit map poisoned")
            .get(data)
            .cloned();
        let recv_buffer = if let Some(response) = mapped {
            response
        } else {
            let mut echo = data.to_vec();
            echo.extend_from_slice(&[0x90, 0x00]);
            echo
        };
        Ok(TransmitResult {
            recv_pci: Some(ScardIORequest {
                protocol: handle.active_protocol(),
                extra_bytes: Vec::new(),
            }),
            recv_buffer,
        })
    }

    fn control(
        &self,
        _handle: &ScardHandle,
        _control_code: u32,
        _in_data: &[u8],
    ) -> Result<Vec<u8>, u32> {
        Ok(vec![0x90, 0x00])
    }

    fn status(&self, handle: &ScardHandle) -> Result<ScardStatus, u32> {
        Ok(ScardStatus {
            reader_names: vec![DUMMY_READER.to_string()],
            state: SCARD_STATE_PRESENT,
            protocol: handle.active_protocol(),
            atr: DUMMY_ATR.to_vec(),
        })
    }

    fn get_status_change(
        &self,
        ctx: &ScardContext,
        timeout: Duration,
        reader_states: &[ReaderStateIn],
    ) -> Result<(Vec<ReaderStateOut>, u32), u32> {
        // Park in small steps so a cancel is noticed promptly even with a
        // long timeout (the real backend is interrupted by SCardCancel).
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if self
                .cancelled
                .lock()
                .expect("cancelled poisoned")
                .remove(&ctx.raw())
            {
                return Err(SCARD_E_CANCELLED);
            }
            std::thread::sleep(GSC_POLL.min(timeout.saturating_sub(start.elapsed())));
        }
        let states = reader_states
            .iter()
            .map(|rs| ReaderStateOut {
                reader_name: rs.reader_name.clone(),
                current_state: rs.current_state,
                event_state: rs.current_state,
                atr: if rs.current_state & SCARD_STATE_PRESENT != 0 {
                    DUMMY_ATR.to_vec()
                } else {
                    Vec::new()
                },
            })
            .collect();
        Ok((states, SCARD_E_TIMEOUT))
    }

    fn begin_transaction(&self, _handle: &ScardHandle) -> Result<(), u32> {
        Ok(())
    }

    fn end_transaction(&self, _handle: &ScardHandle, _disposition: u32) -> Result<(), u32> {
        Ok(())
    }

    fn get_attrib(&self, _handle: &ScardHandle, _attr_id: u32) -> Result<Vec<u8>, u32> {
        Err(SCARD_E_UNSUPPORTED_FEATURE)
    }

    fn set_attrib(&self, _handle: &ScardHandle, _attr_id: u32, _data: &[u8]) -> Result<(), u32> {
        Ok(())
    }

    fn cancel(&self, ctx: &ScardContext) -> Result<(), u32> {
        self.cancelled
            .lock()
            .expect("cancelled poisoned")
            .insert(ctx.raw());
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }
}
