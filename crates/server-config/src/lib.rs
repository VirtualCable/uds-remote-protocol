// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Server configuration (TOML) and ticket validation.
//!
//! The reference server supports two connection methods, selected with
//! `connection_method` in the config file:
//!
//! - `"oneshot"` (the default): a random 48-byte ticket is generated on
//!   startup and printed to stdout; each accepted session must present it.
//!   This is the laboratory mode used throughout development.
//! - `"standalone"`: a fixed, human-typed ticket is configured once and
//!   accepted indefinitely, with optional session parameters (credentials
//!   for the credential provider auto-logon, display geometry, channel
//!   switches). This is the reference/broker-less mode: it exists for
//!   evaluation and as the living example the real broker-backed validator
//!   (phase 4) will mirror. It is NOT a production authentication
//!   mechanism — a real deployment validates and rotates tickets through
//!   the UDS broker.
//!
//! A human ticket is mapped to the wire ticket (`[u8; TICKET_LENGTH]`) as
//! SHA-384 of its UTF-8 bytes, so any string works.
//!
//! File lookup order (first hit wins): the `--config` path, then
//! `uds-rp-server.toml` in the current directory, then next to the
//! executable, then the platform config directory
//! (`/etc/uds-rp/` on Unix, `C:\ProgramData\VirtualCable\UDSRP\` on
//! Windows).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::Digest as _;

use crypt::consts;
use protocol::TICKET_LENGTH;

/// Default config file name.
pub const CONFIG_FILE_NAME: &str = "uds-rp-server.toml";

/// Top-level server configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// `oneshot` or `standalone`.
    pub connection_method: ConnectionMethod,
    /// Shared key (hex, 32 bytes) for the encrypted handshake.
    pub shared_key: String,
    /// Listen address.
    pub bind: SocketAddr,
    /// Standalone section (required when `connection_method = "standalone"`).
    pub standalone: Option<StandaloneConfig>,
    /// Broker section (required when `connection_method = "pq"`; used by
    /// the service, not by the reference server).
    pub broker: Option<BrokerConfig>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            connection_method: ConnectionMethod::Oneshot,
            shared_key: String::new(),
            bind: SocketAddr::from(([0, 0, 0, 0], 19790)),
            standalone: None,
            broker: None,
        }
    }
}

/// How session tickets are validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionMethod {
    /// Random ticket printed on startup (laboratory default).
    Oneshot,
    /// Fixed human ticket from the config file (reference mode).
    Standalone,
    /// Per-session tickets validated against the UDS broker (the
    /// `connection` kind of the `actor/v3` `ticket` action). Only the
    /// service supports this mode.
    Pq,
}

/// Broker connection parameters (`connection_method = "pq"`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BrokerConfig {
    /// Full URL of the ticket endpoint (e.g.
    /// `https://broker/uds/rest/actor/v3/ticket/connection`).
    pub url: String,
    /// Actor token authenticating the service against `actor/v3`.
    pub token: String,
    /// Verify the broker's TLS certificate (keep `true` in production).
    pub verify_ssl: bool,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            token: String::new(),
            verify_ssl: true,
        }
    }
}

/// Console-conflict policy (§18.53): what the service does when the console
/// session belongs to a user other than the one in the session credentials.
/// Server-side mandatory — the client never decides on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleConflictPolicy {
    /// Evict the console owner and let the new session in (VDI default: the
    /// ticket IS the authorization; the console is an assigned resource,
    /// not a possession).
    #[default]
    TakeOver,
    /// Reject the connection (`SessionEnd` reason `CONSOLE_BUSY`).
    Deny,
    /// Ask the CONNECTING client whether to take over; timeout → deny.
    Ask,
}

/// How a console takeover evicts the previous owner (§18.53).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TakeoverMode {
    /// `WTSDisconnectSession`: the evicted session survives disconnected,
    /// keeping its apps. Only when the deployment explicitly wants the old
    /// session preserved (e.g. same-user reconnects).
    Disconnect,
    /// `WTSLogoffSession` (default): the evicted session is logged off,
    /// killing its apps. A takeover reassigns the WHOLE machine — a
    /// surviving disconnected session would hold locks (roaming profiles),
    /// GPO state and resources that no longer belong to it.
    #[default]
    Logoff,
}

/// Default ask timeout for the console-conflict exchange (§18.53).
pub const DEFAULT_CONFLICT_TIMEOUT_SECS: u64 = 30;

