//! P7 settings discovery over shared declarations, never effective values.
//!
//! The integration owner supplies the migrated launcher metadata. Plugin rows
//! borrow the same `ConfigurationField`s registered with `ConfigStore`; this
//! module owns neither a settings registry nor validation/persistence rules.

use crikey_config::DiscoveredSchema;
use crikey_plugin_model::ConfigurationField;

const MAX_SETTINGS: usize = 10_000;
const MAX_METADATA_BYTES: usize = 1024 * 1024;
const MAX_QUERY_BYTES: usize = 4096;
const MAX_OWNED_HITS: usize = 128;
const MAX_OWNED_BYTES: usize = 64 * 1024;

/// View metadata from the single shared launcher settings implementation.
/// Secret declarations are omitted altogether, not merely value-redacted.
#[derive(Clone, Copy)]
pub struct SettingDefinition<'a> {
    pub key: &'a str,
    pub label: &'a str,
    pub description: &'a str,
    pub category: &'a str,
    pub navigation_target: &'a str,
    pub control: &'a str,
    pub requires_restart: bool,
    pub secret: bool,
    pub platforms: &'a [String],
}

impl std::fmt::Debug for SettingDefinition<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.secret || self.control.eq_ignore_ascii_case("password") {
            return f.write_str("SettingDefinition([redacted])");
        }
        f.debug_struct("SettingDefinition")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSearchError {
    MetadataLimit,
    QueryLimit,
    StaleSchema { current: u64 },
}

/// No value/default/allowed-value data is retained or rendered by this index.
pub struct SettingMatch<'a> {
    pub key: String,
    pub label: &'a str,
    pub description: &'a str,
    pub category: &'a str,
    pub navigation_target: &'a str,
    pub control: &'a str,
    pub requires_restart: bool,
    /// Use this exact declaration for plugin validation, rather than another
    /// table of validation rules. It is absent for shared launcher definitions.
    pub plugin_field: Option<&'a ConfigurationField>,
}

// Never format ConfigurationField: its Debug includes defaults and allowed values.
impl std::fmt::Debug for SettingMatch<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingMatch")
            .field("key", &self.key)
            .field("plugin_declaration", &self.plugin_field.is_some())
            .finish_non_exhaustive()
    }
}

/// Safe to send out of the configuration owner's schema-borrowing scope.
/// Re-resolve key/navigation_target against the current schema on edit admission.
/// No ConfigurationField, default, allowed value or effective value crosses here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedSettingMatch {
    pub key: String,
    pub label: String,
    pub description: String,
    pub category: String,
    pub navigation_target: String,
    pub control: String,
    pub requires_restart: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsSearchResults {
    pub schema_revision: u64,
    pub hits: Vec<OwnedSettingMatch>,
    /// More matching metadata exists than the bounded response can carry.
    pub truncated: bool,
}

struct Entry<'a> {
    row: SettingMatch<'a>,
    searchable: String,
}

pub struct SettingsSearch<'a> {
    revision: u64,
    entries: Vec<Entry<'a>>,
}

