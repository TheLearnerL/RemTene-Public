//! Memory pressure event monitoring (stub implementation).
//!
//! This module provides the interface for memory pressure monitoring.
//! The actual macOS dispatch integration will be implemented when we have
//! real RSS data and can validate memory reduction in M3 end-to-end testing.
//!
//! ## Current Status
//!
//! This is a **stub implementation** that:
//! - Provides the public API for memory pressure monitoring
//! - Always returns `None` (no pressure events)
//! - Allows Worker code to integrate the listener without blocking
//!
//! ## Future Work (M3+)
//!
//! When implementing the real listener:
//! 1. Use `dispatch` crate (Apple-maintained) instead of raw FFI
//! 2. Integrate with Worker main loop to call `backend.unload_if_idle()` on pressure
//! 3. Collect real RSS data before/after model unload
//! 4. Validate memory reduction with Activity Monitor or `vmmap`

use std::time::Duration;

/// Memory pressure level reported by the OS.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryPressureLevel {
    /// No memory pressure
    Normal,
    /// Moderate memory pressure (warning)
    Warning,
    /// High memory pressure (critical)
    Critical,
}

/// Memory pressure event listener (stub).
///
/// Current implementation always returns `None` (no pressure events).
/// Real implementation will monitor macOS `dispatch_source_t` memory pressure events.
pub struct MemoryPressureListener;

impl MemoryPressureListener {
    /// Create a new memory pressure listener.
    ///
    /// **Stub**: Does not start any background monitoring.
    pub fn new() -> Self {
        Self
    }

    /// Poll for memory pressure events (non-blocking).
    ///
    /// **Stub**: Always returns `None`.
    pub fn poll(&self) -> Option<MemoryPressureLevel> {
        None
    }

    /// Wait for a memory pressure event with a timeout.
    ///
    /// **Stub**: Always returns `None` after timeout.
    #[allow(unused_variables)]
    pub fn poll_timeout(&self, timeout: Duration) -> Option<MemoryPressureLevel> {
        None
    }
}

impl Default for MemoryPressureListener {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_can_be_created() {
        let _listener = MemoryPressureListener::new();
    }

    #[test]
    fn poll_returns_none() {
        let listener = MemoryPressureListener::new();
        assert_eq!(listener.poll(), None);
    }

    #[test]
    fn poll_timeout_returns_none() {
        let listener = MemoryPressureListener::new();
        assert_eq!(listener.poll_timeout(Duration::from_millis(10)), None);
    }

    #[test]
    fn memory_pressure_level_derives_work() {
        let level = MemoryPressureLevel::Warning;
        let cloned = level;
        assert_eq!(level, cloned);
        assert_eq!(format!("{:?}", level), "Warning");
    }
}