/// Fixed-ticket session definition (reference mode, see module docs).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StandaloneConfig {
    /// The fixed, human-typed ticket.
    pub ticket: String,
    /// Credentials handed to the credential provider for auto-logon.
    pub user: Option<String>,
    /// Password for `user` (keep the file ACL'd: 0600 / admins only).
    pub password: Option<String>,
    /// Optional domain for the auto-logon.
    pub domain: Option<String>,
    /// Display geometry requested for the virtual display.
    pub width: u32,
    /// See `width`.
    pub height: u32,
    /// Fixed console resolution `"WIDTHxHEIGHT"` (§18.37): when set, the
    /// server is authoritative and the client's preference is ignored.
    /// Unset: the agent approaches the client's preferred mode.
    pub resolution: Option<String>,
    /// Frames per second for capture.
    pub fps: u32,
    /// Codec preset name (from `codec::PRESETS`).
    pub preset: String,
    /// Which desktop output to capture: case-insensitive substring of the
    /// adapter description or GDI device string. `None` captures the UDS
    /// virtual display (service default); set it to the physical console
    /// adapter (e.g. `"QXL"`) to mirror the console session instead.
    pub output_name: Option<String>,
    /// Channel switches (flat keys in the TOML `[standalone]` section).
    #[serde(flatten)]
    pub channels: ChannelSwitches,
    /// Blank the desktop wallpaper during sessions (§18.54; the shell
    /// animation reduction is unconditional — this is the only opt-in).
    pub wallpaper_off: bool,
    /// Codec-ladder sensitivity overrides (`[standalone.ladder]` table);
    /// unset keys keep the agent's built-in balanced defaults.
    pub ladder: LadderTuning,
    /// Adaptive-fps overrides (`[standalone.adaptive_fps]` table); the
    /// table's mere presence enables the adapter (§12.7), unset keys keep
    /// the balanced defaults.
    pub adaptive_fps: Option<AdaptiveFpsTuning>,
    /// Per-session telemetry CSV path; `None` keeps telemetry log-only.
    pub stats_csv: Option<String>,
    /// Webcam sink for the session agent (§18.57): `None`/`null` keeps
    /// webcam redirection off; `raw:PATH` records decoded frames (E2E).
    pub cam_sink: Option<String>,
    /// Audio-in sink for the agent (§9.2; empty/`null` = mic off, "uds" =
    /// the udsaudio virtual microphone, "wav:PATH" = E2E file).
    pub mic_sink: Option<String>,
    /// Console-conflict policy (§18.53): another user owns the console.
    pub console_conflict: ConsoleConflictPolicy,
    /// Whether the `ask` conflict message may reveal the console owner's
    /// name to the connecting client (privacy; off by default).
    pub console_conflict_reveal_user: bool,
    /// Ask timeout in seconds; without an explicit answer the connection is
    /// denied (fail-closed).
    pub console_conflict_timeout_secs: u64,
    /// How a takeover evicts the console owner (default logoff kills their
    /// apps and frees the session; disconnect keeps it alive).
    pub console_takeover: TakeoverMode,
}

/// Adaptive-fps tuning overrides (§12.7). Every key is optional; only the
/// set ones travel to the agent.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AdaptiveFpsTuning {
    /// Lowest rate the adapter may select.
    pub min_fps: Option<u32>,
    /// EMA encode load (fraction of the frame budget) that steps down.
    pub high_load: Option<f64>,
    /// EMA encode load below which the rate steps back up.
    pub low_load: Option<f64>,
    /// Sustained-overload time before stepping down (ms).
    pub hold_down_ms: Option<u64>,
    /// Sustained-underload time before stepping up (ms).
    pub hold_up_ms: Option<u64>,
    /// EMA smoothing factor (0, 1].
    pub ema_alpha: Option<f64>,
}

impl AdaptiveFpsTuning {
    /// Compact wire form (`key=value` pairs, comma-separated). The literal
    /// `on` marks "present, all defaults" so the agent can tell an enabled
    /// table apart from no table at all.
    #[must_use]
    pub fn to_wire(&self) -> String {
        let mut parts = Vec::new();
        if let Some(v) = self.min_fps {
            parts.push(format!("min_fps={v}"));
        }
        if let Some(v) = self.high_load {
            parts.push(format!("high={v}"));
        }
        if let Some(v) = self.low_load {
            parts.push(format!("low={v}"));
        }
        if let Some(v) = self.hold_down_ms {
            parts.push(format!("hold_down_ms={v}"));
        }
        if let Some(v) = self.hold_up_ms {
            parts.push(format!("hold_up_ms={v}"));
        }
        if let Some(v) = self.ema_alpha {
            parts.push(format!("ema_alpha={v}"));
        }
        if parts.is_empty() {
            return "on".to_string();
        }
        parts.join(",")
    }
}