impl std::fmt::Debug for SettingsSearch<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsSearch")
            .field("revision", &self.revision)
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl<'a> SettingsSearch<'a> {
    /// Rebuild off-thread when shared schema metadata changes. No configuration
    /// values are accepted, so passwords cannot leak through value indexing.
    pub fn new(
        revision: u64,
        launcher: &[SettingDefinition<'a>],
        plugins: &'a [DiscoveredSchema],
        platform: &str,
    ) -> Result<Self, SettingsSearchError> {
        // Plan every owned key/index string and every retained vector slot
        // before allocating. One exact reservation avoids geometric growth and
        // copying while admitting entries near the aggregate index boundary.
        let mut count = 0usize;
        let mut planned = std::mem::size_of::<Self>();
        let mut plan = |key: &[&str], label: &str, description: &str, category: &str| {
            let key_bytes = key
                .iter()
                .try_fold(0usize, |sum, part| sum.checked_add(part.len()))
                .ok_or(SettingsSearchError::MetadataLimit)?;
            let searchable = lowercase_bytes(key.iter().copied().chain([label, description, category]))
                .ok_or(SettingsSearchError::MetadataLimit)?;
            count = count.checked_add(1).ok_or(SettingsSearchError::MetadataLimit)?;
            planned = planned
                .checked_add(std::mem::size_of::<Entry<'a>>())
                .and_then(|bytes| bytes.checked_add(key_bytes))
                .and_then(|bytes| bytes.checked_add(searchable))
                .ok_or(SettingsSearchError::MetadataLimit)?;
            if count > MAX_SETTINGS || planned > MAX_METADATA_BYTES {
                return Err(SettingsSearchError::MetadataLimit);
            }
            Ok(())
        };
        for definition in launcher {
            if !definition.secret
                && !definition.control.eq_ignore_ascii_case("password")
                && applies(definition.platforms, platform)
            {
                plan(
                    &[definition.key],
                    definition.label,
                    definition.description,
                    definition.category,
                )?;
            }
        }
        for schema in plugins {
            for field in &schema.section.fields {
                if !field.secret && applies(&field.platforms, platform) {
                    plan(
                        &["plugins.", &schema.plugin.0, ".settings.", &field.name],
                        &field.name,
                        &field.description,
                        "Plugins",
                    )?;
                }
            }
        }
        let mut result = Self {
            revision,
            entries: Vec::with_capacity(count),
        };
        let mut bytes =
            std::mem::size_of::<Self>() + result.entries.capacity() * std::mem::size_of::<Entry<'a>>();
        let capacity_spare = result.entries.capacity() - count;
        if capacity_spare * std::mem::size_of::<Entry<'a>>() > MAX_METADATA_BYTES - planned {
            return Err(SettingsSearchError::MetadataLimit);
        }
        for definition in launcher {
            if definition.secret
                || definition.control.eq_ignore_ascii_case("password")
                || !applies(definition.platforms, platform)
            {
                continue;
            }
            if definition.key.len() > MAX_METADATA_BYTES {
                return Err(SettingsSearchError::MetadataLimit);
            }
            result.push(
                SettingMatch {
                    key: definition.key.to_owned(),
                    label: definition.label,
                    description: definition.description,
                    category: definition.category,
                    navigation_target: definition.navigation_target,
                    control: definition.control,
                    requires_restart: definition.requires_restart,
                    plugin_field: None,
                },
                &mut bytes,
            )?;
        }
        for schema in plugins {
            for field in &schema.section.fields {
                if field.secret || !applies(&field.platforms, platform) {
                    continue;
                }
                if schema
                    .plugin
                    .0
                    .len()
                    .saturating_add(field.name.len())
                    .saturating_add(18)
                    > MAX_METADATA_BYTES
                {
                    return Err(SettingsSearchError::MetadataLimit);
                }
                let mut key = String::with_capacity(schema.plugin.0.len() + field.name.len() + 18);
                key.push_str("plugins.");
                key.push_str(&schema.plugin.0);
                key.push_str(".settings.");
                key.push_str(&field.name);
                result.push(
                    SettingMatch {
                        key,
                        label: &field.name,
                        description: &field.description,
                        category: "Plugins",
                        navigation_target: &schema.plugin.0,
                        control: field.kind.as_str(),
                        requires_restart: field.requires_restart,
                        plugin_field: Some(field),
                    },
                    &mut bytes,
                )?;
            }
        }
        Ok(result)
    }

    fn push(&mut self, row: SettingMatch<'a>, bytes: &mut usize) -> Result<(), SettingsSearchError> {
        let needed =
            lowercase_bytes([row.key.as_str(), row.label, row.description, row.category].into_iter())
                .ok_or(SettingsSearchError::MetadataLimit)?;
        let owned = row
            .key
            .capacity()
            .checked_add(needed)
            .ok_or(SettingsSearchError::MetadataLimit)?;
        if self.entries.len() >= self.entries.capacity() || owned > MAX_METADATA_BYTES.saturating_sub(*bytes)
        {
            return Err(SettingsSearchError::MetadataLimit);
        }
        let mut searchable = String::with_capacity(needed);
        let retained = row
            .key
            .capacity()
            .checked_add(searchable.capacity())
            .ok_or(SettingsSearchError::MetadataLimit)?;
        if retained > MAX_METADATA_BYTES.saturating_sub(*bytes) {
            return Err(SettingsSearchError::MetadataLimit);
        }
        for text in [&*row.key, row.label, row.description, row.category] {
            for character in text
                .chars()
                .flat_map(char::to_lowercase)
                .chain(std::iter::once(' '))
            {
                searchable.push(character);
            }
        }
        *bytes += retained;
        self.entries.push(Entry { row, searchable });
        Ok(())
    }

    /// Returned rows point back to the declarations, not a second effective
    /// value store. Resolve winning layers through ConfigStore when displaying.
    pub fn search(
        &self,
        text: &str,
        schema_revision: u64,
    ) -> Result<Vec<&SettingMatch<'a>>, SettingsSearchError> {
        let query = self.checked_query(text, schema_revision)?;
        Ok(self.matching(query).collect())
    }

    fn checked_query(&self, text: &str, schema_revision: u64) -> Result<String, SettingsSearchError> {
        if schema_revision != self.revision {
            return Err(SettingsSearchError::StaleSchema {
                current: self.revision,
            });
        }
        if text.len() > MAX_QUERY_BYTES {
            return Err(SettingsSearchError::QueryLimit);
        }
        Ok(text.to_lowercase())
    }

    fn matching<'s>(&'s self, query: String) -> impl Iterator<Item = &'s SettingMatch<'a>> + 's {
        self.entries
            .iter()
            .filter(move |entry| {
                query
                    .split_whitespace()
                    .all(|word| entry.searchable.contains(word))
            })
            .map(|entry| &entry.row)
    }

    /// Build locally using the exact schema collection registered with ConfigStore,
    /// then return this owned, bounded projection before that collection is replaced.
    /// Request/view identity remains the serialized owner's responsibility.
    pub fn search_owned(
        &self,
        text: &str,
        schema_revision: u64,
    ) -> Result<SettingsSearchResults, SettingsSearchError> {
        let query = self.checked_query(text, schema_revision)?;
        // Plan on the stack before allocating/copying response metadata. Charge
        // both string payloads and every retained Vec slot, including capacity.
        let mut selected = [None; MAX_OWNED_HITS];
        let mut count = 0;
        let mut planned = std::mem::size_of::<SettingsSearchResults>();
        let mut truncated = false;
        for row in self.matching(query) {
            let payload = row.key.len()
                + row.label.len()
                + row.description.len()
                + row.category.len()
                + row.navigation_target.len()
                + row.control.len();
            let size = payload + std::mem::size_of::<OwnedSettingMatch>();
            if count == MAX_OWNED_HITS || size > MAX_OWNED_BYTES - planned {
                truncated = true;
                break;
            }
            selected[count] = Some(row);
            count += 1;
            planned += size;
        }
        let mut response = SettingsSearchResults {
            schema_revision: self.revision,
            hits: Vec::with_capacity(count),
            truncated,
        };
        let mut bytes = std::mem::size_of::<SettingsSearchResults>()
            + response.hits.capacity() * std::mem::size_of::<OwnedSettingMatch>();
        // Account actual capacities rather than relying on allocator rounding.
        if bytes > MAX_OWNED_BYTES {
            response.hits = Vec::new();
            response.truncated = true;
            return Ok(response);
        }
        for row in selected[..count].iter().flatten() {
            let payload = row.key.len()
                + row.label.len()
                + row.description.len()
                + row.category.len()
                + row.navigation_target.len()
                + row.control.len();
            if payload > MAX_OWNED_BYTES - bytes {
                response.truncated = true;
                break;
            }
            let hit = OwnedSettingMatch {
                key: row.key.clone(),
                label: row.label.to_owned(),
                description: row.description.to_owned(),
                category: row.category.to_owned(),
                navigation_target: row.navigation_target.to_owned(),
                control: row.control.to_owned(),
                requires_restart: row.requires_restart,
            };
            let retained = hit.key.capacity()
                + hit.label.capacity()
                + hit.description.capacity()
                + hit.category.capacity()
                + hit.navigation_target.capacity()
                + hit.control.capacity();
            if retained > MAX_OWNED_BYTES - bytes {
                response.truncated = true;
                break;
            }
            bytes += retained;
            response.hits.push(hit);
        }
        Ok(response)
    }
}

