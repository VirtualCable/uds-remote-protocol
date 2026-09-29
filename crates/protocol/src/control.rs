//! Mensajes del canal 0 (control), serializados con postcard.

use serde::{Deserialize, Serialize};

use crate::channel::ChannelId;
use crate::error::ProtocolError;

/// Capacidad anunciada en la negociación (`SERVER_CAPS` / `CLIENT_CAPS`).
///
/// Lista extensible `(kind, version, params)`: un peer que no conoce un
/// `kind` lo ignora, de modo que añadir canales o codecs no rompe peers
/// antiguos.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    /// Tipo de capacidad (canal, codec de vídeo, codec de audio, ...).
    pub kind: u16,
    /// Versión de la capacidad.
    pub version: u16,
    /// Parámetros opacos específicos del tipo.
    pub params: Vec<u8>,
}

/// Mensajes del canal de control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlMessage {
    /// Capacidades del servidor (canales, codecs, topes de política,
    /// puerto/token UDP cuando exista).
    ServerCaps {
        /// Lista de capacidades ofrecidas.
        caps: Vec<Capability>,
    },
    /// Capacidades del cliente + selección.
    ClientCaps {
        /// Lista de capacidades solicitadas.
        caps: Vec<Capability>,
    },
    /// Petición de apertura de canal.
    ChannelOpen {
        /// ID de canal solicitado.
        id: ChannelId,
        /// Tipo de canal.
        kind: u16,
        /// Versión del canal.
        version: u16,
    },
    /// Confirmación de apertura de canal.
    ChannelOpenAck {
        /// ID de canal confirmado.
        id: ChannelId,
    },
    /// Canal no soportado por este peer; la sesión continúa sin él.
    UnsupportedChannel {
        /// ID de canal rechazado.
        id: ChannelId,
    },
    /// Latido del canal de control.
    Keepalive,
}

impl ControlMessage {
    /// Serializa el mensaje (postcard).
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        Ok(postcard::to_allocvec(self)?)
    }

    /// Deserializa un mensaje; rechaza bytes sobrantes.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        Ok(postcard::from_bytes(bytes)?)
    }
}
