//! Constantes del formato criptográfico.

/// Longitud del ticket de sesión (salt de HKDF), en bytes.
pub const TICKET_LENGTH: usize = 48;
/// Longitud del shared secret y de cada clave derivada, en bytes.
pub const KEY_LENGTH: usize = 32;
/// Longitud del tag AES-256-GCM, en bytes.
pub const TAG_LENGTH: usize = 16;
/// Cabecera en claro del paquete de stream: `seq (8B BE) + len (2B BE)`.
pub const STREAM_HEADER_SIZE: usize = 10;
/// Tamaño del campo `channel_id`, cifrado dentro del paquete.
pub const CHANNEL_ID_SIZE: usize = 2;
/// Máximo plaintext por paquete: `len` es u16 y cubre `channel_id + ciphertext + tag`.
pub const MAX_PLAINTEXT: usize = u16::MAX as usize - CHANNEL_ID_SIZE - TAG_LENGTH;
