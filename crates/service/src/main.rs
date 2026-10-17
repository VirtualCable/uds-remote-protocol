//! UDS Remote Protocol orchestrator service (session 0).
//!
//! Owns everything that must live outside the user session (see
//! doc/plan/servicio-y-sesion.md): IddCx monitor lifecycle, session-agent
//! spawning, the named-mutex session watchdog and the named pipes towards
//! the credential provider and the agent.
//!
//! M0 scope: SCM integration, self-install (`--install` / `--uninstall`),
//! file logging, start/stop. Later milestones add the orchestration itself.
//!
//! Non-Windows builds are a stub that only errors out loudly.

use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("udsrp-service: {err:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn run() -> anyhow::Result<()> {
    service::run()
}

#[cfg(not(windows))]
fn run() -> anyhow::Result<()> {
    anyhow::bail!("udsrp-service is a Windows service; this platform is not supported")
}
