// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! The provider filter (`ICredentialProviderFilter`). While we hold a
//! pipe-delivered auto-logon credential, we filter every other provider out
//! so LogonUI cannot fall back to a different tile mid-attempt; otherwise we
//! leave the provider list untouched, so this CP coexists with the inbox
//! providers and with the UDS RDP credential provider (different CLSID).

use windows::{
    Win32::{
        Foundation::{E_INVALIDARG, E_NOTIMPL},
        UI::Shell::{
            CPUS_CHANGE_PASSWORD, CPUS_CREDUI, CPUS_LOGON, CPUS_UNLOCK_WORKSTATION,
            CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION, CREDENTIAL_PROVIDER_USAGE_SCENARIO,
            ICredentialProviderFilter, ICredentialProviderFilter_Impl,
        },
    },
    core::*,
};

use super::store;
use crate::imp::globals;
use crate::{debug_dev, debug_flow};

/// The credential provider filter COM object.
#[implement(ICredentialProviderFilter)]
pub struct UdsRpCredentialsFilter {}

impl UdsRpCredentialsFilter {
    /// Creates the filter.
    pub fn new() -> Self {
        debug_flow!("UdsRpCredentialsFilter::new");
        Self {}
    }

    fn filter(
        &self,
        cpus: CREDENTIAL_PROVIDER_USAGE_SCENARIO,
        dwflags: u32,
        rgclsidproviders: *const GUID,
        rgballow: *mut BOOL,
        cproviders: u32,
    ) -> windows::core::Result<()> {
        // The armed flag lives in the process-global store: the filter is a
        // separate COM object and each enumeration cycle creates fresh
        // provider instances.
        let auto_logon_armed = store::is_valid();
        debug_dev!(
            "Filter called: auto_logon_armed: {auto_logon_armed}, dwflags: {dwflags}, cpus: {cpus:?}"
        );

        match cpus {
            CPUS_LOGON | CPUS_UNLOCK_WORKSTATION => {
                if !auto_logon_armed {
                    debug_dev!("No pipe credentials; leaving the provider list untouched");
                    return Ok(());
                }
                // Auto-logon in flight: only our provider is allowed
                let clsids =
                    unsafe { std::slice::from_raw_parts(rgclsidproviders, cproviders as usize) };
                let allows = unsafe { std::slice::from_raw_parts_mut(rgballow, cproviders as usize) };
                for (clsid, allow) in clsids.iter().zip(allows.iter_mut()) {
                    *allow = (*clsid == globals::CLSID_UDS_RP_CREDENTIAL_PROVIDER).into();
                    debug_dev!("Filter: provider: {clsid:?}, allow: {}", allow.as_bool());
                }
            }
            CPUS_CREDUI | CPUS_CHANGE_PASSWORD => {
                return Err(E_NOTIMPL.into());
            }
            _ => {
                return Err(E_INVALIDARG.into());
            }
        }

        Ok(())
    }
}

impl Default for UdsRpCredentialsFilter {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(non_snake_case)]
impl ICredentialProviderFilter_Impl for UdsRpCredentialsFilter_Impl {
    fn Filter(
        &self,
        cpus: CREDENTIAL_PROVIDER_USAGE_SCENARIO,
        dwflags: u32,
        rgclsidproviders: *const GUID,
        rgballow: *mut BOOL,
        cproviders: u32,
    ) -> windows::core::Result<()> {
        debug_flow!(
            "ICredentialProviderFilter::Filter({cpus:?}, {dwflags}, {rgclsidproviders:?}, {rgballow:?}, {cproviders})"
        );

        self.filter(cpus, dwflags, rgclsidproviders, rgballow, cproviders)
    }

    /// NLA pre-serialized credentials are not part of our flow (credentials
    /// arrive over the pipe), so this is deliberately unimplemented.
    fn UpdateRemoteCredential(
        &self,
        pcpcsin: *const CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION,
        pcpcsout: *mut CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION,
    ) -> windows::core::Result<()> {
        debug_flow!("ICredentialProviderFilter::UpdateRemoteCredential({pcpcsin:?}, {pcpcsout:?})");
        Err(E_NOTIMPL.into())
    }
}
