//! Dependency resolution against an offline index (spec 23.2).
//!
//! Each declared requirement is a name plus a comma-joined PEP-440 *subset* of
//! `==`, `>=`, `>`, `<`, `<=`. Among the indexed versions satisfying every
//! clause the HIGHEST — by release and pre-release ordering — is chosen; a
//! requirement no indexed wheel satisfies is a [`PackageError::Resolution`].

use std::collections::BTreeMap;

use crate::index::{normalize_name, PackageIndex};
use crate::lockfile::{LockedPackage, Lockfile};
use crate::PackageError;

/// Resolve declared deps against an offline index into a byte-stable lockfile.
pub fn resolve(
    requires_python: &str,
    dependencies: &[String],
    index: &PackageIndex,
) -> Result<Lockfile, PackageError> {
    // Group requirements by their canonical package name. This both avoids
    // duplicate lock entries and makes conflicting declarations fail together:
    // `My.Pkg>=1` and `my-pkg<1` describe one package, not two.
    let mut grouped: BTreeMap<String, (Vec<Clause>, Vec<String>)> = BTreeMap::new();
    for dep in dependencies {
        let (name, clauses) = parse_requirement(dep)?;
        let entry = grouped.entry(name).or_default();
        entry.0.extend(clauses);
        entry.1.push(dep.clone());
    }

    let mut packages = Vec::with_capacity(grouped.len());
    for (name, (clauses, declarations)) in grouped {
        // Highest indexed version satisfying every clause, compared with
        // release and pre-release semantics rather than as text.
        let mut matches = Vec::new();
        for candidate in index.versions(&name) {
            let parsed = parse_version(&candidate.version).map_err(|reason| {
                PackageError::Resolution(format!(
                    "indexed package `{name}=={}` has an invalid version: {reason}",
                    candidate.version
                ))
            })?;
            if clauses.iter().all(|c| c.matches(&parsed)) {
                matches.push((candidate, parsed));
            }
        }

        // PEP 440 excludes pre-releases from ordinary ranges when a stable
        // candidate exists, while an exact pre-release or a range with no
        // stable answer still has to be resolvable.
        let stable_exists = matches.iter().any(|(_, version)| !version.is_prerelease());
        let best = matches
            .into_iter()
            .filter(|(_, version)| !stable_exists || !version.is_prerelease())
            .max_by(|(_, left), (_, right)| left.cmp(right));

        match best {
            Some((candidate, _)) => packages.push(LockedPackage {
                name,
                version: candidate.version.clone(),
                hash: candidate.hash.clone(),
            }),
            None => {
                return Err(PackageError::Resolution(format!(
                    "conflicting or unsatisfied requirements for `{name}`: {}",
                    declarations.join(", ")
                )))
            }
        }
    }
    let lockfile = Lockfile {
        requires_python: requires_python.to_owned(),
        packages,
    };
    lockfile.validate()?;
    Ok(lockfile)
}

/// A single comparison clause, e.g. `>=1.0`.
#[derive(Debug)]
struct Clause {
    op: Op,
    version: Version,
}

