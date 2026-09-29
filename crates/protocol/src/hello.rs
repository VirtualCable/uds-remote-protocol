//! Handshake inicial (pre-cifrado): firma + versión + comando + ticket.

use crate::error::ProtocolError;

/// Firma al inicio del handshake ("UDSRP/01").
pub const HANDSHAKE_SIGNATURE: [u8; 8] = *b"UDSRP/01";

/// Versión actual del protocolo.
pub const PROTOCOL_VERSION: u16 = 1;

/// Longitud del ticket de sesión.
pub const TICKET_LENGTH: usize = 48;

/// Comandos del handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Command {
    /// Prueba de alcance/validez (sin abrir sesión).
    Test = 0,
    /// Apertura de sesión nueva.
    Open = 1,
    /// Recuperación de sesión tras un corte (mismo ticket y seqs).
    Recover = 2,
}

impl Command {
    fn from_byte(b: u8) -> Result<Self, ProtocolError> {
        match b {
            0 => Ok(Self::Test),
            1 => Ok(Self::Open),
            2 => Ok(Self::Recover),
            other => Err(ProtocolError::UnknownCommand(other)),
        }
    }
}

/// Mensaje HELLO del handshake (8B firma + 2B versión + 1B comando + 48B ticket).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// Versión del protocolo del emisor.
    pub version: u16,
    /// Comando solicitado.
    pub command: Command,
    /// Ticket de sesión emitido por el broker (o precompartido en la PoC).
    pub ticket: [u8; TICKET_LENGTH],
}

impl Hello {
    /// Tamaño en wire del mensaje codificado.
    pub const ENCODED_LEN: usize = 8 + 2 + 1 + TICKET_LENGTH;

    /// Codifica el mensaje a su forma binaria.
    #[must_use]
    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[..8].copy_from_slice(&HANDSHAKE_SIGNATURE);
        out[8..10].copy_from_slice(&self.version.to_be_bytes());
        out[10] = self.command as u8;
        out[11..].copy_from_slice(&self.ticket);
        out
    }

    /// Decodifica y valida un mensaje HELLO.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        if bytes.len() < Self::ENCODED_LEN {
            return Err(ProtocolError::Truncated);
        }
        if bytes[..8] != HANDSHAKE_SIGNATURE {
            return Err(ProtocolError::BadSignature);
        }
        Ok(Self {
            version: u16::from_be_bytes(bytes[8..10].try_into().expect("slice de 2 bytes")),
            command: Command::from_byte(bytes[10])?,
            ticket: bytes[11..11 + TICKET_LENGTH]
                .try_into()
                .expect("slice de 48 bytes"),
        })
    }
}
