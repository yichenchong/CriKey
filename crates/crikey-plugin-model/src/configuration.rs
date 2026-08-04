//! The `[configuration]` manifest section: a plugin's declared settings schema
//! (spec 21.3).
//!
//! A modern plugin does not receive arbitrary key/value pairs and hope for the
//! best. It declares, in its own manifest, every setting it reads: the field's
//! name, its data type, its default, the rules a value must satisfy, a
//! human-readable description, whether the value is a secret, whether changing
//! it needs a restart, and which platforms it applies to. The host stores
//! configuration as text (spec 21.1 is TOML, and every layer collapses to
//! `key -> string`), so "data type" here means *which strings are acceptable*,
//! and validation is a total function from a candidate string to either
//! acceptance or a named rule violation.
//!
//! # Why the rules live in this crate
//!
//! Nothing here reads a file, a clock or an environment variable: a schema is a
//! value and validating against it is pure. Keeping it beside [`crate::Manifest`]
//! means the declaration and its enforcement cannot drift, and it lets the
//! configuration store (which owns layering and precedence) depend on the schema
//! without the manifest model depending on the store.
//!
//! # Why violations name the rule
//!
//! [`FieldViolation`] carries the field and the *rule identifier* separately
//! from the prose. A message that only said "invalid value" would leave an
//! operator guessing which of several declared constraints refused their edit,
//! and a test asserting on prose would pass for the wrong reason. The rule
//! identifiers are the stable part.
//!
//! # Secrets
//!
//! A field marked `secret = true` is still delivered to its owning plugin —
//! that is what it is for — but its *value* must never appear in a diagnostic,
//! a dump, or an error message. Every message in this module that could mention
//! a value goes through one choke point, [`ConfigurationField::quote`], so the
//! rule cannot be honoured in one branch and forgotten in another.

use serde::{Deserialize, Serialize};

/// A plugin's declared configuration schema.
///
/// Spelled as an array of tables in the manifest, so field order is the order
/// the author wrote and a settings user interface can present them that way:
///
/// ```toml
/// [[configuration.field]]
/// name = "api-key"
/// type = "string"
/// secret = true
/// required = true
/// description = "Token used for the remote search API."
///
/// [[configuration.field]]
/// name = "result-limit"
/// type = "integer"
/// default = 20
/// minimum = 1
/// maximum = 200
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ConfigurationSection {
    /// The declared fields, in declaration order.
    ///
    /// Named `field` on the wire because `[[configuration.field]]` reads as one
    /// field per table, which is what an author is writing.
    #[serde(default, rename = "field")]
    pub fields: Vec<ConfigurationField>,
}

impl ConfigurationSection {
    /// Whether the plugin declared no schema at all.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// The field called `name`, if the plugin declared one.
    pub fn field(&self, name: &str) -> Option<&ConfigurationField> {
        self.fields.iter().find(|field| field.name == name)
    }

    /// Checks the *declaration* itself, independently of any value.
    ///
    /// Run at manifest parse time. Three things make a schema unusable rather
    /// than merely unsatisfied: an unusable field name (it becomes part of a
    /// dotted configuration key, so a name containing a dot would silently
    /// address a different key), two fields with the same name (the second would
    /// shadow the first with no way to say which won), and a default that its
    /// own field would refuse. The last matters most: a plugin whose default is
    /// invalid is broken on a machine with no user settings at all, which is
    /// every machine on first run.
    pub fn validate_declaration(&self) -> Result<(), FieldViolation> {
        for (index, field) in self.fields.iter().enumerate() {
            if !is_usable_field_name(&field.name) {
                return Err(FieldViolation {
                    field: field.name.clone(),
                    rule: RULE_FIELD_NAME,
                    detail: "a field name must be non-empty and contain only letters, digits, \
                             `-` or `_`"
                        .to_owned(),
                });
            }
            if self.fields[..index]
                .iter()
                .any(|earlier| earlier.name == field.name)
            {
                return Err(FieldViolation {
                    field: field.name.clone(),
                    rule: RULE_DUPLICATE_FIELD,
                    detail: "declared more than once".to_owned(),
                });
            }
            if let Some(default) = field.default_text()? {
                field.validate(&default).map_err(|violation| FieldViolation {
                    field: violation.field,
                    rule: RULE_DEFAULT,
                    detail: format!(
                        "the declared default breaks its own `{}` rule: {}",
                        violation.rule, violation.detail
                    ),
                })?;
            }
        }
        Ok(())
    }
}

