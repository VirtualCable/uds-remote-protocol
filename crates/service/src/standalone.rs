// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Connection-mode wiring for the orchestrator service.
//!
//! When the shared server configuration file (`uds-rp-server.toml`, same
//! lookup order as the agent) selects a connection method, the service
//! switches from laboratory behaviour to a real deployment mode:
//!
//! - **standalone**: the session key and ticket come from the file (fixed,
//!   documented) instead of being generated randomly on each service run,
//!   so the reference client can connect with the configured human ticket;
//!   the standalone auto-logon credentials are pushed to the credential
//!   provider through its named pipe, taking precedence over the lab
//!   credentials file.
//! - **pq**: tickets are validated against the UDS broker (the `connection`
//!   kind of the `actor/v3` `ticket` action); each validated session gets
//!   its own key, credentials and parameters from the broker response
//!   (see the `broker` crate).

use std::sync::Arc;

use tracing::{info, warn};
use udscredprov::logic::Credentials;

/// Fixed session parameters resolved from the standalone configuration.
#[derive(Clone)]
pub struct StandaloneRuntime {
    /// Shared session key (PSK) for the crypto layer.
    pub key: [u8; protocol::service_ipc::KEY_LEN],
    /// Fixed ticket clients must present (wire form).
    pub ticket: [u8; protocol::service_ipc::TICKET_LEN],
    /// Listen address for the session agent.
    pub bind: String,
    /// Video codec preset name for the session agent.
    pub preset: String,
    /// Which desktop output the agent captures (substring selector); `None`
    /// or empty captures the console's primary display (mirror mode).
    pub output_name: Option<String>,
    /// Auto-logon credentials for the credential provider, if configured.
    pub creds: Option<Credentials>,
    /// Codec-ladder sensitivity overrides in wire form (empty = defaults).
    pub ladder: String,
    /// Adaptive-fps overrides in wire form (empty = disabled, §12.7).
    pub fps_adapt: String,
    /// Per-session telemetry CSV path (empty = disabled).
    pub stats_csv: String,
    /// Webcam sink for the agent (§18.57; empty/`null` = redirection off).
    pub cam_sink: String,
    /// Audio-in sink for the agent (§9.2; empty/`null` = mic off).
    pub mic_sink: String,
    /// Server-fixed console resolution (§18.37); `None` lets the agent
    /// approach the client's preferred mode.
    pub fixed_resolution: Option<(u32, u32)>,
    /// The client may ask to lock the remote console (`[standalone.channels]
    /// lock`); off by default, server-side mandatory.
    pub allow_lock: bool,
    /// Blank the desktop wallpaper during sessions (§18.54).
    pub wallpaper_off: bool,
    /// Console-conflict policy (§18.53).
    pub console_conflict: server_config::ConsoleConflictPolicy,
    /// Whether the `ask` conflict message may reveal the console owner.
    pub console_conflict_reveal_user: bool,
    /// Ask timeout in seconds (fail-closed).
    pub console_conflict_timeout_secs: u64,
    /// How a takeover evicts the console owner.
    pub console_takeover: server_config::TakeoverMode,
    /// Quality policy in wire form for the agent (§18.69; empty = §12.5
    /// defaults).
    pub quality: String,
}

/// Active connection mode for the service.
pub enum Runtime {
    /// Broker-less reference mode (fixed ticket and key from the config).
    Standalone(StandaloneRuntime),
    /// Broker-backed mode: per-session tickets validated via `actor/v3`.
    Pq(PqRuntime),
}

/// Broker-backed runtime: the provider plus the public listen address.
pub struct PqRuntime {
    /// Validates tickets against the broker and fetches session grants.
    pub provider: Arc<crate::broker::PqProvider>,
    /// Listen address for the validating listener.
    pub bind: String,
}

/// Loads the configured connection mode. Returns `None` when there is no
/// config file or it selects `oneshot` (laboratory defaults apply); logs
/// and returns `None` on malformed files.
pub fn load() -> Option<Runtime> {
    let path = server_config::find_config_file()?;
    let config = match server_config::load(&path) {
        Ok(config) => config,
        Err(err) => {
            warn!(error = %format!("{err:#}"), path = %path.display(), "cannot load the server config; ignoring it");
            return None;
        }
    };
    match config.connection_method {
        server_config::ConnectionMethod::Standalone => {
            load_standalone(&path, &config).map(Runtime::Standalone)
        }
        server_config::ConnectionMethod::Pq => load_pq(&path, &config).map(Runtime::Pq),
        server_config::ConnectionMethod::Oneshot => {
            info!(path = %path.display(), "server config is in oneshot mode; laboratory credentials");
            None
        }
    }
}

/// Broker-backed mode from the `[broker]` section.
fn load_pq(path: &std::path::Path, config: &server_config::ServerConfig) -> Option<PqRuntime> {
    let broker_config = match &config.broker {
        Some(broker) if !broker.url.is_empty() && !broker.token.is_empty() => broker,
        _ => {
            warn!(path = %path.display(), "pq mode without a complete [broker] section (url, token); ignoring the config");
            return None;
        }
    };
    let provider = match crate::broker::PqProvider::new(&broker_config.url, &broker_config.token, broker_config.verify_ssl) {
        Ok(provider) => provider,
        Err(err) => {
            warn!(error = %format!("{err:#}"), path = %path.display(), "cannot build the broker provider; ignoring the config");
            return None;
        }
    };
    info!(path = %path.display(), bind = %config.bind, url = %broker_config.url, "pq mode: tickets validated against the broker");
    Some(PqRuntime {
        provider: Arc::new(provider),
        bind: config.bind.to_string(),
    })
}

/// Standalone (reference) mode from the `[standalone]` section.
fn load_standalone(path: &std::path::Path, config: &server_config::ServerConfig) -> Option<StandaloneRuntime> {
    let standalone = match &config.standalone {
        Some(standalone) if !standalone.ticket.is_empty() => standalone,
        _ => {
            warn!(path = %path.display(), "standalone mode without a [standalone] ticket; ignoring the config");
            return None;
        }
    };
    let key: [u8; protocol::service_ipc::KEY_LEN] = match protocol::hex::decode(&config.shared_key) {
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
        stats_csv: standalone.stats_csv.clone().unwrap_or_default(),
        cam_sink: standalone.cam_sink.clone().unwrap_or_default(),
        mic_sink: standalone.mic_sink.clone().unwrap_or_default(),
        fixed_resolution: standalone
            .resolution
            .as_deref()
            .and_then(|resolution| {
                let (w, h) = resolution.split_once(['x', 'X'])?;
                Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
            }),
        allow_lock: standalone.channels.lock,
        wallpaper_off: standalone.wallpaper_off,
        console_conflict: standalone.console_conflict,
        console_conflict_reveal_user: standalone.console_conflict_reveal_user,
        console_conflict_timeout_secs: standalone.console_conflict_timeout_secs,
        console_takeover: standalone.console_takeover,
        quality: config.quality.to_wire(),
    })
}
