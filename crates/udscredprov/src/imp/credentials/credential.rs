//! The credential tile (`ICredentialProviderCredential`). Serializes either
//! the pipe-delivered auto-logon credential or the interactively typed
//! username/password into a `KERB_INTERACTIVE_UNLOCK_LOGON` for LSA, and
//! reports the logon outcome back to the service over the pipe.

use std::sync::{Arc, RwLock};

use windows::{
    Win32::{
        Foundation::{E_INVALIDARG, E_NOTIMPL, LUID, NTSTATUS},
        Graphics::Gdi::HBITMAP,
        Security::Authentication::Identity::{
            KERB_INTERACTIVE_LOGON, KERB_INTERACTIVE_UNLOCK_LOGON, KerbInteractiveLogon,
            KerbWorkstationUnlockLogon,
        },
        UI::{
            Shell::{
                CPGSR_RETURN_CREDENTIAL_FINISHED, CPUS_UNLOCK_WORKSTATION,
                CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION,
                CREDENTIAL_PROVIDER_FIELD_INTERACTIVE_STATE, CREDENTIAL_PROVIDER_FIELD_STATE,
                CREDENTIAL_PROVIDER_GET_SERIALIZATION_RESPONSE, CREDENTIAL_PROVIDER_STATUS_ICON,
                CREDENTIAL_PROVIDER_USAGE_SCENARIO, ICredentialProviderCredential,
                ICredentialProviderCredential_Impl, ICredentialProviderCredentialEvents,
            },
            WindowsAndMessaging::{IMAGE_BITMAP, LR_CREATEDIBSECTION, LoadImageW},
        },
    },
    core::*,
};
use zeroize::Zeroize;

use super::{fields::CREDENTIAL_PROVIDER_FIELD_DESCRIPTORS, types};
use crate::imp::utils::{com, helpers, lsa};
use crate::imp::{globals, pipe};
use crate::logic;
use crate::{debug_dev, debug_flow};

/// The credential tile COM object.
#[implement(ICredentialProviderCredential)]
#[derive(Clone)]
pub struct UdsRpCredential {
    cpus: CREDENTIAL_PROVIDER_USAGE_SCENARIO,
    values: Arc<RwLock<Vec<String>>>, // Interactive field values
    cookie: Arc<RwLock<Option<u32>>>,
    pipe: pipe::SharedPipe,
}

impl UdsRpCredential {
    /// Creates an empty credential tile sharing the pipe endpoint.
    pub fn new(pipe: pipe::SharedPipe) -> Self {
        debug_flow!("UdsRpCredential::new");
        Self {
            cpus: windows::Win32::UI::Shell::CPUS_LOGON,
            values: Arc::new(RwLock::new(vec![
                String::new();
                types::UdsFieldId::NumFields as usize
            ])),
            cookie: Arc::new(RwLock::new(None)),
            pipe,
        }
    }

    /// Sets the usage scenario (logon vs unlock).
    pub fn set_usage_scenario(&mut self, cpus: CREDENTIAL_PROVIDER_USAGE_SCENARIO) {
        self.cpus = cpus;
    }

    fn get_credential_events(&self) -> Option<ICredentialProviderCredentialEvents> {
        if let Some(cookie) = *self.cookie.read().unwrap() {
            return com::get_from_git(cookie).ok();
        }
        None
    }

    fn register_event_manager(
        &self,
        manager: ICredentialProviderCredentialEvents,
    ) -> windows::core::Result<()> {
        debug_dev!("Registering credential events");
        let cookie = com::register_in_git(manager)?;
        *self.cookie.write().unwrap() = Some(cookie);
        Ok(())
    }

    fn unregister_event_manager(&self) -> windows::core::Result<()> {
        debug_dev!("Unregistering credential events");
        let cookie_opt = *self.cookie.read().unwrap();
        if let Some(cookie) = cookie_opt {
            com::unregister_from_git(cookie)?;
            *self.cookie.write().unwrap() = None;
        }
        Ok(())
    }