/// The data type of a declared field (spec 21.3).
///
/// Every configuration value is stored and transported as text, so a type is a
/// parse rule rather than a storage decision: `integer` means the text must
/// parse as an `i64`, `boolean` means exactly `true` or `false`. `path` is
/// deliberately distinct from `string` even though both accept any text, so a
/// settings interface can offer a file picker and the declaration says what the
/// value means.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigurationKind {
    /// Any text.
    #[default]
    String,
    /// A signed 64-bit integer in decimal.
    Integer,
    /// A finite 64-bit floating-point number.
    Float,
    /// Exactly `true` or `false`.
    Boolean,
    /// Any text, meant as a filesystem path.
    Path,
}

impl ConfigurationKind {
    /// The manifest spelling, for diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Float => "float",
            Self::Boolean => "boolean",
            Self::Path => "path",
        }
    }
}

/// One declared setting (spec 21.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct ConfigurationField {
    /// The field's name, which becomes the last segment of its configuration
    /// key (`plugins.<plugin-id>.settings.<name>`).
    pub name: String,
    /// The data type. Absent means [`ConfigurationKind::String`].
    #[serde(rename = "type", default)]
    pub kind: ConfigurationKind,
    /// The value used when no layer supplies one.
    ///
    /// A TOML scalar rather than a string so an author writes `default = 20` and
    /// `default = true` instead of quoting everything; it is rendered to text by
    /// [`Self::default_text`], which is also where a non-scalar default is
    /// refused.
    #[serde(default)]
    pub default: Option<toml::Value>,
    /// What the setting does, shown to a user.
    #[serde(default)]
    pub description: String,
    /// Whether the value must never be rendered in a diagnostic or dump.
    #[serde(default)]
    pub secret: bool,
    /// Whether a change takes effect only after the plugin restarts.
    ///
    /// Recorded rather than acted on here: the host still delivers the change
    /// live (spec 21.4), and this flag is what lets it also tell the operator
    /// that the plugin will not honour it until restarted.
    #[serde(default)]
    pub requires_restart: bool,
    /// The platforms the field applies to, as `std::env::consts::OS` values.
    /// Empty means every platform.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Whether a value must be present. A field with a default is always
    /// satisfied, so `required` is only meaningful without one.
    #[serde(default)]
    pub required: bool,
    /// Inclusive lower bound for `integer` and `float`.
    #[serde(default)]
    pub minimum: Option<i64>,
    /// Inclusive upper bound for `integer` and `float`.
    #[serde(default)]
    pub maximum: Option<i64>,
    /// Inclusive ceiling on the value's length in characters.
    #[serde(default)]
    pub max_length: Option<usize>,
    /// The complete set of acceptable values. Empty means unrestricted.
    #[serde(default)]
    pub allowed: Vec<String>,
}

/// Rule identifier: the value does not parse as the declared type.
pub const RULE_TYPE: &str = "type";
/// Rule identifier: the value is below `minimum`.
pub const RULE_MINIMUM: &str = "minimum";
/// Rule identifier: the value is above `maximum`.
pub const RULE_MAXIMUM: &str = "maximum";
/// Rule identifier: the value is longer than `max-length`.
pub const RULE_MAX_LENGTH: &str = "max-length";
/// Rule identifier: the value is not in `allowed`.
pub const RULE_ALLOWED: &str = "allowed";
/// Rule identifier: a `required` field has neither a value nor a default.
pub const RULE_REQUIRED: &str = "required";
/// Rule identifier: the field name cannot be part of a configuration key.
pub const RULE_FIELD_NAME: &str = "field-name";
/// Rule identifier: two fields share one name.
pub const RULE_DUPLICATE_FIELD: &str = "duplicate-field";
/// Rule identifier: the declared default is itself unusable.
pub const RULE_DEFAULT: &str = "default";
/// Rule identifier: the plugin declares no such field.
pub const RULE_UNKNOWN_FIELD: &str = "unknown-field";

/// What a diagnostic prints instead of a secret value (spec 21.3).
pub const REDACTED: &str = "<redacted>";

