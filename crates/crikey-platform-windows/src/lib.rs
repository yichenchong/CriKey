//! Windows platform backend.
//!
//! Start Menu shortcuts, packaged apps, `.lnk` parsing, AppUserModelIDs, shell
//! execution, known folders, named pipes, global hotkeys (spec 18.4).
//!
//! Compiled only for its target so platform-independent crates can never
//! accidentally depend on it (spec 5.3).

#![cfg(windows)]

use crikey_platform::{Capability, CapabilityState};

#[derive(Debug, Default)]
pub struct WindowsBackend;

impl WindowsBackend {
    pub fn new() -> Self {
        Self
    }

    /// Capability reporting is honest by default: nothing is claimed until the
    /// corresponding backend service is implemented.
    pub fn capability(&self, _capability: Capability) -> CapabilityState {
        CapabilityState::Unavailable
    }
}
