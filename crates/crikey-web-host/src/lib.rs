//! Out-of-process host for a WPE WebKit web surface behind a plugin page.
//!
//! # Why this is a separate executable
//!
//! The launcher must never link anything that can `dlopen` a browser engine
//! (spec 2.3, acceptance criterion 30, and the same argument
//! `crates/crikey-app/src/cabi_provider.rs` makes for restricted C ABI
//! plugins). Measured, the consequence is concrete: a binary linked against
//! WPE dies at `ld.so` with exit 127, before `main`, on a machine that has no
//! engine. Were the launcher that binary, a user without WPE could not start
//! CriKey at all. So exactly one crate knows how to find an engine, and it is
//! this one.
//!
//! # Why the crate is split in half
//!
//! Everything that can be decided without an engine is decided here, in safe
//! Rust, with tests: converting an exported buffer into a protocol frame, and
//! the ordering rules for driving the engine's input method context. Those are
//! where the correctness arguments live, and they must be testable on a
//! machine with no WPE -- which is every CI runner, and this development host.
//! They are therefore reachable with `--no-default-features`.
//!
//! The `engine` feature adds the half that cannot be tested without an engine:
//! the C shim in `csrc/`, and the executable that drives it. Rust owns protocol
//! validation, input translation, frame conversion, and IME operation ordering.
//! The shim creates the surface, dispatches those operations, and reports engine
//! callbacks. It also enforces WebKit-specific navigation, download, and storage
//! policy at the native APIs that expose those decisions.

#[cfg(all(feature = "engine", target_os = "linux"))]
pub mod engine;
pub mod frame;
pub mod ime;
pub mod input;
#[cfg(any(all(feature = "engine", target_os = "linux"), test))]
mod output;
pub mod state;

pub use frame::{FrameConverter, FrameError};
pub use ime::{char_offset_from_utf8_byte, ImeBridge, ImeOp};
pub use input::{EngineInput, InputError, InputTranslator};
pub use state::{HostAction, HostState, OpenSpec, StateError};
