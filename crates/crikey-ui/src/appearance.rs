//! Pure appearance preferences and a caller-owned, change-driven resolution cache.
//!
//! Serialized overrides are an atomic JSON object, not a second configuration file.
//! Interface sizes are logical layout choices; they never modify OS pixels-per-point.
use std::{fmt, str::FromStr};

use egui::Color32;
use serde::{
    de::{self, MapAccess, Visitor},
    Deserialize, Deserializer,
};

use crate::theme::{self, Metrics, Palette};

pub const MAX_THEME_OVERRIDE_BYTES: usize = 4096;
pub const MAX_THEME_OVERRIDE_ROLES: usize = 11;

macro_rules! preference {
    ($name:ident, $default:ident, {$($variant:ident => $value:literal),+ $(,)?}) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name { $($variant),+ }
        impl Default for $name { fn default() -> Self { Self::$default } }
        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $value),+ }
            }
        }
        impl FromStr for $name {
            type Err = AppearanceError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value { $($value => Ok(Self::$variant),)+ _ => Err(AppearanceError::InvalidPreference(stringify!($name))) }
            }
        }
    };
}
preference!(ThemePreference, System, { System => "system", Light => "light", Dark => "dark" });
preference!(InterfaceSize, Default, { Small => "small", Default => "default", Large => "large" });
preference!(ResultDensity, Default, { Compact => "compact", Default => "default", Comfortable => "comfortable" });

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SystemTheme {
    #[default]
    Unknown,
    Light,
    Dark,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedTheme {
    Light,
    Dark,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeStatus {
    Applied,
    /// System was requested, but the host has no actual system-theme observation.
    SystemFallback,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AppearanceError {
    #[error("Invalid {0} preference")]
    InvalidPreference(&'static str),
    #[error("Theme overrides exceed {MAX_THEME_OVERRIDE_BYTES} bytes")]
    OverridesTooLarge,
    #[error("Theme overrides must be a bounded object of unique known roles and #RRGGBB colors")]
    InvalidOverrides,
    #[error("Insufficient contrast between {foreground} and {background}")]
    Contrast {
        foreground: &'static str,
        background: &'static str,
    },
}

/// Fixed-size validated syntax. No strings or maps survive parsing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThemeOverrides([Option<[u8; 3]>; MAX_THEME_OVERRIDE_ROLES]);
const ROLES: [&str; MAX_THEME_OVERRIDE_ROLES] = [
    "canvas",
    "surface",
    "raised",
    "input",
    "border",
    "text",
    "text-muted",
    "accent",
    "accent-soft",
    "warning",
    "error",
];
impl ThemeOverrides {
    /// Checks syntax/bounds only; use `AppearancePreferences::validate` for contrast.
    pub fn parse(value: &str) -> Result<Self, AppearanceError> {
        if value.len() > MAX_THEME_OVERRIDE_BYTES {
            return Err(AppearanceError::OverridesTooLarge);
        }
        serde_json::from_str(value).map_err(|_| AppearanceError::InvalidOverrides)
    }
    fn apply(self, palette: &mut Palette) {
        let fields = [
            &mut palette.canvas,
            &mut palette.surface,
            &mut palette.raised,
            &mut palette.input,
            &mut palette.border,
            &mut palette.text,
            &mut palette.text_muted,
            &mut palette.accent,
            &mut palette.accent_soft,
            &mut palette.warning,
            &mut palette.error,
        ];
        for (field, color) in fields.into_iter().zip(self.0) {
            if let Some([r, g, b]) = color {
                *field = Color32::from_rgb(r, g, b);
            }
        }
    }
}
impl<'de> Deserialize<'de> for ThemeOverrides {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OverrideVisitor;
        impl<'de> Visitor<'de> for OverrideVisitor {
            type Value = ThemeOverrides;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object of unique known color roles")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut result = ThemeOverrides::default();
                while let Some(key) = map.next_key::<String>()? {
                    let index = ROLES
                        .iter()
                        .position(|role| *role == key)
                        .ok_or_else(|| de::Error::custom("unknown color role"))?;
                    if result.0[index].is_some() {
                        return Err(de::Error::custom("duplicate color role"));
                    }
                    let value = map.next_value::<String>()?;
                    let bytes = value.as_bytes();
                    if bytes.len() != 7 || bytes[0] != b'#' || !bytes[1..].iter().all(u8::is_ascii_hexdigit) {
                        return Err(de::Error::custom("expected #RRGGBB"));
                    }
                    let mut rgb = [0; 3];
                    for (channel, offset) in rgb.iter_mut().zip([1, 3, 5]) {
                        *channel =
                            u8::from_str_radix(&value[offset..offset + 2], 16).map_err(de::Error::custom)?;
                    }
                    result.0[index] = Some(rgb);
                }
                Ok(result)
            }
        }
        deserializer.deserialize_map(OverrideVisitor)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppearancePreferences {
    pub theme: ThemePreference,
    pub interface_size: InterfaceSize,
    pub result_density: ResultDensity,
    pub overrides: ThemeOverrides,
}
impl AppearancePreferences {
    /// System overrides must remain readable under either genuine OS observation.
    pub fn validate(self) -> Result<(), AppearanceError> {
        for resolved in [ResolvedTheme::Light, ResolvedTheme::Dark] {
            if matches!(
                (self.theme, resolved),
                (ThemePreference::Light, ResolvedTheme::Dark) | (ThemePreference::Dark, ResolvedTheme::Light)
            ) {
                continue;
            }
            self.palette(resolved)?;
        }
        Ok(())
    }
    fn palette(self, resolved: ResolvedTheme) -> Result<Palette, AppearanceError> {
        let mut palette = theme::base_palette(resolved);
        self.overrides.apply(&mut palette);
        validate_contrast(&palette)?;
        Ok(palette)
    }
}

/// Immutable resolved values. The native owner installs styling only on change.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedAppearance {
    pub theme: ResolvedTheme,
    pub status: ThemeStatus,
    pub(crate) palette: Palette,
}
impl Default for ResolvedAppearance {
    fn default() -> Self {
        Self {
            theme: ResolvedTheme::Dark,
            status: ThemeStatus::SystemFallback,
            palette: theme::base_palette(ResolvedTheme::Dark),
        }
    }
}
impl ResolvedAppearance {
    pub(crate) fn metrics(&self) -> &Metrics {
        &self.palette.metrics
    }
    pub fn page_palette(&self) -> crikey_core::PagePalette {
        let convert =
            |color: Color32| crikey_core::PageColor::rgba(color.r(), color.g(), color.b(), color.a());
        crikey_core::PagePalette {
            surface: convert(self.palette.surface),
            text: convert(self.palette.text),
            accent: convert(self.palette.accent),
            muted: convert(self.palette.text_muted),
        }
    }
}

/// Caller-owned last-good cache; invalid updates return an error without mutation.
#[derive(Debug, Clone)]
pub struct AppearanceCache {
    preferences: AppearancePreferences,
    system: SystemTheme,
    resolved: ResolvedAppearance,
}
impl AppearanceCache {
    pub fn new(preferences: AppearancePreferences, system: SystemTheme) -> Result<Self, AppearanceError> {
        preferences.validate()?;
        let resolved = resolve(preferences, system)?;
        Ok(Self {
            preferences,
            system,
            resolved,
        })
    }
    /// Returns whether applied palette, metrics, theme or fallback status changed.
    /// Desired preferences and actual system observations are retained even when
    /// they resolve to the same appearance. Invalid edits change neither.
    pub fn update(
        &mut self,
        preferences: AppearancePreferences,
        system: SystemTheme,
    ) -> Result<bool, AppearanceError> {
        let preferences_changed = self.preferences != preferences;
        if !preferences_changed && (self.system == system || preferences.theme != ThemePreference::System) {
            self.system = system;
            return Ok(false);
        }
        if preferences_changed {
            preferences.validate()?;
        }
        let resolved = resolve(preferences, system)?;
        let changed = self.resolved != resolved;
        self.preferences = preferences;
        self.system = system;
        self.resolved = resolved;
        Ok(changed)
    }
    pub fn resolved(&self) -> &ResolvedAppearance {
        &self.resolved
    }
    pub fn preferences(&self) -> &AppearancePreferences {
        &self.preferences
    }
    /// Latest actual host observation, including observations irrelevant to a
    /// currently explicit theme preference.
    pub fn system_theme(&self) -> SystemTheme {
        self.system
    }
}
impl Default for AppearanceCache {
    fn default() -> Self {
        Self {
            preferences: AppearancePreferences::default(),
            system: SystemTheme::Unknown,
            resolved: ResolvedAppearance::default(),
        }
    }
}
fn resolve(
    preferences: AppearancePreferences,
    system: SystemTheme,
) -> Result<ResolvedAppearance, AppearanceError> {
    let (theme, status) = match (preferences.theme, system) {
        (ThemePreference::Light, _) | (ThemePreference::System, SystemTheme::Light) => {
            (ResolvedTheme::Light, ThemeStatus::Applied)
        }
        (ThemePreference::Dark, _) | (ThemePreference::System, SystemTheme::Dark) => {
            (ResolvedTheme::Dark, ThemeStatus::Applied)
        }
        (ThemePreference::System, SystemTheme::Unknown) => (ResolvedTheme::Dark, ThemeStatus::SystemFallback),
    };
    let mut palette = preferences.palette(theme)?;
    palette.metrics = Metrics::new(preferences.interface_size, preferences.result_density);
    Ok(ResolvedAppearance {
        theme,
        status,
        palette,
    })
}
fn luminance(color: Color32) -> f64 {
    let [r, g, b, _] = color.to_array();
    let linear = |value: u8| {
        let value = f64::from(value) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}
fn validate_contrast(palette: &Palette) -> Result<(), AppearanceError> {
    let surfaces = [
        ("canvas", palette.canvas),
        ("surface", palette.surface),
        ("raised", palette.raised),
        ("input", palette.input),
        ("accent-soft", palette.accent_soft),
    ];
    for (background, color) in surfaces {
        for (foreground, text, minimum) in [
            ("text", palette.text, 4.5),
            ("text-muted", palette.text_muted, 4.5),
            ("accent", palette.accent, 4.5),
            ("warning", palette.warning, 4.5),
            ("error", palette.error, 4.5),
        ] {
            let a = luminance(text);
            let b = luminance(color);
            if (a.max(b) + 0.05) / (a.min(b) + 0.05) < minimum {
                return Err(AppearanceError::Contrast {
                    foreground,
                    background,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overrides_reject_duplicate_unknown_and_unbounded_input() {
        for value in [
            r##"{"text":"#ffffff","text":"#eeeeee"}"##,
            r##"{"unknown":"#ffffff"}"##,
            r##"{"text":"#ffffffff"}"##,
            "[]",
        ] {
            assert!(ThemeOverrides::parse(value).is_err());
        }
        assert_eq!(
            ThemeOverrides::parse(&" ".repeat(MAX_THEME_OVERRIDE_BYTES + 1)),
            Err(AppearanceError::OverridesTooLarge)
        );
        assert!(ThemeOverrides::parse(r##"{"text":"#AabbCC"}"##).is_ok());
    }
    #[test]
    fn invalid_contrast_retains_last_good_and_system_fallback_is_truthful() {
        let mut cache = AppearanceCache::default();
        AppearancePreferences::default().validate().unwrap();
        assert_eq!(cache.resolved().status, ThemeStatus::SystemFallback);
        let invalid = AppearancePreferences {
            overrides: ThemeOverrides::parse(r##"{"text":"#14161a"}"##).unwrap(),
            ..AppearancePreferences::default()
        };
        assert!(cache.update(invalid, SystemTheme::Light).is_err());
        assert_eq!(*cache.preferences(), AppearancePreferences::default());
        assert_eq!(cache.resolved().theme, ResolvedTheme::Dark);
        assert!(cache
            .update(AppearancePreferences::default(), SystemTheme::Light)
            .unwrap());
        assert_eq!(cache.resolved().status, ThemeStatus::Applied);
        assert!(!cache
            .update(AppearancePreferences::default(), SystemTheme::Light)
            .unwrap());
    }
    #[test]
    fn cache_reports_applied_changes_not_irrelevant_authority_changes() {
        let light = AppearancePreferences {
            theme: ThemePreference::Light,
            ..AppearancePreferences::default()
        };
        let mut cache = AppearanceCache::new(light, SystemTheme::Unknown).unwrap();
        let applied = *cache.resolved();
        assert!(!cache.update(light, SystemTheme::Dark).unwrap());
        assert_eq!(cache.system, SystemTheme::Dark);
        assert_eq!(*cache.resolved(), applied);
        assert!(!cache
            .update(AppearancePreferences::default(), SystemTheme::Light)
            .unwrap());
        assert_eq!(cache.preferences().theme, ThemePreference::System);
        assert_eq!(*cache.resolved(), applied);
        assert!(cache
            .update(AppearancePreferences::default(), SystemTheme::Unknown)
            .unwrap());
        assert_eq!(cache.resolved().status, ThemeStatus::SystemFallback);
        let dark = AppearancePreferences {
            theme: ThemePreference::Dark,
            ..AppearancePreferences::default()
        };
        // The palette stays dark, but removal of fallback is an applied change.
        assert!(cache.update(dark, SystemTheme::Unknown).unwrap());
        assert_eq!(cache.resolved().status, ThemeStatus::Applied);
        let redundant_override = AppearancePreferences {
            overrides: ThemeOverrides::parse(r##"{"canvas":"#14161a"}"##).unwrap(),
            ..dark
        };
        assert!(!cache.update(redundant_override, SystemTheme::Unknown).unwrap());
        assert_eq!(*cache.preferences(), redundant_override);
    }
    #[test]
    fn muted_text_and_focus_are_checked_on_selected_surfaces() {
        for (json, role) in [
            (r##"{"text-muted":"#888888"}"##, "text-muted"),
            (r##"{"accent":"#333333"}"##, "accent"),
        ] {
            let preferences = AppearancePreferences {
                theme: ThemePreference::Dark,
                overrides: ThemeOverrides::parse(json).unwrap(),
                ..AppearancePreferences::default()
            };
            assert!(matches!(preferences.validate(),
                Err(AppearanceError::Contrast { foreground, .. }) if foreground == role));
        }
    }
    #[test]
    fn light_accent_used_for_small_text_requires_readable_contrast() {
        let preferences = AppearancePreferences {
            theme: ThemePreference::Light,
            overrides: ThemeOverrides::parse(r##"{"accent":"#777777"}"##).unwrap(),
            ..AppearancePreferences::default()
        };
        assert!(matches!(
            preferences.validate(),
            Err(AppearanceError::Contrast {
                foreground: "accent",
                ..
            })
        ));
    }
    #[test]
    fn raw_system_theme_cannot_change_manually_resolved_style() {
        for (theme, opposite) in [
            (ThemePreference::Light, egui::Theme::Dark),
            (ThemePreference::Dark, egui::Theme::Light),
        ] {
            let preferences = AppearancePreferences {
                theme,
                interface_size: InterfaceSize::Large,
                ..AppearancePreferences::default()
            };
            let cache = AppearanceCache::new(preferences, SystemTheme::Unknown).unwrap();
            let context = egui::Context::default();
            theme::install(&context, cache.resolved());
            let expected_theme = match theme {
                ThemePreference::Light => egui::Theme::Light,
                _ => egui::Theme::Dark,
            };
            for system_theme in [Some(opposite), None, Some(opposite)] {
                context.begin_pass(egui::RawInput {
                    system_theme,
                    ..egui::RawInput::default()
                });
                assert_eq!(context.theme(), expected_theme);
                let style = context.style();
                assert_eq!(style.visuals.panel_fill, cache.resolved().palette.canvas);
                assert_eq!(
                    style.visuals.override_text_color,
                    Some(cache.resolved().palette.text)
                );
                assert_eq!(
                    style.spacing.interact_size.y,
                    cache.resolved().metrics().control_height
                );
                assert_eq!(
                    style.text_styles[&egui::TextStyle::Body].size,
                    cache.resolved().metrics().text_body
                );
                let _ = context.end_pass();
            }
        }
    }
    #[test]
    fn metrics_keep_default_and_density_never_shrinks_text() {
        let defaults = Metrics::new(InterfaceSize::Default, ResultDensity::Default);
        assert_eq!(defaults.row_height, 52.0);
        assert_eq!(defaults.compact_window_height, 94.0);
        for size in [InterfaceSize::Small, InterfaceSize::Default, InterfaceSize::Large] {
            let compact = Metrics::new(size, ResultDensity::Compact);
            let comfortable = Metrics::new(size, ResultDensity::Comfortable);
            assert_eq!(compact.text_label, comfortable.text_label);
            assert!(compact.row_height < comfortable.row_height);
            assert!((compact.row_height - compact.row_pad_y * 2.0 - compact.scaled(44.0)).abs() < 0.001);
            assert!((compact.radius_window - compact.radius_medium - compact.panel_margin).abs() < 0.001);
        }
    }
}
