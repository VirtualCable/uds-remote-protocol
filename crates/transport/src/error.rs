//! Errores del crate `transport`.

use protocol::ProtocolError;

/// Errores de transporte y de la capa de stream cifrado.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Error de E/S del socket subyacente.
    #[error("error de E/S: {0}")]
    Io(#[from] std::io::Error),
    /// Error del protocolo (handshake Hello inválido, etc.).
    #[error("error de protocolo: {0}")]
    Protocol(#[from] ProtocolError),
    /// Error criptográfico (descifrado, anti-replay, etc.).
    #[error("error de cifrado: {0}")]
    Crypt(#[from] crypt::CryptError),
    /// El payload excede el máximo de un registro.
    #[error("payload demasiado grande: {size} bytes (máximo {max})")]
    TooLarge {
        /// Tamaño del payload intentado.
        size: usize,
        /// Máximo permitido.
        max: usize,
    },
    /// El peer cerró la conexión.
    #[error("conexión cerrada por el peer")]
    Closed,
}

/// Resultado estándar del crate.
pub type Result<T> = std::result::Result<T, TransportError>;