/// Codec-ladder sensitivity overrides (design doc §8.5). Every key is
/// optional; only the set ones travel to the agent.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LadderTuning {
    /// EMA of changed-area percentage above which the VP9 regime engages.
    pub up_threshold_pct: Option<f64>,
    /// EMA percentage below which the JPEG regime is restored.
    pub down_threshold_pct: Option<f64>,
    /// Milliseconds the EMA must stay above the up threshold to switch.
    pub hold_up_ms: Option<u64>,
    /// Milliseconds the EMA must stay below the down threshold to switch.
    pub hold_down_ms: Option<u64>,
    /// Minimum time in a regime before any metric-driven switch (ms).
    pub min_dwell_ms: Option<u64>,
    /// Dwell multiplier applied when oscillation is detected (> 1.0).
    pub backoff: Option<f64>,
    /// Cap for the dynamic dwell (ms).
    pub dwell_max_ms: Option<u64>,
    /// EMA smoothing factor (0, 1]: weight of the newest sample.
    pub ema_alpha: Option<f64>,
    /// VP9 `cpu_used` for the High regime (0 best/slowest .. 8 fastest;
    /// default 8 — the ladder is the low-latency path).
    pub vp9_cpu_used: Option<i32>,
    /// Row-MT worker threads for the VP9 encoder (default 4; use 2 on small
    /// VMs to leave headroom for capture and the rest of the pipeline).
    pub threads: Option<u32>,
}

impl LadderTuning {
    /// Compact wire form (`key=value` pairs, comma-separated; empty when no
    /// override is set). Parsed back by the session agent.
    #[must_use]
    pub fn to_wire(&self) -> String {
        let mut parts = Vec::new();
        if let Some(v) = self.up_threshold_pct {
            parts.push(format!("up={v}"));
        }
        if let Some(v) = self.down_threshold_pct {
            parts.push(format!("down={v}"));
        }
        if let Some(v) = self.hold_up_ms {
            parts.push(format!("hold_up_ms={v}"));
        }
        if let Some(v) = self.hold_down_ms {
            parts.push(format!("hold_down_ms={v}"));
        }
        if let Some(v) = self.min_dwell_ms {
            parts.push(format!("min_dwell_ms={v}"));
        }
        if let Some(v) = self.backoff {
            parts.push(format!("backoff={v}"));
        }
        if let Some(v) = self.dwell_max_ms {
            parts.push(format!("dwell_max_ms={v}"));
        }
        if let Some(v) = self.ema_alpha {
            parts.push(format!("ema_alpha={v}"));
        }
        if let Some(v) = self.vp9_cpu_used {
            parts.push(format!("vp9_cpu={v}"));
        }
        if let Some(v) = self.threads {
            parts.push(format!("threads={v}"));
        }
        parts.join(",")
    }
}

/// Which optional channels the standalone session offers.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // a config file switch bag
pub struct ChannelSwitches {
    /// Audio out (and in, when the endpoint exists).
    pub audio: bool,
    /// Clipboard bridge.
    pub clipboard: bool,
    /// File transfer spool.
    pub storage: bool,
    /// Virtual smart card reader bridge.
    pub smartcard: bool,
    /// The client may ask to lock the remote console (`LockRequest`).
    /// Server-side policy is mandatory: off means the request is rejected
    /// no matter what the client sends.
    pub lock: bool,
}



impl Default for StandaloneConfig {
    fn default() -> Self {
        Self {
            ticket: String::new(),
            user: None,
            password: None,
            domain: None,
            width: 1920,
            height: 1080,
            resolution: None,
            fps: 30,
            preset: "ladder".to_string(),
            output_name: None,
            channels: ChannelSwitches {
                audio: true,
                clipboard: true,
                storage: false,
                smartcard: false,
                lock: false,
            },
            wallpaper_off: false,
            ladder: LadderTuning::default(),
            adaptive_fps: None,
            stats_csv: None,
            cam_sink: None,
            mic_sink: None,
            console_conflict: ConsoleConflictPolicy::default(),
            console_conflict_reveal_user: false,
            console_conflict_timeout_secs: DEFAULT_CONFLICT_TIMEOUT_SECS,
            console_takeover: TakeoverMode::default(),
        }
    }
}