fn lowercase_bytes<'a>(texts: impl Iterator<Item = &'a str>) -> Option<usize> {
    // Four indexed metadata components, each followed by one ASCII space.
    texts
        .flat_map(str::chars)
        .flat_map(char::to_lowercase)
        .try_fold(4usize, |bytes, character| bytes.checked_add(character.len_utf8()))
}

fn applies(platforms: &[String], platform: &str) -> bool {
    platforms.is_empty() || platforms.iter().any(|supported| supported == platform)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crikey_core::PluginId;
    use crikey_plugin_model::{ConfigurationKind, ConfigurationSection};

    fn field(name: &str, secret: bool) -> ConfigurationField {
        ConfigurationField {
            name: name.into(),
            kind: ConfigurationKind::String,
            default: None,
            description: if secret {
                "NEVER_INDEX_SECRET_METADATA".into()
            } else {
                "Visible setting description".into()
            },
            secret,
            requires_restart: true,
            platforms: Vec::new(),
            required: false,
            minimum: None,
            maximum: None,
            max_length: None,
            allowed: Vec::new(),
        }
    }

    #[test]
    fn secrets_are_absent_and_plugin_validation_uses_the_original_declaration() {
        let schemas = vec![DiscoveredSchema {
            plugin: PluginId("modern.fixture".into()),
            package: "fixture".into(),
            section: ConfigurationSection {
                fields: vec![field("token", true), field("visible", false)],
            },
        }];
        let search = SettingsSearch::new(7, &[], &schemas, "linux").expect("metadata");
        assert!(search
            .search("NEVER_INDEX_SECRET_METADATA", 7)
            .expect("query")
            .is_empty());
        assert!(search.search("token", 7).expect("query").is_empty());
        let results = search.search("visible description", 7).expect("query");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "plugins.modern.fixture.settings.visible");
        assert!(std::ptr::eq(
            results[0].plugin_field.expect("same schema"),
            &schemas[0].section.fields[1]
        ));
        assert_eq!(
            search.search("", 8).err(),
            Some(SettingsSearchError::StaleSchema { current: 7 })
        );
    }

    #[test]
    fn launcher_metadata_is_supplied_not_duplicated_and_is_platform_filtered() {
        let platforms = vec!["windows".into()];
        let definitions = [SettingDefinition {
            key: "launcher.shared",
            label: "Shared setting",
            description: "one metadata source",
            category: "Launcher",
            navigation_target: "launcher.shared",
            control: "boolean",
            requires_restart: false,
            secret: false,
            platforms: &platforms,
        }];
        assert!(SettingsSearch::new(1, &definitions, &[], "linux")
            .expect("metadata")
            .search("", 1)
            .expect("query")
            .is_empty());
        let search = SettingsSearch::new(1, &definitions, &[], "windows").expect("metadata");
        assert_eq!(search.search("metadata", 1).expect("query").len(), 1);
        assert_eq!(
            search.search(&"x".repeat(MAX_QUERY_BYTES + 1), 1).err(),
            Some(SettingsSearchError::QueryLimit)
        );
    }

    #[test]
    fn index_capacity_boundary_accounts_for_slots_keys_and_unicode_expansion() {
        let definition = SettingDefinition {
            key: "k",
            label: "",
            description: "İ",
            category: "",
            navigation_target: "target",
            control: "text",
            requires_restart: false,
            secret: false,
            platforms: &[],
        };
        // Lowercasing U+0130 expands from two bytes to three; four separators
        // and the one-byte key make the exact searchable allocation eight.
        let per_entry = std::mem::size_of::<Entry<'_>>() + 1 + 8;
        let count = (MAX_METADATA_BYTES - std::mem::size_of::<SettingsSearch<'_>>()) / per_entry;
        assert!(count < MAX_SETTINGS);
        let mut definitions = vec![definition; count];
        let index = SettingsSearch::new(1, &definitions, &[], "linux").unwrap();
        let retained = std::mem::size_of::<SettingsSearch<'_>>()
            + index.entries.capacity() * std::mem::size_of::<Entry<'_>>()
            + index
                .entries
                .iter()
                .map(|entry| entry.row.key.capacity() + entry.searchable.capacity())
                .sum::<usize>();
        assert!(retained <= MAX_METADATA_BYTES);
        assert!(retained + per_entry > MAX_METADATA_BYTES);
        assert_eq!(index.entries.len(), count);
        assert_eq!(index.search("i\u{307}", 1).unwrap().len(), count);
        definitions.push(definition);
        assert!(matches!(
            SettingsSearch::new(1, &definitions, &[], "linux"),
            Err(SettingsSearchError::MetadataLimit)
        ));
    }
}
