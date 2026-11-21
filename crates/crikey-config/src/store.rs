//! The layered configuration store (spec 21.1, 21.2, 21.3).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crikey_core::PluginId;
use crikey_platform::{DirectoryConvention, StandardDirectories};
use crikey_plugin_model::{
    ConfigurationSection, FieldViolation, SchedulingProfile, RULE_REQUIRED, RULE_UNKNOWN_FIELD,
};

use crate::layer::{ConfigLayer, LAYER_COUNT};
use crate::snapshot::ConfigurationSnapshot;
use crate::tomlmap;
use crate::watch::ConfigSourceWatch;
use crate::ConfigError;

/// File name of the user's global settings, inside `config_dir()`.
pub const USER_CONFIG_FILE: &str = "config.toml";

/// Directory of named profiles, inside `config_dir()`.
pub const PROFILE_DIRECTORY: &str = "profiles";

/// Directory of per-plugin user settings, inside `config_dir()`.
pub const PLUGIN_DIRECTORY: &str = "plugins";

/// File name of the system-wide administrator policy.
pub const POLICY_FILE: &str = "policy.toml";

/// Selects the [`ConfigLayer::Profile`] file. Read from the layers below it, so
/// an administrator can pin a profile and a user can choose one.
pub const KEY_PROFILE: &str = "launcher.profile";

/// How long the host waits for edits to settle before publishing configuration
/// to plugins, in milliseconds (spec 21.4).
pub const KEY_COALESCE_MS: &str = "launcher.configuration-coalesce-ms";

/// The longest the host will keep deferring a publication while edits keep
/// arriving, in milliseconds (spec 21.4).
pub const KEY_MAXIMUM_WAIT_MS: &str = "launcher.configuration-maximum-wait-ms";

/// How often the host re-examines its configuration files, in milliseconds.
pub const KEY_RELOAD_INTERVAL_MS: &str = "launcher.configuration-reload-interval-ms";

/// Ceiling on the results one query may produce, across all plugins.
///
/// Deliberately absent from [`BUILT_IN_DEFAULTS`]: the host's own default lives
/// with the aggregator that enforces it, and duplicating the number here would
/// let the two drift. An absent key means "whatever the aggregator's default
/// is", which is exactly what the launcher does with it.
pub const KEY_MAX_RESULTS: &str = "launcher.max-results";

/// The global accelerator that raises the launcher (spec 21.2).
///
/// Defaulted here rather than in the launcher because it is the one launcher
/// setting a user has no other way to discover: a resident launcher whose
/// chord is unknown and unwritten is unreachable, so the value the host binds
/// at startup has to be readable through `crikey config get` on a machine that
/// has never been configured.
pub const KEY_ACTIVATION_HOTKEY: &str = "launcher.activation-hotkey";

/// Whether the launcher's footer shows the navigation hint line (spec 21.2).
///
/// Defaulted here rather than in the renderer for the same reason
/// [`KEY_ACTIVATION_HOTKEY`] is: a user who has turned the hints off has taken
/// away the one line on screen that would have named them, so `crikey config
/// get launcher.show-hints` has to answer on a machine that has never been
/// configured — otherwise the setting is one nothing reports and nothing can
/// explain.
pub const KEY_SHOW_HINTS: &str = "launcher.show-hints";

/// Whether the launcher's window is drawn with rounded corners (spec 21.2).
///
/// Defaulted here rather than in the renderer for the same reason
/// [`KEY_SHOW_HINTS`] is: the setting changes nothing but the shape of the
/// window it is read from, so a user who cannot tell whether their edit took
/// has only `crikey config get launcher.rounded-corners` to ask — and that has
/// to answer on a machine that has never been configured, or the one report
/// that could settle the question is the one that says nothing.
pub const KEY_ROUNDED_CORNERS: &str = "launcher.rounded-corners";

/// Remembered preview divider proportion; CLI validation enforces finite 0.3..=0.7.
pub const KEY_PREVIEW_SPLIT: &str = "launcher.preview-split";

/// Live launcher appearance and semantic navigation preferences.
pub const KEY_THEME: &str = "launcher.theme";
pub const KEY_INTERFACE_SIZE: &str = "launcher.interface-size";
pub const KEY_RESULT_DENSITY: &str = "launcher.result-density";
pub const KEY_THEME_OVERRIDES: &str = "launcher.theme-overrides";
pub const KEY_NAVIGATION_STYLE: &str = "launcher.navigation-style";
pub const KEY_NAVIGATION_BINDINGS: &str = "launcher.navigation-bindings";

/// The values the host itself supplies, forming [`ConfigLayer::BuiltInDefaults`].
///
/// Only keys whose default this crate genuinely owns. A key defaulted by another
/// component belongs to that component, not to a second copy here.
pub const BUILT_IN_DEFAULTS: &[(&str, &str)] = &[
    (KEY_COALESCE_MS, "150"),
    (KEY_MAXIMUM_WAIT_MS, "1000"),
    (KEY_RELOAD_INTERVAL_MS, "500"),
    (KEY_ACTIVATION_HOTKEY, "Ctrl+Alt+Space"),
    (KEY_SHOW_HINTS, "true"),
    (KEY_ROUNDED_CORNERS, "true"),
    (KEY_PREVIEW_SPLIT, "0.55"),
    // The settings/UI declaration validates the four supported home modes.
    ("launcher.command-home", "compact"),
    (KEY_THEME, "system"),
    (KEY_INTERFACE_SIZE, "default"),
    (KEY_RESULT_DENSITY, "default"),
    (KEY_THEME_OVERRIDES, "{}"),
    (KEY_NAVIGATION_STYLE, "default"),
    (KEY_NAVIGATION_BINDINGS, "{}"),
];
/// Keys under this prefix are operator-pinned: a user may supply a source
/// declaration, but cannot redirect one an administrator has specified.
const ADMINISTRATOR_PINNED_PREFIX: &str = "catalog.remote.";

const PLUGINS_PREFIX: &str = "plugins.";
const SETTINGS_MARKER: &str = ".settings.";
const ENABLED_SUFFIX: &str = ".enabled";
const SCHEDULING_PROFILE_SUFFIX: &str = ".scheduling-profile";

/// Keys under this prefix name a user-defined alias: `aliases.ss = "Settings"`.
///
/// The key space is open rather than schema-declared, because the whole point
/// is that a user names abbreviations the host could not have anticipated.
const ALIASES_PREFIX: &str = "aliases.";