    fn clear_password(&self) -> windows::core::Result<()> {
        debug_dev!("Clearing password field");

        let mut values_guard = self.values.write().unwrap();
        if !values_guard[types::UdsFieldId::Password as usize].is_empty() {
            values_guard[types::UdsFieldId::Password as usize].zeroize();
        }

        if let Some(events) = self.get_credential_events() {
            debug_dev!("Notifying LogonUI to clear the password field");
            let icred: ICredentialProviderCredential = (*self).clone().into();
            unsafe {
                events.SetFieldString(
                    &icred,
                    types::UdsFieldId::Password as u32,
                    PCWSTR::null(),
                )
            }?;
        }
        Ok(())
    }

    // Parameter names mirror the MSDN signatures
    #[allow(clippy::similar_names)]
    unsafe fn get_field_state(
        &self,
        field_id: u32,
        pcpfs: *mut CREDENTIAL_PROVIDER_FIELD_STATE,
        pcpfis: *mut CREDENTIAL_PROVIDER_FIELD_INTERACTIVE_STATE,
    ) -> windows::core::Result<()> {
        if field_id as usize >= CREDENTIAL_PROVIDER_FIELD_DESCRIPTORS.len() {
            return Err(E_INVALIDARG.into());
        }
        let field = &CREDENTIAL_PROVIDER_FIELD_DESCRIPTORS[field_id as usize];
        unsafe {
            *pcpfs = field.state;
            *pcpfis = field.interactive_state;
        }
        Ok(())
    }

    fn get_string_value(&self, field_id: u32) -> windows::core::Result<PWSTR> {
        if field_id as usize >= CREDENTIAL_PROVIDER_FIELD_DESCRIPTORS.len() {
            return Err(E_INVALIDARG.into());
        }
        let values = self.values.read().unwrap();
        match com::alloc_pwstr(&values[field_id as usize]) {
            Ok(pwstr) => Ok(pwstr),
            Err(_) => Err(E_INVALIDARG.into()),
        }
    }

    fn set_string_value(&self, field_id: u32, psz: &PCWSTR) -> windows::core::Result<()> {
        if (field_id as usize) < CREDENTIAL_PROVIDER_FIELD_DESCRIPTORS.len() {
            let descriptor = &CREDENTIAL_PROVIDER_FIELD_DESCRIPTORS[field_id as usize];
            if descriptor.is_text_field() {
                let new_value = unsafe { (*psz).to_string().unwrap_or_default() };
                self.values.write().unwrap()[field_id as usize] = new_value;
                return Ok(());
            }
        }
        Err(E_INVALIDARG.into())
    }

    fn get_bitmap_value(&self, field_id: u32) -> windows::core::Result<HBITMAP> {
        if field_id == types::UdsFieldId::TileImage as u32 {
            unsafe {
                LoadImageW(
                    Some(globals::get_instance()),
                    helpers::make_int_resource(101),
                    IMAGE_BITMAP,
                    0,
                    0,
                    LR_CREATEDIBSECTION,
                )
                .map(|hbmp_handle| HBITMAP(hbmp_handle.0))
            }
        } else {
            Err(E_INVALIDARG.into())
        }
    }

    fn serialize(
        &self,
        pcpgsr: *mut CREDENTIAL_PROVIDER_GET_SERIALIZATION_RESPONSE,
        pcpcs: *mut CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION,
    ) -> windows::core::Result<()> {
        debug_dev!("GetSerialization called");

        // First choice: the pipe-delivered auto-logon credential (one-shot:
        // consumed here so it does not linger in memory and a failure cannot
        // loop; the service may resend it after reading the LogonResult).
        let pipe_creds = super::store::take();
        let (username, password, domain) = pipe_creds.map_or_else(
            || {
                // Fallback: interactive username/password typed into our tile
                debug_dev!("No pipe credentials, using interactive fields");
                let values_guard = self.values.read().unwrap();
                let (username, domain) = helpers::split_username_domain(
                    &values_guard[types::UdsFieldId::Username as usize],
                );
                let password = values_guard[types::UdsFieldId::Password as usize].clone();
                (username, password, domain)
            },
            |creds| {
                debug_dev!("Using pipe-delivered credentials for {}", creds.username);
                (
                    creds.username.clone(),
                    creds.password().to_string(),
                    creds.domain.clone(),
                )
            },
        );

        let lsa_username = lsa::LsaUnicodeString::new(&username);
        let lsa_password = lsa::LsaUnicodeString::new(&password);
        let lsa_domain = lsa::LsaUnicodeString::new(&domain);

        let interactive_logon = KERB_INTERACTIVE_UNLOCK_LOGON {
            Logon: KERB_INTERACTIVE_LOGON {
                MessageType: if self.cpus == CPUS_UNLOCK_WORKSTATION {
                    KerbWorkstationUnlockLogon
                } else {
                    KerbInteractiveLogon
                },
                LogonDomainName: *lsa_domain.as_lsa(),
                UserName: *lsa_username.as_lsa(),
                Password: *lsa_password.as_lsa(),
            },
            LogonId: LUID::default(),
        };
        let (pkiul_out, cb_total) =
            unsafe { lsa::kerb_interactive_unlock_logon_pack(&interactive_logon)? };

        unsafe {
            std::ptr::write_bytes(pcpcs, 0, 1);
            (*pcpcs).rgbSerialization = pkiul_out;
            (*pcpcs).cbSerialization = cb_total;
            (*pcpcs).ulAuthenticationPackage = lsa::retrieve_negotiate_auth_package()?;
            (*pcpcs).clsidCredentialProvider = globals::CLSID_UDS_RP_CREDENTIAL_PROVIDER;
            *pcpgsr = CPGSR_RETURN_CREDENTIAL_FINISHED;
        }

        Ok(())
    }

