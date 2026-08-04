//! One complete per-plugin configuration state (spec 21.4).

use std::collections::BTreeMap;

use crikey_core::PluginId;

/// The configuration of every plugin the host knows about, at one instant.
///
/// Always the *whole* state, never a delta. Spec 21.4 requires the host to send
/// "the latest complete configuration state rather than every intermediate
/// edit", and a type that could hold a partial state would make that a rule to
/// remember rather than a property of the value. It is `PartialEq` because the
/// publisher's decision to say nothing when nothing changed is an equality test
/// on exactly this.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ConfigurationSnapshot {
    plugins: BTreeMap<PluginId, BTreeMap<String, String>>,
}

impl std::fmt::Debug for ConfigurationSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: BTreeMap<&PluginId, Vec<&str>> = self
            .plugins
            .iter()
            .map(|(plugin, values)| (plugin, values.keys().map(String::as_str).collect()))
            .collect();
        formatter
            .debug_struct("ConfigurationSnapshot")
            .field("plugin_keys", &keys)
            .finish()
    }
}

impl ConfigurationSnapshot {
    /// Builds a snapshot from each plugin's complete settings map.
    pub fn new(plugins: BTreeMap<PluginId, BTreeMap<String, String>>) -> Self {
        Self { plugins }
    }

    /// Every plugin in the snapshot, in a fixed order.
    pub fn plugins(&self) -> &BTreeMap<PluginId, BTreeMap<String, String>> {
        &self.plugins
    }

    /// The settings for one plugin.
    ///
    /// `None` means the host does not know this plugin at all, which is different
    /// from an empty map: a plugin present with no settings must still be told
    /// so, or it would keep applying whatever it was last sent.
    pub fn values_for(&self, plugin: &PluginId) -> Option<&BTreeMap<String, String>> {
        self.plugins.get(plugin)
    }

    /// Whether the snapshot names no plugins.
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}
