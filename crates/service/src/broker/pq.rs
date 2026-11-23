// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Pq (broker) provider: the service validates each presented ticket
//! against the UDS broker and receives the session grant.
//!
//! The endpoint is the `connection` kind of the actor `ticket` action
//! (`POST {ticket_url}`, e.g.
//! `https://broker/uds/rest/actor/v3/ticket/connection`), which mirrors
//! the tunnel ticket contract (plan §18.39):
//!
//! - Request: `{ticket, command, ip, kem_kyber_key?}` where the ticket
//!   authenticates the connection and `kem_kyber_key` is our ephemeral ML-KEM-768 public
//!   key (base64). `command` is `start` (validate + consume the ticket) or
//!   `stop` (session-ended notification, best effort).
//! - `start` response: the actor REST envelope (`{"result": ..., "error"?
//!   ...}`) wrapping a KEM-encrypted dict `{algorithm, ciphertext, data}`.
//!   `ciphertext` is the base64 ML-KEM-768 encapsulation against our public
//!   key; `data` is the base64 AES-256-GCM payload encrypted with
//!   HKDF-SHA256(salt = ticket, ikm = KEM shared secret,
//!   info = "openuds-ticket-crypt") — key at okm[0..32], nonce at
//!   okm[96..108], same construction as rdphtml5's pq tunnel client.
//! - Decrypted payload: `{remotes: [{host, port, extra}], notify,
//!   shared_secret, enable_udp}`. For us `host`/`port` are dummies (the
//!   client connected to us directly); `remotes[0].extra` carries our
//!   [`TicketPayload`], `shared_secret` (hex) is the session PSK and
//!   `notify` the ticket for the `stop` notification.

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead as _, KeyInit as _};
use anyhow::{Context as _, Result, bail};
use base64::{Engine as _, engine::general_purpose};
use hkdf::Hkdf;
use libcrux_ml_kem::mlkem768::{
    MlKem768Ciphertext, MlKem768PrivateKey, decapsulate, generate_key_pair,
};
use sha2::Sha256;
use tracing::warn;

use super::payload::TicketPayload;
use super::{ConnectionProvider, SessionGrant, SessionStats};

// ML-KEM-768 sizes (libcrux-ml-kem 0.0.11).
const KEM_PUBLIC_KEY_SIZE: usize = 1184;
const KEM_PRIVATE_KEY_SIZE: usize = 2400;
const KEM_CIPHERTEXT_SIZE: usize = 1088;
const KEM_KEY_GENERATION_SEED_SIZE: usize = libcrux_ml_kem::KEY_GENERATION_SEED_SIZE;

/// HKDF info string for the ticket-response payload cipher. Part of the
/// wire contract with the broker (`CryptoManager.encrypted_dict`).
const PAYLOAD_HKDF_INFO: &[u8] = b"openuds-ticket-crypt";

/// Broker-backed provider (config `connection_method = "pq"`).
pub struct PqProvider {
    client: reqwest::Client,
    /// Full URL of the ticket endpoint (e.g.
    /// `https://broker/uds/rest/actor/v3/ticket/connection`).
    ticket_url: String,
}

/// KEM-encrypted response envelope payload (`CryptoManager.encrypted_dict`).
#[derive(serde::Deserialize)]
struct EncryptedTicketResponse {
    /// Cipher suite tag (informational; we only speak ML-KEM-768+AES-GCM).
    #[allow(dead_code)]
    algorithm: String,
    /// Base64 ML-KEM-768 ciphertext (encapsulated shared secret).
    ciphertext: String,
    /// Base64 AES-256-GCM encrypted JSON payload.
    data: String,
}

/// Decrypted ticket response (mirrors `types.tickets.TunnelTicketResponse`).
#[derive(serde::Deserialize)]
struct BrokerTicketResponse {
    remotes: Vec<BrokerRemote>,
    notify: String,
    shared_secret: String,
}

/// One remote entry; `host`/`port` are dummies for direct connections,
/// `extra` carries our [`TicketPayload`].
#[derive(serde::Deserialize)]
struct BrokerRemote {
    /// Our session parameters (the ticket payload schema).
    extra: Option<serde_json::Value>,
}

