//! # uds-rp
//!
//! Fachada pública del framework de protocolo de escritorio remoto UDS.
//! Re-exporta los crates internos para consumo desde el launcher.

pub use client;
pub use codec;
pub use crypt;
pub use protocol as core;
pub use server;
pub use transport;
