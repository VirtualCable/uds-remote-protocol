// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! The DirectShow source filter itself (`IBaseFilter` + `IAMFilterMiscFlags`)
//! with a single output pin ([`UdsCamPin`]).

use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{E_POINTER, S_FALSE, S_OK};
use windows::Win32::Media::DirectShow::{
    AM_FILTER_MISC_FLAGS_IS_SOURCE, FILTER_INFO, FILTER_STATE, IBaseFilter, IBaseFilter_Impl,
    IEnumPins, IEnumPins_Impl, IFilterGraph, IMediaFilter_Impl, IPin,
    IAMFilterMiscFlags, IAMFilterMiscFlags_Impl, State_Paused, State_Running, State_Stopped,
};
use windows::Win32::Media::IReferenceClock;
use windows::Win32::System::Com::{CoTaskMemAlloc, IPersist_Impl};
use windows::core::*;

use crate::globals;
use crate::pin::UdsCamPin;
use crate::state::Shared;

/// UDS virtual camera source filter.
#[implement(IBaseFilter, IAMFilterMiscFlags)]
pub struct UdsCamFilter {
    shared: Arc<Shared>,
}

impl UdsCamFilter {
    /// Creates the filter and its output pin, wiring both through the
    /// shared state. Returns the `IBaseFilter` plus the shared `Arc` so the
    /// class factory can complete the wiring (`filter_this`).
    pub fn create() -> (IBaseFilter, Arc<Shared>) {
        // Keep the DLL loaded while any filter instance is alive:
        // DllCanUnloadNow must not report S_OK until every object is gone.
        globals::dll_add_ref();
        let shared = Shared::new();
        let pin: IPin = UdsCamPin::new(shared.clone()).into();
        shared.pin.lock().unwrap().this_pin = Some(pin);
        let filter = Self { shared };
        let shared = filter.shared.clone();
        (filter.into(), shared)
    }
}

impl Drop for UdsCamFilter {
    fn drop(&mut self) {
        // Break the construction-time cycle: the pin state holds the pin's
        // own IPin; dropping the filter releases it. The pin clears
        // `filter_this` on its own Drop.
        self.shared.pin.lock().unwrap().this_pin = None;
        globals::dll_release();
    }
}

#[allow(non_snake_case)]
impl IPersist_Impl for UdsCamFilter_Impl {
    fn GetClassID(&self) -> Result<GUID> {
        Ok(globals::CLSID_UDS_VCAM)
    }
}

#[allow(non_snake_case)]
impl IMediaFilter_Impl for UdsCamFilter_Impl {
    fn Stop(&self) -> Result<()> {
        // After DLL_PROCESS_DETACH the host is exiting: touching COM objects
        // owned by the dying process (allocator, downstream pin) crashes it.
        if globals::is_detached() {
            return Ok(());
        }
        crate::vlog!("filter Stop");
        UdsCamPin::stop_streaming(&self.shared);
        *self.shared.filter_state.lock().unwrap() = State_Stopped;
        Ok(())
    }

    fn Pause(&self) -> Result<()> {
        if globals::is_detached() {
            return Ok(());
        }
        crate::vlog!("filter Pause");
        UdsCamPin::start_streaming(&self.shared);
        *self.shared.filter_state.lock().unwrap() = State_Paused;
        Ok(())
    }

    fn Run(&self, _tstart: i64) -> Result<()> {
        if globals::is_detached() {
            return Ok(());
        }
        crate::vlog!("filter Run");
        UdsCamPin::start_streaming(&self.shared);
        *self.shared.filter_state.lock().unwrap() = State_Running;
        Ok(())
    }

    fn GetState(&self, _dwmillisecstimeout: u32) -> Result<FILTER_STATE> {
        if globals::is_detached() {
            return Ok(State_Stopped);
        }
        Ok(*self.shared.filter_state.lock().unwrap())
    }

    fn SetSyncSource(&self, _pclock: Ref<IReferenceClock>) -> Result<()> {
        // Live push source: no reference clock needed (we timestamp from
        // our own frame counter).
        Ok(())
    }

    fn GetSyncSource(&self) -> Result<IReferenceClock> {
        Err(windows::Win32::Foundation::E_NOINTERFACE.into())
    }
}