/// Session parameters a successful validation yields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionParams {
    /// Auto-logon credentials (standalone may leave them `None`).
    pub user: Option<String>,
    /// See `user`.
    pub password: Option<String>,
    /// See `user`.
    pub domain: Option<String>,
}

/// Validates the ticket a client presents and yields the session
/// parameters. The phase-4 broker validator will implement this same
/// trait against the UDS broker API.
pub trait TicketValidator: Send + Sync + std::fmt::Debug {
    /// `Ok(params)` accepts the session; `Err` rejects it (the handshake
    /// is answered with an error and the connection closed).
    fn validate(&self, ticket: &[u8; TICKET_LENGTH]) -> Result<SessionParams>;
}

/// Maps a human ticket to its wire form: SHA-384 of the UTF-8 bytes.
/// rdphtml5 heritage: only the part before the first '.' is the actual
/// ticket (the broker appends a scrambler after it), so it is stripped
/// here — `ticket.scrambler` and `ticket` map to the same wire bytes.
#[must_use]
pub fn ticket_bytes(human: &str) -> [u8; TICKET_LENGTH] {
    let prefix = human.split('.').next().unwrap_or(human);
    let digest = sha2::Sha384::digest(prefix.as_bytes());
    let mut out = [0u8; TICKET_LENGTH];
    out.copy_from_slice(&digest);
    out
}

/// Laboratory validator: only the exact startup ticket passes.
#[derive(Debug)]
pub struct OneshotValidator {
    ticket: [u8; TICKET_LENGTH],
}

impl OneshotValidator {
    /// Wraps the ticket this server instance generated/read.
    #[must_use]
    pub fn new(ticket: [u8; TICKET_LENGTH]) -> Self {
        Self { ticket }
    }
}

impl TicketValidator for OneshotValidator {
    fn validate(&self, ticket: &[u8; TICKET_LENGTH]) -> Result<SessionParams> {
        if ticket == &self.ticket {
            Ok(SessionParams::default())
        } else {
            bail!("invalid ticket")
        }
    }
}

/// Reference-mode validator: the fixed human ticket from the config.
#[derive(Debug)]
pub struct StandaloneValidator {
    expected: [u8; TICKET_LENGTH],
    params: SessionParams,
}

impl StandaloneValidator {
    /// Builds the validator from the standalone section.
    #[must_use]
    pub fn new(config: &StandaloneConfig) -> Self {
        Self {
            expected: ticket_bytes(&config.ticket),
            params: SessionParams {
                user: config.user.clone(),
                password: config.password.clone(),
                domain: config.domain.clone(),
            },
        }
    }
}

impl TicketValidator for StandaloneValidator {
    fn validate(&self, ticket: &[u8; TICKET_LENGTH]) -> Result<SessionParams> {
        if ticket == &self.expected {
            Ok(self.params.clone())
        } else {
            bail!("invalid ticket")
        }
    }
}

impl ServerConfig {
    /// Parses and validates the config, resolving secrets.
    pub fn validated(self) -> Result<ResolvedConfig> {
        // The reference server only speaks the laboratory modes; pq tickets
        // are validated by the service against the broker.
        if self.connection_method == ConnectionMethod::Pq {
            let broker = self
                .broker
                .as_ref()
                .context("connection_method = \"pq\" requires a [broker] section")?;
            anyhow::ensure!(!broker.url.is_empty(), "broker.url must not be empty");
            anyhow::ensure!(!broker.token.is_empty(), "broker.token must not be empty");
            bail!("connection_method = \"pq\" is only supported by the service");
        }
        anyhow::ensure!(
            !self.shared_key.is_empty(),
            "shared_key must not be empty (hex, 32 bytes)"
        );
        let shared_key = protocol::hex::decode(&self.shared_key).context("bad shared_key hex")?;
        let key: [u8; consts::KEY_LENGTH] = shared_key;
        let validator: Box<dyn TicketValidator> = match self.connection_method {
            ConnectionMethod::Oneshot => {
                let mut ticket = [0u8; TICKET_LENGTH];
                getrandom::fill(&mut ticket)
                    .map_err(|e| anyhow::anyhow!("ticket generation: {e}"))?;
                println!("ticket: {}", protocol::hex::encode(&ticket));
                Box::new(OneshotValidator::new(ticket))
            }
            ConnectionMethod::Standalone => {
                let standalone = self
                    .standalone
                    .context("connection_method = \"standalone\" requires a [standalone] section")?;
                anyhow::ensure!(
                    !standalone.ticket.is_empty(),
                    "standalone.ticket must not be empty"
                );
                Box::new(StandaloneValidator::new(&standalone))
            }
            // Handled above with an early bail (the reference server does
            // not speak pq; only the service does).
            ConnectionMethod::Pq => unreachable!("pq mode bails out before validation"),
        };
        Ok(ResolvedConfig {
            bind: self.bind,
            key,
            validator,
        })
    }
}

