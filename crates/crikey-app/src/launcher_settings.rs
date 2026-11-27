//! Pure launcher settings declarations, readers and edit validation.
//!
//! The CLI and serialized configuration worker share these declarations.
//! Platform binding, activation-conflict checks and persistence stay with the
//! existing configuration/composition owners.

use crikey_config::{
    ConfigStore, BUILT_IN_DEFAULTS, KEY_ACTIVATION_HOTKEY, KEY_MAX_RESULTS, KEY_PROFILE, KEY_ROUNDED_CORNERS,
    KEY_SHOW_HINTS,
};
use crikey_config::{
    KEY_INTERFACE_SIZE, KEY_NAVIGATION_BINDINGS, KEY_NAVIGATION_STYLE, KEY_RESULT_DENSITY, KEY_THEME,
    KEY_THEME_OVERRIDES,
};
use crikey_platform::Accelerator;
use crikey_ui::appearance::{
    AppearancePreferences, InterfaceSize, ResultDensity, ThemeOverrides, ThemePreference,
};
use crikey_ui::navigation::{CompiledNavigation, NavigationStyle, ShortcutSource};
use crikey_ui::{SettingControl, SettingRow};

use crate::settings_search::SettingDefinition;

// This key is not exported by crikey-config; retain the existing CLI spelling.
const KEY_PREVIEW_SPLIT: &str = "launcher.preview-split";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Text,
    /// Existing launcher semantics: only the exact text `false` disables it.
    Toggle,
    Choice {
        options: &'static [(&'static str, &'static str)],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    Live,
    Required,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validator {
    Accelerator,
    PositiveResults,
    ProfileName,
    Boolean,
    PreviewSplit,
    Milliseconds,
    QueryHistory,
    CommandHome,
    Theme,
    InterfaceSize,
    ResultDensity,
    ThemeOverrides,
    NavigationStyle,
    NavigationBindings,
}

#[derive(Debug, Clone, Copy)]
pub struct LauncherSetting {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    pub control: Control,
    pub navigation_target: &'static str,
    /// Empty means all platforms, as in ConfigurationField.
    pub platforms: &'static [String],
    pub restart: Restart,
    pub validator: Validator,
    // These three navigation defaults are historically supplied by the CLI,
    // even without a store. Other display fallbacks must remain unset.
    fallback: Option<&'static str>,
}

pub const THEME_OPTIONS: &[(&str, &str)] = &[("system", "System"), ("light", "Light"), ("dark", "Dark")];
pub const INTERFACE_SIZE_OPTIONS: &[(&str, &str)] =
    &[("small", "Small"), ("default", "Default"), ("large", "Large")];
pub const RESULT_DENSITY_OPTIONS: &[(&str, &str)] = &[
    ("compact", "Compact"),
    ("default", "Default"),
    ("comfortable", "Comfortable"),
];
pub const NAVIGATION_STYLE_OPTIONS: &[(&str, &str)] =
    &[("default", "Default"), ("emacs", "Emacs"), ("vim", "Vim")];

/// The user-facing declarations, in the shared panel/CLI order.
/// Operator-only configuration publication timing remains outside this surface.
pub const LAUNCHER_SETTINGS: &[LauncherSetting] = &[
    LauncherSetting {
        key: KEY_ACTIVATION_HOTKEY,
        label: "Activation hotkey",
        description: "Global accelerator that raises the launcher.",
        category: "Keyboard",
        control: Control::Text,
        navigation_target: KEY_ACTIVATION_HOTKEY,
        platforms: &[],
        restart: Restart::Live,
        validator: Validator::Accelerator,
        fallback: None,
    },
    LauncherSetting {
        key: KEY_MAX_RESULTS,
        label: "Maximum results",
        description: "Limit results across all plugins; unset uses the aggregator default.",
        category: "Launcher",
        control: Control::Text,
        navigation_target: KEY_MAX_RESULTS,
        platforms: &[],
        restart: Restart::Live,
        validator: Validator::PositiveResults,
        fallback: None,
    },
    LauncherSetting {
        key: KEY_PROFILE,
        label: "Configuration profile",
        description: "Select a named configuration profile, not a path.",
        category: "Launcher",
        control: Control::Text,
        navigation_target: KEY_PROFILE,
        platforms: &[],
        restart: Restart::Live,
        validator: Validator::ProfileName,
        fallback: None,
    },
    LauncherSetting {
        key: KEY_SHOW_HINTS,
        label: "Show keyboard hints",
        description: "Display the footer navigation hint line.",
        category: "Appearance",
        control: Control::Toggle,
        navigation_target: KEY_SHOW_HINTS,
        platforms: &[],
        restart: Restart::Live,
        validator: Validator::Boolean,
        fallback: None,
    },
    LauncherSetting {
        key: KEY_ROUNDED_CORNERS,
        label: "Rounded corners",
        description: "Draw the launcher window with rounded corners.",
        category: "Appearance",
        control: Control::Toggle,
        navigation_target: KEY_ROUNDED_CORNERS,
        platforms: &[],
        restart: Restart::Live,
        validator: Validator::Boolean,
        fallback: None,
    },
    LauncherSetting {
        key: KEY_PREVIEW_SPLIT,
        label: "Preview split proportion",
        description: "Remember the preview divider proportion between 0.3 and 0.7.",
        category: "Appearance",
        control: Control::Text,
        navigation_target: KEY_PREVIEW_SPLIT,
        platforms: &[],
        restart: Restart::Live,
        validator: Validator::PreviewSplit,
        fallback: None,
    },
    LauncherSetting {
        key: "launcher.resume-delay-ms",
        label: "Logical resume delay (milliseconds, 0 resets)",
        description: "Retain navigation state for a nonnegative millisecond interval; zero resets it.",
        category: "Launcher",
        control: Control::Text,
        navigation_target: "launcher.resume-delay-ms",
        platforms: &[],
        restart: Restart::Live,
        validator: Validator::Milliseconds,
        fallback: Some("0"),
    },
    LauncherSetting {
        key: "launcher.query-history",
        label: "Query history (off, session, persistent)",
        description: "Choose whether query recall is disabled, session-only, or persisted.",
        category: "Privacy/History",
        control: Control::Text,
        navigation_target: "launcher.query-history",
        platforms: &[],
        restart: Restart::Live,
        validator: Validator::QueryHistory,
        fallback: Some("session"),
    },
    LauncherSetting {
        key: "launcher.command-home",
        label: "Command home (compact, favorites, recent, favorites-and-recent)",
        description: "Choose the commands displayed on the empty-query home surface.",
        category: "Launcher",
        control: Control::Text,
        navigation_target: "launcher.command-home",
        platforms: &[],
        restart: Restart::Live,
        validator: Validator::CommandHome,
        fallback: Some("compact"),
    },
    LauncherSetting {
        key: KEY_THEME,
        label: "Theme (system, light, dark)",
        description: "Follow the system theme or choose light or dark; an unknown system theme uses dark.",
        category: "Appearance",
        control: Control::Choice { options: THEME_OPTIONS },
        navigation_target: KEY_THEME,
        platforms: &[],
        restart: Restart::Live,
        validator: Validator::Theme,
        fallback: Some("system"),
    },
    LauncherSetting {
        key: KEY_INTERFACE_SIZE,
        label: "Interface size (small, default, large)",
        description: "Scale logical interface geometry independently of operating-system display scaling.",
        category: "Appearance",
        control: Control::Choice { options: INTERFACE_SIZE_OPTIONS },
        navigation_target: KEY_INTERFACE_SIZE,
        platforms: &[],
        restart: Restart::Live,
        validator: Validator::InterfaceSize,
        fallback: Some("default"),
    },
    LauncherSetting {
        key: KEY_RESULT_DENSITY,
        label: "Result density (compact, default, comfortable)",
        description: "Choose result-row spacing without changing display scaling.",
        category: "Appearance",
        control: Control::Choice { options: RESULT_DENSITY_OPTIONS },
        navigation_target: KEY_RESULT_DENSITY,
        platforms: &[],
        restart: Restart::Live,
        validator: Validator::ResultDensity,
        fallback: Some("default"),
    },
    LauncherSetting {
        key: KEY_THEME_OVERRIDES,
        label: "Theme color overrides (JSON object)",
        description: "Atomic role-to-#RRGGBB colors, at most 4096 bytes; combined colors must retain readable contrast.",
        category: "Appearance",
        control: Control::Text,
        navigation_target: KEY_THEME_OVERRIDES,
        platforms: &[],
        restart: Restart::Live,
        validator: Validator::ThemeOverrides,
        fallback: Some("{}"),
    },
    LauncherSetting {
        key: KEY_NAVIGATION_STYLE,
        label: "Navigation style (default, emacs, vim)",
        description: "Choose semantic navigation chords; ordinary typing and owned editors retain their keys.",
        category: "Keyboard",
        control: Control::Choice { options: NAVIGATION_STYLE_OPTIONS },
        navigation_target: KEY_NAVIGATION_STYLE,
        platforms: &[],
        restart: Restart::Live,
        validator: Validator::NavigationStyle,
        fallback: Some("default"),
    },
    LauncherSetting {
        key: KEY_NAVIGATION_BINDINGS,
        label: "Navigation bindings (JSON object)",
        description: "Atomic action-to-chord-array overrides, at most 8192 bytes; overlapping shortcuts are rejected.",
        category: "Keyboard",
        control: Control::Text,
        navigation_target: KEY_NAVIGATION_BINDINGS,
        platforms: &[],
        restart: Restart::Live,
        validator: Validator::NavigationBindings,
        fallback: Some("{}"),
    },
];

impl LauncherSetting {
    /// Shared editor/admission budget for this declaration's serialized value.
    pub fn max_edit_bytes(&self) -> usize {
        match self.validator {
            Validator::NavigationBindings => crikey_ui::navigation::MAX_NAVIGATION_BINDINGS_BYTES,
            _ => 4096,
        }
    }

    pub fn search_definition(&self) -> SettingDefinition<'static> {
        SettingDefinition {
            key: self.key,
            label: self.label,
            description: self.description,
            category: self.category,
            navigation_target: self.navigation_target,
            control: match self.control {
                Control::Text => "text",
                Control::Toggle => "boolean",
                Control::Choice { .. } => "choice",
            },
            requires_restart: self.restart == Restart::Required,
            secret: false,
            platforms: self.platforms,
        }
    }

    /// Display text is redacted by ConfigStore; it is not the behaviour reader.
    pub fn read<'a>(&self, store: Option<&'a ConfigStore>) -> SettingRead<'a> {
        SettingRead {
            value: store
                .and_then(|store| store.display_value(self.key))
                .or(self.fallback)
                .unwrap_or(""),
            source: store.and_then(|store| store.layer_of(self.key)).map_or(
                if self.fallback.is_some() {
                    "builtin-defaults"
                } else {
                    "unset"
                },
                |layer| layer.as_str(),
            ),
        }
    }

    pub fn row(&self, store: Option<&ConfigStore>) -> SettingRow {
        let read = self.read(store);
        SettingRow {
            key: self.key.to_owned(),
            label: self.label.to_owned(),
            value: read.value.to_owned(),
            source: read.source.to_owned(),
            control: match self.control {
                Control::Text => SettingControl::Text,
                Control::Toggle => SettingControl::Toggle {
                    on: read_boolean(store, self.key),
                },
                Control::Choice { options } => SettingControl::Choice { options },
            },
        }
    }

    pub fn validate(&self, value: &str) -> Result<(), String> {
        let key = self.key;
        if value.is_empty() {
            return Err(format!("`{key}` cannot be set to an empty value"));
        }
        match self.validator {
            Validator::Accelerator => Accelerator::parse(value)
                .map(|_| ())
                .map_err(|error| format!("`{value}` is not a valid accelerator: {error}")),
            Validator::PositiveResults => match value.parse::<usize>() {
                Ok(limit) if limit > 0 => Ok(()),
                _ => Err(format!(
                    "`{key}` must be a whole number of results above zero, got `{value}`"
                )),
            },
            Validator::ProfileName => {
                if value.contains(['/', '\\']) || value == "." || value == ".." {
                    Err(format!("`{key}` must be a plain profile name, got `{value}`"))
                } else {
                    Ok(())
                }
            }
            Validator::Boolean => {
                if value == "true" || value == "false" {
                    Ok(())
                } else {
                    Err(format!("`{key}` must be `true` or `false`, got `{value}`"))
                }
            }
            Validator::PreviewSplit => match value.parse::<f32>() {
                Ok(value) if value.is_finite() && (0.3..=0.7).contains(&value) => Ok(()),
                _ => Err("Preview split must be a number between 0.3 and 0.7".into()),
            },
            Validator::Milliseconds => value
                .parse::<u64>()
                .map(|_| ())
                .map_err(|_| "Resume delay must be a nonnegative millisecond count".into()),
            Validator::QueryHistory => match value {
                "off" | "session" | "persistent" => Ok(()),
                _ => Err("Query history must be off, session, or persistent".into()),
            },
            Validator::CommandHome => match value {
                "compact" | "favorites" | "recent" | "favorites-and-recent" => Ok(()),
                _ => Err("Command home must be compact, favorites, recent, or favorites-and-recent".into()),
            },
            Validator::Theme => value
                .parse::<ThemePreference>()
                .map(|_| ())
                .map_err(|error| error.to_string()),
            Validator::InterfaceSize => value
                .parse::<InterfaceSize>()
                .map(|_| ())
                .map_err(|error| error.to_string()),
            Validator::ResultDensity => value
                .parse::<ResultDensity>()
                .map(|_| ())
                .map_err(|error| error.to_string()),
            Validator::ThemeOverrides => ThemeOverrides::parse(value)
                .map(|_| ())
                .map_err(|error| error.to_string()),
            Validator::NavigationStyle => NavigationStyle::parse(value)
                .map(|_| ())
                .map_err(|error| error.to_string()),
            Validator::NavigationBindings => {
                crikey_ui::navigation::validate_bindings_json(value).map_err(|error| error.to_string())
            }
        }
    }
}

