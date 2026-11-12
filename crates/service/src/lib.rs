// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! UDS Remote Protocol orchestrator service library.
//!
//! All real functionality is Windows-only; on other platforms this crate is
//! an empty shell so the workspace still builds and lints cleanly.

#[cfg(windows)]
mod agent;
pub mod broker;
#[cfg(windows)]
mod console;
#[cfg(windows)]
mod console_fsm;
#[cfg(windows)]
mod credprov;
#[cfg(windows)]
mod install;
#[cfg(windows)]
mod listener;
#[cfg(windows)]
mod logging;
#[cfg(windows)]
mod sas;
#[cfg(windows)]
mod service_main;
#[cfg(windows)]
mod standalone;

/// CLI entry point: `--install`, `--uninstall`, `--start`, `--stop`,
/// `--console` (debug run in the current console) or, with no arguments, the
/// SCM entry point.
#[cfg(windows)]
pub fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--install") => install::install(),
        Some("--uninstall") => install::uninstall(),
        Some("--start") => install::start(),
        Some("--stop") => install::stop(),
        Some("--console") => {
            service_main::run_console();
            Ok(())
        }
        None => service_main::run_dispatch(),
        Some(other) => anyhow::bail!(
            "unknown argument `{other}`; usage: udsrp-service [--install|--uninstall|--start|--stop|--console]"
        ),
    }
}
