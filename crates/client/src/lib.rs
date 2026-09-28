//! # client
//!
//! Lado cliente del protocolo de escritorio remoto: gestión de sesión,
//! decodificación de vídeo/audio, envío de eventos de teclado/ratón y
//! recuperación de conexión (Recover). No incluye GUI: el render lo aporta
//! el consumidor (testclient en desarrollo, launcher UDS en producción).
