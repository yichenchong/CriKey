//! The `crikey-wasm-host` executable.
//!
//! One process per WebAssembly plugin, spawned by the native supervisor with
//! the protocol endpoint and session token in its restricted environment (spec
//! 16.6) and the module path in [`config::ENV_MODULE`]. It loads and sandboxes
//! the module, then serves the ordinary native protocol through the published
//! SDK, so the supervisor cannot tell it apart from a hand-written native
//! plugin.
//!
//! A load failure exits non-zero with the reason on standard error. That is
//! the honest outcome: the supervisor records an unavailable plugin instead of
//! a plugin that answers nothing (README invariant 7).

use std::process::ExitCode;

use crikey_core::PluginId;
use crikey_plugin_sdk::{serve, ServeConfig};
use crikey_wasm_host::config::HostConfig;
use crikey_wasm_host::guest::Guest;
use crikey_wasm_host::plugin::WasmPlugin;

/// Identity used when the supervisor did not name one. A host-supplied id is
/// the norm; this only keeps a hand-launched host debuggable.
const FALLBACK_PLUGIN_ID: &str = "wasm.unnamed";

fn run() -> Result<(), String> {
    let config = HostConfig::from_env().map_err(|error| error.to_string())?;
    let plugin_id = std::env::var(crikey_plugin_sdk::protocol::ENV_PLUGIN_ID)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| FALLBACK_PLUGIN_ID.to_owned());

    let plugin_name = config.plugin_name.clone();
    let plugin_version = config.plugin_version.clone();
    let guest = Guest::load(config, PluginId(plugin_id.clone())).map_err(|error| error.to_string())?;

    let mut serve_config =
        ServeConfig::from_env(&plugin_id, &plugin_version).map_err(|error| error.to_string())?;
    serve_config.plugin_name = plugin_name;
    serve_config.capabilities.streaming_catalog = true;
    serve_config.capabilities.streaming_suggestions = true;
    // Honoured between batches, which is every point a guest call yields
    // control back to this process. See `plugin::WasmPlugin::suggest`.
    serve_config.capabilities.cancellation = true;

    let mut plugin = WasmPlugin::new(guest);
    serve(&mut plugin, serve_config).map_err(|error| error.to_string())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("crikey-wasm-host: {reason}");
            ExitCode::FAILURE
        }
    }
}
