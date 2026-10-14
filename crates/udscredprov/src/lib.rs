//! UDS Credential Provider — Windows auto-logon credential provider.
//!
//! Windows-only crate: on other hosts it compiles to the pure-Rust `logic`
//! module (the named-pipe protocol, unit-tested on Linux) so the workspace
//! stays buildable and testable on Linux. The Windows side (`imp`,
//! cfg-gated) implements `ICredentialProvider`, `ICredentialProviderFilter`
//! and `ICredentialProviderCredential` following the structure of the
//! reference implementation in the `win-credentials-provider` repository,
//! but fed by the local `uds-rp` service over `\\.\pipe\udsrp-credprov`
//! instead of talking HTTP to the UDS broker.
//!
//! See `doc/plan/credential-provider.md` for the design.

pub mod logic;

#[cfg(windows)]
mod imp;
