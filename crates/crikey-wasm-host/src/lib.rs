//! Out-of-process WebAssembly plugin host for CriKey (spec §2.2 later scope,
//! §16; ADR-0014).
//!
//! # Why this is a separate executable
//!
//! Third-party plugin code never executes in the CriKey UI process, and the
//! main process never loads arbitrary third-party libraries (README invariant
//! 1). A WebAssembly interpreter is a sandbox, but it is also a large,
//! evolving piece of third-party code parsing hostile input. Running it inside
//! the launcher would make every interpreter bug a launcher bug.
//!
//! So a `.wasm` plugin is executed by the `crikey-wasm-host` binary, which the
//! ordinary native supervisor (`crikey-native-host`) launches, restricts and
//! reaps exactly like any other native worker. It speaks the published native
//! protocol through `crikey-plugin-sdk`. Nothing about the wire is new: no
//! payload variant was added and no tag was repurposed (ADR-0004, ADR-0010).
//! Downstream — ranking, aggregation, the UI — a WASM plugin is a native one.
//!
//! # Layout
//!
//! * [`abi`] is the guest ABI: the blob format and the item mapping. Both this
//!   host and a guest link it, so there is exactly one codec.
//! * [`config`] is the launch contract: the environment variables CriKey sets
//!   when it spawns this process, and the capability grant vocabulary.
//! * [`guest`] is the interpreter: validation, linking, sandboxing, fuel and
//!   trap containment.
//! * [`watchdog`] is the wall-clock backstop behind fuel.
//! * [`plugin`] presents the guest to the SDK as an ordinary plugin.
//!
//! [`guest`], [`watchdog`] and [`plugin`] are behind the default `engine`
//! feature. A guest crate depends on this crate with `default-features = false`
//! to get [`abi`] alone, without compiling an interpreter into a `wasm32`
//! artefact.

pub mod abi;
pub mod config;

#[cfg(feature = "engine")]
pub mod guest;
#[cfg(feature = "engine")]
pub mod plugin;
#[cfg(feature = "engine")]
pub mod watchdog;
