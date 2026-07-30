//! Dependency resolution against an offline index (spec 23.2).
//!
//! Each declared requirement is a name plus a comma-joined PEP-440 *subset* of
//! `==`, `>=`, `>`, `<`, `<=`. Among the indexed versions satisfying every
//! clause the HIGHEST — by numeric, not lexical, ordering — is chosen; a
//! requirement no indexed wheel satisfies is a [`PackageError::Resolution`].

use crate::index::PackageIndex;
use crate::lockfile::{LockedPackage, Lockfile};
use crate::PackageError;

/// Resolve declared deps against the index into a byte-stable lockfile.
pub fn resolve(
    requires_python: &str,
    dependencies: &[String],
    index: &PackageIndex,
) -> Result<Lockfile, PackageError> {
    let mut packages = Vec::new();

    for dep in dependencies {
        let (name, clauses) = parse_requirement(dep)?;

        // Highest indexed version satisfying every clause, compared numerically.
        let mut best: Option<(&str, &str, Vec<u64>)> = None;
        for candidate in index.versions(&name) {
            let parsed = parse_version(&candidate.version);
            if clauses.iter().all(|c| c.matches(&parsed)) {
                let better = match &best {
                    Some((_, _, best_parsed)) => cmp_version(&parsed, best_parsed).is_gt(),
                    None => true,
                };
                if better {
                    best = Some((&candidate.version, &candidate.hash, parsed));
                }
            }
        }

        match best {
            Some((version, hash, _)) => packages.push(LockedPackage {
                name: name.clone(),
                version: version.to_owned(),
                hash: hash.to_owned(),
            }),
            None => {
                return Err(PackageError::Resolution(format!(
                    "no indexed version of `{name}` satisfies `{dep}`"
                )))
            }
        }
    }

    Ok(Lockfile {
        requires_python: requires_python.to_owned(),
        packages,
    })
}

/// A single comparison clause, e.g. `>=1.0`.
#[derive(Debug)]
struct Clause {
    op: Op,
    version: Vec<u64>,
}

impl Clause {
    fn matches(&self, candidate: &[u64]) -> bool {
        let ord = cmp_version(candidate, &self.version);
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

/// Split `acme>=1.0,<2.0` into a name and its comparison clauses.
fn parse_requirement(spec: &str) -> Result<(String, Vec<Clause>), PackageError> {
    let spec = spec.trim();
    let split = spec
        .find(['<', '>', '='])
        .ok_or_else(|| PackageError::Resolution(format!("requirement `{spec}` has no version specifier")))?;
    let name = spec[..split].trim().to_owned();
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
        clauses.push(Clause {
            op,
            version: parse_version(rest.trim()),
        });
    }

    if clauses.is_empty() {
        return Err(PackageError::Resolution(format!(
            "requirement `{spec}` has no clauses"
        )));
    }
    Ok((name, clauses))
}

/// Parse a dotted version into numeric components. Non-numeric components
/// contribute 0, keeping comparison total and numeric.
fn parse_version(v: &str) -> Vec<u64> {
    v.split('.')
        .map(|part| part.trim().parse::<u64>().unwrap_or(0))
        .collect()
}

/// Numeric comparison of two dotted versions, zero-padding the shorter.
fn cmp_version(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    let len = a.len().max(b.len());
    for i in 0..len {
        let av = a.get(i).copied().unwrap_or(0);
        let bv = b.get(i).copied().unwrap_or(0);
        match av.cmp(&bv) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}