/// Parses the effective appearance atomically. Invalid external values return
/// an error: retain last-good preferences (defaults at startup) with a diagnostic.
/// Raw settings metadata reports provenance, never applied state.
pub fn configured_appearance(store: Option<&ConfigStore>) -> Result<AppearancePreferences, String> {
    let value = |key| {
        declaration(key)
            .expect("appearance declaration")
            .read(store)
            .value
    };
    let preferences = AppearancePreferences {
        theme: value(KEY_THEME)
            .parse::<ThemePreference>()
            .map_err(|error| error.to_string())?,
        interface_size: value(KEY_INTERFACE_SIZE)
            .parse::<InterfaceSize>()
            .map_err(|error| error.to_string())?,
        result_density: value(KEY_RESULT_DENSITY)
            .parse::<ResultDensity>()
            .map_err(|error| error.to_string())?,
        overrides: ThemeOverrides::parse(value(KEY_THEME_OVERRIDES)).map_err(|error| error.to_string())?,
    };
    preferences.validate().map_err(|error| error.to_string())?;
    Ok(preferences)
}

/// Compiles one effective profile/override pair. Errors must never be presented
/// as applied bindings. Actual OS registrations remain runtime authority.
pub fn configured_navigation(store: Option<&ConfigStore>) -> Result<CompiledNavigation, String> {
    let style = declaration(KEY_NAVIGATION_STYLE)
        .expect("navigation declaration")
        .read(store)
        .value;
    let bindings = declaration(KEY_NAVIGATION_BINDINGS)
        .expect("navigation declaration")
        .read(store)
        .value;
    let navigation = CompiledNavigation::compile(
        NavigationStyle::parse(style).map_err(|error| error.to_string())?,
        bindings,
    )
    .map_err(|error| error.to_string())?;
    if let Some(config) = store {
        validate_navigation_shortcuts(config, &navigation)?;
    }
    Ok(navigation)
}