impl Clause {
    fn matches(&self, candidate: &Version) -> bool {
        let ord = candidate.cmp(&self.version);
        match self.op {
            Op::Eq => ord.is_eq(),
            Op::Ge => ord.is_ge(),
            Op::Gt => ord.is_gt(),
            Op::Le => ord.is_le(),
            Op::Lt => ord.is_lt(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Eq,
    Ge,
    Gt,
    Le,
    Lt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    epoch: u64,
    release: Vec<u64>,
    pre: Option<Vec<PrePart>>,
    post: Option<u64>,
    dev: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PrePart {
    Numeric(u64),
    Text(String),
}

impl Version {
    fn is_prerelease(&self) -> bool {
        self.pre.is_some() || self.dev.is_some()
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let epoch = self.epoch.cmp(&other.epoch);
        if epoch != std::cmp::Ordering::Equal {
            return epoch;
        }
        let release = cmp_release(&self.release, &other.release);
        if release != std::cmp::Ordering::Equal {
            return release;
        }

        // Development releases sort before pre-releases, which sort before a
        // final release. This also makes a dev-only version a real
        // pre-release instead of treating its suffix as zero.
        match (&self.dev, &other.dev) {
            (Some(left), Some(right)) => {
                let order = left.cmp(right);
                if order != std::cmp::Ordering::Equal {
                    return order;
                }
            }
            (Some(_), None) => return std::cmp::Ordering::Less,
            (None, Some(_)) => return std::cmp::Ordering::Greater,
            (None, None) => {}
        }

        match (&self.pre, &other.pre) {
            (Some(left), Some(right)) => {
                let order = cmp_pre_release(left, right);
                if order != std::cmp::Ordering::Equal {
                    return order;
                }
            }
            (Some(_), None) => return std::cmp::Ordering::Less,
            (None, Some(_)) => return std::cmp::Ordering::Greater,
            (None, None) => {}
        }

        match (&self.post, &other.post) {
            (Some(left), Some(right)) => left.cmp(right),
            (Some(_), None) => std::cmp::Ordering::Greater,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (None, None) => std::cmp::Ordering::Equal,
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn cmp_release(left: &[u64], right: &[u64]) -> std::cmp::Ordering {
    let length = left.len().max(right.len());
    for index in 0..length {
        let left_part = left.get(index).copied().unwrap_or(0);
        let right_part = right.get(index).copied().unwrap_or(0);
        let order = left_part.cmp(&right_part);
        if order != std::cmp::Ordering::Equal {
            return order;
        }
    }
    std::cmp::Ordering::Equal
}

fn cmp_pre_release(left: &[PrePart], right: &[PrePart]) -> std::cmp::Ordering {
    for (left, right) in left.iter().zip(right) {
        let order = match (left, right) {
            (PrePart::Numeric(left), PrePart::Numeric(right)) => left.cmp(right),
            (PrePart::Numeric(_), PrePart::Text(_)) => std::cmp::Ordering::Less,
            (PrePart::Text(_), PrePart::Numeric(_)) => std::cmp::Ordering::Greater,
            (PrePart::Text(left), PrePart::Text(right)) => pre_label_rank(left)
                .cmp(&pre_label_rank(right))
                .then_with(|| left.cmp(right)),
        };
        if order != std::cmp::Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

fn pre_label_rank(label: &str) -> u8 {
    match label {
        "a" | "alpha" => 0,
        "b" | "beta" => 1,
        "c" | "rc" | "pre" | "preview" => 2,
        _ => 3,
    }
}

/// Split `acme>=1.0,<2.0` into a canonical name and comparison clauses.
fn parse_requirement(spec: &str) -> Result<(String, Vec<Clause>), PackageError> {
    let spec = spec.trim();
    let split = spec
        .find(['<', '>', '='])
        .ok_or_else(|| PackageError::Resolution(format!("requirement `{spec}` has no version specifier")))?;
    let name = normalize_name(spec[..split].trim());
    if name.is_empty() {
        return Err(PackageError::Resolution(format!(
            "requirement `{spec}` has an empty name"
        )));
    }

    let mut clauses = Vec::new();
    for raw in spec[split..].split(',') {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let (op, rest) = if let Some(r) = raw.strip_prefix("==") {
            (Op::Eq, r)
        } else if let Some(r) = raw.strip_prefix(">=") {
            (Op::Ge, r)
        } else if let Some(r) = raw.strip_prefix("<=") {
            (Op::Le, r)
        } else if let Some(r) = raw.strip_prefix('>') {
            (Op::Gt, r)
        } else if let Some(r) = raw.strip_prefix('<') {
            (Op::Lt, r)
        } else {
            return Err(PackageError::Resolution(format!(
                "requirement `{spec}` has an unsupported operator in `{raw}`"
            )));
        };
        if rest.trim().is_empty() {
            return Err(PackageError::Resolution(format!(
                "requirement `{spec}` has an empty version"
            )));
        }
        let version = parse_version(rest.trim()).map_err(|reason| {
            PackageError::Resolution(format!("requirement `{spec}` has an invalid version: {reason}"))
        })?;
        clauses.push(Clause { op, version });
    }

    if clauses.is_empty() {
        return Err(PackageError::Resolution(format!(
            "requirement `{spec}` has no clauses"
        )));
    }
    Ok((name, clauses))
}

/// Parse the numeric release and common PEP-440/SemVer pre-release suffixes.
fn parse_version(value: &str) -> Result<Version, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("version is empty".to_owned());
    }
    let public = if let Some((public, local)) = value.split_once('+') {
        // Local labels do not affect public ordering, but malformed labels
        // must not silently turn into a different version.
        if local.is_empty() || local.contains('+') {
            return Err("local version label is malformed".to_owned());
        }
        public
    } else {
        value
    };
    let (base, hyphen_pre) = public
        .split_once('-')
        .map_or((public, None), |(base, pre)| (base, Some(pre)));
    if hyphen_pre.is_some_and(str::is_empty) {
        return Err("pre-release label is empty".to_owned());
    }

    let (epoch, release_and_suffix) = if let Some((epoch, rest)) = base.split_once('!') {
        (
            epoch
                .parse::<u64>()
                .map_err(|_| "epoch is not numeric".to_owned())?,
            rest,
        )
    } else {
        (0, base)
    };
    let release_and_suffix = release_and_suffix.strip_prefix('v').unwrap_or(release_and_suffix);
    let release_end = release_and_suffix
        .bytes()
        .take_while(|byte| byte.is_ascii_digit() || *byte == b'.')
        .count();
    let release_text = release_and_suffix[..release_end].trim_end_matches('.');
    if release_text.is_empty() {
        return Err("release segment is missing".to_owned());
    }
    let mut release = Vec::new();
    for segment in release_text.split('.') {
        if segment.is_empty() {
            return Err("release contains an empty segment".to_owned());
        }
        release.push(
            segment
                .parse::<u64>()
                .map_err(|_| "release segment is not a valid integer".to_owned())?,
        );
    }

    let explicit_pre = hyphen_pre.is_some();
    let suffix = &release_and_suffix[release_end..];
    let suffix = match (suffix.is_empty(), hyphen_pre) {
        (true, Some(pre)) => pre,
        (false, Some(_)) => return Err("version has two pre-release suffixes".to_owned()),
        (false, None) => suffix,
        (true, None) => "",
    };

    let (pre, post, dev) = parse_suffix(suffix, explicit_pre)?;
    Ok(Version {
        epoch,
        release,
        pre,
        post,
        dev,
    })
}

/// The three independent trailing components of a PEP 440 version: the
/// pre-release parts, the post-release number and the development number. Each
/// is absent unless the suffix actually declared it, which is why this is three
/// `Option`s rather than one enum: a version may carry any combination.
type VersionSuffix = (Option<Vec<PrePart>>, Option<u64>, Option<u64>);

fn parse_suffix(suffix: &str, allow_arbitrary_pre: bool) -> Result<VersionSuffix, String> {
    if suffix.is_empty() {
        return Ok((None, None, None));
    }
    let lower = suffix.to_ascii_lowercase();
    for (prefix, is_post) in [("post", true), ("rev", true), ("r", true), ("dev", false)] {
        if let Some(number) = lower.strip_prefix(prefix) {
            let number = if number.is_empty() {
                0
            } else {
                number
                    .parse::<u64>()
                    .map_err(|_| "suffix number is not a valid integer".to_owned())?
            };
            return if is_post {
                Ok((None, Some(number), None))
            } else {
                Ok((None, None, Some(number)))
            };
        }
    }
    if !allow_arbitrary_pre
        && !["a", "alpha", "b", "beta", "c", "rc", "pre", "preview"]
            .iter()
            .any(|prefix| {
                lower
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.is_empty() || rest.bytes().all(|byte| byte.is_ascii_digit()))
            })
    {
        return Err("unknown pre-release suffix".to_owned());
    }

    let mut parts = Vec::new();
    for raw in lower.split('.') {
        if raw.is_empty() {
            return Err("pre-release contains an empty segment".to_owned());
        }
        if raw.bytes().all(|byte| byte.is_ascii_digit()) {
            parts
                .push(PrePart::Numeric(raw.parse::<u64>().map_err(|_| {
                    "pre-release number is not a valid integer".to_owned()
                })?));
            continue;
        }
        let digit_start = raw.find(|character: char| character.is_ascii_digit());
        if let Some(digit_start) = digit_start {
            let (label, number) = raw.split_at(digit_start);
            if !number.bytes().all(|byte| byte.is_ascii_digit()) {
                parts.push(PrePart::Text(raw.to_owned()));
                continue;
            }
            parts.push(PrePart::Text(label.to_owned()));
            parts.push(PrePart::Numeric(
                number
                    .parse::<u64>()
                    .map_err(|_| "pre-release number is not a valid integer".to_owned())?,
            ));
        } else {
            parts.push(PrePart::Text(raw.to_owned()));
        }
    }
    Ok((Some(parts), None, None))
}
