//! Remote catalog sources (spec 2.2 "distributed or remote indexing", ADR-0016).
//!
//! A remote source is catalog content that lives on another machine — a shared
//! team index, a file server — and is searched alongside local items. This
//! module owns only the *declaration*: which sources exist, where they are, how
//! often they are refreshed and how large and how trusted a document each may
//! deliver. Fetching, verifying and admitting the document is `crikey-app`'s
//! job.
//!
//! # Nothing is configured by default
//!
//! [`remote_catalog_sources`] returns an empty vector for a store with no
//! `catalog.remote.*` keys, and no layer supplies one. A launcher on a machine
//! that has never heard of a remote index therefore performs no network work
//! at all, which is the whole reason the feature is expressed as configuration
//! rather than as a default endpoint.
//!
//! # Shape
//!
//! ```toml
//! [catalog.remote.team]
//! url = "https://index.example.com/crikey/team/catalog.toml"
//! interval-ms = 900000
//! max-bytes = 33554432
//! require-signature = true
//! signing-key = "team-index"
//! ```
//!
//! The store flattens that to `catalog.remote.team.url` and friends, so a
//! source is discovered by walking the keys rather than by deserialising a
//! table: the store has no tables left by the time this module reads it.

use std::collections::BTreeMap;

use crate::store::ConfigStore;
use crate::ConfigError;

/// Prefix every remote-source key shares.
pub const KEY_REMOTE_CATALOG_PREFIX: &str = "catalog.remote.";

const FIELD_URL: &str = "url";
const FIELD_INTERVAL_MS: &str = "interval-ms";
const FIELD_MAX_BYTES: &str = "max-bytes";
const FIELD_REQUIRE_SIGNATURE: &str = "require-signature";
const FIELD_SIGNING_KEY: &str = "signing-key";

/// How often a source is refreshed when it does not say: once an hour.
///
/// A shared index is not a live feed. An hour is short enough that a team's
/// additions arrive the same working day and long enough that a hundred
/// launchers do not become a load problem for the machine serving them.
pub const DEFAULT_REMOTE_INTERVAL_MS: u64 = 60 * 60 * 1000;

/// Largest document a source may deliver when it does not say: 32 MiB.
///
/// Well below the archive ceiling on purpose. A remote slice crosses a network
/// and is held in memory while it is verified, so the default is the size a
/// shared team index plausibly reaches rather than the size the format allows.
pub const DEFAULT_REMOTE_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Largest `max-bytes` a source may ask for.
///
/// Matches `crikey_catalog::MAX_ARCHIVE_BYTES`, the ceiling the slice decoder
/// itself enforces. Restated rather than imported because configuration does
/// not depend on the catalog store (ADR-0001); a source asking for more than
/// the decoder would ever read is refused here, where the user can see why.
pub const MAX_REMOTE_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Longest a source name may be, so it stays usable as one path component and
/// one catalog owner id.
const MAX_NAME_BYTES: usize = 64;

/// One declared remote catalog source.
///
/// Every field is resolved: an absent optional key becomes the documented
/// default here rather than an `Option` the consumer has to default again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCatalogSource {
    /// The name the user gave the source, which is also what the launcher
    /// derives its catalog owner id from.
    pub name: String,
    /// URL of the source's manifest document.
    pub url: String,
    /// Milliseconds between automatic refreshes. Zero means the source is
    /// refreshed only when a command asks for it.
    pub interval_ms: u64,
    /// Ceiling on the bytes one refresh may read.
    pub max_bytes: u64,
    /// Whether an unsigned document is refused.
    pub require_signature: bool,
    /// The trusted key name the document must be signed by, if the user pinned
    /// one. Pinning a key implies [`Self::require_signature`].
    pub signing_key: Option<String>,
}

