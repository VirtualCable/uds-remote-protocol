// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Process-global storage for the pipe-delivered auto-logon credential.
//!
//! LogonUI creates a *new* provider instance for every enumeration cycle
//! (in particular when re-enumerating after our `CredentialsChanged`), so
//! instance state does not survive the delivery of credentials over the
//! pipe: the instance that receives them is not the instance that gets
//! enumerated. The credential therefore lives in process-wide state shared
//! by every provider/credential/filter instance of this DLL, exactly like
//! the filter's armed flag did.

use std::sync::RwLock;

use super::types::Credential;

/// The pipe-delivered auto-logon credential, if any.
static PIPE_CREDENTIAL: RwLock<Credential> = RwLock::new(Credential::new());

/// Stores (or replaces) the pipe-delivered credential.
pub fn set(creds: crate::logic::Credentials) {
    PIPE_CREDENTIAL.write().unwrap().set(creds);
}

/// Forgets the stored credential, zeroizing the password.
pub fn clear() {
    PIPE_CREDENTIAL.write().unwrap().clear();
}

/// Whether a valid auto-logon credential is stored.
pub fn is_valid() -> bool {
    PIPE_CREDENTIAL.read().unwrap().is_valid()
}

/// Takes the stored credential out (one-shot auto-logon), zeroizing the
/// in-memory copy on drop.
pub fn take() -> Option<crate::logic::Credentials> {
    PIPE_CREDENTIAL.write().unwrap().take()
}