#[allow(non_snake_case)]
impl IBaseFilter_Impl for UdsCamFilter_Impl {
    fn EnumPins(&self) -> Result<IEnumPins> {
        let pin = self.shared.pin.lock().unwrap().this_pin.clone();
        Ok(UdsEnumPins { pin, index: Mutex::new(0) }.into())
    }

    fn FindPin(&self, id: &PCWSTR) -> Result<IPin> {
        let id_str = unsafe { id.to_string().unwrap_or_default() };
        if id_str == "Output" || id_str == "UDS Cam Out" {
            self.shared
                .pin
                .lock()
                .unwrap()
                .this_pin
                .clone()
                .ok_or_else(|| windows::Win32::Foundation::E_UNEXPECTED.into())
        } else {
            Err(windows::Win32::Media::DirectShow::VFW_E_NOT_FOUND.into())
        }
    }

    fn QueryFilterInfo(&self, pinfo: *mut FILTER_INFO) -> Result<()> {
        if pinfo.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            let info = &mut *pinfo;
            let name: Vec<u16> = format!("{}\0", self.shared.name.lock().unwrap())
                .encode_utf16()
                .collect();
            let n = name.len().min(127);
            info.achName[..n].copy_from_slice(&name[..n]);
            info.pGraph = std::mem::ManuallyDrop::new(self.shared.graph.lock().unwrap().clone());
        }
        Ok(())
    }

    fn JoinFilterGraph(&self, pgraph: Ref<IFilterGraph>, pname: &PCWSTR) -> Result<()> {
        crate::vlog!("filter JoinFilterGraph");
        *self.shared.graph.lock().unwrap() = pgraph.as_ref().cloned();
        if !pname.is_null()
            && let Ok(name) = unsafe { pname.to_string() }
        {
            *self.shared.name.lock().unwrap() = name;
        }
        Ok(())
    }

    fn QueryVendorInfo(&self) -> Result<PWSTR> {
        let text: Vec<u16> = "Virtual Cable S.L.\0".encode_utf16().collect();
        unsafe {
            let buf = CoTaskMemAlloc(text.len() * 2).cast::<u16>();
            if buf.is_null() {
                return Err(windows::Win32::Foundation::E_UNEXPECTED.into());
            }
            std::ptr::copy_nonoverlapping(text.as_ptr(), buf, text.len());
            Ok(PWSTR(buf))
        }
    }
}

impl IAMFilterMiscFlags_Impl for UdsCamFilter_Impl {
    fn GetMiscFlags(&self) -> u32 {
        AM_FILTER_MISC_FLAGS_IS_SOURCE.0 as u32
    }
}

/// `IEnumPins` over our single output pin.
#[implement(IEnumPins)]
struct UdsEnumPins {
    pin: Option<IPin>,
    index: Mutex<usize>,
}

#[allow(non_snake_case)]
impl IEnumPins_Impl for UdsEnumPins_Impl {
    fn Next(&self, cpins: u32, pppins: *mut Option<IPin>, pcfetched: *mut u32) -> HRESULT {
        if pppins.is_null() {
            return E_POINTER;
        }
        let mut fetched = 0u32;
        {
            let mut index = self.index.lock().unwrap();
            while fetched < cpins && *index == 0 {
                if let Some(pin) = &self.pin {
                    unsafe { *pppins.add(fetched as usize) = Some(pin.clone()) };
                    fetched += 1;
                }
                *index += 1;
            }
        }
        if !pcfetched.is_null() {
            unsafe { *pcfetched = fetched };
        }
        if fetched == cpins { S_OK } else { S_FALSE }
    }

    fn Skip(&self, cpins: u32) -> Result<()> {
        let mut index = self.index.lock().unwrap();
        *index = (*index + cpins as usize).min(1);
        if cpins > 0 && *index >= 1 {
            return Err(S_FALSE.into());
        }
        Ok(())
    }

    fn Reset(&self) -> Result<()> {
        *self.index.lock().unwrap() = 0;
        Ok(())
    }

    fn Clone(&self) -> Result<IEnumPins> {
        Ok(UdsEnumPins { pin: self.pin.clone(), index: Mutex::new(*self.index.lock().unwrap()) }.into())
    }
}
