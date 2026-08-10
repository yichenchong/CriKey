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
mod remote_catalog;
mod snapshot;
mod store;
mod tomlmap;
mod watch;

use std::path::PathBuf;

pub use discovery::{discover_plugin_schemas, DiscoveredSchema, SchemaProblem};
pub use layer::{ConfigLayer, LAYER_COUNT};
pub use publisher::ConfigurationPublisher;
pub use remote_catalog::{
    remote_catalog_sources, RemoteCatalogSource, DEFAULT_REMOTE_INTERVAL_MS, DEFAULT_REMOTE_MAX_BYTES,
    KEY_REMOTE_CATALOG_PREFIX, MAX_REMOTE_MAX_BYTES,
};
pub use snapshot::ConfigurationSnapshot;
pub use store::{
    administrator_policy_path, ConfigStore, BUILT_IN_DEFAULTS, KEY_ACTIVATION_HOTKEY, KEY_COALESCE_MS,
    KEY_MAXIMUM_WAIT_MS, KEY_MAX_RESULTS, KEY_PROFILE, KEY_RELOAD_INTERVAL_MS, PLUGIN_DIRECTORY, POLICY_FILE,
    PROFILE_DIRECTORY, USER_CONFIG_FILE,
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
    ///
    /// Deliberately does NOT carry the `toml::de::Error`: both its `Display` and
    /// its `Debug` reproduce the offending SOURCE LINE. Configuration is parsed
    /// before any plugin schema registers, so nothing yet knows that `api-key`
    /// is secret, and a malformed `api-key = hunter2` would reach stderr with the
    /// token intact — around [`ConfigStore::display_value`] entirely (spec 21.3).
    /// The path plus a location and the parser's reason are enough to find the
    /// line, and none of the three can contain a value.
    #[error("cannot parse {path}{location}: {reason}")]
    Parse {
        path: PathBuf,
        /// ` at line L column C`, or empty when the parser reported no span.
        location: String,
        /// The parser's own explanation. It names syntax and keys, never values.
        reason: String,
    },
    /// The selected profile name would escape the profiles directory.
    #[error("invalid profile `{name}` in {path}: {reason}")]
    Profile {
        path: PathBuf,
        name: String,
        reason: &'static str,
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
    /// A host setting is present but not usable as written (ADR-0016).
    ///
    /// Named by key and reason, never by value. This crate parses settings
    /// before any plugin schema registers, so it does not yet know which keys
    /// are secret, and "that is not a number" is a complaint about the key
    /// rather than something the value has to be quoted for (spec 21.3).
    #[error("`{key}` is not usable: {reason}")]
    Setting { key: String, reason: &'static str },
}

impl ConfigError {
    /// Builds [`ConfigError::Parse`] from a TOML failure without retaining the
    /// parser's error value.
    ///
    /// `text` is consulted only to turn the reported byte span into a line and
    /// column; not one byte of it is copied into the message. This is the single
    /// place a `toml::de::Error` is allowed to be observed, so the rule that no
    /// diagnostic may echo unparsed configuration source cannot be honoured here
    /// and forgotten at another call site (spec 21.3).
    pub(crate) fn parse(path: &std::path::Path, text: &str, error: &toml::de::Error) -> Self {
        let location = match error.span() {
            Some(span) => {
                // `span.start` may land on a UTF-8 boundary inside the document;
                // clamping keeps the slice legal for a truncated or odd span.
                let mut start = span.start.min(text.len());
                while start > 0 && !text.is_char_boundary(start) {
                    start -= 1;
                }
                let consumed = &text[..start];
                let line = consumed.matches('\n').count() + 1;
                let column = consumed.rsplit('\n').next().unwrap_or_default().chars().count() + 1;
                format!(" at line {line} column {column}")
            }
            None => String::new(),
        };
        Self::Parse {
            path: path.to_path_buf(),
            location,
            // `message()` is the reason alone; `Display` would append the
            // annotated source line, which is exactly what must not escape.
            reason: error.message().replace('\n', "; "),
        }
    }
}
