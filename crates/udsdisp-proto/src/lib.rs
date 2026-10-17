//! Shared, host-independent pieces of the UDS Virtual Display driver:
//! display modes, the IOCTL wire protocol and the device interface GUID.
//!
//! Lives in its own crate (no Windows dependencies, unit-tested on Linux) so
//! both the UMDF driver (`udsdisp`) and its userland clients
//! (`udsdisp-ctl`, the orchestrator service) can use it without pulling the
//! WDK build machinery in.

pub mod logic;
