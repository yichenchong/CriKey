//! The launch contract between CriKey and one `crikey-wasm-host` process.
//!
//! A wasm plugin is started by the ordinary native supervisor, so everything
//! the host needs beyond the protocol endpoint arrives as environment
//! variables on a restricted environment (spec 16.6). These names and the
//! grant vocabulary are the contract; the launcher's side of it lives in
//! `crikey-app`'s `wasm_launch` module. Neither crate depends on the other —
//! the launcher must not link an interpreter — so both pin the literal strings
//! in a test and a rename breaks at least one of them.
//!
//! # Nothing is granted by default
//!
//! [`Grants`] starts empty. A capability appears only because the manifest
//! asked for it and the launcher put its token in [`ENV_GRANTS`]. An absent or
//! empty variable is a plugin with no host capabilities at all, which is the
//! posture for every module that does not say otherwise.

use std::path::PathBuf;
use std::time::Duration;

use crate::abi::Limits;

/// Path to the `.wasm` module this process must load. Required.
pub const ENV_MODULE: &str = "CRIKEY_WASM_MODULE";
/// Human-readable plugin name advertised in the handshake.
pub const ENV_PLUGIN_NAME: &str = "CRIKEY_WASM_PLUGIN_NAME";
/// Plugin release version advertised in the handshake.
pub const ENV_PLUGIN_VERSION: &str = "CRIKEY_WASM_PLUGIN_VERSION";
/// Linear-memory ceiling in bytes for the guest instance.
pub const ENV_MEMORY_BYTES: &str = "CRIKEY_WASM_MEMORY_BYTES";
/// Fuel charged per millisecond of the hard deadline.
pub const ENV_FUEL_PER_MS: &str = "CRIKEY_WASM_FUEL_PER_MS";
/// Advisory deadline handed to the guest in each suggestion request.
pub const ENV_SOFT_DEADLINE_MS: &str = "CRIKEY_WASM_SUGGEST_SOFT_DEADLINE_MS";
/// Enforced deadline: the fuel budget and the watchdog both derive from it.
pub const ENV_HARD_DEADLINE_MS: &str = "CRIKEY_WASM_SUGGEST_HARD_DEADLINE_MS";
/// Maximum number of items accepted from one guest batch.
pub const ENV_MAX_ITEMS: &str = "CRIKEY_WASM_MAX_ITEMS";
/// Maximum size in bytes of one guest response blob.
pub const ENV_MAX_RESPONSE_BYTES: &str = "CRIKEY_WASM_MAX_RESPONSE_BYTES";
/// Comma-separated granted capability tokens; absent means none.
pub const ENV_GRANTS: &str = "CRIKEY_WASM_GRANTS";

/// Grant token for the confined `crikey::read_file` import.
pub const GRANT_FILESYSTEM_READ: &str = "filesystem-read";
/// Grant token for the `crikey::env_get` import.
pub const GRANT_ENVIRONMENT: &str = "environment";

/// Default guest linear-memory ceiling.
pub const DEFAULT_MEMORY_BYTES: usize = 64 * 1024 * 1024;
/// Hard ceiling on the guest linear-memory ceiling, whatever is configured.
pub const MAX_MEMORY_BYTES: usize = 512 * 1024 * 1024;
/// Default fuel charged per millisecond of hard deadline.
///
/// Fuel counts executed instructions, not time. This constant is the
/// calibration between the two and is deliberately generous: undercharging
/// would cut short a plugin that is merely slow, and the watchdog, not fuel,
/// is what guarantees the wall clock. See ADR-0014.
pub const DEFAULT_FUEL_PER_MS: u64 = 5_000_000;
/// Fuel floor, so a tiny deadline still admits a call that does real work.
pub const MIN_FUEL_PER_CALL: u64 = 1_000_000;
/// Fuel ceiling, so a large deadline cannot authorise an unbounded run.
pub const MAX_FUEL_PER_CALL: u64 = 200_000_000_000;
/// Default advisory deadline, matching the manifest model's own default.
pub const DEFAULT_SOFT_DEADLINE_MS: u64 = 50;
/// Default enforced deadline, matching the manifest model's own default.
pub const DEFAULT_HARD_DEADLINE_MS: u64 = 500;
/// Multiple of the hard deadline the watchdog allows before it aborts.
///
/// Fuel is a proportional bound, so the watchdog must not fire on a call that
/// fuel would legitimately have let finish. It is a backstop against a fuel
/// calibration that is too generous, not a second deadline.
pub const WATCHDOG_SLACK: u32 = 4;
/// Floor on the watchdog window, so a small deadline does not produce a window
/// shorter than process scheduling noise.
pub const WATCHDOG_FLOOR: Duration = Duration::from_secs(2);

