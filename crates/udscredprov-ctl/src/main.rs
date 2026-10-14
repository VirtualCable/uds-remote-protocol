//! Test/verification harness for the UDS credential provider (`udscredprov`).
//! Windows-only.
//!
//! Plays the named-pipe **server** role that the `uds-rp` service will own in
//! production (`\\.\pipe\udsrp-credprov`), so the credential provider DLL
//! loaded by LogonUI can connect to it as a client. The wire protocol is
//! encoded/decoded by `udscredprov::logic` (unit-tested on Linux).
//!
//! Subcommands:
//!
//! - `listen [--timeout <secs>]`: wait for the provider to connect and print
//!   its `Hello`, then keep the pipe open until the timeout;
//! - `ping [--timeout <secs>]`: wait for the provider, send `Ping`, expect
//!   `Pong`;
//! - `send <user> <domain> <password> [--await-result <secs>]`: wait for the
//!   provider, deliver `SetCredentials`, expect `Ack`, and optionally wait for
//!   the `LogonResult` the provider reports from `ReportResult`;
//! - `clear [--timeout <secs>]`: send `ClearCredentials`, expect `Ack`.

#[cfg(windows)]
mod imp;

fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        imp::run()
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!(
            "udscredprov-ctl only runs on Windows: it talks to the udscredprov named pipe"
        )
    }
}
