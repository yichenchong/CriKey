//! Declared plugin permissions (spec 20).
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
