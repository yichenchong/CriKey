//! Finding the `[configuration]` schemas of installed plugins (spec 21.2 layer 5).
//!
//! The plugin-defaults layer comes from manifests, so something has to read
//! them. That happens here rather than by asking the providers, for one reason:
//! `crikey config` must be able to say which layer won a key and whether a field
//! is secret WITHOUT starting a single plugin process. A schema is a declaration
//! on disk; reading it needs no interpreter, no worker and no supervisor.
//!
//! The launcher and `crikey config` therefore read the same declarations the same
//! way, which is the point — a `secret` flag that only one of them honoured would
//! be worse than none.
//!
//! # The Legacy Compatibility Layer is not here
//!
//! A `legacy-python` package keeps Keypirinha configuration syntax and its own
//! notification contract (spec 21.1 last line, spec 14). It is skipped
//! deliberately: routing a legacy plugin's settings through this store would
//! change the format its own configuration path already reads.

use std::path::{Path, PathBuf};

use crikey_core::PluginId;
use crikey_plugin_model::{ConfigurationSection, Manifest, Runtime};

/// One installed plugin's declared configuration schema.
#[derive(Debug, Clone)]
pub struct DiscoveredSchema {
    /// The namespaced host identity, matching what the providers register.
    pub plugin: PluginId,
    /// The package directory the manifest was read from.
    pub package: PathBuf,
    /// The declared schema, already validated by [`Manifest::parse`].
    pub section: ConfigurationSection,
}

/// A package whose manifest could not be used.
///
/// Reported rather than swallowed: a plugin whose schema failed to load silently
/// loses its defaults, and the operator would see a plugin misbehaving with no
/// explanation.
#[derive(Debug, Clone)]
pub struct SchemaProblem {
    /// The package directory.
    pub package: PathBuf,
    /// One line naming what was wrong.
    pub reason: String,
}

/// Reads every plugin manifest under `roots` and returns their schemas.
///
/// `roots` are package-root directories; each immediate subdirectory holding a
/// `crikey.toml` is one package, matching how the modern and native providers
/// discover packages. A root that does not exist contributes nothing and is not a
/// problem: an operator who names no plugin roots has no plugins, which is the
/// ordinary case.
pub fn discover_plugin_schemas(roots: &[PathBuf]) -> (Vec<DiscoveredSchema>, Vec<SchemaProblem>) {
    let mut schemas = Vec::new();
    let mut problems = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut packages: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        // Sorted so two machines with the same plugins register them in the same
        // order, and a diagnostic naming "the first problem" names the same one.
        packages.sort();
        for package in packages {
            match read_package(&package) {
                Ok(Some(schema)) => schemas.push(schema),
                Ok(None) => {}
                Err(reason) => problems.push(SchemaProblem { package, reason }),
            }
        }
    }
    (schemas, problems)
}

/// Reads one package's manifest, or explains why it could not be read.
///
/// `Ok(None)` covers the two "nothing to do" cases: no `crikey.toml` at all (the
/// directory is not a plugin package), and a runtime whose configuration this
/// store does not own.
fn read_package(package: &Path) -> Result<Option<DiscoveredSchema>, String> {
    let manifest_path = package.join("crikey.toml");
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read crikey.toml: {error}")),
    };
    let manifest = Manifest::parse(&text).map_err(|error| format!("invalid crikey.toml: {error}"))?;
    let Some(namespace) = runtime_namespace(manifest.plugin.runtime) else {
        return Ok(None);
    };
    Ok(Some(DiscoveredSchema {
        plugin: PluginId(format!("{namespace}.{}", manifest.plugin.id)),
        package: package.to_path_buf(),
        section: manifest.configuration,
    }))
}

