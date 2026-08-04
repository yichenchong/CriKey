//! Layered configuration for CriKey (spec 21).
//!
//! # What this crate owns
//!
//! * The TOML format new CriKey and modern-plugin configuration uses (spec 21.1).
//! * The seven-layer precedence and, crucially, its *observability*: which layer
//!   won a key is an answer this crate gives, not a claim it makes (spec 21.2).
//! * Validation of values against the `[configuration]` schema a plugin declares,
//!   including the `secret` flag that keeps a value out of every diagnostic
//!   (spec 21.3; the schema itself lives in `crikey-plugin-model`).
//! * Coalescing rapid changes into one publication of the latest complete state
//!   (spec 21.4).
//!
//! # What this crate deliberately does not own
//!
//! Legacy plugin configuration. A `legacy-python` package keeps Keypirinha
//! configuration syntax and its own notification contract (spec 21.1 last line,
//! spec 14), handled entirely by `crikey-legacy-compat`. Nothing here reads or
//! writes a legacy settings file, and [`discover_plugin_schemas`] skips legacy
//! packages by runtime rather than by path, so a legacy package sitting in a
//! modern root is still left alone.
//!
//! # Platform independence
//!
//! No desktop API is called from here (spec 5.3). The only platform-shaped
//! decision is where the standard directories and the administrator policy file
//! are, and both are pure functions of a [`crikey_platform::DirectoryConvention`]
//! — which is why the Windows and macOS rules are testable on any host.
//!
//! # Typical use
//!
//! ```no_run
//! use crikey_config::{ConfigStore, ConfigurationPublisher};
//! use crikey_platform::StandardDirectories;
//! use std::time::{Duration, Instant};
//!
//! let directories = StandardDirectories::for_process()?;
//! let mut store = ConfigStore::load(&directories)?;
//! // Plugin defaults join the store as layer 5.
//! let (schemas, _problems) = crikey_config::discover_plugin_schemas(&[]);
//! for schema in &schemas {
//!     for problem in store.register_plugin_schema(&schema.plugin, &schema.section) {
//!         eprintln!("crikey: {problem}");
//!     }
//! }
//! // Changes are coalesced before they reach a plugin.
//! let mut publisher = ConfigurationPublisher::new(Duration::from_millis(150), Duration::from_secs(1));
//! publisher.observe(store.configuration_snapshot(), Instant::now());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod discovery;
mod layer;
mod publisher;
mod snapshot;
mod store;
mod tomlmap;
mod watch;

use std::path::PathBuf;

pub use discovery::{discover_plugin_schemas, DiscoveredSchema, SchemaProblem};
pub use layer::{ConfigLayer, LAYER_COUNT};
pub use publisher::ConfigurationPublisher;
pub use snapshot::ConfigurationSnapshot;
pub use store::{
    administrator_policy_path, ConfigStore, BUILT_IN_DEFAULTS, KEY_COALESCE_MS, KEY_MAXIMUM_WAIT_MS,
    KEY_MAX_RESULTS, KEY_PROFILE, KEY_RELOAD_INTERVAL_MS, PLUGIN_DIRECTORY, POLICY_FILE, PROFILE_DIRECTORY,
    USER_CONFIG_FILE,
};
pub use watch::ConfigSourceWatch;

/// Everything that can go wrong loading, validating or saving configuration.
///
/// Every variant names the file it is about. A configuration error whose message
/// did not say which of up to a dozen files was at fault would leave the user
/// opening each one in turn, and the whole reason this crate keeps one source per
/// layer is that the answer is always a single path.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A file exists but could not be read. An ABSENT file is never this: every
    /// layer is optional, and a machine with no configuration must start.
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A file exists and is not valid TOML (spec 21.1).
    #[error("cannot parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    /// A value has no textual spelling, such as an array. Named by key so the
    /// user can find the line rather than being told the file is bad.
    #[error("{path}: `{key}` must be a string, number or boolean")]
    UnsupportedValue { path: PathBuf, key: String },
    /// Two keys cannot both exist in one TOML document, because one is a strict
    /// prefix of the other.
    #[error("{path}: `{key}` cannot be written because another key uses it as a table")]
    KeyConflict { path: PathBuf, key: String },
    /// A value breaks the rule its plugin declared for that field (spec 21.3).
    ///
    /// The message names the plugin, the field and the rule. It never contains
    /// the value of a field the plugin marked `secret`.
    #[error("plugin `{plugin}`: {violation}")]
    Schema {
        plugin: String,
        #[source]
        violation: crikey_plugin_model::FieldViolation,
    },
    /// The user's configuration file could not be written.
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The user's configuration could not be rendered as TOML.
    #[error("cannot serialise {path}: {source}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: toml::ser::Error,
    },
}
