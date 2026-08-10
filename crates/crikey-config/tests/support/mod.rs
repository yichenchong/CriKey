//! Shared fixture: a configuration directory on disk, written file by file.
//!
//! Every layer has exactly one source, so a test states precedence by choosing
//! which of those files exist. That is the whole design under test: a fixture
//! that reached into the store's internals to place a value in a layer would
//! prove only that the store can be told what to say.

#![allow(dead_code)] // Each integration test binary uses a different subset.

use std::path::{Path, PathBuf};

use crikey_config::{ConfigError, ConfigStore, PLUGIN_DIRECTORY, PROFILE_DIRECTORY, USER_CONFIG_FILE};
use crikey_platform::{DirectoryConvention, DirectoryEnvironment, StandardDirectories};
use crikey_plugin_model::{ConfigurationSection, Manifest};

/// A private configuration tree, removed on drop.
///
/// Hand-rolled rather than pulling in a temporary-directory dependency: this
/// crate has no other test-only dependency, and the whole need is one directory
/// per test.
pub struct Fixture {
    root: PathBuf,
}

impl Fixture {
    /// A fresh, empty configuration tree named after the calling test.
    pub fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "crikey-config-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("a temporary directory can be created");
        Self { root }
    }

    /// The tree's root, which is also `config_dir()`.
    pub fn config_dir(&self) -> &Path {
        &self.root
    }

    /// The directories a store loaded from this fixture sees.
    ///
    /// All four are pinned with the `CRIKEY_*_DIR` overrides so resolution never
    /// consults the real `HOME` and one test cannot see another's files. `HOME`
    /// is pinned as well because the base layout is computed before the
    /// overrides are applied, and it must not be the developer's own.
    /// `APPDATA` and `LOCALAPPDATA` are pinned for the same reason: they are
    /// what the Windows base layout is computed from.
    pub fn directories(&self) -> StandardDirectories {
        let environment = [
            "HOME",
            "APPDATA",
            "LOCALAPPDATA",
            "CRIKEY_CONFIG_DIR",
            "CRIKEY_DATA_DIR",
            "CRIKEY_CACHE_DIR",
            "CRIKEY_STATE_DIR",
        ]
        .into_iter()
        .fold(DirectoryEnvironment::new(), |environment, variable| {
            environment.set(variable, self.root.as_os_str())
        });
        // The host's own convention, not a fixed one: `temp_dir()` answers
        // `C:\...` on Windows, which the XDG rule rightly refuses as not
        // absolute, and the fixture would fail for a reason that has nothing
        // to do with configuration.
        StandardDirectories::resolve(DirectoryConvention::current(), &environment)
            .expect("every directory is pinned by an override")
    }

    /// Writes `config_dir()/config.toml`.
    pub fn user_global(&self, text: &str) {
        self.write(&self.root.join(USER_CONFIG_FILE), text);
    }

    /// Removes `config_dir()/config.toml`.
    pub fn remove_user_global(&self) {
        let _ = std::fs::remove_file(self.root.join(USER_CONFIG_FILE));
    }

    /// Writes `config_dir()/profiles/<name>.toml`.
    pub fn profile(&self, name: &str, text: &str) {
        self.write(
            &self.root.join(PROFILE_DIRECTORY).join(format!("{name}.toml")),
            text,
        );
    }

    /// Removes `config_dir()/profiles/<name>.toml`.
    pub fn remove_profile(&self, name: &str) {
        let _ = std::fs::remove_file(self.root.join(PROFILE_DIRECTORY).join(format!("{name}.toml")));
    }

    /// Writes `config_dir()/plugins/<plugin>.toml`.
    pub fn plugin_settings(&self, plugin: &str, text: &str) {
        self.write(
            &self.root.join(PLUGIN_DIRECTORY).join(format!("{plugin}.toml")),
            text,
        );
    }

    /// Removes `config_dir()/plugins/<plugin>.toml`.
    pub fn remove_plugin_settings(&self, plugin: &str) {
        let _ = std::fs::remove_file(self.root.join(PLUGIN_DIRECTORY).join(format!("{plugin}.toml")));
    }

    /// Writes an administrator policy file inside the fixture, since a test
    /// cannot write to the real system policy path.
    pub fn policy(&self, text: &str) {
        let path = self.policy_path();
        self.write(&path, text);
    }

    /// The policy path this fixture uses, whether or not it exists.
    pub fn policy_path(&self) -> PathBuf {
        self.root.join("policy.toml")
    }

    /// Removes the policy file.
    pub fn remove_policy(&self) {
        let _ = std::fs::remove_file(self.policy_path());
    }

    /// Loads a store over this fixture, with the fixture's policy file.
    pub fn load(&self) -> Result<ConfigStore, ConfigError> {
        ConfigStore::load_with_policy(&self.directories(), Some(&self.policy_path()))
    }

    /// Loads a store over this fixture with no administrator policy at all.
    pub fn load_without_policy(&self) -> Result<ConfigStore, ConfigError> {
        ConfigStore::load_with_policy(&self.directories(), None)
    }

    fn write(&self, path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the parent directory can be created");
        }
        std::fs::write(path, text).expect("the fixture file can be written");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Parses a `[configuration]` section out of a minimal manifest.
///
/// Goes through [`Manifest::parse`] rather than constructing the section, so a
/// test's schema is exactly what a plugin author would get from the same text —
/// including the declaration checks the parser runs.
pub fn schema(configuration: &str) -> ConfigurationSection {
    let text = format!(
        "manifest-version = 1\n\n[plugin]\nid = \"example\"\nname = \"Example\"\n\
         version = \"1.0.0\"\nruntime = \"python\"\n\
         entrypoint = {{ any = \"example:Plugin\" }}\n\n{configuration}"
    );
    Manifest::parse(&text)
        .expect("the fixture schema is valid")
        .configuration
}
