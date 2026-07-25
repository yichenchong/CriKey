//! Linux platform backend.
//!
//! XDG desktop entries and base directories, DBus, Freedesktop notifications,
//! Secret Service, portals, X11/Wayland where available (spec 18.6).
//!
//! Compiled only for its target so platform-independent crates can never
//! accidentally depend on it (spec 5.3).

#![cfg(target_os = "linux")]

use crikey_platform::{Capability, CapabilityState};

#[derive(Debug, Default)]
pub struct LinuxBackend;

impl LinuxBackend {
    /// Stable backend identifier surfaced by diagnostics and `crikey version`.
    pub const NAME: &'static str = "linux";

    pub fn new() -> Self {
        Self
    }

    /// Capability reporting is honest by default: nothing is claimed until the
    /// corresponding backend service is implemented.
    pub fn capability(&self, _capability: Capability) -> CapabilityState {
        CapabilityState::Unavailable
    }
}