    fn report_result(&self, ntsstatus: NTSTATUS, ntssubstatus: NTSTATUS) {
        let success = ntsstatus.is_ok() && ntssubstatus.is_ok();
        let message = if success {
            "Logon successful".to_string()
        } else {
            format!(
                "Logon failed (status 0x{:08X}, substatus 0x{:08X})",
                ntsstatus.0, ntssubstatus.0
            )
        };
        crate::imp::utils::log::info!("ReportResult: {message}");
        pipe::send_response(
            &self.pipe,
            &logic::Response::LogonResult {
                success,
                status: ntsstatus.0,
                substatus: ntssubstatus.0,
                message,
            },
        );
        if !success {
            // Do not keep the typed password around after a failed attempt
            let _ = self.clear_password();
        }
    }
}

impl Default for UdsRpCredential {
    fn default() -> Self {
        Self::new(pipe::shared_pipe().clone())
    }
}

#[allow(non_snake_case)]
impl ICredentialProviderCredential_Impl for UdsRpCredential_Impl {
    fn Advise(
        &self,
        pcpce: windows::core::Ref<'_, ICredentialProviderCredentialEvents>,
    ) -> windows::core::Result<()> {
        debug_flow!("ICredentialProviderCredential::Advise({:?})", pcpce.unwrap());
        self.register_event_manager(pcpce.unwrap().clone())
    }

    fn UnAdvise(&self) -> windows::core::Result<()> {
        debug_flow!("ICredentialProviderCredential::UnAdvise()");
        self.unregister_event_manager()
    }

    fn SetSelected(&self) -> windows::core::Result<BOOL> {
        debug_flow!("ICredentialProviderCredential::SetSelected()");
        Ok(false.into())
    }