impl ConfigurationField {
    /// Whether this field is in force on `os` (a `std::env::consts::OS` value).
    ///
    /// A field restricted to other platforms is not merely ignored: it must not
    /// contribute a default either, or a Linux host would carry a Windows-only
    /// setting's default forever.
    pub fn applies_to(&self, os: &str) -> bool {
        self.platforms.is_empty() || self.platforms.iter().any(|name| name == os)
    }

    /// The declared default rendered as text, or `None` when none was declared.
    ///
    /// Only TOML scalars are accepted. An array or table default would have no
    /// single textual spelling the store could round-trip, and inventing one
    /// (JSON? a TOML fragment?) would make the same manifest mean different
    /// things to a different reader.
    pub fn default_text(&self) -> Result<Option<String>, FieldViolation> {
        let Some(value) = &self.default else {
            return Ok(None);
        };
        match value {
            toml::Value::String(text) => Ok(Some(text.clone())),
            toml::Value::Integer(number) => Ok(Some(number.to_string())),
            toml::Value::Float(number) => Ok(Some(number.to_string())),
            toml::Value::Boolean(flag) => Ok(Some(flag.to_string())),
            toml::Value::Datetime(stamp) => Ok(Some(stamp.to_string())),
            toml::Value::Array(_) | toml::Value::Table(_) => Err(FieldViolation {
                field: self.name.clone(),
                rule: RULE_DEFAULT,
                detail: "a default must be a scalar (string, integer, float, boolean or datetime)".to_owned(),
            }),
        }
    }

    /// Accepts `value` or names the rule that refused it.
    ///
    /// Rules are checked in a fixed order — type, then range, then length, then
    /// membership — so the reported rule is deterministic for a value that
    /// breaks several. The offending text is quoted only when the field is not a
    /// secret: an error message is a diagnostic, and a diagnostic that echoed an
    /// API token would leak it into every log that captured it (spec 21.3).
    pub fn validate(&self, value: &str) -> Result<(), FieldViolation> {
        let quoted = self.quote(value);
        let numeric = match self.kind {
            ConfigurationKind::String | ConfigurationKind::Path => None,
            ConfigurationKind::Integer => {
                let parsed: i64 = value.trim().parse().map_err(|_| FieldViolation {
                    field: self.name.clone(),
                    rule: RULE_TYPE,
                    detail: format!("{quoted} is not an integer"),
                })?;
                Some(integer_as_float(parsed))
            }
            ConfigurationKind::Float => {
                let parsed: f64 = value.trim().parse().map_err(|_| FieldViolation {
                    field: self.name.clone(),
                    rule: RULE_TYPE,
                    detail: format!("{quoted} is not a number"),
                })?;
                if !parsed.is_finite() {
                    return Err(FieldViolation {
                        field: self.name.clone(),
                        rule: RULE_TYPE,
                        detail: format!("{quoted} is not a finite number"),
                    });
                }
                Some(parsed)
            }
            ConfigurationKind::Boolean => {
                if value.trim() != "true" && value.trim() != "false" {
                    return Err(FieldViolation {
                        field: self.name.clone(),
                        rule: RULE_TYPE,
                        detail: format!("{quoted} is not `true` or `false`"),
                    });
                }
                None
            }
        };

        if let (Some(number), Some(minimum)) = (numeric, self.minimum) {
            if number < integer_as_float(minimum) {
                return Err(FieldViolation {
                    field: self.name.clone(),
                    rule: RULE_MINIMUM,
                    detail: format!("{quoted} is below the declared minimum {minimum}"),
                });
            }
        }
        if let (Some(number), Some(maximum)) = (numeric, self.maximum) {
            if number > integer_as_float(maximum) {
                return Err(FieldViolation {
                    field: self.name.clone(),
                    rule: RULE_MAXIMUM,
                    detail: format!("{quoted} is above the declared maximum {maximum}"),
                });
            }
        }
        if let Some(limit) = self.max_length {
            let length = value.chars().count();
            if length > limit {
                return Err(FieldViolation {
                    field: self.name.clone(),
                    rule: RULE_MAX_LENGTH,
                    // The LENGTH is safe to state even for a secret; the value is not.
                    detail: format!("is {length} characters, above the declared max-length {limit}"),
                });
            }
        }
        if !self.allowed.is_empty() && !self.allowed.iter().any(|candidate| candidate == value) {
            return Err(FieldViolation {
                field: self.name.clone(),
                rule: RULE_ALLOWED,
                detail: format!(
                    "{quoted} is not one of the declared values: {}",
                    self.allowed.join(", ")
                ),
            });
        }
        Ok(())
    }

