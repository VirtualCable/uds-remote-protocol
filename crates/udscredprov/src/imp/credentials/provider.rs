// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! The provider COM object (`ICredentialProvider` +
//! `ICredentialProviderSetUserArray`): owns the credential tile and the
//! LogonUI event manager. The pipe client is process-global (see
//! `crate::imp::pipe`).

use std::sync::{Arc, RwLock};

use windows::{
    Win32::{
        Foundation::{E_INVALIDARG, E_NOTIMPL},
        UI::Shell::{
            CPUS_CHANGE_PASSWORD, CPUS_CREDUI, CPUS_LOGON, CPUS_PLAP, CPUS_UNLOCK_WORKSTATION,
            CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION, CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR,
            CREDENTIAL_PROVIDER_NO_DEFAULT, CREDENTIAL_PROVIDER_USAGE_SCENARIO,
            ICredentialProvider, ICredentialProvider_Impl, ICredentialProviderCredential,
            ICredentialProviderEvents, ICredentialProviderSetUserArray,
            ICredentialProviderSetUserArray_Impl, ICredentialProviderUserArray,
        },
    },
    core::*,
};

use super::{
    credential::UdsRpCredential, fields::CREDENTIAL_PROVIDER_FIELD_DESCRIPTORS, store,
    types::UdsFieldId,
};
use crate::imp::pipe;
use crate::imp::utils::com;
use crate::imp::utils::log::{self, error};
use crate::{debug_dev, debug_flow};

/// GIT cookie and advise context of the most recently Advise'd provider
/// instance. The pipe callback must be able to fire `CredentialsChanged`
/// even when the instance that received the credentials is not the one
/// LogonUI Advise'd (each enumeration cycle creates fresh instances).
static LAST_EVENTS: RwLock<Option<(u32, usize)>> = RwLock::new(None);

/// The credential provider COM object.
#[implement(ICredentialProvider, ICredentialProviderSetUserArray)]
#[derive(Clone)]
pub struct UdsRpCredentialsProvider {
    credential: Arc<RwLock<UdsRpCredential>>,
    cookie: Arc<RwLock<Option<u32>>>,
    up_advise_context: Arc<RwLock<usize>>,
}

impl UdsRpCredentialsProvider {
    /// Creates the provider and ensures the process-global pipe client
    /// thread is running.
    pub fn new() -> Self {
        log::reset_flow_counter();

        debug_flow!("UdsRpCredentialsProvider::new");
        pipe::ensure_started();
        Self {
            credential: Arc::new(RwLock::new(UdsRpCredential::new(
                pipe::shared_pipe().clone(),
            ))),
            cookie: Arc::new(RwLock::new(None)),
            up_advise_context: Arc::new(RwLock::new(0)),
        }
    }

    /// Fires `CredentialsChanged` so LogonUI re-enumerates (auto-logon
    /// trigger). See [`fire_credentials_changed`].
    fn set_usage_scenario(
        &self,
        cpus: CREDENTIAL_PROVIDER_USAGE_SCENARIO,
    ) -> windows::core::Result<()> {
        match cpus {
            CPUS_LOGON | CPUS_UNLOCK_WORKSTATION => {
                // Unlike the reference provider, we do NOT reset the stored
                // credential here: ours arrive asynchronously over the pipe,
                // and LogonUI re-runs SetUsageScenario when re-enumerating
                // after our CredentialsChanged — resetting would wipe the
                // just-delivered credentials before the auto-logon attempt.
                self.credential.write().unwrap().set_usage_scenario(cpus);
                Ok(())
            }
            CPUS_CREDUI | CPUS_CHANGE_PASSWORD | CPUS_PLAP => Err(E_NOTIMPL.into()),
            _ => Err(E_INVALIDARG.into()),
        }
    }

    // Pre-serialized credentials (e.g. from an RDP redirect) are not part of
    // our flow: credentials always arrive over the pipe. Left as a stub like
    // the reference does.
    #[allow(clippy::not_unsafe_ptr_arg_deref)] // COM needs the signature as is
    fn unserialize(&self, _pcpcs: *const CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION) {}

    fn register_event_manager(
        &self,
        pcpe: ICredentialProviderEvents,
        upadvisecontext: usize,
    ) -> windows::core::Result<()> {
        let cookie = com::register_in_git(pcpe)?;
        *self.cookie.write().unwrap() = Some(cookie);
        *self.up_advise_context.write().unwrap() = upadvisecontext;
        *LAST_EVENTS.write().unwrap() = Some((cookie, upadvisecontext));
        Ok(())
    }

    fn unregister_event_manager(&self) -> windows::core::Result<()> {
        let cookie_opt = *self.cookie.read().unwrap();
        if let Some(cookie) = cookie_opt {
            com::unregister_from_git(cookie)?;
            *self.cookie.write().unwrap() = None;
            *self.up_advise_context.write().unwrap() = 0;
            let mut last = LAST_EVENTS.write().unwrap();
            if last.is_some_and(|(c, _)| c == cookie) {
                *last = None;
            }
        }
        Ok(())
    }

