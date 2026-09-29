//! Stream cifrado extremo a extremo sobre cualquier [`ReliableTransport`].
//!
//! Esta capa es idéntica tanto en conexión directa como a través del futuro
//! túnel: aplica el handshake Hello y el framing cifrado de `crypt`
//! (registro `|seq 8B|len 2B|channel_id cifrado|payload|tag|`, ver estudio §6).

use crypt::{SessionKeys, StreamCrypt, StreamDecrypt, consts, derive_session_keys};
use protocol::{ChannelId, Command, Hello, PROTOCOL_VERSION, TICKET_LENGTH};

use crate::error::{Result, TransportError};
use crate::traits::ReliableTransport;

/// Stream fiable con handshake Hello y cifrado E2E por encima de un
/// [`ReliableTransport`] crudo (TCP directo o stream de túnel).
pub struct EncryptedStream<T: ReliableTransport> {
    inner: T,
    tx: StreamCrypt,
    rx: StreamDecrypt,
    peer_hello: Option<Hello>,
    // Buffer de recepción reutilizable (sin allocs en el hot path, riesgos r2 §2.1).
    rx_buf: Vec<u8>,
}

impl<T: ReliableTransport> EncryptedStream<T> {
    /// Crea el stream cifrado con claves de emisión y recepción explícitas.
    #[must_use]
    pub fn new(inner: T, tx_key: &[u8; consts::KEY_LENGTH], rx_key: &[u8; consts::KEY_LENGTH]) -> Self {
        Self {
            inner,
            tx: StreamCrypt::new(tx_key),
            rx: StreamDecrypt::new(rx_key),
            peer_hello: None,
            rx_buf: Vec::with_capacity(2048),
        }
    }

    /// Handshake de cliente: envía el Hello (`Open` + ticket) y deriva las
    /// claves de sesión del ticket y la clave precompartida (PSK de la PoC;
    /// ML-KEM llega después).
    pub async fn client_handshake(
        mut inner: T,
        ticket: &[u8; TICKET_LENGTH],
        key: &[u8; consts::KEY_LENGTH],
    ) -> Result<Self> {
        let hello = Hello {
            version: PROTOCOL_VERSION,
            command: Command::Open,
            ticket: *ticket,
        };
        inner.send(&hello.encode()).await?;
        let keys = derive_session_keys(ticket, key)?;
        Ok(Self::with_keys(inner, &keys, Role::Client))
    }

    /// Handshake de servidor: lee y valida el Hello del peer y deriva las
    /// claves de sesión del ticket recibido y la clave precompartida.
    pub async fn server_handshake(mut inner: T, key: &[u8; consts::KEY_LENGTH]) -> Result<Self> {
        let mut buf = [0u8; Hello::ENCODED_LEN];
        read_full(&mut inner, &mut buf).await?;
        let hello = Hello::decode(&buf)?;
        let keys = derive_session_keys(&hello.ticket, key)?;
        let mut stream = Self::with_keys(inner, &keys, Role::Server);
        stream.peer_hello = Some(hello);
        Ok(stream)
    }

    fn with_keys(inner: T, keys: &SessionKeys, role: Role) -> Self {
        let (tx_key, rx_key) = match role {
            Role::Client => (&keys.c2s, &keys.s2c),
            Role::Server => (&keys.s2c, &keys.c2s),
        };
        Self::new(inner, tx_key, rx_key)
    }

    /// Hello del peer (solo disponible en el lado servidor tras el handshake).
    #[must_use]
    pub fn peer_hello(&self) -> Option<&Hello> {
        self.peer_hello.as_ref()
    }

    /// Envía `payload` cifrado en el canal indicado, como un único registro.
    pub async fn send(&mut self, channel: ChannelId, payload: &[u8]) -> Result<()> {
        if payload.len() > consts::MAX_PLAINTEXT {
            return Err(TransportError::TooLarge {
                size: payload.len(),
                max: consts::MAX_PLAINTEXT,
            });
        }
        let record = self.tx.encrypt(channel, payload)?;
        self.inner.send(&record).await
    }

    /// Recibe un registro completo: devuelve canal y longitud del payload
    /// descifrado, copiado en `buf`.
    pub async fn recv_into(&mut self, buf: &mut [u8]) -> Result<(ChannelId, usize)> {
        let mut header = [0u8; consts::STREAM_HEADER_SIZE];
        read_full(&mut self.inner, &mut header).await?;
        let len = u16::from_be_bytes([header[8], header[9]]) as usize;

        self.rx_buf.clear();
        self.rx_buf.extend_from_slice(&header);
        self.rx_buf.resize(consts::STREAM_HEADER_SIZE + len, 0);
        read_full(&mut self.inner, &mut self.rx_buf[consts::STREAM_HEADER_SIZE..]).await?;

        let (channel, plain) = self.rx.decrypt(&self.rx_buf)?;
        if plain.len() > buf.len() {
            return Err(TransportError::TooLarge {
                size: plain.len(),
                max: buf.len(),
            });
        }
        buf[..plain.len()].copy_from_slice(&plain);
        Ok((channel, plain.len()))
    }
}

#[derive(Clone, Copy)]
enum Role {
    Client,
    Server,
}

/// Lee exactamente `buf.len()` bytes del transporte (un `read` de TCP puede
/// devolver menos de lo pedido).
async fn read_full<T: ReliableTransport>(t: &mut T, buf: &mut [u8]) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        filled += t.recv_into(&mut buf[filled..]).await?;
    }
    Ok(())
}
