//! UDS Remote Protocol orchestrator service library.
//!
//! All real functionality is Windows-only; on other platforms this crate is
//! an empty shell so the workspace still builds and lints cleanly.

#[cfg(windows)]
mod agent;
#[cfg(windows)]
mod install;
#[cfg(windows)]
mod logging;
#[cfg(windows)]
mod service_main;

/// CLI entry point: `--install`, `--uninstall`, `--console` (debug run in
/// the current console) or, with no arguments, the SCM entry point.
#[cfg(windows)]
pub fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--install") => install::install(),
        Some("--uninstall") => install::uninstall(),
        Some("--console") => service_main::run_console(),
        None => service_main::run_dispatch(),
        Some(other) => anyhow::bail!(
            "unknown argument `{other}`; usage: udsrp-service [--install|--uninstall|--console]"
        ),
    }
}