/// Validates a complete desired snapshot, never claims that OS grabs succeeded.
/// Runtime consumers separately diagnose AppliedActivation/AppliedCommand
/// against registrations actually held after failed replacement.
pub fn validate_configuration(config: &ConfigStore) -> Result<(), String> {
    configured_appearance(Some(config))?;
    configured_navigation(Some(config))?;
    Ok(())
}

fn validate_navigation_shortcuts(
    config: &ConfigStore,
    navigation: &CompiledNavigation,
) -> Result<(), String> {
    let activation = Accelerator::parse(
        config
            .get(KEY_ACTIVATION_HOTKEY)
            .ok_or("Launcher activation is unavailable")?,
    )
    .map_err(|error| error.to_string())?;
    if let Some(conflict) = navigation.conflict(activation, ShortcutSource::DesiredActivation) {
        return Err(format!(
            "Launcher activation conflicts with navigation: {conflict:?}"
        ));
    }
    let preferences = config
        .get(crikey_config::command_preferences::KEY_COMMAND_PREFERENCES)
        .map(crikey_config::command_preferences::CommandPreferences::decode)
        .transpose()?
        .unwrap_or_default();
    for record in preferences.records {
        if let Some(hotkey) = record.hotkey {
            let chord = Accelerator::parse(&hotkey).map_err(|error| error.to_string())?;
            if chord == activation {
                return Err(
                    "This shortcut is assigned to launcher activation and a command; unassign it first"
                        .into(),
                );
            }
            if let Some(conflict) = navigation.conflict(chord, ShortcutSource::SavedCommand) {
                return Err(format!(
                    "Saved command shortcut conflicts with navigation: {conflict:?}"
                ));
            }
        }
    }
    Ok(())
}

