//! Standalone (broker-less) mode wiring for the orchestrator service.
//!
//! When the shared server configuration file (`uds-rp-server.toml`, same
//! lookup order as the agent) selects `connection_method = "standalone"`,
//! the service switches from laboratory behaviour to reference behaviour:
//!
//! - the session key and ticket come from the file (fixed, documented)
//!   instead of being generated randomly on each service run, so the
//!   reference client can connect with the configured human ticket;
//! - the standalone auto-logon credentials are pushed to the credential
//!   provider through its named pipe, taking precedence over the lab
//!   credentials file.
//!
//! This is the reference deployment the phase-4 broker integration mirrors:
//! the broker-backed service will fetch per-session tickets and credentials
//! from the broker instead of a file.

use tracing::{info, warn};
use udscredprov::logic::Credentials;

/// Fixed session parameters resolved from the standalone configuration.
pub struct StandaloneRuntime {
    /// Shared session key (PSK) for the crypto layer.
    pub key: [u8; service_proto::KEY_LEN],
    /// Fixed ticket clients must present (wire form).
    pub ticket: [u8; service_proto::TICKET_LEN],
    /// Listen address for the session agent.
    pub bind: String,
    /// Video codec preset name for the session agent.
    pub preset: String,
    /// Which desktop output the agent captures (substring selector); `None`
    /// keeps the service default (the UDS virtual display).
    pub output_name: Option<String>,
    /// Auto-logon credentials for the credential provider, if configured.
    pub creds: Option<Credentials>,
    /// Codec-ladder sensitivity overrides in wire form (empty = defaults).
    pub ladder: String,
    /// Adaptive-fps overrides in wire form (empty = disabled, §12.7).
    pub fps_adapt: String,
}

impl StandaloneRuntime {
    /// Whether sessions will capture the UDS virtual display and therefore
    /// need the virtual monitor attached. Console mirroring (`output_name`
    /// set to the console adapter, e.g. `"QXL"`) must NOT attach it: the
    /// topology churn destabilizes single-head console drivers for no
    /// benefit (the virtual display is not the capture target).
    pub fn needs_virtual_display(&self) -> bool {
        self.output_name
            .as_deref()
            .is_none_or(|name| name.to_lowercase().contains("uds"))
    }
}

/// Loads the standalone runtime from the shared config file. Returns `None`
/// when there is no config file or it is not in standalone mode (laboratory
/// defaults apply); logs and returns `None` on malformed files.
pub fn load() -> Option<StandaloneRuntime> {
    let path = server_config::find_config_file()?;
    let config = match server_config::load(&path) {
        Ok(config) => config,
        Err(err) => {
            warn!(error = %format!("{err:#}"), path = %path.display(), "cannot load the server config; ignoring it");
            return None;
        }
    };
    if config.connection_method != server_config::ConnectionMethod::Standalone {
        info!(path = %path.display(), "server config is not in standalone mode; laboratory credentials");
        return None;
    }
    let standalone = match &config.standalone {
        Some(standalone) if !standalone.ticket.is_empty() => standalone,
        _ => {
            warn!(path = %path.display(), "standalone mode without a [standalone] ticket; ignoring the config");
            return None;
        }
    };
    let key: [u8; service_proto::KEY_LEN] = match protocol::hex::decode(&config.shared_key) {
        Ok(key) => key,
        Err(err) => {
            warn!(error = %format!("{err:#}"), path = %path.display(), "bad shared_key in the server config; ignoring it");
            return None;
        }
    };
    let creds = match (&standalone.user, &standalone.password) {
        (Some(user), Some(password)) => {
            // An empty/missing domain on a domain-joined machine resolves
            // against the DC; "." pins the logon to the local accounts.
            let domain = standalone.domain.as_deref().unwrap_or("").trim();
            let domain = if domain.is_empty() { "." } else { domain };
            Some(Credentials::new(user.trim(), domain, password))
        }
        _ => None,
    };
    info!(
        path = %path.display(),
        bind = %config.bind,
        has_credentials = creds.is_some(),
        "standalone mode: fixed ticket active (reference deployment)"
    );
    Some(StandaloneRuntime {
        key,
        ticket: server_config::ticket_bytes(&standalone.ticket),
        bind: config.bind.to_string(),
        preset: standalone.preset.clone(),
        output_name: standalone.output_name.clone(),
        creds,
        ladder: standalone.ladder.to_wire(),
        fps_adapt: standalone
            .adaptive_fps
            .as_ref()
            .map(server_config::AdaptiveFpsTuning::to_wire)
            .unwrap_or_default(),
    })
}
