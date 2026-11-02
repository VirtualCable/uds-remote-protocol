// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! COM class factory: creates the provider and filter objects.

use windows::{
    Win32::Foundation::{CLASS_E_NOAGGREGATION, E_INVALIDARG, E_NOINTERFACE},
    Win32::System::Com::{IClassFactory, IClassFactory_Impl},
    Win32::UI::Shell::{ICredentialProvider, ICredentialProviderFilter},
    core::*,
};

use crate::debug_flow;
use crate::imp::credentials::filter::UdsRpCredentialsFilter;
use crate::imp::credentials::provider::UdsRpCredentialsProvider;
use crate::imp::globals::{dll_add_ref, dll_release};

/// `IClassFactory` for this DLL's COM objects.
#[implement(IClassFactory)]
pub struct ClassFactory;

impl ClassFactory {
    /// Creates the class factory.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ClassFactory {
    fn default() -> Self {
        Self::new()
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
        debug_flow!("ClassFactory::CreateInstance called");
        // Aggregation not supported
        if !punkouter.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        if ppvobject.is_null() || riid.is_null() {
            return Err(E_INVALIDARG.into());
        }
        unsafe {
            *ppvobject = core::ptr::null_mut();

            match *riid {
                ICredentialProvider::IID => {
                    let provider: ICredentialProvider = UdsRpCredentialsProvider::new().into();
                    // into_raw transfers ownership of the heap object to COM
                    *ppvobject = provider.into_raw();
                    Ok(())
                }
                ICredentialProviderFilter::IID => {
                    let filter: ICredentialProviderFilter = UdsRpCredentialsFilter::new().into();
                    *ppvobject = filter.into_raw();
                    Ok(())
                }
                _ => Err(E_NOINTERFACE.into()),
            }
        }
    }

    fn LockServer(&self, f_lock: BOOL) -> Result<()> {
        debug_flow!("ClassFactory::LockServer called with {f_lock:?}");
        if f_lock.as_bool() {
            dll_add_ref();
        } else {
            dll_release();
        }
        Ok(())
    }
}
