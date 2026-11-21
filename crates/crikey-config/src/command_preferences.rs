//! Versioned command preferences in one layered configuration value, separate from textual aliases.
use crikey_core::{
    command::{CommandRef, MAX_COMMANDS},
    PluginId,
};
use std::collections::BTreeSet;

pub const KEY_COMMAND_PREFERENCES: &str = "commands.preferences";
pub const MAX_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPreference {
    pub reference: CommandRef,
    pub hidden: bool,
    pub alias: Option<String>,
    pub hotkey: Option<String>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandPreferences {
    pub records: Vec<CommandPreference>,
    pub favorites: Vec<CommandRef>,
    pub fallbacks: Vec<CommandRef>,
}
fn reference_value(reference: &CommandRef) -> toml::Value {
    toml::Value::Array(vec![
        toml::Value::String(reference.provider_id.0.clone()),
        toml::Value::String(reference.command_id.clone()),
    ])
}
fn parse_reference(value: &toml::Value) -> Result<CommandRef, String> {
    let array = value.as_array().ok_or("invalid command reference")?;
    if array.len() != 2 {
        return Err("invalid command reference".into());
    }
    let reference = CommandRef {
        provider_id: PluginId(array[0].as_str().ok_or("invalid owner")?.into()),
        command_id: array[1].as_str().ok_or("invalid command ID")?.into(),
    };
    reference.validate().map_err(str::to_owned)?;
    Ok(reference)
}
impl CommandPreferences {
    pub fn validate(&self) -> Result<(), String> {
        if self.records.len() + self.favorites.len() + self.fallbacks.len() > MAX_COMMANDS {
            return Err("too many command preferences".into());
        }
        let mut references = BTreeSet::new();
        let mut aliases = BTreeSet::new();
        let mut hotkeys = BTreeSet::new();
        for record in &self.records {
            let reference = &record.reference;
            reference.validate().map_err(str::to_owned)?;
            if !references.insert(reference) {
                return Err("duplicate command preference".into());
            }
            if let Some(alias) = &record.alias {
                if alias.is_empty()
                    || alias.len() > 256
                    || alias.chars().any(char::is_whitespace)
                    || alias.chars().any(char::is_control)
                    || !aliases.insert(alias.to_lowercase())
                {
                    return Err("invalid or duplicate command alias".into());
                }
            }
            if let Some(hotkey) = &record.hotkey {
                if hotkey.len() > 256 {
                    return Err("command hotkey exceeds byte limit".into());
                }
                let parsed = crikey_platform::Accelerator::parse(hotkey)
                    .map_err(|error| format!("invalid command hotkey: {error}"))?;
                if !hotkeys.insert(parsed.canonical()) {
                    return Err("duplicate command hotkey".into());
                }
            }
        }
        for list in [&self.favorites, &self.fallbacks] {
            let mut seen = BTreeSet::new();
            for reference in list {
                reference.validate().map_err(str::to_owned)?;
                if !seen.insert(reference) {
                    return Err("duplicate ordered command".into());
                }
            }
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<String, String> {
        self.validate()?;
        let mut root = toml::Table::new();
        root.insert("version".into(), toml::Value::Integer(1));
        root.insert(
            "favorites".into(),
            toml::Value::Array(self.favorites.iter().map(reference_value).collect()),
        );
        root.insert(
            "fallbacks".into(),
            toml::Value::Array(self.fallbacks.iter().map(reference_value).collect()),
        );
        let mut records = Vec::with_capacity(self.records.len());
        for record in &self.records {
            let mut table = toml::Table::new();
            table.insert("reference".into(), reference_value(&record.reference));
            table.insert("hidden".into(), toml::Value::Boolean(record.hidden));
            if let Some(alias) = &record.alias {
                table.insert("alias".into(), toml::Value::String(alias.clone()));
            }
            if let Some(hotkey) = &record.hotkey {
                table.insert("hotkey".into(), toml::Value::String(hotkey.clone()));
            }
            records.push(toml::Value::Table(table));
        }
        root.insert("records".into(), toml::Value::Array(records));
        let text = toml::to_string(&root).map_err(|error| error.to_string())?;
        if text.len() > MAX_DOCUMENT_BYTES {
            return Err("command preferences exceed byte limit".into());
        }
        Ok(text)
    }
    pub fn decode(text: &str) -> Result<Self, String> {
        if text.len() > MAX_DOCUMENT_BYTES {
            return Err("command preferences exceed byte limit".into());
        }
        let root: toml::Table =
            toml::from_str(text).map_err(|_| "invalid command preference document".to_owned())?;
        if root.get("version").and_then(toml::Value::as_integer) != Some(1)
            || root
                .keys()
                .any(|key| !["version", "records", "favorites", "fallbacks"].contains(&key.as_str()))
        {
            return Err("unsupported command preference schema".into());
        }
        let list = |key: &str| -> Result<Vec<CommandRef>, String> {
            root.get(key)
                .and_then(toml::Value::as_array)
                .ok_or_else(|| format!("missing {key}"))?
                .iter()
                .map(parse_reference)
                .collect()
        };
        let rows = root
            .get("records")
            .and_then(toml::Value::as_array)
            .ok_or("missing records")?;
        if rows.len() > MAX_COMMANDS {
            return Err("too many records".into());
        }
        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let table = row.as_table().ok_or("invalid record")?;
            if table
                .keys()
                .any(|key| !["reference", "hidden", "alias", "hotkey"].contains(&key.as_str()))
            {
                return Err("unknown command preference field".into());
            }
            let optional = |key: &str| -> Result<Option<String>, String> {
                table
                    .get(key)
                    .map(|value| {
                        value
                            .as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| format!("invalid {key}"))
                    })
                    .transpose()
            };
            records.push(CommandPreference {
                reference: parse_reference(table.get("reference").ok_or("missing reference")?)?,
                hidden: table
                    .get("hidden")
                    .and_then(toml::Value::as_bool)
                    .ok_or("invalid hidden flag")?,
                alias: optional("alias")?,
                hotkey: optional("hotkey")?,
            });
        }
        let preferences = Self {
            records,
            favorites: list("favorites")?,
            fallbacks: list("fallbacks")?,
        };
        preferences.validate()?;
        Ok(preferences)
    }
}

impl crate::ConfigStore {
    /// Checks only assignments winning in this resolved candidate context.
    pub fn validate_command_hotkey_conflicts(&self) -> Result<(), String> {
        let activation = self
            .get(crate::KEY_ACTIVATION_HOTKEY)
            .ok_or("launcher activation is unavailable")?;
        let activation =
            crikey_platform::Accelerator::parse(activation).map_err(|error| error.to_string())?;
        let preferences = self
            .get(KEY_COMMAND_PREFERENCES)
            .map(CommandPreferences::decode)
            .transpose()?
            .unwrap_or_default();
        for record in preferences.records {
            if let Some(hotkey) = record.hotkey {
                let command =
                    crikey_platform::Accelerator::parse(&hotkey).map_err(|error| error.to_string())?;
                if activation.canonical() == command.canonical() {
                    return Err(
                        "This shortcut is assigned to launcher activation and a command; unassign it first"
                            .into(),
                    );
                }
            }
        }
        Ok(())
    }
    /// Reads exactly one target layer, not the effective session-overridden document.
    pub fn command_preferences_at(
        &self,
        layer: crate::ConfigLayer,
    ) -> Result<Option<CommandPreferences>, String> {
        self.layers[layer.index()]
            .get(KEY_COMMAND_PREFERENCES)
            .map(|text| CommandPreferences::decode(text))
            .transpose()
    }
    /// Commits using this store's exact loaded-source revision. Blocking: worker only.
    pub fn commit_command_preferences(
        &mut self,
        preferences: &CommandPreferences,
        session: bool,
    ) -> Result<(), String> {
        self.commit_command_preferences_if_revision(preferences, session, &self.revision())
            .map_err(|error| error.to_string())
    }

    /// Caller-owned drafts retain their revision; conflicts never replace this
    /// published store or silently rebase the draft onto external changes.
    pub fn commit_command_preferences_if_revision(
        &mut self,
        preferences: &CommandPreferences,
        session: bool,
        expected: &crate::ConfigRevision,
    ) -> Result<(), crate::ConfigError> {
        if &self.revision() != expected {
            return Err(crate::ConfigError::Conflict);
        }
        let text = preferences.encode().map_err(|_| crate::ConfigError::Setting {
            key: KEY_COMMAND_PREFERENCES.to_owned(),
            reason: "invalid command preferences",
        })?;
        if session {
            // Session publication writes no files, but must still reject a
            // draft based on externally changed (including shadowed) sources.
            expected.check()?;
            self.set_session_override(KEY_COMMAND_PREFERENCES, &text);
            Ok(())
        } else {
            self.commit_user_global(KEY_COMMAND_PREFERENCES, &text, expected)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reference(id: &str) -> CommandRef {
        CommandRef {
            provider_id: PluginId("test.owner".into()),
            command_id: id.into(),
        }
    }
    #[test]
    fn schema_roundtrip_retains_order_and_rejects_duplicate_aliases() {
        let mut preferences = CommandPreferences {
            records: vec![CommandPreference {
                reference: reference("one"),
                hidden: true,
                alias: Some("go".into()),
                hotkey: Some("Ctrl+Alt+G".into()),
            }],
            favorites: vec![reference("two"), reference("one")],
            fallbacks: vec![reference("two")],
        };
        assert_eq!(
            CommandPreferences::decode(&preferences.encode().unwrap()).unwrap(),
            preferences
        );
        preferences.records.push(CommandPreference {
            reference: reference("two"),
            hidden: false,
            alias: Some("GO".into()),
            hotkey: None,
        });
        assert!(preferences.encode().is_err());
        assert!(CommandPreferences::decode("version = 2").is_err());
    }
}