/// Layered configuration, resolved once and then queried per key.
///
/// # Shape
///
/// Seven flat `key -> text` maps, one per [`ConfigLayer`], plus the plugin
/// schemas registered against it. A lookup walks the layers from the highest
/// precedence downward and stops at the first hit, so [`Self::layer_of`] is not
/// a separate bookkeeping structure that could disagree with [`Self::get`] —
/// they are the same walk.
///
/// # What each layer is read from
///
/// | Layer | Source |
/// |---|---|
/// | [`ConfigLayer::BuiltInDefaults`] | [`BUILT_IN_DEFAULTS`], compiled in |
/// | [`ConfigLayer::AdministratorPolicy`] | the system policy file for this platform |
/// | [`ConfigLayer::UserGlobal`] | `config_dir()/config.toml` |
/// | [`ConfigLayer::Profile`] | `config_dir()/profiles/<launcher.profile>.toml` |
/// | [`ConfigLayer::PluginDefaults`] | each plugin's `[configuration]` defaults |
/// | [`ConfigLayer::UserPlugin`] | `config_dir()/plugins/<plugin-id>.toml` |
/// | [`ConfigLayer::SessionOverride`] | [`Self::set_session_override`], memory only |
///
/// Exactly one source per layer, which is what makes the precedence in
/// [`ConfigLayer`] meaningful instead of a rule about which part of a file a key
/// came from.
///
/// # Missing files are not errors
///
/// A machine with no configuration at all must start. Every file layer is
/// optional; an absent file contributes nothing. A file that exists but cannot
/// be read or parsed IS an error, because silently ignoring a user's settings
/// because of a typo is worse than refusing to start with the reason.
#[derive(Clone)]
pub struct ConfigStore {
    pub(crate) layers: [BTreeMap<String, String>; LAYER_COUNT],
    /// Where [`Self::save`] writes. Retained so a store built for a temporary
    /// directory saves back into it.
    pub(crate) config_path: PathBuf,
    /// Every file consulted, in read order, for change detection.
    sources: Vec<PathBuf>,
    /// Registered `[configuration]` schemas, by plugin id. The only thing that
    /// knows which keys hold secrets.
    schemas: BTreeMap<String, ConfigurationSection>,
    /// Exact discovered owners authorized to edit their own user documents.
    discovered_schemas: BTreeMap<String, crate::DiscoveredSchema>,
    /// Whether a plugin key with no registered schema must be treated as secret.
    ///
    /// Set by [`Self::redact_unregistered_plugins`] when schema discovery was
    /// incomplete. See that method for why the default is `false`.
    redact_unregistered: bool,
    pub(crate) revision: crate::ConfigRevision,
    directories: StandardDirectories,
    policy: Option<PathBuf>,
    pending: BTreeMap<String, Option<String>>,
    user_document: BTreeMap<String, String>,
}

/// Prints the SHAPE of the store and none of its values.
///
/// Hand-written rather than derived because `layers` holds every effective raw
/// value, including fields a schema marks `secret`, and `{store:?}` in a
/// diagnostic or a failing assertion would print them without passing through
/// [`ConfigStore::display_value`] (spec 21.3). Every value is redacted rather
/// than only the known secrets: a `Debug` rendering is never the way to read
/// configuration — [`ConfigStore::display_value`] is — so there is nothing to
/// trade against failing closed, and a store whose schemas have not registered
/// yet does not even know which keys are secret.
impl std::fmt::Debug for ConfigStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<Vec<&str>> = self
            .layers
            .iter()
            .map(|layer| layer.keys().map(String::as_str).collect())
            .collect();
        formatter
            .debug_struct("ConfigStore")
            .field("config_path", &self.config_path)
            .field("sources", &self.sources)
            .field("schemas", &self.schemas.keys().collect::<Vec<_>>())
            .field("redact_unregistered", &self.redact_unregistered)
            .field("layer_keys", &keys)
            .finish_non_exhaustive()
    }
}

impl ConfigStore {
    /// Loads every layer for this process (spec 21.2).
    ///
    /// The administrator policy path follows this platform's convention, which
    /// is the one thing here that is not derived from
    /// [`StandardDirectories`] — a policy file is not per-user, so it is not in
    /// any per-user directory.
    pub fn load(directories: &StandardDirectories) -> Result<Self, ConfigError> {
        let policy = administrator_policy_path(DirectoryConvention::current());
        Self::load_with_policy(directories, Some(policy.as_path()))
    }

    /// Loads every layer, taking the administrator policy from `policy`.
    ///
    /// The seam [`Self::load`] is built on. Public because a test must be able to
    /// state the policy file rather than write to a system path, and because an
    /// installation that keeps its policy elsewhere has somewhere to say so.
    /// `None` means this deployment has no policy file.
    pub fn load_with_policy(
        directories: &StandardDirectories,
        policy: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        Self::load_with_overrides(directories, policy, &[])
    }

    /// Loads every layer and applies process-only overrides before selecting a
    /// profile. This is the production entry point for layer 7.
    pub fn load_with_overrides(
        directories: &StandardDirectories,
        policy: Option<&Path>,
        overrides: &[(String, String)],
    ) -> Result<Self, ConfigError> {
        let config_dir = directories.config_dir();
        let config_path = config_dir.join(USER_CONFIG_FILE);
        let mut store = Self {
            layers: Default::default(),
            config_path: config_path.clone(),
            sources: Vec::new(),
            schemas: BTreeMap::new(),
            discovered_schemas: BTreeMap::new(),
            redact_unregistered: false,
            revision: Default::default(),
            directories: directories.clone(),
            policy: policy.map(Path::to_path_buf),
            pending: BTreeMap::new(),
            user_document: BTreeMap::new(),
        };
        for (key, value) in BUILT_IN_DEFAULTS {
            store.layers[ConfigLayer::BuiltInDefaults.index()].insert((*key).to_owned(), (*value).to_owned());
        }
        if let Some(policy) = policy {
            store.read_file_into(ConfigLayer::AdministratorPolicy, policy, "")?;
        }
        store.read_file_into(ConfigLayer::UserGlobal, &config_path, "")?;
        store.user_document = store.layers[ConfigLayer::UserGlobal.index()].clone();
        for (key, value) in overrides {
            store.set_session_override(key, value);
        }
        if let Some(profile) = store.get(KEY_PROFILE).map(str::to_owned) {
            if !valid_profile_name(&profile) {
                return Err(ConfigError::Profile {
                    path: config_dir.join(PROFILE_DIRECTORY),
                    name: profile,
                    reason: "must be one safe path component",
                });
            }
            let path = config_dir.join(PROFILE_DIRECTORY).join(format!("{profile}.toml"));
            store.read_file_into(ConfigLayer::Profile, &path, "")?;
        }
        store.read_plugin_files(&config_dir.join(PLUGIN_DIRECTORY))?;
        store.revision.session = store.layers[ConfigLayer::SessionOverride.index()].clone();
        store.revision.check()?;
        Ok(store)
    }

