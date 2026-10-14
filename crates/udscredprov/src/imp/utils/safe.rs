//! Owning wrapper for Win32 `HANDLE`s (named pipe handles here): closes the
//! handle on drop of the last clone. Taken from the reference provider.

use std::sync::{Arc, RwLock};
use windows::Win32::Foundation::{CloseHandle, HANDLE};

#[derive(Debug)]
struct HandleInner {
    handle: HANDLE,
    owned: bool,
}

// Win32 HANDLEs are process-wide and safe to use from any thread.
unsafe impl Send for HandleInner {}
unsafe impl Sync for HandleInner {}

/// Shareable, owning wrapper around a Win32 `HANDLE`.
#[derive(Debug, Clone)]
pub struct SafeHandle {
    inner: Arc<RwLock<HandleInner>>,
}

impl SafeHandle {
    /// Creates a `SafeHandle` that owns the handle (closes it on drop), unless
    /// the handle is invalid.
    pub fn new(handle: HANDLE) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HandleInner {
                handle,
                owned: !handle.is_invalid(),
            })),
        }
    }

    /// Borrows the raw handle.
    pub fn get(&self) -> HANDLE {
        self.inner.read().unwrap().handle
    }

    /// Whether the wrapped handle is valid.
    pub fn is_valid(&self) -> bool {
        !self.inner.read().unwrap().handle.is_invalid()
    }

    /// Closes the handle early (idempotent) and marks it invalid.
    pub fn clear(&self) {
        let mut inner = self.inner.write().unwrap();
        if inner.owned && !inner.handle.is_invalid() {
            unsafe {
                let _ = CloseHandle(inner.handle);
            }
        }
        inner.handle = HANDLE::default();
        inner.owned = false;
    }
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        if self.owned && !self.handle.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

impl std::fmt::Display for SafeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SafeHandle({:p})", self.inner.read().unwrap().handle.0)
    }
}

impl Default for SafeHandle {
    fn default() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HandleInner {
                handle: HANDLE::default(),
                owned: false,
            })),
        }
    }
}
