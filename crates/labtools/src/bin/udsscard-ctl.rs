// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Control/verification utility for the UDS virtual smart card reader
//! driver (`udsscard`, UMDF). Windows-only.
//!
//! Subcommands:
//!
//! - `run [--atr HEX] [--no-auto-inject]`: play the service side of the
//!   driver IPC — create the command and IO pipes, wait for the driver,
//!   auto-inject the fake card (unless `--no-auto-inject`), answer every
//!   TRANSMIT with `6D 00`, and accept stdin commands `inject`, `eject`,
//!   `quit`.

#[cfg(windows)]
#[path = "udsscard-ctl/imp.rs"]
mod imp;

fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        imp::run()
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("udsscard-ctl only runs on Windows: it talks to the udsscard UMDF driver")
    }
}
