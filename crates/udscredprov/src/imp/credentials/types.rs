//! Field descriptors and the pipe-delivered credential container.
//! Mirrors `credentials/types.rs` of the reference provider, with the
//! credential itself holding a real username/domain/password (delivered over
//! the pipe) instead of a broker ticket.

use std::mem;
use windows::Win32::{
    System::Com::{CoTaskMemAlloc, CoTaskMemFree},
    UI::Shell::{
        CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR, CREDENTIAL_PROVIDER_FIELD_INTERACTIVE_STATE,
        CREDENTIAL_PROVIDER_FIELD_STATE, CREDENTIAL_PROVIDER_FIELD_TYPE, SHStrDupW,
    },
};
use windows::core::{GUID, PWSTR};

use crate::logic;

/// Rust mirror of `CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR`, converted to the
/// COM structure (allocated in COM task memory) on demand.
#[derive(Debug, Clone)]
pub struct CredentialFieldDescriptor {
    /// Field identifier (`UdsFieldId`).
    pub field_id: u32,
    /// Field type (`CPFT_*`).
    pub field_type: CREDENTIAL_PROVIDER_FIELD_TYPE,
    /// Static label shown by LogonUI.
    pub label: &'static str,
    /// Field type GUID (usually zeroed).
    pub guid: GUID,
    /// Display state (`CPFS_*`).
    pub state: CREDENTIAL_PROVIDER_FIELD_STATE,
    /// Interactive state (`CPFIS_*`).
    pub interactive_state: CREDENTIAL_PROVIDER_FIELD_INTERACTIVE_STATE,
}

impl CredentialFieldDescriptor {
    /// Converts into a COM-allocated `CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR`.
    /// Ownership of the result passes to the caller (LogonUI frees it).
    // Name mirrors the reference provider; `&self` is fine because the
    // descriptor is Copy-sized data, not consumed.
    #[allow(clippy::wrong_self_convention)]
    pub fn into_com_alloc(
        &self,
    ) -> windows::core::Result<*mut CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR> {
        unsafe {
            let ptr = CoTaskMemAlloc(mem::size_of::<CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR>())
                .cast::<CREDENTIAL_PROVIDER_FIELD_DESCRIPTOR>();

            if ptr.is_null() {
                return Err(windows::core::Error::from_thread());
            }

            (*ptr).dwFieldID = self.field_id;
            (*ptr).cpft = self.field_type;

            let Ok(wide_label) = widestring::U16CString::from_str(self.label) else {
                CoTaskMemFree(Some(ptr as _));
                return Err(windows::core::Error::from_thread());
            };
            match SHStrDupW(PWSTR(wide_label.as_ptr().cast_mut())) {
                Ok(s) => (*ptr).pszLabel = s,
                Err(e) => {
                    CoTaskMemFree(Some(ptr as _));
                    return Err(e);
                }
            }

            (*ptr).guidFieldType = self.guid;

            Ok(ptr)
        }
    }

    /// Whether this is an editable text field (plain or password).
    pub fn is_text_field(&self) -> bool {
        self.field_type == windows::Win32::UI::Shell::CPFT_EDIT_TEXT
            || self.field_type == windows::Win32::UI::Shell::CPFT_PASSWORD_TEXT
    }
}

/// Identifiers of the fields of our credential tile. Keep `NumFields` last:
/// it is the field count.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum UdsFieldId {
    /// Tile image (our logo).
    TileImage = 0,
    /// Username edit box.
    Username,
    /// Password edit box.
    Password,
    /// Submit button.
    SubmitButton,
    /// Field count (not a real field).
    NumFields,
}

/// Pipe-delivered auto-logon credential. `None` means no auto-logon is
/// armed; the tile then behaves as a plain interactive username/password
/// credential. Zeroization is handled by `logic::Credentials` on drop.
#[derive(Debug, Default)]
pub struct Credential {
    creds: Option<logic::Credentials>,
}

impl Credential {
    /// Creates an empty credential container (const so it can back a static).
    pub const fn new() -> Self {
        Self { creds: None }
    }

    /// Stores (or replaces) the pipe-delivered credential.
    pub fn set(&mut self, creds: logic::Credentials) {
        self.creds = Some(creds);
    }

    /// Forgets the stored credential, zeroizing the password.
    pub fn clear(&mut self) {
        self.creds = None;
    }

    /// Whether a valid auto-logon credential is stored.
    pub fn is_valid(&self) -> bool {
        self.creds.as_ref().is_some_and(logic::Credentials::is_valid)
    }

    /// Takes the stored credential out (one-shot auto-logon), zeroizing the
    /// in-memory copy on drop.
    pub fn take(&mut self) -> Option<logic::Credentials> {
        self.creds.take()
    }
}
