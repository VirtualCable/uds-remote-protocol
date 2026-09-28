//! # server
//!
//! Lado servidor del protocolo de escritorio remoto.
//!
//! La captura de pantalla y la inyección de input van detrás de un trait de
//! plataforma (`PlatformBackend`), de modo que el núcleo de sesión es
//! portable. La implementación de referencia es Windows (Desktop Duplication
//! API + `SendInput`), detrás de la feature `windows-capture`; Linux/macOS
//! podrán añadirse en el futuro sin tocar el core.