    fn get_credential_count(&self) -> (u32, u32, BOOL) {
        // With pipe-delivered credentials we are the default tile and ask
        // LogonUI for an immediate auto-logon (no UI shown). Without them we
        // behave as a normal interactive tile alongside the other providers.
        let has_valid_creds = store::is_valid();
        debug_dev!("get_credential_count: has_valid_creds: {has_valid_creds}");

        let pdwcount = 1; // 0 would hide our tile entirely
        let (pwdefault, pwautologonwithdefault) = if has_valid_creds {
            (0, true.into())
        } else {
            (CREDENTIAL_PROVIDER_NO_DEFAULT, false.into())
        };
        (pdwcount, pwdefault, pwautologonwithdefault)
    }

    fn get_credential_at(
        &self,
        index: u32,
    ) -> windows::core::Result<ICredentialProviderCredential> {
        if index == 0 {
            Ok(self.credential.read().unwrap().clone().into())
        } else {
            Err(windows::core::Error::from_hresult(E_INVALIDARG))
        }
    }
}

impl Default for UdsRpCredentialsProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Fires `CredentialsChanged` through the most recently Advise'd provider
/// instance, so LogonUI re-enumerates (auto-logon trigger). No-op when
/// LogonUI has not Advise'd any instance yet.
pub fn fire_credentials_changed() -> windows::core::Result<()> {
    if let Some((cookie, advise_context)) = *LAST_EVENTS.read().unwrap() {
        let event_manager: ICredentialProviderEvents = com::get_from_git(cookie)?;
        unsafe { event_manager.CredentialsChanged(advise_context)? };
    }
    Ok(())
}

impl Drop for UdsRpCredentialsProvider {
    fn drop(&mut self) {
        let cookie_opt = *self.cookie.read().unwrap();
        if let Some(cookie) = cookie_opt
            && let Err(e) = com::unregister_from_git(cookie)
        {
            error!("Failed to unregister from GIT: {e:?}");
        }
    }
}

#[allow(non_snake_case)]
impl ICredentialProvider_Impl for UdsRpCredentialsProvider_Impl {
    fn SetUsageScenario(
        &self,
        cpus: CREDENTIAL_PROVIDER_USAGE_SCENARIO,
        dwflags: u32,
    ) -> windows::core::Result<()> {
        debug_flow!("ICredentialProvider::SetUsageScenario({cpus:?}, {dwflags})");
        self.set_usage_scenario(cpus)
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)] // COM needs the signature as is
    fn SetSerialization(
        &self,
        pcpcs: *const CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION,
    ) -> windows::core::Result<()> {
        debug_flow!("ICredentialProvider::SetSerialization({pcpcs:?})");
        self.unserialize(pcpcs);
        Ok(())
    }

    fn Advise(
        &self,
        pcpe: windows::core::Ref<'_, ICredentialProviderEvents>,
        upadvisecontext: usize,
    ) -> windows::core::Result<()> {
        debug_flow!(
            "ICredentialProvider::Advise({:?}, {upadvisecontext:?})",
            pcpe.unwrap()
        );
        self.register_event_manager(pcpe.unwrap().clone(), upadvisecontext)
    }

    fn UnAdvise(&self) -> windows::core::Result<()> {
        debug_flow!("ICredentialProvider::UnAdvise()");
        self.unregister_event_manager()
    }

    fn GetFieldDescriptorCount(&self) -> windows::core::Result<u32> {
        debug_flow!("ICredentialProvider::GetFieldDescriptorCount()");
        Ok(UdsFieldId::NumFields as u32)
    }

    fn GetFieldDescriptorAt(
        &self,
        dwindex: u32,
    ) -> windows::core::Result<*mut CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR> {
        debug_flow!("ICredentialProvider::GetFieldDescriptorAt({dwindex})");
        if dwindex >= UdsFieldId::NumFields as u32 {
            return Err(windows::core::Error::from_hresult(E_INVALIDARG));
        }
        CREDENTIAL_PROVIDER_FIELD_DESCRIPTORS[dwindex as usize].into_com_alloc()
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)] // COM needs the signature as is
    fn GetCredentialCount(
        &self,
        pdwcount: *mut u32,
        pdwdefault: *mut u32,
        pbautologonwithdefault: *mut BOOL,
    ) -> windows::core::Result<()> {
        debug_flow!(
            "ICredentialProvider::GetCredentialCount({pdwcount:?}, {pdwdefault:?}, {pbautologonwithdefault:?})"
        );
        unsafe { (*pdwcount, *pdwdefault, *pbautologonwithdefault) = self.get_credential_count() };
        Ok(())
    }

    fn GetCredentialAt(&self, dwindex: u32) -> Result<ICredentialProviderCredential> {
        debug_flow!("ICredentialProvider::GetCredentialAt({dwindex})");
        self.get_credential_at(dwindex)
    }
}

#[allow(non_snake_case)]
impl ICredentialProviderSetUserArray_Impl for UdsRpCredentialsProvider_Impl {
    fn SetUserArray(
        &self,
        users: windows_core::Ref<'_, ICredentialProviderUserArray>,
    ) -> windows_core::Result<()> {
        debug_flow!(
            "ICredentialProviderSetUserArray::SetUserArray({:?})",
            users.unwrap()
        );
        Ok(())
    }
}