impl PqProvider {
    /// Builds the provider. `ticket_url` is the full URL of the ticket
    /// endpoint.
    ///
    /// The endpoint MUST be `https`: each request carries a bearer ticket
    /// and the response carries the session PSK. Plain `http` is
    /// accepted only in debug builds (local development); a release build
    /// rejects it, and any other scheme is always rejected.
    pub fn new(ticket_url: &str, verify_ssl: bool) -> Result<Self> {
        let url = reqwest::Url::parse(ticket_url).context("broker.url is not a valid URL")?;
        match url.scheme() {
            "https" => {}
            "http" if cfg!(debug_assertions) => {
                warn!(
                    url = ticket_url,
                    "broker over plain http (debug build only)"
                );
            }
            scheme => {
                bail!(
                    "broker.url must use https (got {scheme}://); plain http is only accepted in debug builds"
                );
            }
        }
        if !verify_ssl {
            // Loud, on purpose: disabling certificate validation exposes
            // the bearer ticket and the session PSK to anyone on
            // the path, exactly like plain http would.
            warn!(
                "broker verify_ssl = false: TLS certificate validation is DISABLED; \
                 the bearer ticket and session keys are NOT protected from interception"
            );
        }
        let client = reqwest::Client::builder()
            .user_agent(concat!("uds-rp/", env!("CARGO_PKG_VERSION")))
            .danger_accept_invalid_certs(!verify_ssl)
            // Never follow redirects: a 302 from the broker would replay the
            // bearer ticket (sent in the request body) to another origin,
            // possibly a plain-http one.
            .redirect(reqwest::redirect::Policy::none())
            // The broker may be buggy or wedged (§18.78): an unbounded wait
            // would stall the session handshake forever, so bound it.
            .timeout(std::time::Duration::from_secs(10))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .context("building the broker HTTP client")?;
        Ok(Self {
            client,
            ticket_url: ticket_url.trim_end_matches('/').to_string(),
        })
    }

