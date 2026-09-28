//! # crypt
//!
//! Criptografía del protocolo, siguiendo el modelo de tunnel-server UDS:
//!
//! - AES-256-GCM por paquete (el tag de 16B hace de MAC; no hay HMAC aparte).
//! - Nonce derivado de contador `seq` monotónico; anti-replay estricto en
//!   stream y por ventana deslizante en datagramas.
//! - Derivación de claves con HKDF-SHA256 (salt = ticket de 48B).
//! - ML-KEM-768 para el intercambio de secreto (decisión abierta, fase 1+).