    /// Reads one TOML document into `layer`, scoping its keys under `prefix`.
    ///
    /// An absent file is recorded as a watched source anyway: creating it later
    /// is a configuration change, and a watcher that only knew about files that
    /// already existed would never notice the first `config.toml` a user writes.
    fn read_file_into(&mut self, layer: ConfigLayer, path: &Path, prefix: &str) -> Result<(), ConfigError> {
        self.sources.push(path.to_path_buf());
        let state = crate::persistence::SourceState::read(path)?;
        if state.is_directory {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "configuration document is a directory",
                ),
            });
        }
        let bytes = state.bytes.clone();
        self.revision.sources.insert(path.to_path_buf(), state);
        let text = match bytes {
            Some(bytes) => String::from_utf8(bytes).map_err(|_| ConfigError::Read {
                path: path.to_path_buf(),
                source: std::io::Error::new(std::io::ErrorKind::InvalidData, "configuration is not UTF-8"),
            })?,
            None => return Ok(()),
        };
        let table: toml::Table = text
            .parse()
            .map_err(|source: toml::de::Error| ConfigError::parse(path, &text, &source))?;
        let mut flat = BTreeMap::new();
        tomlmap::flatten(&table, prefix, path, &mut flat)?;
        self.layers[layer.index()].extend(flat);
        Ok(())
    }

    /// Reads `config_dir()/plugins/*.toml` into [`ConfigLayer::UserPlugin`].
    ///
    /// Each file's stem is the plugin id and every key inside it is relative to
    /// `plugins.<id>`, so a user editing one plugin's settings never has to
    /// repeat the plugin's name and cannot accidentally address another plugin.
    /// Read in sorted order so two files that somehow name the same key resolve
    /// the same way on every machine.
    fn read_plugin_files(&mut self, directory: &Path) -> Result<(), ConfigError> {
        self.sources.push(directory.to_path_buf());
        self.revision.sources.insert(
            directory.to_path_buf(),
            crate::persistence::SourceState::read(directory)?,
        );
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(ConfigError::Read {
                    path: directory.to_path_buf(),
                    source: error,
                })
            }
        };
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| ConfigError::Read {
                path: directory.to_path_buf(),
                source,
            })?;
            let path = entry.path();
            if path.extension().is_some_and(|extension| extension == "toml") {
                files.push(path);
            }
        }
        files.sort();
        for path in files {
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let prefix = format!("{PLUGINS_PREFIX}{stem}");
            self.read_file_into(ConfigLayer::UserPlugin, &path.clone(), &prefix)?;
        }
        Ok(())
    }

    /// The winning value for `key`, or `None` if no layer supplies one.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.resolve(key).map(|(_, value)| value)
    }

    /// Which layer supplied the winning value for `key` (spec 21.2).
    ///
    /// The whole point of the layer model being observable rather than asserted:
    /// when a setting appears not to take effect, this is the answer.
    pub fn layer_of(&self, key: &str) -> Option<ConfigLayer> {
        self.resolve(key).map(|(layer, _)| layer)
    }

    /// The administrator owns remote-catalog routing when it declares it.
    /// This is intentionally narrower than the ordinary layer order: a
    /// machine policy must be able to pin a trusted index against a user's
    /// replacement, while unrelated settings retain the documented order.
    fn resolve(&self, key: &str) -> Option<(ConfigLayer, &str)> {
        if key.starts_with(ADMINISTRATOR_PINNED_PREFIX) {
            if let Some(value) = self.layers[ConfigLayer::AdministratorPolicy.index()].get(key) {
                return Some((ConfigLayer::AdministratorPolicy, value.as_str()));
            }
        }
        ConfigLayer::ALL.iter().rev().find_map(|layer| {
            self.layers[layer.index()]
                .get(key)
                .map(|value| (*layer, value.as_str()))
        })
    }

    /// Every key any layer supplies, in sorted order.
    pub fn keys(&self) -> BTreeSet<&str> {
        self.layers
            .iter()
            .flat_map(|layer| layer.keys().map(String::as_str))
            .collect()
    }

    /// The effective settings for `plugin`, keyed by declared field name.
    ///
    /// The `plugins.<id>.settings.` prefix is stripped: a plugin declared the
    /// field as `theme` and receives it as `theme`. It has no use for the host's
    /// key namespace, and handing it one would invite it to parse the plugin id
    /// back out of its own configuration.
    pub fn plugin_values(&self, plugin: &PluginId) -> BTreeMap<String, String> {
        let prefix = format!("{PLUGINS_PREFIX}{}{SETTINGS_MARKER}", plugin.0);
        let declared = self.schemas.get(&plugin.0);
        let mut fields = BTreeSet::new();
        for layer in &self.layers {
            for key in layer.keys() {
                if let Some(field) = key.strip_prefix(&prefix) {
                    if declared
                        .and_then(|section| section.field(field))
                        .is_none_or(|field| field.applies_to(std::env::consts::OS))
                    {
                        fields.insert(field.to_owned());
                    }
                }
            }
        }
        fields
            .into_iter()
            .filter_map(|field| {
                let value = self.get(&format!("{prefix}{field}"))?;
                Some((field, value.to_owned()))
            })
            .collect()
    }

    /// Every user-defined alias, as `alias -> target`, resolved by layer.
    ///
    /// An alias names a thing the user types; the target names the item they
    /// mean. Both come back exactly as written: the target is compared against
    /// item text by the catalog, which owns the folding rules, and doing it
    /// here would fold twice under two different conventions.
    ///
    /// An empty target removes an alias a lower layer defined, which is the
    /// only way a profile can retract a global alias: the layers merge by key,
    /// so there is otherwise no spelling for "not this one".
    pub fn aliases(&self) -> BTreeMap<String, String> {
        let mut names = BTreeSet::new();
        for layer in &self.layers {
            for key in layer.keys() {
                if let Some(alias) = key.strip_prefix(ALIASES_PREFIX) {
                    if !alias.is_empty() {
                        names.insert(alias.to_owned());
                    }
                }
            }
        }
        names
            .into_iter()
            .filter_map(|alias| {
                let target = self.get(&format!("{ALIASES_PREFIX}{alias}"))?;
                (!target.trim().is_empty()).then(|| (alias, target.to_owned()))
            })
            .collect()
    }

    /// Every plugin id an operator has switched off (spec 21.2).
    ///
    /// [`Self::plugin_enabled`] is a point query and cannot answer "which are
    /// off", which is what the launcher needs BEFORE discovery: a disabled
    /// plugin must not cost a worker process, and the only proof it did not run
    /// is that nothing started it. Ids are returned exactly as they appear in the
    /// key, so the caller gets the namespaced identity the providers register.
    pub fn disabled_plugins(&self) -> BTreeSet<String> {
        let mut candidates = BTreeSet::new();
        for layer in &self.layers {
            for key in layer.keys() {
                if let Some(plugin) = key
                    .strip_prefix(PLUGINS_PREFIX)
                    .and_then(|rest| rest.strip_suffix(ENABLED_SUFFIX))
                {
                    if !plugin.is_empty() {
                        candidates.insert(plugin.to_owned());
                    }
                }
            }
        }
        candidates
            .into_iter()
            .filter(|plugin| !self.plugin_enabled(&PluginId(plugin.clone())))
            .collect()
    }

    /// Whether `plugin` may be loaded. Absent means enabled (spec 21.2).
    ///
    /// Only the exact text `false` disables: a store that treated any
    /// unrecognised text as "off" would silently disable a plugin over a typo,
    /// which is the failure mode that is hardest to diagnose.
    pub fn plugin_enabled(&self, plugin: &PluginId) -> bool {
        self.get(&plugin_enabled_key(plugin)) != Some("false")
    }

    /// Records whether `plugin` may be loaded, in the user's global file.
    pub fn set_plugin_enabled(&mut self, plugin: &PluginId, on: bool) {
        self.set_user_global(&plugin_enabled_key(plugin), &on.to_string());
    }

    /// The scheduling profile an operator pinned for `plugin`, if any.
    ///
    /// Unrecognised text is `None` rather than an error: this is a host hint,
    /// and a typo must leave the manifest-derived profile in force rather than
    /// take the plugin out of the query path.
    pub fn scheduling_profile(&self, plugin: &PluginId) -> Option<SchedulingProfile> {
        parse_scheduling_profile(self.get(&plugin_scheduling_profile_key(plugin))?)
    }

    /// Pins, or unpins, the scheduling profile for `plugin`.
    pub fn set_scheduling_profile(&mut self, plugin: &PluginId, profile: Option<SchedulingProfile>) {
        let key = plugin_scheduling_profile_key(plugin);
        match profile {
            Some(profile) => {
                self.set_user_global(&key, scheduling_profile_name(profile));
            }
            None => {
                self.layers[ConfigLayer::UserGlobal.index()].remove(&key);
                self.pending.insert(key, None);
            }
        }
    }

    /// Records `value` for `key` in the user's global layer, where
    /// [`Self::save`] will persist it.
    ///
    /// The layer, not the winning value: an administrator policy or a session
    /// override still outranks what this writes, and the caller is expected to
    /// ask [`Self::layer_of`] afterwards rather than assume the edit is what
    /// the launcher will now read. Silently promoting the write to whichever
    /// layer happens to win would let a settings panel overwrite a policy the
    /// user is not permitted to change.
    pub fn set_user_global(&mut self, key: &str, value: &str) {
        self.layers[ConfigLayer::UserGlobal.index()].insert(key.to_owned(), value.to_owned());
        self.pending.insert(key.to_owned(), Some(value.to_owned()));
    }

    /// Sets a value for this process only (spec 21.2, layer 7).
    ///
    /// Never persisted: [`Self::save`] writes the user-global layer, and a
    /// session override that survived the session would not be one.
    pub fn set_session_override(&mut self, key: &str, value: &str) {
        self.layers[ConfigLayer::SessionOverride.index()].insert(key.to_owned(), value.to_owned());
        self.revision.session = self.layers[ConfigLayer::SessionOverride.index()].clone();
    }

    /// Drops a session override, exposing whatever layer was underneath.
    pub fn clear_session_override(&mut self, key: &str) {
        self.layers[ConfigLayer::SessionOverride.index()].remove(key);
        self.revision.session = self.layers[ConfigLayer::SessionOverride.index()].clone();
    }

    /// Compares exact discovered declaration/owner identities, not values or sources.
    pub fn same_discovered_schemas(&self, other: &Self) -> bool {
        self.revision.schema_owners == other.revision.schema_owners
    }

    /// Replace discovery as one complete snapshot, removing vanished owners.
    /// Returns structured validation failures; callers must redact diagnostics.
    pub fn replace_discovered_schemas(&mut self, owners: &[crate::DiscoveredSchema]) -> Vec<ConfigError> {
        self.schemas.clear();
        self.discovered_schemas.clear();
        self.revision.schema_owners.clear();
        self.layers[ConfigLayer::PluginDefaults.index()].clear();
        self.redact_unregistered = true;
        owners
            .iter()
            .flat_map(|owner| self.register_discovered_schema(owner))
            .collect()
    }
    /// Registers an exact discovered owner for plugin-user transactions.
    /// Section-only registration deliberately does not authorize file edits.
    pub fn register_discovered_schema(&mut self, owner: &crate::DiscoveredSchema) -> Vec<ConfigError> {
        let signature = match toml::to_string(&owner.section) {
            Ok(signature) => signature,
            Err(source) => {
                return vec![ConfigError::Serialize {
                    path: owner.package.join("crikey.toml"),
                    source,
                }]
            }
        };
        let manifest = owner.package.join("crikey.toml");
        match crate::persistence::SourceState::read(&manifest) {
            Ok(source) => {
                if let Some(bytes) = source.bytes.as_deref() {
                    let actual = std::str::from_utf8(bytes)
                        .ok()
                        .and_then(|text| crikey_plugin_model::Manifest::parse(text).ok());
                    let matches = actual.is_some_and(|manifest| {
                        toml::to_string(&manifest.configuration).ok().as_deref() == Some(signature.as_str())
                    });
                    if !matches {
                        return vec![ConfigError::Conflict];
                    }
                }
                self.revision.sources.insert(manifest.clone(), source);
                if !self.sources.contains(&manifest) {
                    self.sources.push(manifest);
                }
            }
            Err(error) => return vec![error],
        }
        let problems = self.register_plugin_schema(&owner.plugin, &owner.section);
        self.revision
            .schema_owners
            .insert(owner.plugin.0.clone(), (owner.package.clone(), signature));
        self.discovered_schemas
            .insert(owner.plugin.0.clone(), owner.clone());
        problems
    }

    /// Installs `section`'s defaults and validates the values already loaded
    /// against it (spec 21.3), for the platform this process runs on.
    pub fn register_plugin_schema(
        &mut self,
        plugin: &PluginId,
        section: &ConfigurationSection,
    ) -> Vec<ConfigError> {
        self.register_plugin_schema_for(plugin, section, std::env::consts::OS)
    }

    /// Installs `section`'s defaults and validates loaded values against it, as
    /// they apply on `os`.
    ///
    /// Returns every problem rather than the first: a plugin with two bad
    /// settings should tell the operator about both, and a caller that stopped
    /// at the first would make them fix one, restart, and discover the next.
    ///
    /// A value that violates its field's rules is REMOVED from the layer it came
    /// from, so the plugin is delivered its default instead of a value its own
    /// schema refuses. That is what "reject" has to mean here: reporting the
    /// violation and then delivering the value anyway would make the schema
    /// decorative.
    ///
    /// `os` is a parameter because the platform restrictions in spec 21.3 are
    /// otherwise only testable on the platform they name.
    pub fn register_plugin_schema_for(
        &mut self,
        plugin: &PluginId,
        section: &ConfigurationSection,
        os: &str,
    ) -> Vec<ConfigError> {
        self.discovered_schemas.remove(&plugin.0);
        self.revision.schema_owners.remove(&plugin.0);
        let mut problems = Vec::new();
        let prefix = format!("{PLUGINS_PREFIX}{}{SETTINGS_MARKER}", plugin.0);

        for field in &section.fields {
            let key = format!("{prefix}{}", field.name);
            if !field.applies_to(os) {
                // A field that does not apply here contributes no default. Any
                // value a user set for it is left alone rather than reported:
                // a shared configuration directory is expected to carry the
                // settings of every machine that uses it.
                continue;
            }
            match field.default_text() {
                Ok(Some(default)) => {
                    self.layers[ConfigLayer::PluginDefaults.index()].insert(key.clone(), default);
                }
                Ok(None) => {}
                Err(violation) => {
                    problems.push(ConfigError::Schema {
                        plugin: plugin.0.clone(),
                        violation,
                    });
                    continue;
                }
            }
            loop {
                match self.resolve(&key) {
                    Some((layer, value)) => {
                        if let Err(violation) = field.validate(value) {
                            problems.push(ConfigError::Schema {
                                plugin: plugin.0.clone(),
                                violation,
                            });
                            self.layers[layer.index()].remove(&key);
                            continue;
                        }
                    }
                    None if field.required => problems.push(ConfigError::Schema {
                        plugin: plugin.0.clone(),
                        violation: FieldViolation {
                            field: field.name.clone(),
                            rule: RULE_REQUIRED,
                            detail: "is required and no layer supplies a value".to_owned(),
                        },
                    }),
                    None => {}
                }
                break;
            }
        }

        // A key in the plugin's namespace that the plugin does not declare is
        // almost always a misspelling, and the plugin would never read it. Named
        // here rather than passed through, so the operator learns why their edit
        // did nothing.
        let declared: BTreeSet<&str> = section.fields.iter().map(|field| field.name.as_str()).collect();
        let mut undeclared = BTreeSet::new();
        for layer in &self.layers {
            for key in layer.keys() {
                if let Some(field) = key.strip_prefix(&prefix) {
                    if !declared.contains(field) {
                        undeclared.insert(field.to_owned());
                    }
                }
            }
        }
        for field in undeclared {
            problems.push(ConfigError::Schema {
                plugin: plugin.0.clone(),
                violation: FieldViolation {
                    field: field.clone(),
                    rule: RULE_UNKNOWN_FIELD,
                    detail: format!("`{}` declares no such configuration field", plugin.0),
                },
            });
        }

        self.schemas.insert(plugin.0.clone(), section.clone());
        problems
    }

    /// Whether `key` names a field that must be redacted in diagnostics.
    ///
    /// A namespace whose schema discovery failed is also secret: the host cannot
    /// prove that an undeclared key is non-secret, and displaying it would turn
    /// an unrelated manifest error into a token dump (spec 21.3).
    pub fn is_secret(&self, key: &str) -> bool {
        let Some((plugin, field)) = split_setting_key(key) else {
            return false;
        };
        match self.schemas.get(plugin).and_then(|section| section.field(field)) {
            Some(declared) => declared.secret,
            None => self.redact_unregistered,
        }
    }

    /// Fail closed for every namespace without a successfully loaded schema.
    ///
    /// Schema discovery is performed by the CLI and launcher after the store
    /// loads its raw layers. If any package failed to load, unknown namespaces
    /// are conservatively redacted until their schema is known.
    pub fn redact_unregistered_plugins(&mut self) {
        self.redact_unregistered = true;
    }

    /// The value of `key` as it may be shown to a human.
    ///
    /// The ONLY rendering path any diagnostic, listing or dump may use. Callers
    /// that want the real value for delivery to its owning plugin use
    /// [`Self::plugin_values`]; callers that want to print something use this,
    /// and therefore cannot print a secret by omission (spec 21.3).
    pub fn display_value(&self, key: &str) -> Option<&str> {
        let value = self.get(key)?;
        if self.is_secret(key) {
            Some(crikey_plugin_model::REDACTED)
        } else {
            Some(value)
        }
    }

    /// The complete per-plugin configuration state, for publication (spec 21.4).
    ///
    /// "Complete" means every plugin the store knows about is present, with its
    /// full settings map — including plugins whose maps are empty. A plugin that
    /// vanished from the snapshot would leave the last state it received in
    /// force, which is exactly the intermediate-state bug the coalescing in
    /// [`crate::ConfigurationPublisher`] exists to prevent.
    pub fn configuration_snapshot(&self) -> ConfigurationSnapshot {
        let mut plugins: BTreeSet<String> = self.schemas.keys().cloned().collect();
        for layer in &self.layers {
            for key in layer.keys() {
                if let Some((plugin, _)) = split_setting_key(key) {
                    plugins.insert(plugin.to_owned());
                }
            }
        }
        ConfigurationSnapshot::new(
            plugins
                .into_iter()
                .map(|plugin| {
                    let id = PluginId(plugin);
                    let values = self.plugin_values(&id);
                    (id, values)
                })
                .collect(),
        )
    }

    /// A watch over every file this store was read from.
    pub fn source_watch(&self) -> ConfigSourceWatch {
        ConfigSourceWatch::over(&self.sources)
    }

    /// Where [`Self::save`] writes.
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Atomically saves pending user-global edits, checking the exact loaded
    /// sources under the shared cross-process writer lock. Blocking: worker only.
    /// Prefer `commit_user_global` when editing a published store: setters stage
    /// drafts immediately, whereas that method publishes only after success.
    pub fn save(&mut self) -> Result<(), ConfigError> {
        let _lock = crate::persistence::writer_lock(&self.config_path)?;
        self.save_locked(&[])
    }

    fn save_locked(&mut self, validated_sources: &[crate::ConfigRevision]) -> Result<(), ConfigError> {
        for revision in validated_sources {
            revision.check()?;
        }
        self.revision.check()?;
        // Freshly load retained layers while holding the writer lock.
        let mut validation = self.reload()?;
        for (key, value) in &self.pending {
            let Some(value) = value else { continue };
            if key == KEY_PROFILE && !valid_profile_name(value) {
                return Err(ConfigError::Setting {
                    key: key.clone(),
                    reason: "profile must be one safe path component",
                });
            }
            if let Some((plugin, field)) = split_setting_key(key) {
                if let Some(field) = self.schemas.get(plugin).and_then(|schema| schema.field(field)) {
                    field.validate(value).map_err(|violation| ConfigError::Schema {
                        plugin: plugin.to_owned(),
                        violation,
                    })?;
                }
            }
        }
        let mut flat = self.user_document.clone();
        for (key, value) in &self.pending {
            match value {
                Some(value) => {
                    flat.insert(key.clone(), value.clone());
                }
                None => {
                    flat.remove(key);
                }
            }
        }
        // Validate the prospective profile before persisting its selector. Its
        // exact source joins the final check even if the old store never read it.
        validation.layers[ConfigLayer::UserGlobal.index()] = flat;
        validation.layers[ConfigLayer::Profile.index()].clear();
        if let Some(profile) = validation.get(KEY_PROFILE).map(str::to_owned) {
            if !valid_profile_name(&profile) {
                return Err(ConfigError::Setting {
                    key: KEY_PROFILE.to_owned(),
                    reason: "profile must be one safe path component",
                });
            }
            let path = self
                .directories
                .config_dir()
                .join(PROFILE_DIRECTORY)
                .join(format!("{profile}.toml"));
            validation.read_file_into(ConfigLayer::Profile, &path, "")?;
        }
        let document = tomlmap::nest(
            &validation.layers[ConfigLayer::UserGlobal.index()],
            &self.config_path,
        )?;
        let text = toml::to_string_pretty(&document).map_err(|source| ConfigError::Serialize {
            path: self.config_path.clone(),
            source,
        })?;
        // A non-cooperating editor is not protected by our lock. Recheck after
        // validation, immediately before replacement; never overwrite a change
        // observed while preparing the candidate.
        self.revision.check()?;
        for revision in validated_sources {
            revision.check()?;
            // Retain prospective sources in atomic_write's post-staging check.
            validation.revision.sources.extend(revision.sources.clone());
        }
        crate::persistence::atomic_write(&self.config_path, text.as_bytes(), &validation.revision)?;
        let reloaded = self.reload().map_err(|_| ConfigError::PersistedReload)?;
        *self = reloaded;
        Ok(())
    }

    /// Fresh read/change/save/effective reload under the existing writer lock.
    /// The resident configuration worker is the sole caller/publication owner.
    pub fn commit_plugin_enabled(&mut self, plugin: &PluginId, enabled: bool) -> Result<(), ConfigError> {
        let _lock = crate::persistence::writer_lock(&self.config_path)?;
        let mut fresh = self.reload()?;
        fresh.set_user_global(
            &format!("plugins.{}.enabled", plugin.0),
            if enabled { "true" } else { "false" },
        );
        fresh.save_locked(&[])?;
        *self = fresh;
        Ok(())
    }

    /// Fresh retained-input read before replacing the complete discovered schema set.
    pub fn reload_unregistered(&self) -> Result<Self, ConfigError> {
        let overrides: Vec<_> = self.layers[ConfigLayer::SessionOverride.index()]
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Self::load_with_overrides(&self.directories, self.policy.as_deref(), &overrides)
    }

    /// Reloads retained directories, policy, session values and plugin schemas.
    /// Blocking: the configuration worker owns publication of this complete store.
    pub fn reload(&self) -> Result<Self, ConfigError> {
        let overrides: Vec<_> = self.layers[ConfigLayer::SessionOverride.index()]
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        self.reload_with_session_overrides(&overrides)
    }

    /// Resolves the retained policy/profile/user inputs as a fresh process,
    /// without promoting or retaining any session-only document.
    pub fn reload_without_session_overrides(&self) -> Result<Self, ConfigError> {
        self.reload_with_session_overrides(&[])
    }

    /// Projects a user-global draft through real current and restart precedence,
    /// without writing disk or promoting either context's winning values.
    /// Blocking: reloads retained sources for the non-session candidate.
    pub fn project_user_global_candidates(
        &self,
        key: &str,
        value: &str,
    ) -> Result<(Self, Self), ConfigError> {
        let mut current = self.clone();
        current.set_user_global(key, value);
        let mut retained = self.reload_without_session_overrides()?;
        retained.set_user_global(key, value);
        if key == KEY_PROFILE {
            // Changing the selector must validate the new profile's values,
            // not the profile layer retained from the old snapshot.
            for candidate in [&mut current, &mut retained] {
                candidate.layers[ConfigLayer::Profile.index()].clear();
                if let Some(profile) = candidate.get(KEY_PROFILE).map(str::to_owned) {
                    let directory = candidate.directories.config_dir().join(PROFILE_DIRECTORY);
                    if !valid_profile_name(&profile) {
                        return Err(ConfigError::Profile {
                            path: directory,
                            name: profile,
                            reason: "must be one safe path component",
                        });
                    }
                    candidate.read_file_into(
                        ConfigLayer::Profile,
                        &directory.join(format!("{profile}.toml")),
                        "",
                    )?;
                }
            }
        }
        Ok((current, retained))
    }

    fn reload_with_session_overrides(&self, overrides: &[(String, String)]) -> Result<Self, ConfigError> {
        let mut store = Self::load_with_overrides(&self.directories, self.policy.as_deref(), overrides)?;
        for (plugin, schema) in &self.schemas {
            store.register_plugin_schema(&PluginId(plugin.clone()), schema);
        }
        for owner in self.discovered_schemas.values() {
            let path = owner.package.join("crikey.toml");
            if self.revision.sources.get(&path) != Some(&crate::persistence::SourceState::read(&path)?) {
                return Err(ConfigError::Conflict);
            }
            let _ = store.register_discovered_schema(owner);
        }
        store.redact_unregistered = self.redact_unregistered;
        Ok(store)
    }

    /// An opaque exact-source token, including shadowed documents and session
    /// values. Keep it with the caller-owned draft, not just its winning value.
    pub fn revision(&self) -> crate::ConfigRevision {
        self.revision.clone()
    }

    /// Sources read by a projection but absent from its original store.
    /// Existing sources remain protected by the original revision.
    pub fn additional_source_revision(&self, original: &Self) -> crate::ConfigRevision {
        crate::ConfigRevision {
            sources: self
                .revision
                .sources
                .iter()
                .filter(|(path, _)| !original.revision.sources.contains_key(*path))
                .map(|(path, state)| (path.clone(), state.clone()))
                .collect(),
            ..Default::default()
        }
    }

    /// Validates and persists one edit without changing this published store on
    /// failure. Blocking filesystem/lock operations must run off the UI thread.
    pub fn commit_user_global(
        &mut self,
        key: &str,
        value: &str,
        expected: &crate::ConfigRevision,
    ) -> Result<(), ConfigError> {
        self.commit_user_global_checked(key, value, expected, &[])
    }

    /// Retains source snapshots read by semantic validation through the same
    /// writer lock and the final atomic replacement check.
    pub fn commit_user_global_checked(
        &mut self,
        key: &str,
        value: &str,
        expected: &crate::ConfigRevision,
        validated_sources: &[crate::ConfigRevision],
    ) -> Result<(), ConfigError> {
        if &self.revision != expected {
            return Err(ConfigError::Conflict);
        }
        if let Some((plugin, field)) = split_setting_key(key) {
            let declaration = self
                .schemas
                .get(plugin)
                .and_then(|schema| schema.field(field))
                .filter(|field| field.applies_to(std::env::consts::OS))
                .ok_or_else(|| ConfigError::Setting {
                    key: key.to_owned(),
                    reason: "configuration field is unavailable",
                })?;
            declaration
                .validate(value)
                .map_err(|violation| ConfigError::Schema {
                    plugin: plugin.to_owned(),
                    violation,
                })?;
        }
        let mut candidate = self.clone();
        candidate.set_user_global(key, value);
        let _lock = crate::persistence::writer_lock(&candidate.config_path)?;
        candidate.save_locked(validated_sources)?;
        *self = candidate;
        Ok(())
    }

    /// Commits one declared field to `plugins/<namespaced-id>.toml` (UserPlugin),
    /// not the weaker global layer. `field` is the schema-relative field name.
    ///
    /// Call only on the serialized configuration worker: takes the same blocking
    /// cross-process lock as `save`, with no independent store or polling owner.
    /// The caller must retain the exact discovered owner with its draft and
    /// re-register discovery replacements before admission. Unregistered owners
    /// are unavailable; changed owner/schema or source revisions are conflicts.
    /// Success publishes a full retained-input reload. `PersistedReload` means
    /// disk was committed but this store remains unchanged; reload before retry.
    /// External editors racing the final check/rename cannot be excluded.
    pub fn commit_plugin_user(
        &mut self,
        owner: &crate::DiscoveredSchema,
        field: &str,
        value: &str,
        expected: &crate::ConfigRevision,
    ) -> Result<(), ConfigError> {
        self.commit_plugin_user_with_reload(owner, field, value, expected, Self::reload)
    }

    fn commit_plugin_user_with_reload(
        &mut self,
        owner: &crate::DiscoveredSchema,
        field: &str,
        value: &str,
        expected: &crate::ConfigRevision,
        reload: impl FnOnce(&Self) -> Result<Self, ConfigError>,
    ) -> Result<(), ConfigError> {
        let unavailable = || ConfigError::Setting {
            key: format!("plugins.{}.settings.{field}", owner.plugin.0),
            reason: "configuration field or discovered owner is unavailable",
        };
        if &self.revision != expected {
            return Err(ConfigError::Conflict);
        }
        let registered = self
            .discovered_schemas
            .get(&owner.plugin.0)
            .ok_or_else(unavailable)?;
        let path = self
            .directories
            .config_dir()
            .join(PLUGIN_DIRECTORY)
            .join(format!("{}.toml", owner.plugin.0));
        let signature = toml::to_string(&owner.section).map_err(|source| ConfigError::Serialize {
            path: path.clone(),
            source,
        })?;
        if registered.package != owner.package
            || self
                .revision
                .schema_owners
                .get(&owner.plugin.0)
                .is_none_or(|(_, registered)| registered != &signature)
        {
            return Err(ConfigError::Conflict);
        }
        // Portable single-component spelling; rejects separators, drive names,
        // traversal, alternate streams and trailing-dot aliases on Windows.
        let namespace = owner.plugin.0.split('.').next().unwrap_or_default();
        const RESERVED: &[&str] = &[
            "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
            "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
        ];
        if !owner.plugin.0.contains('.')
            || owner.plugin.0.split('.').any(|part| {
                part.is_empty()
                    || !part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            })
            || RESERVED
                .iter()
                .any(|reserved| namespace.eq_ignore_ascii_case(reserved))
        {
            return Err(unavailable());
        }
        let declaration = registered
            .section
            .field(field)
            .filter(|declaration| declaration.applies_to(std::env::consts::OS))
            .ok_or_else(unavailable)?;
        declaration
            .validate(value)
            .map_err(|violation| ConfigError::Schema {
                plugin: owner.plugin.0.clone(),
                violation,
            })?;
        let _lock = crate::persistence::writer_lock(&self.config_path)?;
        self.revision.check()?;
        let directory = path.parent().expect("plugin document has a parent");
        crate::persistence::check_plugin_path(directory, true)?;
        crate::persistence::check_plugin_path(&path, false)?;
        // Read the actual raw document, not the schema-filtered effective map:
        // invalid/unknown unrelated fields must survive this single-field edit.
        let mut flat = BTreeMap::new();
        if let Some(bytes) = self
            .revision
            .sources
            .get(&path)
            .and_then(|state| state.bytes.as_deref())
        {
            let text = std::str::from_utf8(bytes).map_err(|_| ConfigError::Read {
                path: path.clone(),
                source: std::io::Error::other("configuration is not UTF-8"),
            })?;
            let table: toml::Table = text
                .parse()
                .map_err(|source: toml::de::Error| ConfigError::parse(&path, text, &source))?;
            tomlmap::flatten(&table, "", &path, &mut flat)?;
        }
        flat.insert(format!("settings.{field}"), value.to_owned());
        let nested = tomlmap::nest(&flat, &path)?;
        let text = toml::to_string_pretty(&nested).map_err(|source| ConfigError::Serialize {
            path: path.clone(),
            source,
        })?;
        self.revision.check()?;
        let mut persistence_revision = self.revision.clone();
        if !directory.exists() {
            std::fs::create_dir(directory).map_err(|source| ConfigError::Write {
                path: directory.to_path_buf(),
                source,
            })?;
            let state = crate::persistence::SourceState::read(directory)?;
            if !state.is_directory || !state.entries.is_empty() {
                return Err(ConfigError::Conflict);
            }
            persistence_revision
                .sources
                .insert(directory.to_path_buf(), state);
        }
        crate::persistence::check_plugin_path(directory, true)?;
        crate::persistence::check_plugin_path(&path, false)?;
        // Stage outside the watched plugin directory: creating the temporary
        // file there would itself invalidate its exact directory revision.
        crate::persistence::atomic_write_in(
            &path,
            text.as_bytes(),
            &persistence_revision,
            self.directories.config_dir(),
            true,
        )?;
        let reloaded = reload(self).map_err(|_| ConfigError::PersistedReload)?;
        *self = reloaded;
        Ok(())
    }
}

