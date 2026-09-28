//! # transport
//!
//! Abstracción de transporte del protocolo: el core no sabe si habla con
//! sockets directos o con un túnel intermedio.
//!
//! - [`ReliableTransport`]: stream fiable y ordenado (equivalente TCP).
//! - [`DatagramTransport`]: datagramas no fiables (equivalente UDP), opcional.
//! - [`Connector`]: fábrica de conexiones (lado cliente).
//!
//! Implementaciones:
//! - `DirectConnector` (fase PoC): TCP + UDP reales con probe de detección.
//! - `TunnelConnector` (fase 4, crate aparte): adaptador a tunnel-server UDS.
