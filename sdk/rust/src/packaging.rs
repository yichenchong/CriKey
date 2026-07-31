//! Author-side native package layout checks (spec 23.3).
//!
//! This module deliberately validates a directory only.  Archive construction
//! and installation remain host/package-manager responsibilities.

use std::fs;
use std::path::{Path, PathBuf};

use crate::SdkError;

/// Paths required for one native plugin package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageLayout {
    pub manifest: PathBuf,
    pub entrypoint: PathBuf,
}

/// Validates `crikey.toml` and the platform-specific native entrypoint without
/// loading or executing the plugin (spec 23.3, 24.1).
pub fn validate_layout(dir: &Path, os: &str, arch: &str) -> Result<PackageLayout, SdkError> {
    if !dir.is_dir() {
        return Err(SdkError::Config(format!(
            "plugin directory does not exist: {}",
            dir.display()
        )));
    }
    let manifest = dir.join("crikey.toml");
    let text = fs::read_to_string(&manifest)
        .map_err(|error| SdkError::Config(format!("cannot read {}: {error}", manifest.display())))?;
    let manifest_version = find_scalar(&text, "manifest-version")
        .ok_or_else(|| SdkError::Config("manifest-version is missing".to_owned()))?;
    if manifest_version != "1" {
        return Err(SdkError::Config(format!(
            "unsupported manifest-version {manifest_version}"
        )));
    }
    let runtime = find_scalar_in_plugin(&text, "runtime")
        .ok_or_else(|| SdkError::Config("[plugin].runtime is missing".to_owned()))?;
    if runtime != "native" {
        return Err(SdkError::Config(format!(
            "plugin runtime must be native, got {runtime}"
        )));
    }
    let platform = format!("{os}-{arch}");
    let relative = find_entrypoint(&text, &platform)
        .ok_or_else(|| SdkError::Config(format!("[plugin].entrypoint is missing for {platform}")))?;
    let relative_path = Path::new(&relative);
    if relative.is_empty() || relative_path.is_absolute() || has_parent_component(relative_path) {
        return Err(SdkError::Config(format!(
            "entrypoint must be a relative path inside the plugin directory: {relative}"
        )));
    }
    let entrypoint = dir.join(relative_path);
    if !entrypoint.is_file() {
        return Err(SdkError::Config(format!(
            "native entrypoint is missing: {}",
            entrypoint.display()
        )));
    }
    Ok(PackageLayout { manifest, entrypoint })
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
}

fn find_scalar(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.split('#').next()?.trim();
        let (left, right) = line.split_once('=')?;
        if left.trim() != key {
            return None;
        }
        parse_string_or_bare(right.trim())
    })
}

fn find_scalar_in_plugin(text: &str, key: &str) -> Option<String> {
    let mut in_plugin = false;
    for line in text.lines() {
        let line = line.split('#').next().map_or("", str::trim);
        if line.starts_with('[') {
            in_plugin = line == "[plugin]";
            continue;
        }
        if !in_plugin {
            continue;
        }
        let (left, right) = match line.split_once('=') {
            Some(parts) => parts,
            None => continue,
        };
        if left.trim() == key {
            return parse_string_or_bare(right.trim());
        }
    }
    None
}

fn find_entrypoint(text: &str, platform: &str) -> Option<String> {
    let dotted_key = format!("entrypoint.{platform}");
    let mut scalar = None;
    let mut inline = None;
    let mut dotted = None;
    let mut in_plugin = false;
    for line in text.lines() {
        let line = line.split('#').next().map_or("", str::trim);
        if line.starts_with('[') {
            in_plugin = line == "[plugin]";
            continue;
        }
        if !in_plugin {
            continue;
        }
        let (left, right) = match line.split_once('=') {
            Some(parts) => parts,
            None => continue,
        };
        let key = left.trim();
        let value = right.trim();
        if key == dotted_key {
            dotted = parse_string_or_bare(value);
            continue;
        }
        if key != "entrypoint" {
            continue;
        }
        if value.starts_with('{') && value.ends_with('}') {
            if let Some(entries) = parse_inline_table(&value[1..value.len() - 1]) {
                inline = entries
                    .into_iter()
                    .find_map(|(entry_platform, path)| (entry_platform == platform).then_some(path));
            }
        } else {
            scalar = parse_string_or_bare(value);
        }
    }
    dotted.or(inline).or(scalar)
}

fn parse_inline_table(input: &str) -> Option<Vec<(String, String)>> {
    let mut entries = Vec::new();
    for part in input.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (key, value) = part.split_once('=')?;
        let key = parse_string_or_bare(key.trim())?;
        let value = parse_string_or_bare(value.trim())?;
        entries.push((key, value));
    }
    Some(entries)
}

fn parse_string_or_bare(value: &str) -> Option<String> {
    parse_quoted(value).or_else(|| {
        (!value.is_empty() && value.chars().all(|character| !character.is_whitespace()))
            .then(|| value.to_owned())
    })
}

fn parse_quoted(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return None;
    }
    let inner = &value[1..value.len() - 1];
    let mut output = String::with_capacity(inner.len());
    let mut escaped = false;
    for character in inner.chars() {
        if escaped {
            let decoded = match character {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '\\' => '\\',
                '"' => '"',
                _ => return None,
            };
            output.push(decoded);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            output.push(character);
        }
    }
    (!escaped).then_some(output)
}