/// The system-wide policy file for `convention` (spec 21.2, layer 2).
///
/// Not derived from [`StandardDirectories`]: those are per-user, and a policy an
/// administrator deploys must not sit anywhere the user can rewrite. `Xdg` uses
/// `/etc`, Windows uses `%PROGRAMDATA%` (falling back to the documented default
/// when the variable is unset, because a service account may not have it), and
/// macOS uses the system-wide `Application Support`.
pub fn administrator_policy_path(convention: DirectoryConvention) -> PathBuf {
    match convention {
        DirectoryConvention::Xdg => PathBuf::from("/etc").join("crikey").join(POLICY_FILE),
        DirectoryConvention::Windows => {
            let root = std::env::var_os("PROGRAMDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
            root.join("CriKey").join(POLICY_FILE)
        }
        DirectoryConvention::MacOs => PathBuf::from("/Library")
            .join("Application Support")
            .join("CriKey")
            .join(POLICY_FILE),
    }
}

/// `plugins.<id>.enabled` (spec 21.2).
fn plugin_enabled_key(plugin: &PluginId) -> String {
    format!("{PLUGINS_PREFIX}{}{ENABLED_SUFFIX}", plugin.0)
}

/// `plugins.<id>.scheduling-profile`.
fn plugin_scheduling_profile_key(plugin: &PluginId) -> String {
    format!("{PLUGINS_PREFIX}{}{SCHEDULING_PROFILE_SUFFIX}", plugin.0)
}

/// Splits `plugins.<id>.settings.<field>` into its plugin id and field name.
///
/// A plugin id contains dots (`modern.example`), so the split is anchored on the
/// LAST `.settings.` and the field name is required to be dot-free — which the
/// manifest model already guarantees for a declared field.
fn split_setting_key(key: &str) -> Option<(&str, &str)> {
    let rest = key.strip_prefix(PLUGINS_PREFIX)?;
    let marker = rest.rfind(SETTINGS_MARKER)?;
    let plugin = &rest[..marker];
    let field = &rest[marker + SETTINGS_MARKER.len()..];
    if plugin.is_empty() || field.is_empty() || field.contains('.') {
        return None;
    }
    Some((plugin, field))
}

/// The configuration spelling of a scheduling profile.
///
/// Hand-written rather than routed through serde so the store does not have to
/// serialise a one-variant document to answer; `the_wire_spelling_matches_serde`
/// below pins the two together, so a rename in the manifest model cannot leave
/// this behind.
fn scheduling_profile_name(profile: SchedulingProfile) -> &'static str {
    match profile {
        SchedulingProfile::LegacyStrict => "legacy-strict",
        SchedulingProfile::LegacyOptimized => "legacy-optimized",
        SchedulingProfile::Modern => "modern",
    }
}