/// Every remote catalog source the store declares, ordered by name.
///
/// A key naming a field this crate does not know is an error rather than a
/// silent no-op: a user who wrote `intervall-ms` expects a refresh interval,
/// and a launcher that ignored it would look like it was working.
pub fn remote_catalog_sources(store: &ConfigStore) -> Result<Vec<RemoteCatalogSource>, ConfigError> {
    // The winning value per key, so an administrator policy pinning a source's
    // URL beats a user's, exactly as for every other key.
    collect(
        store
            .keys()
            .into_iter()
            .filter_map(|key| store.get(key).map(|value| (key, value))),
    )
}

/// The whole rule, over `key -> winning value` pairs.
///
/// Split from [`remote_catalog_sources`] so every rule below is testable
/// against literal keys: a store cannot be built without a filesystem, and a
/// declaration rule is not a statement about layering.
fn collect<'a>(
    entries: impl Iterator<Item = (&'a str, &'a str)>,
) -> Result<Vec<RemoteCatalogSource>, ConfigError> {
    let mut declared: BTreeMap<&str, BTreeMap<&str, &str>> = BTreeMap::new();
    for (key, value) in entries {
        let Some(rest) = key.strip_prefix(KEY_REMOTE_CATALOG_PREFIX) else {
            continue;
        };
        let Some((name, field)) = rest.split_once('.') else {
            return Err(ConfigError::Setting {
                key: key.to_owned(),
                reason: "must be `catalog.remote.<name>.<field>`",
            });
        };
        check_name(key, name)?;
        declared.entry(name).or_default().insert(field, value);
    }

    let mut sources = Vec::with_capacity(declared.len());
    for (name, fields) in declared {
        sources.push(build(name, &fields)?);
    }
    Ok(sources)
}

fn build(name: &str, fields: &BTreeMap<&str, &str>) -> Result<RemoteCatalogSource, ConfigError> {
    for field in fields.keys() {
        if !matches!(
            *field,
            FIELD_URL | FIELD_INTERVAL_MS | FIELD_MAX_BYTES | FIELD_REQUIRE_SIGNATURE | FIELD_SIGNING_KEY
        ) {
            return Err(ConfigError::Setting {
                key: qualify(name, field),
                reason: "is not a remote catalog source setting",
            });
        }
    }

    let Some(url) = fields.get(FIELD_URL) else {
        return Err(ConfigError::Setting {
            key: qualify(name, FIELD_URL),
            reason: "is required: a remote catalog source must say where it is",
        });
    };
    check_url(&qualify(name, FIELD_URL), url)?;

    let interval_ms = match fields.get(FIELD_INTERVAL_MS) {
        Some(text) => number(&qualify(name, FIELD_INTERVAL_MS), text, 0, u64::MAX)?,
        None => DEFAULT_REMOTE_INTERVAL_MS,
    };
    let max_bytes = match fields.get(FIELD_MAX_BYTES) {
        Some(text) => number(&qualify(name, FIELD_MAX_BYTES), text, 1, MAX_REMOTE_MAX_BYTES)?,
        None => DEFAULT_REMOTE_MAX_BYTES,
    };
    let signing_key = match fields.get(FIELD_SIGNING_KEY) {
        Some(key_name) => {
            check_name(&qualify(name, FIELD_SIGNING_KEY), key_name)?;
            Some((*key_name).to_owned())
        }
        None => None,
    };
    let require_signature = match fields.get(FIELD_REQUIRE_SIGNATURE) {
        Some(text) => boolean(&qualify(name, FIELD_REQUIRE_SIGNATURE), text)?,
        // Pinning a key is a stronger statement than the flag it implies, so a
        // source that names a signer is signature-checked whether or not it
        // also spelled the flag out.
        None => signing_key.is_some(),
    };
    if signing_key.is_some() && !require_signature {
        return Err(ConfigError::Setting {
            key: qualify(name, FIELD_REQUIRE_SIGNATURE),
            reason: "cannot be false while `signing-key` names a required signer",
        });
    }

    Ok(RemoteCatalogSource {
        name: name.to_owned(),
        url: (*url).to_owned(),
        interval_ms,
        max_bytes,
        require_signature,
        signing_key,
    })
}

