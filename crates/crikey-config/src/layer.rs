//! The seven configuration layers and their precedence (spec 21.2).

/// One source of configuration values, ordered by precedence.
///
/// The declaration order IS the precedence order: a later variant outranks an
/// earlier one, and the derived [`Ord`] is what [`crate::ConfigStore`] resolves
/// with. Reordering these variants changes the product's behaviour, which is why
/// the order is fixed by specification rather than by convenience.
///
/// # Why plugin defaults outrank user-global settings
///
/// Read naively, layer 5 beating layer 3 says a plugin's own default overrides
/// what the user wrote, which would be absurd. It is not what the ordering
/// means, because each layer has exactly one source and those sources address
/// different things:
///
/// * Layers 1–4 are the *host's* settings, from the narrowest scope outward:
///   compiled-in defaults, then an administrator's policy, then the user's
///   global file, then the profile the user selected.
/// * Layer 5 is what a *plugin* declares as its own default in its manifest.
/// * Layer 6 is what the user wrote *for that specific plugin*, in that plugin's
///   own file.
///
/// So the ordering says: a plugin's declared default beats a global sweep that
/// happened to name the same key, and the user's plugin-specific file beats the
/// plugin's default. That is the sensible reading, and it is the one this crate
/// implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConfigLayer {
    /// Compiled into the host. Always present, never written to disk.
    BuiltInDefaults,
    /// A system-wide policy file an administrator deploys.
    AdministratorPolicy,
    /// The user's own `config.toml`. The only layer [`crate::ConfigStore::save`]
    /// rewrites.
    UserGlobal,
    /// The profile named by `launcher.profile`.
    Profile,
    /// Defaults declared by each plugin's `[configuration]` section.
    PluginDefaults,
    /// The user's per-plugin file, `plugins/<plugin-id>.toml`.
    UserPlugin,
    /// Set in memory for this process only; never persisted.
    SessionOverride,
}

/// How many layers there are, so the store can hold a fixed-size array rather
/// than a map that could be missing one.
pub const LAYER_COUNT: usize = 7;

impl ConfigLayer {
    /// Every layer, lowest precedence first.
    pub const ALL: [Self; LAYER_COUNT] = [
        Self::BuiltInDefaults,
        Self::AdministratorPolicy,
        Self::UserGlobal,
        Self::Profile,
        Self::PluginDefaults,
        Self::UserPlugin,
        Self::SessionOverride,
    ];

    /// The layer's index into a [`LAYER_COUNT`]-sized array.
    pub const fn index(self) -> usize {
        match self {
            Self::BuiltInDefaults => 0,
            Self::AdministratorPolicy => 1,
            Self::UserGlobal => 2,
            Self::Profile => 3,
            Self::PluginDefaults => 4,
            Self::UserPlugin => 5,
            Self::SessionOverride => 6,
        }
    }

    /// The layer's stable name, as printed by `crikey config layers`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BuiltInDefaults => "built-in-defaults",
            Self::AdministratorPolicy => "administrator-policy",
            Self::UserGlobal => "user-global",
            Self::Profile => "profile",
            Self::PluginDefaults => "plugin-defaults",
            Self::UserPlugin => "user-plugin",
            Self::SessionOverride => "session-override",
        }
    }
}

impl std::fmt::Display for ConfigLayer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_declared_order_is_the_specified_precedence_order() {
        // Spec 21.2 lists the layers 1..7 with the last winning. Asserted
        // pairwise rather than as a sorted-list check, so a swapped pair names
        // itself in the failure.
        assert!(ConfigLayer::BuiltInDefaults < ConfigLayer::AdministratorPolicy);
        assert!(ConfigLayer::AdministratorPolicy < ConfigLayer::UserGlobal);
        assert!(ConfigLayer::UserGlobal < ConfigLayer::Profile);
        assert!(ConfigLayer::Profile < ConfigLayer::PluginDefaults);
        assert!(ConfigLayer::PluginDefaults < ConfigLayer::UserPlugin);
        assert!(ConfigLayer::UserPlugin < ConfigLayer::SessionOverride);
    }

    #[test]
    fn every_layer_has_a_distinct_index_inside_the_array_bound() {
        let mut seen = [false; LAYER_COUNT];
        for layer in ConfigLayer::ALL {
            let index = layer.index();
            assert!(index < LAYER_COUNT, "{layer} indexes out of bounds");
            assert!(!seen[index], "{layer} shares an index with an earlier layer");
            seen[index] = true;
        }
        assert!(seen.iter().all(|slot| *slot), "a layer has no index");
    }

    #[test]
    fn the_index_order_matches_the_precedence_order() {
        // The store resolves by walking indices downward, so an index that did
        // not follow `Ord` would silently invert precedence.
        for pair in ConfigLayer::ALL.windows(2) {
            assert!(
                pair[0].index() < pair[1].index(),
                "{} must index below {}",
                pair[0],
                pair[1]
            );
        }
    }
}