/// Shared CLI/GUI validation through real current AND restart precedence.
/// Carry returned source receipts into `commit_user_global_checked`; discarding
/// them would drop prospective profiles from the final atomic source check.
pub fn validate_edit(
    config: &ConfigStore,
    key: &str,
    value: &str,
) -> Result<[crikey_config::ConfigRevision; 2], String> {
    let mut receipts = Default::default();
    validate(key, value)?;
    if matches!(
        key,
        KEY_THEME
            | KEY_INTERFACE_SIZE
            | KEY_RESULT_DENSITY
            | KEY_THEME_OVERRIDES
            | KEY_NAVIGATION_STYLE
            | KEY_NAVIGATION_BINDINGS
            | KEY_ACTIVATION_HOTKEY
            | KEY_PROFILE
    ) {
        let (current, restart) = config
            .project_user_global_candidates(key, value)
            .map_err(|error| format!("Configuration could not be projected: {error}"))?;
        for (context, candidate) in [("Current", &current), ("Restart", &restart)] {
            // Validate the entire affected group, but do not make an unrelated
            // invalid group prevent a repair. Profiles can change both groups.
            let result = match key {
                KEY_THEME | KEY_INTERFACE_SIZE | KEY_RESULT_DENSITY | KEY_THEME_OVERRIDES => {
                    configured_appearance(Some(candidate)).map(|_| ())
                }
                KEY_NAVIGATION_STYLE | KEY_NAVIGATION_BINDINGS | KEY_ACTIVATION_HOTKEY => {
                    configured_navigation(Some(candidate)).map(|_| ())
                }
                KEY_PROFILE => validate_configuration(candidate),
                _ => unreachable!("validated preference group"),
            };
            result.map_err(|error| format!("{context} configuration: {error}"))?;
        }
        receipts = [
            current.additional_source_revision(config),
            restart.additional_source_revision(config),
        ];
    }
    Ok(receipts)
}