fn qualify(name: &str, field: &str) -> String {
    format!("{KEY_REMOTE_CATALOG_PREFIX}{name}.{field}")
}

/// Accepts one lowercase, path-safe, id-safe name.
///
/// Deliberately narrow. The name becomes a catalog owner id and a cache file
/// name, so anything that would need escaping in either place is refused where
/// the user can read the rule instead of somewhere downstream.
fn check_name(key: &str, name: &str) -> Result<(), ConfigError> {
    let acceptable = !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_'));
    if acceptable {
        Ok(())
    } else {
        Err(ConfigError::Setting {
            key: key.to_owned(),
            reason: "must be 1 to 64 lowercase letters, digits, hyphens or underscores",
        })
    }
}

/// Accepts the two schemes a remote catalog document may arrive over.
///
/// `https` is the network case and `file` the mounted-share case, which is what
/// "a remote file server" means once the operating system has mounted it. Plain
/// `http` is absent on purpose: a catalog document decides what a user's
/// keystrokes launch, so it is not fetched over a transport a bystander can
/// rewrite.
fn check_url(key: &str, url: &str) -> Result<(), ConfigError> {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("file://"))
    else {
        return Err(ConfigError::Setting {
            key: key.to_owned(),
            reason: "must be an `https://` or `file://` URL",
        });
    };
    if url
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(ConfigError::Setting {
            key: key.to_owned(),
            reason: "must not contain whitespace or control characters",
        });
    }
    // A manifest names its slice relative to its own directory, so the URL has
    // to have one: `https://host` alone gives the fetcher nothing to join to.
    if !rest.contains('/') || rest.ends_with('/') {
        return Err(ConfigError::Setting {
            key: key.to_owned(),
            reason: "must name a document, not a host or a directory",
        });
    }
    Ok(())
}

fn number(key: &str, text: &str, low: u64, high: u64) -> Result<u64, ConfigError> {
    match text.parse::<u64>() {
        Ok(value) if value >= low && value <= high => Ok(value),
        Ok(_) => Err(ConfigError::Setting {
            key: key.to_owned(),
            reason: "is outside the range this setting allows",
        }),
        Err(_) => Err(ConfigError::Setting {
            key: key.to_owned(),
            reason: "must be a whole number",
        }),
    }
}

