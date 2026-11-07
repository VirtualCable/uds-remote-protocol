// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Standalone provider: the TOML config is the source of truth. The fixed
//! human ticket is validated by constant-time compare; the grant carries
//! the configured key, credentials and channel switches.

use anyhow::{Result, bail};

use super::{ConnectionProvider, SessionGrant};
use super::payload::{ChannelPolicy, DisplayPolicy, SessionCredentials, VideoPolicy};

/// Standalone (broker-less) provider: one fixed ticket, fixed key.
pub struct StandaloneProvider {
    expected_ticket: [u8; protocol::TICKET_LENGTH],
    grant: SessionGrant,
}

impl StandaloneProvider {
    /// Builds the provider from the server config (requires the
    /// `[standalone]` section; the caller checked `connection_method`).
    pub fn new(config: &server_config::ServerConfig) -> Result<Self> {
        let standalone = config
            .standalone
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("connection_method = \"pq\" unset but [standalone] missing"))?;
        anyhow::ensure!(
            !standalone.ticket.is_empty(),
            "standalone.ticket must not be empty"
        );
        let key: [u8; 32] = protocol::hex::decode(&config.shared_key)?;
        let credentials = match (&standalone.user, &standalone.password) {
            (Some(user), Some(password)) => Some(SessionCredentials {
                user: user.trim().to_string(),
                domain: standalone.domain.clone().unwrap_or_else(|| ".".to_string()),
                password: password.clone(),
            }),
            _ => None,
        };
        let channels = ChannelPolicy {
            audio: standalone.channels.audio,
            clipboard: standalone.channels.clipboard,
            storage: standalone.channels.storage,
            smartcard: standalone.channels.smartcard,
            webcam: false,
            printer: false,
            lock: standalone.channels.lock,
        };
        let grant = SessionGrant {
            key,
            credentials,
            display: Some(DisplayPolicy {
                width: standalone.width,
                height: standalone.height,
                fps: standalone.fps,
                fixed: false,
            }),
            video: Some(VideoPolicy {
                preset: Some(standalone.preset.clone()),
                ladder: Some(standalone.ladder.to_wire()),
                adaptive_fps: standalone
                    .adaptive_fps
                    .as_ref()
                    .map(server_config::AdaptiveFpsTuning::to_wire),
            }),
            channels,
            wallpaper_off: standalone.wallpaper_off,
            console_conflict: standalone.console_conflict,
            console_conflict_reveal_user: standalone.console_conflict_reveal_user,
            console_conflict_timeout_secs: standalone.console_conflict_timeout_secs,
            console_takeover: standalone.console_takeover,
            notify: None,
        };
        Ok(Self {
            expected_ticket: server_config::ticket_bytes(&standalone.ticket),
            grant,
        })
    }
}

#[async_trait::async_trait]
impl ConnectionProvider for StandaloneProvider {
    async fn start(
        &self,
        ticket: &[u8; protocol::TICKET_LENGTH],
        _client_ip: &str,
    ) -> Result<SessionGrant> {
        if !constant_time_eq(ticket, &self.expected_ticket) {
            bail!("invalid ticket");
        }
        Ok(self.grant.clone())
    }

    fn mode(&self) -> &'static str {
        "standalone"
    }
}

/// Constant-time byte compare (tickets are not secret, but leaking the
/// mismatch position through timing costs nothing to avoid).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use server_config::{ConnectionMethod, ServerConfig, StandaloneConfig};

    fn config() -> ServerConfig {
        ServerConfig {
            connection_method: ConnectionMethod::Standalone,
            shared_key: "733b5b19754ebb4af75efa1b885e078116b85abb8488c4381078bf5fcb90958c"
                .to_string(),
            standalone: Some(StandaloneConfig {
                ticket: "lab-ticket".to_string(),
                user: Some("dkmaster".to_string()),
                password: Some("Temporal2012".to_string()),
                ..StandaloneConfig::default()
            }),
            ..ServerConfig::default()
        }
    }

    #[tokio::test]
    async fn accepts_the_configured_ticket() {
        let provider = StandaloneProvider::new(&config()).unwrap();
        let ticket = server_config::ticket_bytes("lab-ticket");
        let grant = provider.start(&ticket, "127.0.0.1").await.unwrap();
        assert_eq!(grant.key[0], 0x73);
        assert_eq!(
            grant.credentials.as_ref().map(|c| c.user.as_str()),
            Some("dkmaster")
        );
    }

    #[tokio::test]
    async fn rejects_a_wrong_ticket() {
        let provider = StandaloneProvider::new(&config()).unwrap();
        let ticket = server_config::ticket_bytes("wrong-ticket");
        assert!(provider.start(&ticket, "127.0.0.1").await.is_err());
    }

    #[tokio::test]
    async fn the_scrambler_suffix_is_ignored() {
        // rdphtml5 heritage: "ticket.scrambler" maps like "ticket".
        let provider = StandaloneProvider::new(&config()).unwrap();
        let ticket = server_config::ticket_bytes("lab-ticket.zzz");
        assert!(provider.start(&ticket, "127.0.0.1").await.is_ok());
    }
}
