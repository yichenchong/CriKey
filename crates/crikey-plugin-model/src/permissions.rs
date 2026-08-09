//! Declared plugin permissions (spec 20).
//!
//! These values record what a manifest requests. A declaration is only a
//! request: it becomes a confinement exactly where the host performs the
//! privileged operation *for* the plugin and consults the grant first. Where
//! the operation happens inside the plugin's own process — a native plugin
//! opening a file, a Python plugin resolving a hostname — nothing here can
//! stop it, and the honest answer is to report the declaration as unhonoured
//! through [`crate::Manifest::unhonoured_declarations`] rather than to let a
//! manifest read like a sandbox.
//!
//! The UI must not claim a plugin is sandboxed where the operating system does
//! not actually enforce it.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FilesystemScope {
    None,
    UserSelected,
    PluginData,
    /// The plugin's own installed package directory.
    ///
    /// The one region the host reads on a plugin's behalf (icon and other
    /// package resources), which is why it needs a name of its own: every
    /// other scope describes files the plugin's own process opens, where the
    /// host has nothing to gate.
    Package,
    Home,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FilesystemAccess {
    Read,
    Write,
    ReadWrite,
}

impl FilesystemAccess {
    pub fn allows_read(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }

    pub fn allows_write(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct FilesystemPermission {
    pub scope: FilesystemScope,
    pub access: FilesystemAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClipboardPermission {
    #[default]
    None,
    Read,
    Write,
    ReadWrite,
}

impl ClipboardPermission {
    pub fn allows_read(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }

    pub fn allows_write(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Permissions {
    #[serde(default)]
    pub filesystem: Vec<FilesystemPermission>,
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub network_listener: bool,
    #[serde(default)]
    pub clipboard: ClipboardPermission,
    #[serde(default)]
    pub process: bool,
    #[serde(default)]
    pub window_enumeration: bool,
    #[serde(default)]
    pub window_control: bool,
    #[serde(default)]
    pub notifications: bool,
    #[serde(default)]
    pub secrets: bool,
    #[serde(default)]
    pub environment: bool,
    #[serde(default)]
    pub native_library_loading: bool,
    #[serde(default)]
    pub background_execution: bool,
}

impl Permissions {
    /// Whether the host may read a file in `scope` on this plugin's behalf.
    ///
    /// [`FilesystemScope::Package`] is granted by installation rather than by
    /// declaration: a package's own shipped files are what the plugin was
    /// installed as, the host already bounds and escape-checks that read, and
    /// requiring a declaration would silently strip the icons of every plugin
    /// written before this gate existed. An author who declares filesystem
    /// access and lists only [`FilesystemScope::None`] is taken at their word
    /// and refused, which is the one way a declaration can tighten this.
    ///
    /// Every other scope names files the plugin's own process opens directly,
    /// so a grant here would be enforcement theatre; those declarations are
    /// reported by [`crate::Manifest::unhonoured_declarations`] instead.
    pub fn allows_filesystem_read(&self, scope: FilesystemScope) -> bool {
        match scope {
            FilesystemScope::None => false,
            FilesystemScope::Package => {
                self.filesystem.is_empty()
                    || self
                        .filesystem
                        .iter()
                        .any(|entry| entry.scope != FilesystemScope::None)
            }
            wanted => self.filesystem.iter().any(|entry| {
                (entry.scope == wanted || entry.scope == FilesystemScope::Any) && entry.access.allows_read()
            }),
        }
    }

    /// The grants a legacy Keypirinha package runs under.
    ///
    /// A legacy package ships no `crikey.toml`, so there is no author
    /// declaration to consult — and "no manifest" must not quietly mean
    /// "everything permitted". This is the host's own explicit answer: the
    /// compatibility layer promises `keypirinha_util`'s execution helpers, so
    /// host-mediated process launch is granted, together with the package
    /// resource read every plugin's icons depend on. Nothing else is, and
    /// nothing here confines the CPython child, which reaches the clipboard,
    /// the network and the filesystem through its own interpreter. `crikey
    /// plugin doctor` prints this posture for every legacy entry so that it is
    /// a stated decision rather than an omission.
    pub fn legacy_compatibility_baseline() -> Self {
        Self {
            process: true,
            filesystem: vec![FilesystemPermission {
                scope: FilesystemScope::Package,
                access: FilesystemAccess::Read,
            }],
            ..Self::default()
        }
    }
}