/// The host id namespace for a runtime, or `None` when this store does not own
/// that runtime's configuration.
///
/// The namespaces are the ones the providers already construct
/// (`modern.<id>`, `native.<id>`); this is a second reader of the same rule
/// rather than a second rule, and it exists because `crikey config` must resolve
/// a plugin's keys without loading the plugin.
fn runtime_namespace(runtime: Runtime) -> Option<&'static str> {
    match runtime {
        Runtime::Python => Some("modern"),
        Runtime::Native => Some("native"),
        // Legacy keeps its own configuration path and syntax (spec 21.1, 14).
        Runtime::LegacyPython => None,
        // No host executes these, so nothing would ever read their settings.
        Runtime::Wasm | Runtime::Builtin => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "crikey-config-discovery-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("a temporary directory can be created");
            Self(path)
        }

        fn package(&self, name: &str, manifest: &str) -> PathBuf {
            let package = self.0.join(name);
            std::fs::create_dir_all(&package).expect("create");
            std::fs::write(package.join("crikey.toml"), manifest).expect("write");
            package
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn manifest(id: &str, runtime: &str, configuration: &str) -> String {
        format!(
            "manifest-version = 1\n\n[plugin]\nid = \"{id}\"\nname = \"{id}\"\n\
             version = \"1.0.0\"\nruntime = \"{runtime}\"\n\
             entrypoint = {{ any = \"{id}:Plugin\" }}\n\n{configuration}"
        )
    }

    #[test]
    fn a_modern_package_is_discovered_under_the_modern_namespace() {
        let temp = TempDir::new("modern");
        temp.package(
            "example",
            &manifest(
                "example",
                "python",
                "[[configuration.field]]\nname = \"theme\"\ndefault = \"dark\"\n",
            ),
        );
        let (schemas, problems) = discover_plugin_schemas(&[temp.0.clone()]);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].plugin, PluginId("modern.example".to_owned()));
        assert_eq!(schemas[0].section.fields.len(), 1);
    }

    #[test]
    fn a_native_package_is_discovered_under_the_native_namespace() {
        let temp = TempDir::new("native");
        temp.package("tool", &manifest("tool", "native", ""));
        let (schemas, _) = discover_plugin_schemas(&[temp.0.clone()]);
        assert_eq!(schemas[0].plugin, PluginId("native.tool".to_owned()));
    }

    #[test]
    fn a_legacy_package_is_left_to_the_legacy_compatibility_layer() {
        let temp = TempDir::new("legacy");
        temp.package("old", &manifest("old", "legacy-python", ""));
        let (schemas, problems) = discover_plugin_schemas(&[temp.0.clone()]);
        assert!(
            schemas.is_empty(),
            "legacy configuration must not be routed through this store"
        );
        assert!(
            problems.is_empty(),
            "skipping legacy is not a problem: {problems:?}"
        );
    }

    #[test]
    fn a_directory_without_a_manifest_is_not_a_package_and_not_a_problem() {
        let temp = TempDir::new("bare");
        std::fs::create_dir_all(temp.0.join("notes")).expect("create");
        let (schemas, problems) = discover_plugin_schemas(&[temp.0.clone()]);
        assert!(schemas.is_empty());
        assert!(problems.is_empty());
    }

    #[test]
    fn a_package_with_an_unusable_schema_is_reported_by_path() {
        let temp = TempDir::new("broken");
        let package = temp.package(
            "broken",
            &manifest(
                "broken",
                "python",
                "[[configuration.field]]\nname = \"limit\"\ntype = \"integer\"\n\
                 default = 0\nminimum = 5\n",
            ),
        );
        let (schemas, problems) = discover_plugin_schemas(&[temp.0.clone()]);
        assert!(schemas.is_empty());
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].package, package);
        assert!(problems[0].reason.contains("limit"), "{}", problems[0].reason);
    }

    #[test]
    fn a_root_that_does_not_exist_yields_nothing_and_is_not_a_problem() {
        let (schemas, problems) = discover_plugin_schemas(&[PathBuf::from("/nonexistent/crikey/roots")]);
        assert!(schemas.is_empty());
        assert!(problems.is_empty());
    }
}