    fn SetDeselected(&self) -> windows::core::Result<()> {
        debug_flow!("ICredentialProviderCredential::SetDeselected()");
        self.clear_password()
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)] // COM needs the signature as is
    // Parameter names mirror the MSDN signatures
    #[allow(clippy::similar_names)]
    fn GetFieldState(
        &self,
        dwfieldid: u32,
        pcpfs: *mut CREDENTIAL_PROVIDER_FIELD_STATE,
        pcpfis: *mut CREDENTIAL_PROVIDER_FIELD_INTERACTIVE_STATE,
    ) -> windows::core::Result<()> {
        debug_flow!(
            "ICredentialProviderCredential::GetFieldState({dwfieldid}, {pcpfs:?}, {pcpfis:?})"
        );
        unsafe { self.get_field_state(dwfieldid, pcpfs, pcpfis) }
    }

    fn GetStringValue(&self, dwfieldid: u32) -> windows::core::Result<PWSTR> {
        debug_flow!("ICredentialProviderCredential::GetStringValue({dwfieldid})");
        self.get_string_value(dwfieldid)
    }

    fn GetBitmapValue(&self, dwfieldid: u32) -> windows::core::Result<HBITMAP> {
        debug_flow!("ICredentialProviderCredential::GetBitmapValue({dwfieldid})");
        self.get_bitmap_value(dwfieldid)
    }

    fn GetCheckboxValue(
        &self,
        dwfieldid: u32,
        pbchecked: *mut BOOL,
        ppszlabel: *mut PWSTR,
    ) -> windows::core::Result<()> {
        debug_flow!(
            "ICredentialProviderCredential::GetCheckboxValue({dwfieldid}, {pbchecked:?}, {ppszlabel:?})"
        );
        Err(E_NOTIMPL.into())
    }

    fn GetSubmitButtonValue(&self, dwfieldid: u32) -> windows::core::Result<u32> {
        debug_flow!("ICredentialProviderCredential::GetSubmitButtonValue({dwfieldid})");
        if dwfieldid == types::UdsFieldId::SubmitButton as u32 {
            Ok(types::UdsFieldId::Password as u32)
        } else {
            Err(E_INVALIDARG.into())
        }
    }

    fn GetComboBoxValueCount(
        &self,
        dwfieldid: u32,
        pcitems: *mut u32,
        pdwselecteditem: *mut u32,
    ) -> windows::core::Result<()> {
        debug_flow!(
            "ICredentialProviderCredential::GetComboBoxValueCount({dwfieldid}, {pcitems:?}, {pdwselecteditem:?})"
        );
        Err(E_NOTIMPL.into())
    }

    fn GetComboBoxValueAt(&self, dwfieldid: u32, dwitem: u32) -> windows::core::Result<PWSTR> {
        debug_flow!("ICredentialProviderCredential::GetComboBoxValueAt({dwfieldid}, {dwitem})");
        Err(E_NOTIMPL.into())
    }

    fn SetStringValue(&self, dwfieldid: u32, psz: &PCWSTR) -> windows::core::Result<()> {
        debug_flow!("ICredentialProviderCredential::SetStringValue({dwfieldid}, {psz:?})");
        self.set_string_value(dwfieldid, psz)
    }

    fn SetCheckboxValue(&self, dwfieldid: u32, bchecked: BOOL) -> windows::core::Result<()> {
        debug_flow!("ICredentialProviderCredential::SetCheckboxValue({dwfieldid}, {bchecked:?})");
        Err(E_NOTIMPL.into())
    }

    fn SetComboBoxSelectedValue(
        &self,
        dwfieldid: u32,
        dwselecteditem: u32,
    ) -> windows::core::Result<()> {
        debug_flow!(
            "ICredentialProviderCredential::SetComboBoxSelectedValue({dwfieldid}, {dwselecteditem})"
        );
        Err(E_NOTIMPL.into())
    }

    fn CommandLinkClicked(&self, dwfieldid: u32) -> windows::core::Result<()> {
        debug_flow!("ICredentialProviderCredential::CommandLinkClicked({dwfieldid})");
        Err(E_NOTIMPL.into())
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)] // COM needs the signature as is
    fn GetSerialization(
        &self,
        pcpgsr: *mut CREDENTIAL_PROVIDER_GET_SERIALIZATION_RESPONSE,
        pcpcs: *mut CREDENTIAL_PROVIDER_CREDENTIAL_SERIALIZATION,
        ppszoptionalstatustext: *mut PWSTR,
        pcpsioptionalstatusicon: *mut CREDENTIAL_PROVIDER_STATUS_ICON,
    ) -> windows::core::Result<()> {
        debug_flow!(
            "ICredentialProviderCredential::GetSerialization({pcpgsr:?}, {pcpcs:?}, {ppszoptionalstatustext:?}, {pcpsioptionalstatusicon:?})"
        );
        self.serialize(pcpgsr, pcpcs)
    }

    fn ReportResult(
        &self,
        ntsstatus: NTSTATUS,
        ntssubstatus: NTSTATUS,
        ppszoptionalstatustext: *mut PWSTR,
        pcpsioptionalstatusicon: *mut CREDENTIAL_PROVIDER_STATUS_ICON,
    ) -> windows::core::Result<()> {
        debug_flow!(
            "ICredentialProviderCredential::ReportResult({ntsstatus:?}, {ntssubstatus:?}, {ppszoptionalstatustext:?}, {pcpsioptionalstatusicon:?})"
        );
        self.report_result(ntsstatus, ntssubstatus);
        Ok(())
    }
}