fn boolean(key: &str, text: &str) -> Result<bool, ConfigError> {
    match text {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ConfigError::Setting {
            key: key.to_owned(),
            reason: "must be `true` or `false`",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies the whole declaration rule to literal `key = value` pairs.
    fn declare(entries: &[(&str, &str)]) -> Result<Vec<RemoteCatalogSource>, ConfigError> {
        collect(entries.iter().map(|(key, value)| (*key, *value)))
    }

    #[test]
    fn a_configuration_with_no_remote_keys_declares_no_sources() {
        assert_eq!(
            declare(&[("launcher.profile", "work")]).expect("an empty declaration is not an error"),
            Vec::new()
        );
    }

    #[test]
    fn a_url_alone_is_a_complete_source_with_documented_defaults() {
        let sources = declare(&[("catalog.remote.team.url", "https://example.com/team/index.txt")])
            .expect("a url is enough");
        assert_eq!(
            sources,
            vec![RemoteCatalogSource {
                name: "team".to_owned(),
                url: "https://example.com/team/index.txt".to_owned(),
                interval_ms: DEFAULT_REMOTE_INTERVAL_MS,
                max_bytes: DEFAULT_REMOTE_MAX_BYTES,
                require_signature: false,
                signing_key: None,
            }]
        );
    }

    #[test]
    fn sources_are_ordered_by_name_regardless_of_key_order() {
        let names: Vec<String> = declare(&[
            ("catalog.remote.zulu.url", "file:///srv/zulu.txt"),
            ("catalog.remote.alpha.url", "file:///srv/alpha.txt"),
        ])
        .expect("both sources are complete")
        .into_iter()
        .map(|source| source.name)
        .collect();
        assert_eq!(names, vec!["alpha".to_owned(), "zulu".to_owned()]);
    }

    #[test]
    fn pinning_a_signer_requires_a_signature_without_the_flag() {
        let source = declare(&[
            ("catalog.remote.team.url", "https://example.com/team/index.txt"),
            ("catalog.remote.team.signing-key", "team-index"),
        ])
        .expect("a pinned key is enough")
        .remove(0);
        assert!(source.require_signature, "a pinned signer implies enforcement");
        assert_eq!(source.signing_key.as_deref(), Some("team-index"));
    }

    #[test]
    fn a_pinned_signer_cannot_be_disarmed_by_the_flag() {
        let error = declare(&[
            ("catalog.remote.team.url", "https://example.com/team/index.txt"),
            ("catalog.remote.team.signing-key", "team-index"),
            ("catalog.remote.team.require-signature", "false"),
        ])
        .expect_err("the two settings contradict");
        assert!(
            error.to_string().contains("require-signature"),
            "the contradiction names the key that has to change: {error}"
        );
    }

    #[test]
    fn a_source_without_a_url_is_refused_by_name() {
        let error = declare(&[("catalog.remote.team.interval-ms", "1000")])
            .expect_err("an interval alone names nothing");
        assert!(
            error.to_string().contains("catalog.remote.team.url"),
            "the message names the missing key: {error}"
        );
    }

    #[test]
    fn plain_http_is_refused() {
        let error = declare(&[("catalog.remote.team.url", "http://example.com/team/index.txt")])
            .expect_err("http is not a trusted transport here");
        assert!(
            error.to_string().contains("https"),
            "the message says which schemes are allowed: {error}"
        );
    }

    #[test]
    fn a_url_naming_only_a_host_is_refused() {
        assert!(
            declare(&[("catalog.remote.team.url", "https://example.com")]).is_err(),
            "a manifest URL must have a directory to resolve its slice against"
        );
    }

    #[test]
    fn a_misspelled_field_is_refused_rather_than_ignored() {
        let error = declare(&[
            ("catalog.remote.team.url", "https://example.com/team/index.txt"),
            ("catalog.remote.team.intervall-ms", "1000"),
        ])
        .expect_err("a typo must not silently do nothing");
        assert!(
            error.to_string().contains("intervall-ms"),
            "the message names the key the user has to fix: {error}"
        );
    }

    #[test]
    fn an_oversized_ceiling_is_refused() {
        let ceiling = (MAX_REMOTE_MAX_BYTES + 1).to_string();
        assert!(
            declare(&[
                ("catalog.remote.team.url", "https://example.com/team/index.txt"),
                ("catalog.remote.team.max-bytes", &ceiling),
            ])
            .is_err(),
            "a ceiling above the decoder's own limit is not a ceiling"
        );
    }

    #[test]
    fn a_zero_ceiling_is_refused() {
        assert!(
            declare(&[
                ("catalog.remote.team.url", "https://example.com/team/index.txt"),
                ("catalog.remote.team.max-bytes", "0"),
            ])
            .is_err(),
            "a source that may read nothing is a misconfiguration, not a disabled source"
        );
    }

    #[test]
    fn a_zero_interval_means_manual_refresh_only() {
        let source = declare(&[
            ("catalog.remote.team.url", "https://example.com/team/index.txt"),
            ("catalog.remote.team.interval-ms", "0"),
        ])
        .expect("zero is a legal interval")
        .remove(0);
        assert_eq!(source.interval_ms, 0);
    }

    #[test]
    fn a_name_that_would_need_escaping_is_refused() {
        assert!(
            declare(&[("catalog.remote.Team Index.url", "https://example.com/t/i.txt")]).is_err(),
            "a source name becomes an owner id and a file name"
        );
    }
}
