//! Between a TOML document and a flat `key -> text` map.
//!
//! Every layer collapses to dotted keys of text. That is deliberate: precedence
//! (spec 21.2) is defined per key, and a tree-merge would have to invent rules
//! for what it means for a higher layer to supply a table where a lower one
//! supplied a scalar. Flattening first makes precedence a single lookup with no
//! such rules to get wrong, and it is why [`crate::ConfigStore::layer_of`] can
//! answer for any key at all.
//!
//! Text rather than a typed value because a value's type is declared by the
//! plugin's schema (spec 21.3), not by however the user happened to spell it in
//! a file: `port = 8080` and `port = "8080"` mean the same setting, and the
//! schema is what decides whether it is an integer.

use std::collections::BTreeMap;
use std::path::Path;

use crate::ConfigError;

/// Flattens `table` into `out` under `prefix`, joining nested keys with dots.
///
/// `prefix` is empty for a whole-document read and non-empty for a per-plugin
/// file, whose contents are relative to `plugins.<plugin-id>`.
///
/// An array has no single textual spelling, so it is refused by name instead of
/// being silently skipped: a user who wrote one expects it to take effect, and
/// quietly ignoring it would leave them editing a file that does nothing.
pub(crate) fn flatten(
    table: &toml::Table,
    prefix: &str,
    path: &Path,
    out: &mut BTreeMap<String, String>,
) -> Result<(), ConfigError> {
    for (name, value) in table {
        let key = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        match value {
            toml::Value::Table(nested) => flatten(nested, &key, path, out)?,
            other => match render_scalar(other) {
                Some(text) => {
                    out.insert(key, text);
                }
                None => {
                    return Err(ConfigError::UnsupportedValue {
                        path: path.to_path_buf(),
                        key,
                    })
                }
            },
        }
    }
    Ok(())
}

/// The text a TOML scalar denotes, or `None` for a value that is not a scalar.
pub(crate) fn render_scalar(value: &toml::Value) -> Option<String> {
    match value {
        toml::Value::String(text) => Some(text.clone()),
        toml::Value::Integer(number) => Some(number.to_string()),
        toml::Value::Float(number) => Some(number.to_string()),
        toml::Value::Boolean(flag) => Some(flag.to_string()),
        toml::Value::Datetime(stamp) => Some(stamp.to_string()),
        toml::Value::Array(_) | toml::Value::Table(_) => None,
    }
}

/// Rebuilds a nested TOML document from dotted keys, for writing.
///
/// Refuses a set of keys where one is a strict prefix of another (`a.b` and
/// `a.b.c`): TOML cannot hold both, and picking a winner would silently discard
/// a setting the caller asked to persist.
pub(crate) fn nest(flat: &BTreeMap<String, String>, path: &Path) -> Result<toml::Table, ConfigError> {
    let mut root = toml::Table::new();
    for (key, value) in flat {
        let mut segments = key.split('.').peekable();
        let mut table = &mut root;
        while let Some(segment) = segments.next() {
            if segments.peek().is_none() {
                if table.get(segment).is_some_and(toml::Value::is_table) {
                    return Err(ConfigError::KeyConflict {
                        path: path.to_path_buf(),
                        key: key.clone(),
                    });
                }
                table.insert(segment.to_owned(), typed(value));
                break;
            }
            let entry = table
                .entry(segment.to_owned())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            let toml::Value::Table(nested) = entry else {
                return Err(ConfigError::KeyConflict {
                    path: path.to_path_buf(),
                    key: key.clone(),
                });
            };
            table = nested;
        }
    }
    Ok(root)
}

