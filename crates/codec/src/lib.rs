//! # codec
//!
//! Abstracción de codecs: el protocolo negocia codecs por capacidades y el
//! core solo ve los traits [`VideoEncoder`]/[`VideoDecoder`] y
//! [`AudioEncoder`]/[`AudioDecoder`].
//!
//! Implementaciones detrás de features (ninguna por defecto):
//! - `codec-vp9`: vídeo VP9 vía libvpx (royalty-free).
//! - `codec-opus`: audio Opus (royalty-free), con límites por dirección
//!   (bitrate, sample rate) negociados en el handshake.
