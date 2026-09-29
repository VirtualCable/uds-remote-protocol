//! Errores del crate `protocol`.

/// Errores de codificación/decodificación del protocolo.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    /// Firma del handshake no reconocida.
    #[error("bad handshake signature")]
    BadSignature,
    /// Comando de handshake desconocido.
    #[error("unknown handshake command: {0:#04X}")]
    UnknownCommand(u8),
    /// Buffer demasiado corto para el mensaje.
    #[error("truncated message")]
    Truncated,
    /// Error de serialización postcard.
    #[error("serialization error: {0}")]
    Serialization(#[from] postcard::Error),
}