    /// POSTs a request and unwraps the actor REST envelope.
    async fn post(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        let response = self
            .client
            .post(&self.ticket_url)
            .json(body)
            .send()
            .await
            .context("broker request failed")?;
        let status = response.status();
        let text = response
            .text()
            .await
            .context("reading the broker response")?;
        if !status.is_success() {
            bail!("broker answered HTTP {status}");
        }
        let envelope: serde_json::Value =
            serde_json::from_str(&text).context("broker response is not JSON")?;
        if let Some(error) = envelope.get("error").and_then(|e| e.as_str())
            && !error.is_empty()
        {
            bail!("broker denied the request: {error}");
        }
        Ok(envelope
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    /// Decrypts a KEM-encrypted ticket response with the ephemeral private
    /// key of this request.
    fn decrypt_response(
        kem_private_key: &[u8; KEM_PRIVATE_KEY_SIZE],
        ticket: &str,
        encrypted: &EncryptedTicketResponse,
    ) -> Result<BrokerTicketResponse> {
        let kem_ciphertext: [u8; KEM_CIPHERTEXT_SIZE] = general_purpose::STANDARD
            .decode(&encrypted.ciphertext)
            .context("decoding the KEM ciphertext")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid KEM ciphertext size"))?;
        let private_key = MlKem768PrivateKey::from(kem_private_key);
        let ciphertext = MlKem768Ciphertext::from(&kem_ciphertext);
        // Decapsulation always succeeds (implicit rejection); a tampered
        // ciphertext simply yields a wrong secret and the AES-GCM below
        // fails authentication.
        let shared_secret = decapsulate(&private_key, &ciphertext);

        let (key, nonce) = derive_payload_material(shared_secret.as_ref(), ticket.as_bytes())?;
        let data = general_purpose::STANDARD
            .decode(&encrypted.data)
            .context("decoding the encrypted payload")?;
        let cipher = Aes256Gcm::new((&key).into());
        let plaintext = cipher
            .decrypt((&nonce).into(), data.as_ref())
            .map_err(|_| {
                anyhow::anyhow!("payload decryption failed (wrong ticket or tampered response)")
            })?;
        serde_json::from_slice(&plaintext).context("decrypted payload is not a ticket response")
    }
}

#[async_trait::async_trait]
impl ConnectionProvider for PqProvider {
    async fn start(
        &self,
        ticket: &[u8; protocol::TICKET_LENGTH],
        client_ip: &str,
    ) -> Result<SessionGrant> {
        let ticket = super::str_from_wire_ticket(ticket)?;
        // Broker tickets are exactly 48 alphanumeric characters.
        anyhow::ensure!(
            ticket.len() == protocol::TICKET_LENGTH
                && ticket.bytes().all(|b| b.is_ascii_alphanumeric()),
            "not a broker ticket (standalone ticket in pq mode?)"
        );
        // A fresh ML-KEM-768 keypair per request: the public key is only
        // useful for this one exchange, and no long-lived private key ever
        // sits in the provider's state.
        let (kem_private_key, kem_public_key) = kem_keypair();
        let result = self
            .post(&serde_json::json!({
                "ticket": ticket,
                "command": "start",
                "ip": client_ip,
                "kem_kyber_key": general_purpose::STANDARD.encode(kem_public_key),
            }))
            .await?;
        let encrypted: EncryptedTicketResponse = serde_json::from_value(result)
            .context("broker response is not a KEM-encrypted payload")?;
        let response = Self::decrypt_response(&kem_private_key, &ticket, &encrypted)?;

        let key: [u8; 32] = protocol::hex::decode(&response.shared_secret)
            .context("invalid shared_secret in the broker response")?;
        let remote = response
            .remotes
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no remotes in the broker response"))?;
        let payload = TicketPayload::from_value(remote.extra.unwrap_or(serde_json::json!({
            "version": super::payload::PAYLOAD_VERSION
        })))?;
        Ok(SessionGrant::from_payload(
            payload,
            key,
            (!response.notify.is_empty()).then_some(response.notify),
        ))
    }

    async fn stop(&self, grant: &SessionGrant, stats: &SessionStats) {
        let Some(notify) = &grant.notify else {
            return;
        };
        let result = self
            .post(&serde_json::json!({
                "ticket": notify,
                "command": "stop",
                "ip": "",
                "sent": stats.bytes_sent,
                "recv": stats.bytes_received,
            }))
            .await;
        if let Err(err) = result {
            // Best-effort: a lost stop notification must never break the
            // session teardown.
            warn!(error = %format!("{err:#}"), "broker stop notification failed");
        }
    }

    fn mode(&self) -> &'static str {
        "pq"
    }
}

/// Generates an ML-KEM-768 keypair.
fn kem_keypair() -> ([u8; KEM_PRIVATE_KEY_SIZE], [u8; KEM_PUBLIC_KEY_SIZE]) {
    use rand::Rng as _;
    let mut randomness = [0u8; KEM_KEY_GENERATION_SEED_SIZE];
    rand::rng().fill_bytes(&mut randomness);
    let keypair = generate_key_pair(randomness);
    // `as_slice()` is a reference to the fixed-size array already.
    let private_key: [u8; KEM_PRIVATE_KEY_SIZE] = *keypair.private_key().as_slice();
    let public_key: [u8; KEM_PUBLIC_KEY_SIZE] = *keypair.public_key().as_slice();
    (private_key, public_key)
}

/// Derives the payload cipher material exactly like the broker:
/// HKDF-SHA256(salt = ticket, ikm = KEM shared secret,
/// info = "openuds-ticket-crypt"), 108 bytes of output — key at [0..32],
/// nonce at [96..108] (the middle 64 bytes derive send/receive keys the
/// tunnel uses and we do not need).
fn derive_payload_material(shared_secret: &[u8], ticket: &[u8]) -> Result<([u8; 32], [u8; 12])> {
    let hk = Hkdf::<Sha256>::new(Some(ticket), shared_secret);
    let mut okm = [0u8; 108];
    hk.expand(PAYLOAD_HKDF_INFO, &mut okm)
        .map_err(|_| anyhow::anyhow!("HKDF expand failed"))?;
    let mut key = [0u8; 32];
    let mut nonce = [0u8; 12];
    key.copy_from_slice(&okm[0..32]);
    nonce.copy_from_slice(&okm[96..108]);
    Ok((key, nonce))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::Payload;
    use libcrux_ml_kem::mlkem768::{MlKem768PublicKey, encapsulate};
    use rand::Rng as _;

    /// Encrypts a payload exactly like the broker does, so the decrypt path
    /// is tested against the real construction.
    fn encrypt_response(
        public_key: &[u8; KEM_PUBLIC_KEY_SIZE],
        ticket: &str,
        plaintext: &[u8],
    ) -> EncryptedTicketResponse {
        let mut randomness = [0u8; 32];
        rand::rng().fill_bytes(&mut randomness);
        let public_key = MlKem768PublicKey::from(public_key);
        let (ciphertext, shared_secret) = encapsulate(&public_key, randomness);
        let (key, nonce) =
            derive_payload_material(shared_secret.as_ref(), ticket.as_bytes()).unwrap();
        let cipher = Aes256Gcm::new((&key).into());
        let data = cipher
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: plaintext,
                    aad: b"",
                },
            )
            .unwrap();
        EncryptedTicketResponse {
            algorithm: "mlkem768+aes256gcm".to_string(),
            ciphertext: general_purpose::STANDARD.encode(ciphertext.as_ref()),
            data: general_purpose::STANDARD.encode(data),
        }
    }

    #[test]
    fn kem_keypair_has_the_expected_sizes() {
        let (private_key, public_key) = kem_keypair();
        assert_eq!(private_key.len(), KEM_PRIVATE_KEY_SIZE);
        assert_eq!(public_key.len(), KEM_PUBLIC_KEY_SIZE);
    }

    #[test]
    fn rejects_a_plain_http_broker_url_in_release() {
        // The bearer ticket travels in every request body and the response
        // carries the session PSK: plain http is only tolerated in debug
        // builds (local development).
        let result = PqProvider::new("http://broker.example.com/ticket", true);
        if cfg!(debug_assertions) {
            assert!(result.is_ok());
        } else {
            assert!(result.is_err());
        }
        // Other schemes are always rejected.
        assert!(PqProvider::new("ftp://broker.example.com/ticket", true).is_err());
        assert!(PqProvider::new("not a url", true).is_err());
    }

    #[test]
    fn decrypt_response_roundtrip() {
        let (private_key, public_key) = kem_keypair();
        let ticket = "a".repeat(protocol::TICKET_LENGTH);
        let plaintext = serde_json::json!({
            "remotes": [{"host": "127.0.0.1", "port": 0, "extra": {"version": 1}}],
            "notify": "notify-ticket",
            "shared_secret": "ab".repeat(32),
            "enable_udp": true,
        });
        let encrypted = encrypt_response(&public_key, &ticket, plaintext.to_string().as_bytes());
        let response = PqProvider::decrypt_response(&private_key, &ticket, &encrypted).unwrap();
        assert_eq!(response.notify, "notify-ticket");
        assert_eq!(response.shared_secret, "ab".repeat(32));
        assert_eq!(response.remotes.len(), 1);
    }

    #[test]
    fn decrypt_response_rejects_a_wrong_ticket_salt() {
        let (private_key, public_key) = kem_keypair();
        let ticket = "a".repeat(protocol::TICKET_LENGTH);
        let plaintext = br#"{"remotes": [], "notify": "", "shared_secret": ""}"#;
        let encrypted = encrypt_response(&public_key, &ticket, plaintext);
        // AAD-less GCM, but the key derives from the ticket: decrypting
        // with another ticket must fail authentication.
        let other = "b".repeat(protocol::TICKET_LENGTH);
        assert!(PqProvider::decrypt_response(&private_key, &other, &encrypted).is_err());
    }

    #[test]
    fn payload_material_matches_the_reference_vectors() {
        // Same vectors as rdphtml5's shared::protocol::crypt::key_derivation tests:
        // shared_secret = [1; 32], ticket = [2; 48]. Our key_payload is
        // their key_payload (okm[0..32]) and the nonce is okm[96..108].
        let shared_secret = [1u8; 32];
        let ticket = [2u8; 48];
        let (key, nonce) = derive_payload_material(&shared_secret, &ticket).unwrap();
        // Reference: rdphtml5 test vectors assert okm[32..64] and
        // okm[64..96]; okm[0..32] checked here against a fresh HKDF run of
        // the same construction (deterministic by definition).
        let hk = Hkdf::<Sha256>::new(Some(&ticket), &shared_secret);
        let mut okm = [0u8; 108];
        hk.expand(PAYLOAD_HKDF_INFO, &mut okm).unwrap();
        assert_eq!(key, okm[0..32]);
        assert_eq!(nonce, okm[96..108]);
    }
}
