//! Identificadores de canal y rangos (estudio §5, riesgos.md r2 §4.2).

/// Identificador de canal lógico.
pub type ChannelId = u16;

/// Canal 0: control (handshake post-login, negociación, keepalive, métricas).
pub const CONTROL: ChannelId = 0;
/// Canal 1: display (frames de vídeo S→C).
pub const DISPLAY: ChannelId = 1;
/// Canal 2: audio del escritorio remoto (S→C).
pub const AUDIO_OUT: ChannelId = 2;
/// Canal 3: micrófono del cliente (C→S).
pub const AUDIO_IN: ChannelId = 3;
/// Canal 4: input (teclado y ratón, C→S).
pub const INPUT: ChannelId = 4;

/// Último ID reservado a canales core de UDS (`0x0000..=0x00FF`).
pub const CORE_MAX: ChannelId = 0x00FF;