/// Chooses how to spell `text` in a written document.
///
/// An integer or boolean is written unquoted so a hand-edited file stays
/// idiomatic, but ONLY when the typed spelling renders back to exactly the same
/// text. `"007"` therefore stays a string: writing `7` would change the value the
/// next read produces, which is a silent mutation of the user's data.
fn typed(text: &str) -> toml::Value {
    if let Ok(number) = text.parse::<i64>() {
        if number.to_string() == text {
            return toml::Value::Integer(number);
        }
    }
    if text == "true" {
        return toml::Value::Boolean(true);
    }
    if text == "false" {
        return toml::Value::Boolean(false);
    }
    toml::Value::String(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> toml::Table {
        text.parse().expect("the fixture is valid TOML")
    }

    #[test]
    fn nested_tables_flatten_to_dotted_keys() {
        let mut out = BTreeMap::new();
        flatten(
            &parse("[launcher]\nprofile = \"work\"\n\n[plugins.modern.example.settings]\ntheme = \"dark\"\n"),
            "",
            Path::new("config.toml"),
            &mut out,
        )
        .expect("scalars flatten");
        assert_eq!(out.get("launcher.profile").map(String::as_str), Some("work"));
        assert_eq!(
            out.get("plugins.modern.example.settings.theme")
                .map(String::as_str),
            Some("dark")
        );
    }

    #[test]
    fn a_prefix_scopes_every_key_from_a_per_plugin_file() {
        let mut out = BTreeMap::new();
        flatten(
            &parse("[settings]\ntheme = \"light\"\n"),
            "plugins.modern.example",
            Path::new("modern.example.toml"),
            &mut out,
        )
        .expect("scalars flatten");
        assert_eq!(
            out.keys().collect::<Vec<_>>(),
            ["plugins.modern.example.settings.theme"]
        );
    }

    #[test]
    fn every_toml_scalar_type_flattens_to_its_text() {
        let mut out = BTreeMap::new();
        flatten(
            &parse("a = 1\nb = true\nc = 1.5\nd = \"text\"\n"),
            "",
            Path::new("config.toml"),
            &mut out,
        )
        .expect("scalars flatten");
        assert_eq!(out.get("a").map(String::as_str), Some("1"));
        assert_eq!(out.get("b").map(String::as_str), Some("true"));
        assert_eq!(out.get("c").map(String::as_str), Some("1.5"));
        assert_eq!(out.get("d").map(String::as_str), Some("text"));
    }

    #[test]
    fn an_array_value_is_refused_by_key_rather_than_ignored() {
        let mut out = BTreeMap::new();
        let error = flatten(
            &parse("[launcher]\nroots = [\"a\", \"b\"]\n"),
            "",
            Path::new("config.toml"),
            &mut out,
        )
        .expect_err("an array has no textual spelling");
        let ConfigError::UnsupportedValue { key, .. } = error else {
            panic!("expected an unsupported-value error, got {error}");
        };
        assert_eq!(key, "launcher.roots");
    }

    #[test]
    fn nesting_and_flattening_round_trip_a_flat_map() {
        let flat = BTreeMap::from([
            ("launcher.profile".to_owned(), "work".to_owned()),
            ("plugins.modern.x.enabled".to_owned(), "false".to_owned()),
            ("plugins.modern.x.settings.limit".to_owned(), "20".to_owned()),
            ("plugins.modern.x.settings.pin".to_owned(), "007".to_owned()),
        ]);
        let document = nest(&flat, Path::new("config.toml")).expect("no key conflicts");
        let mut back = BTreeMap::new();
        flatten(&document, "", Path::new("config.toml"), &mut back).expect("scalars flatten");
        assert_eq!(back, flat, "a save/load cycle must not alter any value");
    }

    #[test]
    fn a_key_that_is_a_prefix_of_another_cannot_be_written() {
        let flat = BTreeMap::from([
            ("launcher.profile".to_owned(), "work".to_owned()),
            ("launcher.profile.name".to_owned(), "work".to_owned()),
        ]);
        let error = nest(&flat, Path::new("config.toml"))
            .expect_err("TOML cannot hold a scalar and a table under one name");
        assert!(matches!(error, ConfigError::KeyConflict { .. }), "{error}");
    }

    #[test]
    fn an_integer_valued_setting_is_written_unquoted_and_reads_back_identically() {
        let flat = BTreeMap::from([("launcher.max-results".to_owned(), "50".to_owned())]);
        let document = nest(&flat, Path::new("config.toml")).expect("no key conflicts");
        let rendered = toml::to_string_pretty(&document).expect("a table serialises");
        assert!(rendered.contains("max-results = 50"), "{rendered}");
    }
}
