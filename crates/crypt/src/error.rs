//! Errores del crate `crypt`.

/// Errores de cifrado, descifrado y framing criptográfico.
#[derive(Debug, thiserror::Error)]
pub enum CryptError {
    /// Fallo de AEAD: tag inválido, AAD manipulado o clave incorrecta.
    #[error("AEAD verification failed")]
    Aead,
    /// Paquete repetido o más viejo que el último `seq` procesado.
    #[error("replay detected: got seq {got}, expected >= {expected}")]
    Replay {
        /// `seq` recibido.
        got: u64,
        /// Mínimo `seq` aceptable.
        expected: u64,
    },
    /// Paquete demasiado corto para contener cabecera + tag.
    #[error("packet too short: {0} bytes")]
    PacketTooShort(usize),
    /// Payload demasiado grande para el campo `len` de 16 bits.
    #[error("packet too large: {0} bytes of plaintext")]
    PacketTooLarge(usize),
    /// HKDF no pudo expandir el material pedido.
    #[error("key derivation failed")]
    KeyDerivation,
}