fn parse_scheduling_profile(text: &str) -> Option<SchedulingProfile> {
    [
        SchedulingProfile::LegacyStrict,
        SchedulingProfile::LegacyOptimized,
        SchedulingProfile::Modern,
    ]
    .into_iter()
    .find(|profile| scheduling_profile_name(*profile) == text)
}

fn valid_profile_name(name: &str) -> bool {
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\', ':']) {
        return false;
    }
    let mut components = std::path::Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_persisted_reload_failure_keeps_publication_but_not_the_old_disk() {
        use crikey_platform::DirectoryEnvironment;
        let root = tempfile::tempdir().unwrap();
        let environment = [
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "CRIKEY_CONFIG_DIR",
            "CRIKEY_DATA_DIR",
            "CRIKEY_CACHE_DIR",
            "CRIKEY_STATE_DIR",
        ]
        .into_iter()
        .fold(DirectoryEnvironment::new(), |environment, key| {
            environment.set(key, root.path().as_os_str())
        });
        let directories = StandardDirectories::resolve(DirectoryConvention::current(), &environment).unwrap();
        let mut store = ConfigStore::load_with_policy(&directories, None).unwrap();
        let owner = crate::DiscoveredSchema {
            plugin: PluginId("modern.example".into()),
            package: root.path().join("package"),
            section: toml::from_str("[[field]]\nname = \"value\"\ndefault = \"old\"\n").unwrap(),
        };
        assert!(store.register_discovered_schema(&owner).is_empty());
        let expected = store.revision();
        let result = store.commit_plugin_user_with_reload(&owner, "value", "new", &expected, |store| {
            assert!(store
                .directories
                .config_dir()
                .join("plugins/modern.example.toml")
                .is_file());
            std::fs::write(store.config_path(), "[launcher]\nvalue = [invalid\n").unwrap();
            store.reload()
        });
        assert!(matches!(result, Err(ConfigError::PersistedReload)));
        assert_eq!(store.revision(), expected);
        assert_eq!(store.get("plugins.modern.example.settings.value"), Some("old"));
        std::fs::remove_file(store.config_path()).unwrap();
        let consumer = store.reload().unwrap();
        assert_eq!(consumer.get("plugins.modern.example.settings.value"), Some("new"));
        assert_eq!(
            consumer.layer_of("plugins.modern.example.settings.value"),
            Some(ConfigLayer::UserPlugin)
        );
    }

    #[test]
    fn the_wire_spelling_matches_the_manifest_models_serde_spelling() {
        for profile in [
            SchedulingProfile::LegacyStrict,
            SchedulingProfile::LegacyOptimized,
            SchedulingProfile::Modern,
        ] {
            let serialised = toml::Value::try_from(profile).expect("the enum serialises");
            assert_eq!(
                serialised.as_str(),
                Some(scheduling_profile_name(profile)),
                "the configuration spelling drifted from the manifest model's"
            );
            assert_eq!(
                parse_scheduling_profile(scheduling_profile_name(profile)),
                Some(profile)
            );
        }
    }

    #[test]
    fn an_unrecognised_scheduling_profile_is_not_a_profile() {
        assert_eq!(parse_scheduling_profile("modrn"), None);
    }

    #[test]
    fn a_settings_key_splits_on_the_last_settings_marker() {
        assert_eq!(
            split_setting_key("plugins.modern.example.settings.theme"),
            Some(("modern.example", "theme"))
        );
        assert_eq!(split_setting_key("plugins.modern.example.enabled"), None);
        assert_eq!(split_setting_key("launcher.profile"), None);
        assert_eq!(split_setting_key("plugins.modern.example.settings."), None);
    }

    #[test]
    fn each_convention_places_the_policy_file_outside_any_per_user_directory() {
        assert_eq!(
            administrator_policy_path(DirectoryConvention::Xdg),
            PathBuf::from("/etc/crikey/policy.toml")
        );
        assert_eq!(
            administrator_policy_path(DirectoryConvention::MacOs),
            PathBuf::from("/Library/Application Support/CriKey/policy.toml")
        );
        let windows = administrator_policy_path(DirectoryConvention::Windows);
        assert!(
            windows.ends_with(PathBuf::from("CriKey").join("policy.toml")),
            "{}",
            windows.display()
        );
    }
}