/// Capabilities the manifest granted this module.
///
/// A field that is `false` means the corresponding host import is not defined
/// in the linker at all, so a module that imports it fails to instantiate with
/// a named refusal. There is no runtime "permission denied" return value to
/// probe, because there is no function to call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Grants {
    /// Read files under the package directory through `crikey::read_file`.
    pub filesystem_read: bool,
    /// Read process environment variables through `crikey::env_get`.
    pub environment: bool,
}

impl Grants {
    /// Parses the comma-separated token list from [`ENV_GRANTS`].
    ///
    /// An unrecognised token is refused rather than ignored: the launcher and
    /// this host ship together, so a token this build does not know means the
    /// two disagree about what was granted, and guessing in either direction
    /// is wrong.
    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        let mut grants = Self::default();
        for token in value.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match token {
                GRANT_FILESYSTEM_READ => grants.filesystem_read = true,
                GRANT_ENVIRONMENT => grants.environment = true,
                other => return Err(ConfigError::UnknownGrant(other.to_owned())),
            }
        }
        Ok(grants)
    }
}

/// Why a `crikey-wasm-host` process refused to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A required variable was absent or empty.
    Missing(&'static str),
    /// A numeric variable did not parse, or parsed to zero where zero is
    /// meaningless.
    NotANumber { variable: &'static str, value: String },
    /// [`ENV_GRANTS`] named a capability this build does not define.
    UnknownGrant(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(variable) => write!(formatter, "{variable} is not set"),
            Self::NotANumber { variable, value } => {
                write!(formatter, "{variable} is not a positive integer: {value:?}")
            }
            Self::UnknownGrant(token) => write!(
                formatter,
                "{ENV_GRANTS} names capability {token:?}, which this host does not define"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Everything one `crikey-wasm-host` process needs beyond its protocol
/// endpoint.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// The module to load.
    pub module: PathBuf,
    /// Directory a granted `crikey::read_file` is confined to.
    pub package_root: PathBuf,
    /// Human-readable plugin name.
    pub plugin_name: String,
    /// Plugin release version.
    pub plugin_version: String,
    /// Guest linear-memory ceiling in bytes.
    pub memory_bytes: usize,
    /// Fuel charged per millisecond of [`HostConfig::hard_deadline_ms`].
    pub fuel_per_ms: u64,
    /// Advisory deadline handed to the guest.
    pub soft_deadline_ms: u64,
    /// Enforced deadline behind fuel and the watchdog.
    pub hard_deadline_ms: u64,
    /// Decoding ceilings for guest responses.
    pub limits: Limits,
    /// Capabilities the manifest granted.
    pub grants: Grants,
}

impl HostConfig {
    /// Fuel budget for one guest call.
    pub fn fuel_per_call(&self) -> u64 {
        self.hard_deadline_ms
            .saturating_mul(self.fuel_per_ms)
            .clamp(MIN_FUEL_PER_CALL, MAX_FUEL_PER_CALL)
    }

    /// Wall-clock window after which the watchdog aborts the process.
    pub fn watchdog_window(&self) -> Duration {
        Duration::from_millis(self.hard_deadline_ms.saturating_mul(u64::from(WATCHDOG_SLACK)))
            .max(WATCHDOG_FLOOR)
    }

    /// Reads the configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// [`Self::from_env`] against an injected lookup, so the parsing rules are
    /// testable without mutating the process environment.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let module = lookup(ENV_MODULE)
            .filter(|value| !value.trim().is_empty())
            .ok_or(ConfigError::Missing(ENV_MODULE))?;
        let module = PathBuf::from(module);
        // The supervisor sets the child's working directory to the package
        // directory (crikey-app's provider), so that is the confinement root
        // for a granted read. Falling back to the module's own directory keeps
        // a hand-launched host confined rather than unconfined.
        let package_root = std::env::current_dir()
            .ok()
            .or_else(|| module.parent().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));

        let number = |variable: &'static str, default: u64| -> Result<u64, ConfigError> {
            match lookup(variable) {
                None => Ok(default),
                Some(value) => value
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .filter(|parsed| *parsed > 0)
                    .ok_or(ConfigError::NotANumber { variable, value }),
            }
        };

        let memory_bytes = usize::try_from(number(ENV_MEMORY_BYTES, DEFAULT_MEMORY_BYTES as u64)?)
            .unwrap_or(MAX_MEMORY_BYTES)
            .min(MAX_MEMORY_BYTES);
        let fuel_per_ms = number(ENV_FUEL_PER_MS, DEFAULT_FUEL_PER_MS)?;
        let hard_deadline_ms = number(ENV_HARD_DEADLINE_MS, DEFAULT_HARD_DEADLINE_MS)?;
        let soft_deadline_ms = number(ENV_SOFT_DEADLINE_MS, DEFAULT_SOFT_DEADLINE_MS)?.min(hard_deadline_ms);
        let defaults = Limits::default();
        let max_items = usize::try_from(number(ENV_MAX_ITEMS, defaults.max_items as u64)?)
            .unwrap_or(Limits::MAX_ITEMS)
            .min(Limits::MAX_ITEMS);
        let max_blob_bytes = usize::try_from(number(ENV_MAX_RESPONSE_BYTES, defaults.max_blob_bytes as u64)?)
            .unwrap_or(Limits::MAX_BLOB_BYTES)
            .min(Limits::MAX_BLOB_BYTES);
        let grants = match lookup(ENV_GRANTS) {
            Some(value) => Grants::parse(&value)?,
            None => Grants::default(),
        };

        Ok(Self {
            module,
            package_root,
            plugin_name: lookup(ENV_PLUGIN_NAME).unwrap_or_else(|| "wasm plugin".to_owned()),
            plugin_version: lookup(ENV_PLUGIN_VERSION).unwrap_or_else(|| "0.0.0".to_owned()),
            memory_bytes,
            fuel_per_ms,
            soft_deadline_ms,
            hard_deadline_ms,
            limits: Limits {
                max_items,
                max_blob_bytes,
                ..defaults
            },
            grants,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    /// The launcher writes these literals from its own copy of the contract in
    /// `crikey-app`'s `wasm_launch` module. Neither crate depends on the
    /// other, so this test is the thing that makes a rename visible.
    #[test]
    fn the_launch_contract_names_are_pinned() {
        assert_eq!(ENV_MODULE, "CRIKEY_WASM_MODULE");
        assert_eq!(ENV_PLUGIN_NAME, "CRIKEY_WASM_PLUGIN_NAME");
        assert_eq!(ENV_PLUGIN_VERSION, "CRIKEY_WASM_PLUGIN_VERSION");
        assert_eq!(ENV_MEMORY_BYTES, "CRIKEY_WASM_MEMORY_BYTES");
        assert_eq!(ENV_FUEL_PER_MS, "CRIKEY_WASM_FUEL_PER_MS");
        assert_eq!(ENV_SOFT_DEADLINE_MS, "CRIKEY_WASM_SUGGEST_SOFT_DEADLINE_MS");
        assert_eq!(ENV_HARD_DEADLINE_MS, "CRIKEY_WASM_SUGGEST_HARD_DEADLINE_MS");
        assert_eq!(ENV_MAX_ITEMS, "CRIKEY_WASM_MAX_ITEMS");
        assert_eq!(ENV_MAX_RESPONSE_BYTES, "CRIKEY_WASM_MAX_RESPONSE_BYTES");
        assert_eq!(ENV_GRANTS, "CRIKEY_WASM_GRANTS");
        assert_eq!(GRANT_FILESYSTEM_READ, "filesystem-read");
        assert_eq!(GRANT_ENVIRONMENT, "environment");
    }

    #[test]
    fn a_module_path_is_required() {
        assert_eq!(
            HostConfig::from_lookup(lookup(&[])).expect_err("no module"),
            ConfigError::Missing(ENV_MODULE)
        );
        assert_eq!(
            HostConfig::from_lookup(lookup(&[(ENV_MODULE, "   ")])).expect_err("blank module"),
            ConfigError::Missing(ENV_MODULE)
        );
    }

    #[test]
    fn absent_configuration_grants_nothing() {
        let config = HostConfig::from_lookup(lookup(&[(ENV_MODULE, "p.wasm")])).expect("defaults");
        assert_eq!(config.grants, Grants::default());
        assert!(!config.grants.filesystem_read);
        assert!(!config.grants.environment);
    }

    #[test]
    fn an_unknown_grant_token_refuses_the_launch() {
        let error = HostConfig::from_lookup(lookup(&[
            (ENV_MODULE, "p.wasm"),
            (ENV_GRANTS, "filesystem-read,teleport"),
        ]))
        .expect_err("unknown grant");
        assert_eq!(error, ConfigError::UnknownGrant("teleport".to_owned()));
        assert!(error.to_string().contains("teleport"));
    }

    #[test]
    fn known_grant_tokens_parse_in_any_order_and_tolerate_spacing() {
        let grants = Grants::parse(" environment , filesystem-read ,").expect("parse");
        assert!(grants.filesystem_read && grants.environment);
    }

    #[test]
    fn a_non_numeric_limit_refuses_the_launch_rather_than_falling_back() {
        let error = HostConfig::from_lookup(lookup(&[(ENV_MODULE, "p.wasm"), (ENV_MAX_ITEMS, "lots")]))
            .expect_err("bad number");
        assert_eq!(
            error,
            ConfigError::NotANumber {
                variable: ENV_MAX_ITEMS,
                value: "lots".to_owned()
            }
        );
    }

    #[test]
    fn configured_ceilings_are_clamped_to_the_hard_maximums() {
        let config = HostConfig::from_lookup(lookup(&[
            (ENV_MODULE, "p.wasm"),
            (ENV_MEMORY_BYTES, "999999999999"),
            (ENV_MAX_ITEMS, "999999999"),
            (ENV_MAX_RESPONSE_BYTES, "999999999999"),
        ]))
        .expect("clamped");
        assert_eq!(config.memory_bytes, MAX_MEMORY_BYTES);
        assert_eq!(config.limits.max_items, Limits::MAX_ITEMS);
        assert_eq!(config.limits.max_blob_bytes, Limits::MAX_BLOB_BYTES);
    }

    #[test]
    fn the_advisory_deadline_never_exceeds_the_enforced_one() {
        let config = HostConfig::from_lookup(lookup(&[
            (ENV_MODULE, "p.wasm"),
            (ENV_SOFT_DEADLINE_MS, "5000"),
            (ENV_HARD_DEADLINE_MS, "300"),
        ]))
        .expect("clamped");
        assert_eq!(config.soft_deadline_ms, 300);
        assert_eq!(config.hard_deadline_ms, 300);
    }

    #[test]
    fn the_fuel_budget_tracks_the_deadline_between_its_floor_and_ceiling() {
        let build = |hard: &str, per_ms: &str| {
            HostConfig::from_lookup(lookup(&[
                (ENV_MODULE, "p.wasm"),
                (ENV_HARD_DEADLINE_MS, hard),
                (ENV_FUEL_PER_MS, per_ms),
            ]))
            .expect("config")
        };
        assert_eq!(build("100", "1000").fuel_per_call(), MIN_FUEL_PER_CALL);
        assert_eq!(build("100", "1000000").fuel_per_call(), 100_000_000);
        assert_eq!(build("100000", "999999999").fuel_per_call(), MAX_FUEL_PER_CALL);
    }

    #[test]
    fn the_watchdog_window_never_falls_below_its_floor() {
        let build = |hard: &str| {
            HostConfig::from_lookup(lookup(&[(ENV_MODULE, "p.wasm"), (ENV_HARD_DEADLINE_MS, hard)]))
                .expect("config")
                .watchdog_window()
        };
        assert_eq!(build("50"), WATCHDOG_FLOOR);
        assert_eq!(build("5000"), Duration::from_millis(20_000));
    }
}
