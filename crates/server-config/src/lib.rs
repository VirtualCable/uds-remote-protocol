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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            connection_method: ConnectionMethod::Oneshot,
            shared_key: String::new(),
            bind: SocketAddr::from(([0, 0, 0, 0], 19790)),
            standalone: None,
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
}

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
            fps: 30,
            preset: "vp9-balanced".to_string(),
            output_name: None,
            channels: ChannelSwitches {
                audio: true,
                clipboard: true,
                storage: false,
                smartcard: false,
            },
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