    /// Renders `value` for a diagnostic, redacting a secret field.
    ///
    /// The single choke point through which any of this module's messages may
    /// mention a value, so the secret rule cannot be forgotten in one branch.
    fn quote(&self, value: &str) -> String {
        if self.secret {
            REDACTED.to_owned()
        } else {
            format!("`{value}`")
        }
    }
}

/// Widens a declared bound for comparison against a parsed value.
///
/// Bounds are declared as TOML integers (an author writing `minimum = 1` should
/// not have to write `1.0`), while a `float` field's value is an `f64`. Every
/// bound a plugin can plausibly declare is exactly representable; the cast is
/// named here, once, rather than scattered as inline `as` with a lint waiver at
/// each site.
#[allow(clippy::cast_precision_loss)]
fn integer_as_float(value: i64) -> f64 {
    value as f64
}

/// A named refusal: which field, which declared rule, and why.
///
/// `rule` is a `&'static str` from the `RULE_*` constants rather than an enum
/// because callers only ever compare or print it, and the set grows whenever a
/// new declared rule is added; an enum would force every downstream `match` to
/// change for a purely additive rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldViolation {
    /// The declared field the value belongs to.
    pub field: String,
    /// One of the `RULE_*` identifiers.
    pub rule: &'static str,
    /// Prose. Never contains a secret field's value.
    pub detail: String,
}

impl std::fmt::Display for FieldViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "field `{}` violates its `{}` rule: {}",
            self.field, self.rule, self.detail
        )
    }
}

impl std::error::Error for FieldViolation {}