/// Borrowed, redacted display/provenance; no second configuration store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingRead<'a> {
    pub value: &'a str,
    pub source: &'static str,
}

pub fn declaration(key: &str) -> Option<&'static LauncherSetting> {
    LAUNCHER_SETTINGS.iter().find(|setting| setting.key == key)
}

pub fn rows(store: Option<&ConfigStore>) -> Vec<SettingRow> {
    LAUNCHER_SETTINGS
        .iter()
        .map(|setting| setting.row(store))
        .collect()
}

pub fn search_definitions() -> [SettingDefinition<'static>; LAUNCHER_SETTINGS.len()] {
    std::array::from_fn(|index| LAUNCHER_SETTINGS[index].search_definition())
}

/// Declaration validation only. The owner must ALSO validate activation-hotkey
/// conflicts against current and restart preferences and perform real OS binding.
pub fn validate(key: &str, value: &str) -> Result<(), String> {
    // Preserve the existing empty-value diagnostic even for unknown keys.
    if value.is_empty() {
        return Err(format!("`{key}` cannot be set to an empty value"));
    }
    declaration(key)
        .ok_or_else(|| format!("`{key}` is not a launcher setting"))?
        .validate(value)
}

fn read_boolean(store: Option<&ConfigStore>, key: &str) -> bool {
    store.and_then(|store| store.get(key)) != Some("false")
}

pub fn configured_show_hints(store: Option<&ConfigStore>) -> bool {
    read_boolean(store, KEY_SHOW_HINTS)
}
pub fn configured_rounded_corners(store: Option<&ConfigStore>) -> bool {
    read_boolean(store, KEY_ROUNDED_CORNERS)
}

pub fn configured_hotkey(store: Option<&ConfigStore>) -> String {
    store
        .and_then(|store| store.get(KEY_ACTIVATION_HOTKEY))
        .unwrap_or_else(|| {
            BUILT_IN_DEFAULTS
                .iter()
                .find(|(key, _)| *key == KEY_ACTIVATION_HOTKEY)
                .map_or("", |(_, value)| *value)
        })
        .to_owned()
}

pub fn configured_preview_split(store: Option<&ConfigStore>) -> f32 {
    store
        .and_then(|store| store.get(KEY_PREVIEW_SPLIT))
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| value.is_finite() && (0.3..=0.7).contains(value))
        .unwrap_or(0.55)
}

pub fn configured_resume_delay(store: Option<&ConfigStore>) -> std::time::Duration {
    std::time::Duration::from_millis(
        store
            .and_then(|store| store.get("launcher.resume-delay-ms"))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0),
    )
}

pub fn configured_query_history(store: Option<&ConfigStore>) -> crate::QueryHistoryMode {
    match store.and_then(|store| store.get("launcher.query-history")) {
        Some("off") => crate::QueryHistoryMode::Off,
        Some("persistent") => crate::QueryHistoryMode::Persistent,
        _ => crate::QueryHistoryMode::Session,
    }
}

#[cfg(test)]
#[path = "launcher_settings_tests.rs"]
mod tests;
