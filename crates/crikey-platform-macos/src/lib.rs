//! macOS platform backend.
//!
//! Application bundles, Launch Services, Spotlight metadata, Keychain,
//! accessibility-based window integration (spec 18.5).
//!
//! Compiled only for its target so platform-independent crates can never
//! accidentally depend on it (spec 5.3).

#![cfg(target_os = "macos")]

use crikey_platform::{Capability, CapabilityState};

#[derive(Debug, Default)]
pub struct MacOsBackend;

impl MacOsBackend {
    pub fn new() -> Self {
        Self
    }

    /// Capability reporting is honest by default: nothing is claimed until the
    /// corresponding backend service is implemented.
    pub fn capability(&self, _capability: Capability) -> CapabilityState {
        CapabilityState::Unavailable
    }
}