/// A validated config, ready to serve.
pub struct ResolvedConfig {
    /// Listen address.
    pub bind: SocketAddr,
    /// Shared key for the handshake.
    pub key: [u8; consts::KEY_LENGTH],
    /// The ticket validator for the chosen connection method.
    pub validator: Box<dyn TicketValidator>,
}

impl std::fmt::Debug for ResolvedConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedConfig")
            .field("bind", &self.bind)
            .field("validator", &self.validator)
            .finish_non_exhaustive()
    }
}

/// Loads a config file from an explicit path.
pub fn load(path: &Path) -> Result<ServerConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
}

/// The platform fallback config directory.
#[must_use]
pub fn platform_config_dir() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\ProgramData\VirtualCable\UDSRP")
    } else {
        PathBuf::from("/etc/uds-rp")
    }
}

/// Search order: cwd, executable directory, platform config directory.
#[must_use]
pub fn find_config_file() -> Option<PathBuf> {
    let mut candidates = vec![PathBuf::from(CONFIG_FILE_NAME)];
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join(CONFIG_FILE_NAME));
    }
    candidates.push(platform_config_dir().join(CONFIG_FILE_NAME));
    candidates.into_iter().find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_HEX: &str =
        "733b5b19754ebb4af75efa1b885e078116b85abb8488c4381078bf5fcb90958c";

    #[test]
    fn human_ticket_maps_to_48_bytes() {
        let a = ticket_bytes("lab-ticket");
        let b = ticket_bytes("lab-ticket");
        let c = ticket_bytes("other");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), TICKET_LENGTH);
    }

    #[test]
    fn ticket_scrambler_suffix_is_ignored() {
        assert_eq!(ticket_bytes("lab.scrambler"), ticket_bytes("lab"));
    }

    #[test]
    fn standalone_accepts_the_fixed_ticket() {
        let config = StandaloneConfig {
            ticket: "lab-ticket".to_string(),
            user: Some("alice".to_string()),
            password: Some("secret".to_string()),
            ..StandaloneConfig::default()
        };
        let validator = StandaloneValidator::new(&config);
        let params = validator
            .validate(&ticket_bytes("lab-ticket"))
            .expect("valid ticket");
        assert_eq!(params.user.as_deref(), Some("alice"));
        assert!(validator.validate(&ticket_bytes("nope")).is_err());
    }

    #[test]
    fn oneshot_accepts_only_the_exact_ticket() {
        let validator = OneshotValidator::new([7u8; TICKET_LENGTH]);
        assert!(validator.validate(&[7u8; TICKET_LENGTH]).is_ok());
        assert!(validator.validate(&[8u8; TICKET_LENGTH]).is_err());
    }

    #[test]
    fn parses_a_standalone_config() {
        let text = format!(
            r#"
connection_method = "standalone"
shared_key = "{KEY_HEX}"
bind = "0.0.0.0:19790"

[standalone]
ticket = "lab-ticket"
user = "alice"
password = "secret"
width = 1680
height = 1050
smartcard = true
"#
        );
        let config: ServerConfig = toml::from_str(&text).expect("parse");
        assert_eq!(config.connection_method, ConnectionMethod::Standalone);
        let resolved = config.validated().expect("validated");
        let params = resolved
            .validator
            .validate(&ticket_bytes("lab-ticket"))
            .expect("valid ticket");
        assert_eq!(params.user.as_deref(), Some("alice"));
        assert!(resolved.validator.validate(&[0u8; TICKET_LENGTH]).is_err());
    }

    #[test]
    fn standalone_requires_its_section() {
        let text = format!(
            r#"
connection_method = "standalone"
shared_key = "{KEY_HEX}"
"#
        );
        let config: ServerConfig = toml::from_str(&text).expect("parse");
        assert!(config.validated().is_err());
    }

    #[test]
    fn rejects_an_empty_shared_key() {
        let config = ServerConfig::default();
        assert!(config.validated().is_err());
    }
}
