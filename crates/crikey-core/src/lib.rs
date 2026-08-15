//! Shared primitives for every CriKey subsystem.
//!
//! This crate is platform independent by contract: it must never call a
//! Windows, macOS or Linux desktop API (spec 5.3).

pub mod action;
pub mod activation;
pub mod error;
pub mod generation;
pub mod item;
pub mod path;

pub use action::{Action, ActionId, ExecutionPolicy};
pub use activation::{ActivationPattern, ActivationPatternError, COMPILED_SIZE_LIMIT, MAX_PATTERN_BYTES};
pub use error::{CoreError, Result};
pub use generation::{Generation, GenerationTracker};
pub use item::{ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId, PLUGIN_DEFINED_PREFIX};
pub use path::PlatformPath;
