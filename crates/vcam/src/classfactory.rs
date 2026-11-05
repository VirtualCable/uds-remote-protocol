// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! COM class factory for the UDS virtual camera filter.

use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_INVALIDARG};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::core::*;

use crate::filter::UdsCamFilter;
use crate::globals::{dll_add_ref, dll_release};

/// `IClassFactory` for the vcam filter.
#[implement(IClassFactory)]
pub struct ClassFactory;

impl ClassFactory {
    /// Creates the class factory.
    pub fn new() -> Self {
        dll_add_ref();
        Self
    }
}

impl Drop for ClassFactory {
    fn drop(&mut self) {
        dll_release();
    }
}

#[allow(non_snake_case)]
impl IClassFactory_Impl for ClassFactory_Impl {
    #[allow(clippy::not_unsafe_ptr_arg_deref)] // COM needs the signature as is
    fn CreateInstance(
        &self,
        punkouter: Ref<'_, IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        if !punkouter.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        if ppvobject.is_null() || riid.is_null() {
            return Err(E_INVALIDARG.into());
        }
        unsafe {
            *ppvobject = core::ptr::null_mut();
            let (filter, shared) = UdsCamFilter::create();
            *shared.filter_this.lock().unwrap() = Some(filter.clone());
            let unknown: IUnknown = filter.cast()?;
            let hr = unknown.query(riid, ppvobject);
            if hr.is_ok() { Ok(()) } else { Err(hr.into()) }
        }
    }

    fn LockServer(&self, f_lock: BOOL) -> Result<()> {
        if f_lock.as_bool() {
            dll_add_ref();
        } else {
            dll_release();
        }
        Ok(())
    }
}