/// Whether `name` can be the last segment of a dotted configuration key.
///
/// A dot would make `plugins.p.settings.a.b` ambiguous between a field called
/// `a.b` and a nested table, and whitespace would not survive a round trip
/// through a hand-edited TOML file.
fn is_usable_field_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, kind: ConfigurationKind) -> ConfigurationField {
        ConfigurationField {
            name: name.to_owned(),
            kind,
            default: None,
            description: String::new(),
            secret: false,
            requires_restart: false,
            platforms: Vec::new(),
            required: false,
            minimum: None,
            maximum: None,
            max_length: None,
            allowed: Vec::new(),
        }
    }

    #[test]
    fn an_integer_field_refuses_text_and_names_the_type_rule() {
        let declared = field("limit", ConfigurationKind::Integer);
        let violation = declared.validate("twenty").expect_err("text is not an integer");
        assert_eq!(violation.field, "limit");
        assert_eq!(violation.rule, RULE_TYPE);
    }

    #[test]
    fn a_value_below_the_declared_minimum_names_the_minimum_rule() {
        let mut declared = field("limit", ConfigurationKind::Integer);
        declared.minimum = Some(1);
        let violation = declared.validate("0").expect_err("zero is below one");
        assert_eq!(violation.rule, RULE_MINIMUM);
        declared.validate("1").expect("the bound is inclusive");
    }

    #[test]
    fn a_value_above_the_declared_maximum_names_the_maximum_rule() {
        let mut declared = field("limit", ConfigurationKind::Integer);
        declared.maximum = Some(200);
        assert_eq!(
            declared.validate("201").expect_err("201 exceeds 200").rule,
            RULE_MAXIMUM
        );
        declared.validate("200").expect("the bound is inclusive");
    }

    #[test]
    fn a_value_outside_the_declared_set_names_the_allowed_rule() {
        let mut declared = field("theme", ConfigurationKind::String);
        declared.allowed = vec!["dark".to_owned(), "light".to_owned()];
        assert_eq!(
            declared
                .validate("solar")
                .expect_err("solar was not declared")
                .rule,
            RULE_ALLOWED
        );
        declared.validate("dark").expect("a declared value is accepted");
    }

    #[test]
    fn a_value_longer_than_max_length_names_the_max_length_rule() {
        let mut declared = field("prefix", ConfigurationKind::String);
        declared.max_length = Some(3);
        assert_eq!(
            declared
                .validate("abcd")
                .expect_err("four characters exceed three")
                .rule,
            RULE_MAX_LENGTH
        );
        declared.validate("abc").expect("the ceiling is inclusive");
    }

    #[test]
    fn a_boolean_field_accepts_only_true_and_false() {
        let declared = field("enabled", ConfigurationKind::Boolean);
        declared.validate("true").expect("true is a boolean");
        declared.validate("false").expect("false is a boolean");
        assert_eq!(
            declared
                .validate("yes")
                .expect_err("yes is not a TOML boolean")
                .rule,
            RULE_TYPE
        );
    }

    #[test]
    fn a_float_field_refuses_a_non_finite_value() {
        let declared = field("weight", ConfigurationKind::Float);
        declared.validate("1.5").expect("a finite float is accepted");
        assert_eq!(
            declared.validate("inf").expect_err("infinity is not usable").rule,
            RULE_TYPE
        );
    }

    #[test]
    fn a_secret_fields_violation_never_quotes_the_value() {
        let mut declared = field("api-key", ConfigurationKind::Integer);
        declared.secret = true;
        let violation = declared
            .validate("hunter2-the-real-token")
            .expect_err("a token is not an integer");
        assert!(
            !violation.detail.contains("hunter2"),
            "a secret field's value leaked into a diagnostic: {}",
            violation.detail
        );
        assert!(violation.detail.contains(REDACTED), "{}", violation.detail);
        assert!(
            !violation.to_string().contains("hunter2"),
            "a secret field's value leaked into Display: {violation}"
        );
    }

    #[test]
    fn a_secret_fields_allowed_violation_names_the_rule_without_the_value() {
        let mut declared = field("api-key", ConfigurationKind::String);
        declared.secret = true;
        declared.allowed = vec!["alpha".to_owned()];
        let violation = declared.validate("beta").expect_err("beta was not declared");
        assert_eq!(violation.rule, RULE_ALLOWED);
        assert!(!violation.detail.contains("beta"), "{}", violation.detail);
    }

    #[test]
    fn a_platform_restricted_field_applies_only_to_the_named_platforms() {
        let mut declared = field("registry-path", ConfigurationKind::Path);
        declared.platforms = vec!["windows".to_owned()];
        assert!(declared.applies_to("windows"));
        assert!(!declared.applies_to("linux"));
        assert!(field("any", ConfigurationKind::String).applies_to("linux"));
    }

    #[test]
    fn a_scalar_default_renders_as_text_and_a_table_default_is_refused() {
        let mut declared = field("limit", ConfigurationKind::Integer);
        declared.default = Some(toml::Value::Integer(20));
        assert_eq!(
            declared.default_text().expect("a scalar renders"),
            Some("20".to_owned())
        );
        declared.default = Some(toml::Value::Array(vec![toml::Value::Integer(1)]));
        assert_eq!(
            declared
                .default_text()
                .expect_err("an array has no textual spelling")
                .rule,
            RULE_DEFAULT
        );
    }

    #[test]
    fn a_declaration_whose_default_breaks_its_own_rule_is_refused() {
        let mut declared = field("limit", ConfigurationKind::Integer);
        declared.minimum = Some(10);
        declared.default = Some(toml::Value::Integer(1));
        let section = ConfigurationSection {
            fields: vec![declared],
        };
        let violation = section
            .validate_declaration()
            .expect_err("a default below the declared minimum is a broken schema");
        assert_eq!(violation.rule, RULE_DEFAULT);
        assert!(violation.detail.contains(RULE_MINIMUM), "{}", violation.detail);
    }

    #[test]
    fn two_fields_with_one_name_are_refused() {
        let section = ConfigurationSection {
            fields: vec![
                field("theme", ConfigurationKind::String),
                field("theme", ConfigurationKind::String),
            ],
        };
        assert_eq!(
            section
                .validate_declaration()
                .expect_err("a shadowed field is ambiguous")
                .rule,
            RULE_DUPLICATE_FIELD
        );
    }

    #[test]
    fn a_field_name_containing_a_dot_is_refused() {
        let section = ConfigurationSection {
            fields: vec![field("outer.inner", ConfigurationKind::String)],
        };
        assert_eq!(
            section
                .validate_declaration()
                .expect_err("a dotted name would address a different key")
                .rule,
            RULE_FIELD_NAME
        );
    }
}
