//! The `crikey plugin` command family (spec 28; 21.2, 23, 26).
//!
//! Ten subcommands over two sources of truth. Seven work on the local
//! inventory — `list`, `install`, `remove`, `enable`, `disable`, `doctor` and
//! `scheduling-profile` — and three work on the configured plugin indexes:
//! `search`, `show` and `index update` (spec 2.2). `install` spans both: it
//! takes a path or a URL as it always has, and resolves anything shaped like a
//! bare plugin id through the index.
//!
//! # One id, and it is the namespaced one
//!
//! Every command names a plugin by the id the query pipeline uses —
//! `legacy.foo`, `modern.foo`, `native.foo` (spec 10.2) — because that is the id
//! every diagnostic, journal entry and config key already carries, and because a
//! bare `foo` does not identify one plugin: two runtimes may ship the same bare
//! id. A bare id is still accepted for typing convenience, and is *resolved*
//! against the inventory rather than guessed: if it matches two plugins the
//! command names both and refuses.
//!
//! # Where the inventory comes from
//!
//! Two sources, deliberately both: plugins installed by `crikey plugin install`
//! (under the standard data directory), and plugins discovered on the live
//! `CRIKEY_*_ROOTS` paths that `crikey run` scans. A command that could only see
//! installed plugins would be unable to disable or diagnose the ones the
//! launcher is actually loading, which is the majority of them during
//! development.
//!
//! # The output contract
//!
//! Whitespace-separated `key=value` tokens with uppercase percent-encoded
//! values, byte-identical to `crikey package` and the `crikey dev` commands, so
//! `cut`, `grep` and `sort` are a complete reader. Plugin ids, package paths and
//! diagnostic prose are all third-party strings that may hold a space, an `=` or
//! a `%`; every value is therefore encoded rather than quoted.
//!
//! # Three statuses
//!
//! [`EX_OK`] for an operation that completed and found nothing wrong,
//! [`EX_INVALID`] for one that completed and reached a bad verdict — an unknown
//! plugin, a refused install, a degraded `doctor` — and [`EX_USAGE`] for an
//! argument list this module could not parse. A script that cannot tell "this
//! plugin is broken" from "you typed the subcommand wrong" reports both as red.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crikey_config::ConfigStore;
use crikey_core::PluginId;
use crikey_legacy_compat::{
    discover_interpreter, LegacyPackage, PackageLoader, Severity, PACKAGE_ARCHIVE_EXTENSION,
};
use crikey_package_manager::{
    index_max_age, index_urls, search as index_search, Freshness, IndexEntry, IndexOutcome, IndexSnapshot,
    InstallSource, InstalledPlugin, PluginIndexClient, PluginInstaller, SignaturePolicy,
    KEY_INDEX_MAX_AGE_SECONDS, KEY_INDEX_URLS,
};
use crikey_platform::{PluginKind, StandardDirectories};
use crikey_plugin_model::{ConcurrencySection, Manifest, Runtime, SchedulingProfile};
use crikey_plugin_supervisor::{shared_budget_from_section, BudgetKind};
use crikey_python_host::RuntimeProfile;

use crate::legacy_commands::{compatibility_diagnostics, scan_windows_only_dependency, LegacyObservations};
use crate::package_commands::signature_policy;

/// A completed operation that found nothing wrong.
const EX_OK: u8 = 0;
/// A completed operation that reached a bad verdict.
const EX_INVALID: u8 = 1;
/// An argument list this module could not parse.
const EX_USAGE: u8 = 64;

/// Runs the subcommand following `crikey plugin`.
pub(crate) fn run(args: &[String]) -> ExitCode {
    let Some(command) = args.first().map(String::as_str) else {
        return refuse(
            "`plugin` needs list, search, show, index, install, remove, enable, disable, doctor or \
             scheduling-profile",
        );
    };

    if command == "-h" || command == "--help" {
        if args.len() == 1 {
            print!("{}", plugin_help());
            return ExitCode::from(EX_OK);
        }
        return refuse("`plugin --help` takes no additional arguments");
    }

    let rest = &args[1..];
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        if let Err(message) = validate_help_args(command, rest) {
            return refuse(&message);
        }
        print_help(command);
        return ExitCode::from(EX_OK);
    }

    match command {
        "list" => list(rest),
        "search" => search(rest),
        "show" => show(rest),
        "index" => index_command(rest),
        "install" => install(rest),
        "remove" => remove(rest),
        "enable" => set_enabled(rest, true),
        "disable" => set_enabled(rest, false),
        "doctor" => doctor(rest),
        "scheduling-profile" => scheduling_profile(rest),
        other => refuse(&format!("unknown plugin subcommand `{other}`")),
    }
}

/// Whether `-h`/`--help` was asked for in an argument list this subcommand could
/// have accepted.
///
/// Help is honoured beside the positional arguments the subcommand takes, and
/// refused beside anything else. An unknown option silently swallowed by `--help`
/// is how a typo becomes a command that appears to work and does nothing.
fn validate_help_args(command: &str, args: &[String]) -> Result<(), String> {
    // `install` is the one subcommand carrying an option, so `--help` beside it
    // must recognise that option rather than report a typo — while every option
    // that is not it is still refused below.
    let args = if command == "install" {
        take_unsigned_policy(args)?.1
    } else {
        args.to_vec()
    };
    let positionals = args
        .iter()
        .filter(|argument| *argument != "-h" && *argument != "--help")
        .count();
    let allowed = match command {
        "list" => 0,
        "install" | "remove" | "enable" | "disable" => 1,
        "doctor" => 1,
        "search" | "show" | "index" => 1,
        "scheduling-profile" => 2,
        other => return Err(format!("unknown plugin subcommand `{other}`")),
    };
    if let Some(option) = args
        .iter()
        .find(|argument| argument.starts_with('-') && *argument != "-h" && *argument != "--help")
    {
        return Err(format!("`plugin {command}` does not understand `{option}`"));
    }
    if positionals > allowed {
        return Err(format!(
            "`plugin {command}` takes at most {allowed} argument(s), got {positionals}"
        ));
    }
    Ok(())
}

/// The positional arguments of a subcommand, refusing options and empty values.
fn positional(command: &str, args: &[String], maximum: usize) -> Result<Vec<String>, String> {
    if let Some(option) = args.iter().find(|argument| argument.starts_with('-')) {
        return Err(format!("`plugin {command}` does not understand `{option}`"));
    }
    if args.len() > maximum {
        return Err(format!(
            "`plugin {command}` takes at most {maximum} argument(s), got {}",
            args.len()
        ));
    }
    if args.iter().any(String::is_empty) {
        return Err(format!("`plugin {command}` was given an empty argument"));
    }
    Ok(args.to_vec())
}

// ---------------------------------------------------------------------------
// The inventory
// ---------------------------------------------------------------------------

/// Where a plugin was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Installed under the standard data directory by `crikey plugin install`.
    Installed,
    /// Discovered on a live `CRIKEY_*_ROOTS` path, as `crikey run` scans them.
    Root,
}

impl Origin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Root => "root",
        }
    }
}

/// One plugin as the command line sees it.
///
/// The manifest is retained rather than reduced to the fields `list` prints,
/// because `doctor` needs the `[concurrency]` section and the declared profile
/// out of the same parse. Re-reading `crikey.toml` per subcommand would let two
/// lines of one report describe two different versions of the file.
#[derive(Debug)]
struct PluginEntry {
    /// Namespaced canonical id: `<kind>.<id>`.
    plugin: PluginId,
    /// Raw manifest or package id, which is what the installer keys on.
    id: String,
    kind: PluginKind,
    /// Declared version, or `-` for a format that carries none.
    version: String,
    root: PathBuf,
    origin: Origin,
    /// `None` for a legacy package, which has no manifest, and for a manifest
    /// that did not parse — [`Self::problem`] then says why.
    manifest: Option<Manifest>,
    /// Why this plugin cannot load, if it cannot.
    problem: Option<String>,
    /// A native package whose binary is unsigned (spec 23.4).
    unsigned_binary: bool,
}

impl PluginEntry {
    /// The profile this plugin will actually run under, and where that came from.
    fn profile(&self, config: &ConfigStore) -> (SchedulingProfile, &'static str) {
        if let Some(profile) = config.scheduling_profile(&self.plugin) {
            return (profile, "config");
        }
        if let Some(profile) = self
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.plugin.scheduling_profile)
        {
            return (profile, "manifest");
        }
        (default_profile(self.kind), "default")
    }

    /// The declared concurrency budgets, or the host defaults for a format that
    /// declares none.
    fn concurrency(&self) -> ConcurrencySection {
        self.manifest
            .as_ref()
            .map(|manifest| manifest.concurrency.clone())
            .unwrap_or_default()
    }
}

/// The profile a plugin of `kind` runs under when nothing overrides it.
///
/// Legacy packages are `legacy-strict` because spec 7.2 makes that the only
/// conforming default for Keypirinha-compatible code; everything else is
/// `modern`.
fn default_profile(kind: PluginKind) -> SchedulingProfile {
    match kind {
        PluginKind::Legacy => SchedulingProfile::LegacyStrict,
        PluginKind::Modern | PluginKind::Native => SchedulingProfile::Modern,
    }
}

/// Every plugin this host can see, sorted by namespaced id.
#[derive(Debug, Default)]
struct Inventory {
    entries: Vec<PluginEntry>,
    /// Roots and packages that could not be read at all, so `list` and `doctor`
    /// report them instead of a shorter list that looks complete.
    unreadable: Vec<String>,
}

impl Inventory {
    /// Collects installed plugins and the plugins on the live discovery roots.
    fn collect(directories: &StandardDirectories) -> Self {
        let mut inventory = Self::default();

        match PluginInstaller::new(directories).list() {
            Ok(installed) => {
                for plugin in installed {
                    inventory.push_installed(plugin);
                }
            }
            Err(error) => inventory
                .unreadable
                .push(format!("installed plugins could not be listed: {error}")),
        }

        for root in crate::legacy_package_roots() {
            inventory.scan_legacy_root(&root);
        }
        for root in crate::modern_plugin_roots() {
            inventory.scan_manifest_root(&root, PluginKind::Modern);
        }
        for root in crate::native_plugin_roots() {
            inventory.scan_manifest_root(&root, PluginKind::Native);
        }

        // Sorted by id, then installed copies before discovery-root ones, then
        // path. Deterministic so two runs diff, and installed-first so a command
        // that acts on one copy of a shadowed plugin acts on the copy CriKey
        // owns.
        inventory.entries.sort_by(|left, right| {
            left.plugin
                .0
                .cmp(&right.plugin.0)
                .then_with(|| (left.origin != Origin::Installed).cmp(&(right.origin != Origin::Installed)))
                .then_with(|| left.root.cmp(&right.root))
        });
        inventory
    }

    fn push_installed(&mut self, installed: InstalledPlugin) {
        let mut entry = PluginEntry {
            plugin: namespaced(installed.kind, &installed.id),
            id: installed.id,
            kind: installed.kind,
            version: display_version(&installed.version),
            root: installed.root,
            origin: Origin::Installed,
            manifest: None,
            problem: None,
            unsigned_binary: installed.unsigned_binary,
        };
        if entry.kind != PluginKind::Legacy {
            match read_manifest(&entry.root) {
                Ok(manifest) => {
                    entry.version = display_version(&manifest.plugin.version);
                    entry.manifest = Some(manifest);
                }
                Err(problem) => entry.problem = Some(problem),
            }
        }
        self.entries.push(entry);
    }

    /// Discovers `<root>/<id>/crikey.toml` packages, exactly as the modern and
    /// native providers do.
    fn scan_manifest_root(&mut self, root: &Path, kind: PluginKind) {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) => {
                self.unreadable
                    .push(format!("cannot scan `{}`: {error}", root.display()));
                return;
            }
        };
        let mut directories: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.join("crikey.toml").is_file())
            .collect();
        directories.sort();
        for directory in directories {
            match read_manifest(&directory) {
                Ok(manifest) => {
                    // Each provider owns exactly one runtime, so a package of
                    // another runtime sitting in this root is that other
                    // provider's, not a defect in this one.
                    if manifest_kind(manifest.plugin.runtime) != Some(kind) {
                        continue;
                    }
                    self.entries.push(PluginEntry {
                        plugin: namespaced(kind, &manifest.plugin.id),
                        id: manifest.plugin.id.clone(),
                        kind,
                        version: display_version(&manifest.plugin.version),
                        root: directory,
                        origin: Origin::Root,
                        manifest: Some(manifest),
                        problem: None,
                        unsigned_binary: false,
                    });
                }
                Err(problem) => {
                    let id = directory
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| directory.display().to_string());
                    self.entries.push(PluginEntry {
                        plugin: namespaced(kind, &id),
                        id,
                        kind,
                        version: "-".to_owned(),
                        root: directory,
                        origin: Origin::Root,
                        manifest: None,
                        problem: Some(problem),
                        unsigned_binary: false,
                    });
                }
            }
        }
    }

    /// Discovers legacy packages under `root` through the Legacy Compatibility
    /// Layer's own loader.
    ///
    /// The loader is what establishes a package's id and entry point, and it is
    /// what refuses a hostile archive; a second scanner here would be a second
    /// place for a path traversal to be got wrong. Extraction goes to the same
    /// per-user cache `crikey run` uses, so listing plugins does not re-extract
    /// what the launcher already has.
    fn scan_legacy_root(&mut self, root: &Path) {
        let cache_root = match crate::legacy_cache_root() {
            Ok(cache_root) => cache_root,
            Err(message) => {
                self.unreadable
                    .push(format!("legacy packages could not be read: {message}"));
                return;
            }
        };
        let loader = PackageLoader::new(cache_root);
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) => {
                self.unreadable
                    .push(format!("cannot scan `{}`: {error}", root.display()));
                return;
            }
        };
        let mut candidates: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_dir()
                    || path
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case(PACKAGE_ARCHIVE_EXTENSION))
            })
            .collect();
        candidates.sort();
        for candidate in candidates {
            match loader.load(&candidate) {
                Ok(package) => self.entries.push(PluginEntry {
                    plugin: namespaced(PluginKind::Legacy, package.id.as_str()),
                    id: package.id.as_str().to_owned(),
                    kind: PluginKind::Legacy,
                    // The `.keypirinha-package` format carries no version, and
                    // inventing one would make a migrated package look released.
                    version: "-".to_owned(),
                    root: candidate,
                    origin: Origin::Root,
                    manifest: None,
                    problem: None,
                    unsigned_binary: false,
                }),
                Err(error) => self.unreadable.push(format!(
                    "cannot load legacy package `{}`: {error}",
                    candidate.display()
                )),
            }
        }
    }

    /// Every copy of the plugin an operator's argument names, installed copies
    /// first.
    ///
    /// An exact namespaced id wins over a bare one. A bare id that matches two
    /// *different* namespaced ids is refused by naming both: guessing whether
    /// `notes` meant `legacy.notes` or `modern.notes` would silently disable the
    /// wrong plugin and the operator would have no way to tell.
    ///
    /// One namespaced id may still have several copies — the same plugin
    /// installed and also sitting on a discovery root. That is not ambiguity
    /// about *which plugin* was named, so it is not refused; it is shadowing,
    /// which `doctor` reports and `remove` resolves by acting on the installed
    /// copy, the only one it owns.
    fn resolve(&self, wanted: &str) -> Result<Vec<&PluginEntry>, String> {
        let named: Vec<&PluginEntry> = self
            .entries
            .iter()
            .filter(|entry| entry.plugin.0 == wanted)
            .collect();
        if !named.is_empty() {
            return Ok(named);
        }
        // `entries` is already sorted installed-first within one id, so both
        // branches hand back the copy CriKey owns at index 0.
        let bare: Vec<&PluginEntry> = self.entries.iter().filter(|entry| entry.id == wanted).collect();
        let mut distinct: Vec<&str> = bare.iter().map(|entry| entry.plugin.0.as_str()).collect();
        distinct.dedup();
        match distinct.as_slice() {
            [_] => Ok(bare),
            [] => Err(format!(
                "no plugin `{wanted}` is installed or on a discovery root; known plugins: {}",
                self.known()
            )),
            several => Err(format!(
                "`{wanted}` is ambiguous: {}. Name one of them exactly.",
                several.join(", ")
            )),
        }
    }

    /// Every known plugin as a comma-separated list, for a refusal message.
    fn known(&self) -> String {
        if self.entries.is_empty() {
            return "none".to_owned();
        }
        self.entries
            .iter()
            .map(|entry| entry.plugin.0.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The pipeline id for a plugin of `kind` (spec 10.2 namespacing).
fn namespaced(kind: PluginKind, id: &str) -> PluginId {
    PluginId(format!("{}.{id}", kind.directory_name()))
}

/// The plugin kind a declared manifest runtime belongs to.
///
/// `c-abi` shares the `native` kind and the `native` plugin root: a restricted
/// C-ABI package is a native package whose entrypoint happens to be a shared
/// library rather than a program, and `crikey-cabi-host` is the executable
/// CriKey actually supervises (ADR-0015).
///
/// `wasm` shares them for the same reason: `crikey-wasm-host` is the supervised
/// executable and the module ships inside a package of the same shape
/// (ADR-0014). A `builtin` has no installable kind at all, because it is
/// compiled into the launcher and cannot appear under a plugin root.
fn manifest_kind(runtime: Runtime) -> Option<PluginKind> {
    match runtime {
        Runtime::LegacyPython => Some(PluginKind::Legacy),
        Runtime::Python => Some(PluginKind::Modern),
        Runtime::Native | Runtime::CAbi | Runtime::Wasm => Some(PluginKind::Native),
        Runtime::Builtin => None,
    }
}

/// A version for display, turning the empty string a version-less format yields
/// into an explicit `-` rather than a blank column that reads as a parse bug.
fn display_version(version: &str) -> String {
    if version.trim().is_empty() {
        "-".to_owned()
    } else {
        version.to_owned()
    }
}

fn read_manifest(root: &Path) -> Result<Manifest, String> {
    let path = root.join("crikey.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read `{}`: {error}", path.display()))?;
    Manifest::parse(&text).map_err(|error| format!("invalid `{}`: {error}", path.display()))
}

/// The standard directories and the layered config, or a refusal.
///
/// Both or neither: every subcommand needs the directories to find plugins and
/// the store to know which are enabled, and a command that reported an enabled
/// state it had not actually read would be worse than one that refused.
fn open_host() -> Result<(StandardDirectories, ConfigStore), String> {
    let directories = StandardDirectories::for_process()
        .map_err(|error| format!("cannot resolve the standard directories: {error}"))?;
    let config =
        ConfigStore::load(&directories).map_err(|error| format!("cannot load the configuration: {error}"))?;
    Ok((directories, config))
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn list(args: &[String]) -> ExitCode {
    if let Err(message) = positional("list", args, 0) {
        return refuse(&message);
    }
    let (directories, config) = match open_host() {
        Ok(host) => host,
        Err(message) => return fail(&message),
    };
    let inventory = Inventory::collect(&directories);

    field("plugins", &inventory.entries.len().to_string());
    for (index, entry) in inventory.entries.iter().enumerate() {
        let (profile, source) = entry.profile(&config);
        println!(
            "plugin={index} id={} raw={} version={} kind={} enabled={} scheduling_profile={} \
             profile_source={} origin={} root={} status={}",
            encode(&entry.plugin.0),
            encode(&entry.id),
            encode(&entry.version),
            entry.kind.directory_name(),
            config.plugin_enabled(&entry.plugin),
            profile_name(profile),
            source,
            entry.origin.as_str(),
            encode(&entry.root.display().to_string()),
            encode(entry.problem.as_deref().unwrap_or("ok")),
        );
    }
    report_unreadable(&inventory);
    ExitCode::from(EX_OK)
}

/// Prints every root or package the inventory could not read.
///
/// Never silent: a shorter list that looks complete is how an operator concludes
/// their plugin was uninstalled when the directory was merely unreadable.
fn report_unreadable(inventory: &Inventory) {
    field("unreadable", &inventory.unreadable.len().to_string());
    for (index, problem) in inventory.unreadable.iter().enumerate() {
        println!("unreadable={index} reason={}", encode(problem));
    }
}

// ---------------------------------------------------------------------------
// install and remove
// ---------------------------------------------------------------------------

/// Installs a plugin, under the operator's provenance policy (ADR-0012).
fn install(args: &[String]) -> ExitCode {
    let (unsigned, rest) = match take_unsigned_policy(args) {
        Ok(split) => split,
        Err(message) => return refuse(&message),
    };
    let arguments = match positional("install", &rest, 1) {
        Ok(arguments) => arguments,
        Err(message) => return refuse(&message),
    };
    let Some(wanted) = arguments.first() else {
        return refuse(
            "`plugin install` needs a directory, archive, URL, `.keypirinha-package` or an indexed id",
        );
    };
    let (directories, config) = match open_host() {
        Ok(host) => host,
        Err(message) => return fail(&message),
    };
    // A path or a URL installs exactly as it always did. Only an argument that
    // is neither — and is shaped like a plugin id rather than a mistyped path —
    // is resolved through the configured index, so adding the index cannot
    // change what an existing invocation means.
    match InstallSource::detect(wanted) {
        Ok(source) => {
            let policy = match signature_policy(unsigned.as_deref()) {
                Ok(policy) => policy,
                Err(message) => return fail(&message),
            };
            install_source(&directories, &source, policy)
        }
        Err(unavailable) => {
            if !is_plugin_id(wanted) {
                return fail(&format!("cannot install `{wanted}`: {unavailable}"));
            }
            // Refused rather than accepted and ignored: an indexed install
            // answers the provenance question with the index's own signature,
            // so there is no unsigned-package decision left for the flag to
            // make, and a flag that silently does nothing is how an operator
            // concludes a policy is in force when it is not.
            if let Some(given) = unsigned.as_deref() {
                return fail(&format!(
                    "`--unsigned-policy {given}` applies to an archive or a URL; `{wanted}` is an \
                     indexed id, whose provenance comes from the trusted index signature that pins \
                     its package digest"
                ));
            }
            install_from_index(&directories, &config, wanted)
        }
    }
}

/// Splits `--unsigned-policy VALUE` out of `plugin install`'s arguments.
///
/// A hand-rolled split rather than a flag table, because this is the family's
/// only option and everything it does not consume still reaches [`positional`],
/// which refuses options — so a mistyped flag cannot be swallowed here.
fn take_unsigned_policy(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    const FLAG: &str = "--unsigned-policy";
    let mut policy: Option<String> = None;
    let mut rest = Vec::new();
    let mut position = 0;
    while position < args.len() {
        let argument = args[position].as_str();
        let value = if argument == FLAG {
            let value = args
                .get(position + 1)
                .ok_or_else(|| format!("`plugin install` needs a value after `{FLAG}`"))?;
            position += 2;
            value.as_str()
        } else if let Some(value) = argument
            .strip_prefix(FLAG)
            .and_then(|rest| rest.strip_prefix('='))
        {
            position += 1;
            value
        } else {
            rest.push(argument.to_owned());
            position += 1;
            continue;
        };
        if value.is_empty() {
            return Err(format!("`plugin install {FLAG}` was given an empty value"));
        }
        if policy.is_some() {
            return Err(format!("`plugin install` was given `{FLAG}` twice"));
        }
        policy = Some(value.to_owned());
    }
    Ok((policy, rest))
}

/// Runs the one install pipeline, whatever produced `source`.
///
/// An indexed install differs from a URL install in exactly one place — the
/// digest check that happens before this is called — and shares the lock, the
/// staging, the validation and the rollback with every other install, because a
/// second installation path is a second set of atomicity bugs.
///
/// `policy` decides what happens to a native archive carrying no
/// `<package>.sig`. It reaches the installer here and nowhere else, which is
/// what makes the configured `packages.unsigned-policy` mean anything at
/// install time rather than only under `crikey package verify`.
fn install_source(
    directories: &StandardDirectories,
    source: &InstallSource,
    policy: SignaturePolicy,
) -> ExitCode {
    let mut installer = PluginInstaller::new(directories).with_signature_policy(policy);
    // Nothing to stop: this process is not the launcher, and a live launcher is
    // refused by the installer's exclusive lock rather than asked to quit.
    match installer.install(source, &mut |_| Ok(())) {
        Ok(installed) => {
            print_installed(&installed, "installed");
            ExitCode::from(EX_OK)
        }
        Err(error) => fail(&format!("installation failed: {error}")),
    }
}

/// Whether `value` is shaped like a plugin id rather than a path.
fn is_plugin_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn remove(args: &[String]) -> ExitCode {
    let arguments = match positional("remove", args, 1) {
        Ok(arguments) => arguments,
        Err(message) => return refuse(&message),
    };
    let Some(wanted) = arguments.first() else {
        return refuse("`plugin remove` needs a plugin id");
    };
    let (directories, _) = match open_host() {
        Ok(host) => host,
        Err(message) => return fail(&message),
    };
    let inventory = Inventory::collect(&directories);
    let copies = match inventory.resolve(wanted) {
        Ok(copies) => copies,
        Err(message) => return fail(&message),
    };
    // Only an installed copy is CriKey's to delete. A plugin an operator put on
    // a discovery root themselves is theirs, and deleting it would be a command
    // reaching outside the directories it owns.
    let Some(installed) = copies.iter().find(|entry| entry.origin == Origin::Installed) else {
        let entry = copies[0];
        return fail(&format!(
            "`{}` was discovered on a plugin root (`{}`), not installed by CriKey; \
             remove it from that root instead",
            entry.plugin.0,
            entry.root.display()
        ));
    };
    // The kind is carried through with the id: a bare id is only unique within
    // one runtime, and the entry resolved above already knows which runtime the
    // user named.
    let id = installed.id.clone();
    let kind = installed.kind;

    let mut installer = PluginInstaller::new(&directories);
    match installer.remove(kind, &id) {
        Ok(removed) => {
            print_installed(&removed, "removed");
            ExitCode::from(EX_OK)
        }
        Err(error) => fail(&format!("removal failed: {error}")),
    }
}

fn print_installed(plugin: &InstalledPlugin, verdict: &str) {
    field("plugin", &namespaced(plugin.kind, &plugin.id).0);
    field("id", &plugin.id);
    field("version", &display_version(&plugin.version));
    field("kind", plugin.kind.directory_name());
    field("root", &plugin.root.display().to_string());
    field(
        "unsigned_binary",
        if plugin.unsigned_binary { "true" } else { "false" },
    );
    field("verdict", verdict);
}

// ---------------------------------------------------------------------------
// The plugin index
// ---------------------------------------------------------------------------

/// The index client this host's configuration describes.
///
/// Nothing is configured by default, and nothing is guessed when nothing is
/// configured: a launcher that reached for a hardcoded host the moment a user
/// typed `plugin search` would be making a network request nobody asked for, to
/// a service this project does not run.
fn open_index(directories: &StandardDirectories, config: &ConfigStore) -> Result<PluginIndexClient, String> {
    let urls = index_urls(config.get(KEY_INDEX_URLS));
    if urls.is_empty() {
        return Err(format!(
            "no plugin index is configured; set `{KEY_INDEX_URLS}` to a comma-separated list of \
             index URLs in {}",
            config.config_path().display()
        ));
    }
    let max_age = index_max_age(config.get(KEY_INDEX_MAX_AGE_SECONDS));
    PluginIndexClient::new(directories, urls, max_age)
        .map_err(|error| format!("the plugin index is unusable: {error}"))
}

/// Prints one line per configured index and returns the usable snapshots and
/// whether any index was refused.
///
/// Every index is reported before any of them is read, for the same reason
/// `list` reports unreadable roots: a search across three indexes of which one
/// was refused otherwise looks exactly like a complete answer.
fn report_indexes(outcomes: Vec<IndexOutcome>) -> (Vec<IndexSnapshot>, bool) {
    field("indexes", &outcomes.len().to_string());
    let mut snapshots = Vec::new();
    let mut refused = false;
    for (position, outcome) in outcomes.into_iter().enumerate() {
        match outcome.snapshot {
            Ok(snapshot) => {
                let (age, reason) = match &snapshot.freshness {
                    Freshness::Fresh => ("-".to_owned(), "-".to_owned()),
                    Freshness::Stale { age_seconds, reason } => (age_seconds.to_string(), reason.clone()),
                };
                println!(
                    "index={position} url={} signer={} fingerprint={} freshness={} age_seconds={age} \
                     generated_at={} plugins={} status=ok reason={}",
                    encode(&snapshot.url),
                    encode(&snapshot.signer.name),
                    encode(&snapshot.signer.fingerprint),
                    snapshot.freshness.as_str(),
                    encode(&snapshot.document.generated_at),
                    snapshot.document.plugins.len(),
                    encode(&reason),
                );
                snapshots.push(snapshot);
            }
            Err(error) => {
                refused = true;
                println!(
                    "index={position} url={} signer=- fingerprint=- freshness=unavailable age_seconds=- \
                     generated_at=- plugins=0 status=refused reason={}",
                    encode(&outcome.url),
                    encode(&error.to_string()),
                );
            }
        }
    }
    (snapshots, refused)
}

/// The exit status for a report whose indexes were all readable, or not.
fn index_status(refused: bool) -> ExitCode {
    if refused {
        ExitCode::from(EX_INVALID)
    } else {
        ExitCode::from(EX_OK)
    }
}

fn search(args: &[String]) -> ExitCode {
    let arguments = match positional("search", args, 1) {
        Ok(arguments) => arguments,
        Err(message) => return refuse(&message),
    };
    let Some(query) = arguments.first() else {
        return refuse("`plugin search` needs a query");
    };
    let (directories, config) = match open_host() {
        Ok(host) => host,
        Err(message) => return fail(&message),
    };
    let client = match open_index(&directories, &config) {
        Ok(client) => client,
        Err(message) => return fail(&message),
    };

    field("query", query);
    let (snapshots, refused) = report_indexes(client.load(false));
    let hits = index_search(&snapshots, query);
    field("matches", &hits.len().to_string());
    for (position, hit) in hits.iter().enumerate() {
        println!(
            "match={position} id={} name={} version={} runtime={} quality={} index_url={} summary={}",
            encode(&hit.entry.id),
            encode(&hit.entry.name),
            encode(&hit.entry.version),
            encode(&hit.entry.runtime),
            hit.quality.as_str(),
            encode(&hit.index_url),
            encode(&hit.entry.summary),
        );
    }
    index_status(refused)
}

fn show(args: &[String]) -> ExitCode {
    let arguments = match positional("show", args, 1) {
        Ok(arguments) => arguments,
        Err(message) => return refuse(&message),
    };
    let Some(wanted) = arguments.first() else {
        return refuse("`plugin show` needs a plugin id");
    };
    let (directories, config) = match open_host() {
        Ok(host) => host,
        Err(message) => return fail(&message),
    };
    let client = match open_index(&directories, &config) {
        Ok(client) => client,
        Err(message) => return fail(&message),
    };

    field("plugin", wanted);
    let (snapshots, refused) = report_indexes(client.load(false));
    let listings: Vec<(&IndexSnapshot, &IndexEntry)> = snapshots
        .iter()
        .filter_map(|snapshot| snapshot.document.entry(wanted).map(|entry| (snapshot, entry)))
        .collect();
    if listings.is_empty() {
        return fail(&format!("`{wanted}` is not listed by any configured index"));
    }
    field("listings", &listings.len().to_string());
    for (position, (snapshot, entry)) in listings.iter().enumerate() {
        print_listing(position, entry, snapshot);
    }
    index_status(refused)
}

/// One index entry, in full. Every field the format carries is printed: `show`
/// exists so an operator can decide whether to install something, and a field
/// omitted from the report is a field they cannot weigh.
fn print_listing(position: usize, entry: &IndexEntry, snapshot: &IndexSnapshot) {
    println!(
        "listing={position} id={} name={} version={} runtime={} licence={} homepage={} \
         download_url={} package_digest={} signer_fingerprint={} index_url={} freshness={} summary={}",
        encode(&entry.id),
        encode(&entry.name),
        encode(&entry.version),
        encode(&entry.runtime),
        encode(entry.licence.as_deref().unwrap_or("-")),
        encode(entry.homepage.as_deref().unwrap_or("-")),
        encode(&entry.download_url),
        encode(&entry.package_digest),
        encode(&entry.signer_fingerprint),
        encode(&snapshot.url),
        snapshot.freshness.as_str(),
        encode(&entry.summary),
    );
}

fn index_command(args: &[String]) -> ExitCode {
    let arguments = match positional("index", args, 1) {
        Ok(arguments) => arguments,
        Err(message) => return refuse(&message),
    };
    let Some(action) = arguments.first() else {
        return refuse("`plugin index` needs update");
    };
    if action != "update" {
        return refuse(&format!("unknown `plugin index` action `{action}`"));
    }
    let (directories, config) = match open_host() {
        Ok(host) => host,
        Err(message) => return fail(&message),
    };
    let client = match open_index(&directories, &config) {
        Ok(client) => client,
        Err(message) => return fail(&message),
    };

    let (snapshots, refused) = report_indexes(client.load(true));
    // An update that fell back to a cached copy did not update. It is reported
    // as a bad verdict rather than a success, because a script that treats
    // "refreshed" and "served you yesterday's catalogue" alike will install
    // yesterday's catalogue believing it is today's.
    let stale = snapshots
        .iter()
        .any(|snapshot| snapshot.freshness != Freshness::Fresh);
    index_status(refused || stale)
}

/// Installs the plugin an index lists under `wanted`.
///
/// The digest is checked before the installer is told anything exists: an
/// archive that does not hash to what the index published is deleted and the
/// refusal names both digests, so an operator can tell a corrupted download
/// from a stale index from a substituted package.
fn install_from_index(directories: &StandardDirectories, config: &ConfigStore, wanted: &str) -> ExitCode {
    let client = match open_index(directories, config) {
        Ok(client) => client,
        Err(message) => return fail(&message),
    };
    let (snapshots, _) = report_indexes(client.load(false));
    let listings: Vec<&IndexSnapshot> = snapshots
        .iter()
        .filter(|snapshot| snapshot.document.entry(wanted).is_some())
        .collect();
    let snapshot = match listings.as_slice() {
        [] => return fail(&format!("`{wanted}` is not listed by any configured index")),
        [only] => *only,
        // Two publishers offering the same id are two different sets of bytes,
        // and picking one would be picking for the operator.
        many => {
            let urls: Vec<&str> = many.iter().map(|snapshot| snapshot.url.as_str()).collect();
            return fail(&format!(
                "`{wanted}` is listed by {} configured indexes ({}); install it by download URL to \
                 say which one you mean",
                many.len(),
                urls.join(", ")
            ));
        }
    };
    let entry = snapshot
        .document
        .entry(wanted)
        .expect("the snapshot was selected because it lists this id");

    if entry.parsed_runtime().is_none() {
        return fail(&format!(
            "cannot install `{wanted}`: unsupported runtime `{}` in the plugin index",
            entry.runtime
        ));
    }
    let staging = directories.cache_dir().join("plugin-index").join("downloads");
    if let Err(error) = std::fs::create_dir_all(&staging) {
        return fail(&format!("{} could not be created: {error}", staging.display()));
    }
    let downloaded = staging.join(format!("{wanted}-{}", std::process::id()));
    println!(
        "resolved={} version={} index_url={} download_url={} package_digest={}",
        encode(&entry.id),
        encode(&entry.version),
        encode(&snapshot.url),
        encode(&entry.download_url),
        encode(&entry.package_digest),
    );
    if let Err(error) = client.download_package(entry, &downloaded) {
        return fail(&format!("cannot install `{wanted}`: {error}"));
    }

    // The runtime the index declares decides which of the installer's two
    // archive shapes the bytes are, because a Keypirinha package and a modern
    // one are told apart by extension and a downloaded file has none. An
    // unknown runtime is searchable metadata, never permission to guess an
    // installation format.
    let source = match entry.parsed_runtime() {
        Some(Runtime::LegacyPython) => InstallSource::LegacyPackage(downloaded.clone()),
        Some(Runtime::Python | Runtime::Native | Runtime::CAbi | Runtime::Wasm) => {
            InstallSource::Archive(downloaded.clone())
        }
        Some(Runtime::Builtin) | None => {
            let _ = std::fs::remove_file(&downloaded);
            return fail(&format!(
                "cannot install `{wanted}`: unsupported runtime `{}` in the plugin index",
                entry.runtime
            ));
        }
    };
    // Provenance is established already and by something stronger than a
    // sidecar: the index document was verified against the trust store before
    // this entry was read, and `download_package` refused any bytes that did
    // not hash to the digest that signed document pins. Applying the
    // unsigned-package policy on top would refuse a package a trusted key has
    // already vouched for, over a `<url>.sig` nothing fetches.
    let status = install_source(directories, &source, SignaturePolicy::unchecked());
    let _ = std::fs::remove_file(&downloaded);
    status
}

// ---------------------------------------------------------------------------
// enable and disable
// ---------------------------------------------------------------------------

fn set_enabled(args: &[String], enabled: bool) -> ExitCode {
    let command = if enabled { "enable" } else { "disable" };
    let arguments = match positional(command, args, 1) {
        Ok(arguments) => arguments,
        Err(message) => return refuse(&message),
    };
    let Some(wanted) = arguments.first() else {
        return refuse(&format!("`plugin {command}` needs a plugin id"));
    };
    let (directories, mut config) = match open_host() {
        Ok(host) => host,
        Err(message) => return fail(&message),
    };
    let inventory = Inventory::collect(&directories);
    // The config key is the plugin id, so every copy of a shadowed plugin is
    // covered by one setting and any copy answers for the id.
    let plugin = match inventory.resolve(wanted) {
        Ok(copies) => copies[0].plugin.clone(),
        Err(message) => return fail(&message),
    };

    config.set_plugin_enabled(&plugin, enabled);
    if let Err(error) = config.save() {
        return fail(&format!("cannot save the configuration: {error}"));
    }

    field("plugin", &plugin.0);
    field("enabled", if enabled { "true" } else { "false" });
    // Says plainly that nothing changed in the running launcher, because the
    // disabled state is applied at discovery: a plugin already loaded keeps
    // serving until the next launch, and an operator who expected it to vanish
    // would otherwise conclude the command did nothing.
    field("applies", "next launch");
    field("verdict", if enabled { "enabled" } else { "disabled" });
    ExitCode::from(EX_OK)
}

// ---------------------------------------------------------------------------
// scheduling-profile
// ---------------------------------------------------------------------------

/// The three profile spellings, plus `default` to clear an override.
fn parse_profile(value: &str) -> Result<Option<SchedulingProfile>, String> {
    match value {
        "legacy-strict" => Ok(Some(SchedulingProfile::LegacyStrict)),
        "legacy-optimized" => Ok(Some(SchedulingProfile::LegacyOptimized)),
        "modern" => Ok(Some(SchedulingProfile::Modern)),
        "default" => Ok(None),
        other => Err(format!(
            "unknown scheduling profile `{other}`; expected legacy-strict, legacy-optimized, modern or default"
        )),
    }
}

fn profile_name(profile: SchedulingProfile) -> &'static str {
    match profile {
        SchedulingProfile::LegacyStrict => "legacy-strict",
        SchedulingProfile::LegacyOptimized => "legacy-optimized",
        SchedulingProfile::Modern => "modern",
    }
}

fn scheduling_profile(args: &[String]) -> ExitCode {
    let arguments = match positional("scheduling-profile", args, 2) {
        Ok(arguments) => arguments,
        Err(message) => return refuse(&message),
    };
    let Some(wanted) = arguments.first() else {
        return refuse("`plugin scheduling-profile` needs a plugin id");
    };
    let requested = match arguments.get(1).map(String::as_str).map(parse_profile) {
        Some(Ok(profile)) => Some(profile),
        Some(Err(message)) => return refuse(&message),
        None => None,
    };

    let (directories, mut config) = match open_host() {
        Ok(host) => host,
        Err(message) => return fail(&message),
    };
    let inventory = Inventory::collect(&directories);
    let entry = match inventory.resolve(wanted) {
        Ok(copies) => copies[0],
        Err(message) => return fail(&message),
    };

    let Some(requested) = requested else {
        let (profile, source) = entry.profile(&config);
        field("plugin", &entry.plugin.0);
        field("kind", entry.kind.directory_name());
        field("scheduling_profile", profile_name(profile));
        field("profile_source", source);
        field("verdict", "reported");
        return ExitCode::from(EX_OK);
    };

    config.set_scheduling_profile(&entry.plugin, requested);
    if let Err(error) = config.save() {
        return fail(&format!("cannot save the configuration: {error}"));
    }

    field("plugin", &entry.plugin.0);
    field("kind", entry.kind.directory_name());
    let (profile, source) = entry.profile(&config);
    field("scheduling_profile", profile_name(profile));
    field("profile_source", source);
    // Spec 7.2: a legacy plugin may be moved off `legacy-strict`, and that is a
    // deliberate departure from the conformance guarantee rather than a tuning
    // knob. Setting it is permitted; leaving the operator to discover from a bug
    // report that their Keypirinha plugin now debounces and caches is not.
    if let Some(departure) = departure(entry.kind, profile) {
        field("departure", departure);
    }
    field("verdict", "set");
    ExitCode::from(EX_OK)
}

/// How `profile` departs from what a plugin of `kind` is guaranteed under, if it
/// does.
fn departure(kind: PluginKind, profile: SchedulingProfile) -> Option<&'static str> {
    match (kind, profile) {
        (PluginKind::Legacy, SchedulingProfile::LegacyStrict) => None,
        (PluginKind::Legacy, _) => Some(
            "this departs from `legacy-strict`: the plugin was written for Keypirinha's \
             every-keystroke dispatch, and under any other profile CriKey may debounce its \
             queries and serve cached dynamic results, which spec 7.2 does not guarantee it \
             tolerates",
        ),
        (PluginKind::Modern | PluginKind::Native, SchedulingProfile::Modern) => None,
        (PluginKind::Modern | PluginKind::Native, _) => Some(
            "this applies legacy scheduling to a plugin written for the modern profile: its \
             declared `[query]` debounce and activation gating are ignored",
        ),
    }
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// Reports actionable per-plugin health (spec 26.1, 26.2; acceptance 31.29).
///
/// Every number here is measured in this process rather than copied out of a
/// launcher that is not running: the manifest is parsed here, the declared
/// `[concurrency]` budgets are built here and probed with one unit of each of
/// the four work kinds, and a legacy package's compatibility findings come from
/// the same [`crikey_legacy_compat::LegacyDiagnostics`] store the developer
/// commands feed. A refusal counter read out of a dead process would be zero
/// for every plugin and would mean nothing at all; the counters printed here are
/// the ones this invocation's own probe moved, which is a fact.
///
/// The verdict is `degraded` when a plugin's manifest does not parse, when a
/// positive concurrency limit still refuses admission, or when the Legacy
/// Compatibility Layer files a `blocking` finding about it. A `warning` or
/// `info` finding is printed without changing the verdict: the scheduling
/// profile is reported as an `info` finding for every legacy plugin, and a
/// command that called that degraded would be red on a healthy host forever.
fn doctor(args: &[String]) -> ExitCode {
    let arguments = match positional("doctor", args, 1) {
        Ok(arguments) => arguments,
        Err(message) => return refuse(&message),
    };
    let (directories, config) = match open_host() {
        Ok(host) => host,
        Err(message) => return fail(&message),
    };
    let inventory = Inventory::collect(&directories);

    let selected: Vec<&PluginEntry> = match arguments.first() {
        // Every copy, not just the first: a plugin present both installed and on
        // a discovery root is the case an operator most needs a diagnosis of,
        // and reporting one of the two would describe a plugin the launcher may
        // not be the one loading.
        Some(wanted) => match inventory.resolve(wanted) {
            Ok(copies) => copies,
            Err(message) => return fail(&message),
        },
        None => inventory.entries.iter().collect(),
    };
    // How many copies each id has, so shadowing is reported per plugin.
    let mut copies_of: BTreeMap<&str, usize> = BTreeMap::new();
    for entry in &inventory.entries {
        *copies_of.entry(entry.plugin.0.as_str()).or_default() += 1;
    }

    // Resolved once, not per plugin: interpreter discovery walks the filesystem,
    // and every legacy package on this host is going to run under the same one.
    let interpreter = discover_interpreter(&RuntimeProfile::LegacyCompatibility).ok();

    let mut degraded = 0usize;
    let mut budget_line = 0usize;
    let mut warning_line = 0usize;
    field("plugins", &selected.len().to_string());
    for (index, entry) in selected.iter().enumerate() {
        let (profile, source) = entry.profile(&config);
        let mut unhealthy = entry.problem.is_some();

        println!(
            "plugin={index} id={} kind={} version={} enabled={} scheduling_profile={} \
             profile_source={} origin={} manifest={} root={}",
            encode(&entry.plugin.0),
            entry.kind.directory_name(),
            encode(&entry.version),
            config.plugin_enabled(&entry.plugin),
            profile_name(profile),
            source,
            entry.origin.as_str(),
            encode(match (&entry.problem, entry.kind) {
                (Some(problem), _) => problem.as_str(),
                (None, PluginKind::Legacy) => "none-required",
                (None, _) => "valid",
            }),
            encode(&entry.root.display().to_string()),
        );
        if entry.unsigned_binary {
            println!(
                "note={index} plugin={} unsigned_binary=true",
                encode(&entry.plugin.0)
            );
        }
        if let Some(departure) = departure(entry.kind, profile) {
            println!(
                "note={index} plugin={} departure={}",
                encode(&entry.plugin.0),
                encode(departure)
            );
        }
        // Declarations the manifest is allowed to carry and this build cannot
        // act on. Reported, never degrading: the plugin works, one line of its
        // manifest simply buys it nothing, and an author has no other way to
        // discover that.
        for unhonoured in entry
            .manifest
            .as_ref()
            .map(Manifest::unhonoured_declarations)
            .unwrap_or_default()
        {
            println!(
                "note={index} plugin={} unhonoured_declaration={} reason={}",
                encode(&entry.plugin.0),
                encode(unhonoured.field),
                encode(unhonoured.reason),
            );
        }
        // A legacy package ships no manifest, so the loop above has nothing to
        // report for it — and "nothing reported" would read as "nothing
        // granted", which is the opposite of the truth. Keypirinha plugins
        // were written for a host with no permission model at all, so the host
        // applies a posture of its own and names it here.
        if matches!(entry.kind, PluginKind::Legacy) {
            println!(
                "note={index} plugin={} legacy_permission_posture={} host_mediated_grants={} \
                 unconfined={}",
                encode(&entry.plugin.0),
                encode("compatibility-baseline"),
                encode("process,filesystem-package-read"),
                encode("clipboard,network,filesystem-in-child-interpreter"),
            );
        }
        // Two copies of one plugin id is a load failure waiting to happen: the
        // owning provider registers the id once and records the second copy
        // unavailable, so which of them serves depends on discovery order. Not
        // this command's to resolve, but never left unsaid.
        if copies_of
            .get(entry.plugin.0.as_str())
            .is_some_and(|count| *count > 1)
        {
            println!(
                "note={index} plugin={} shadowed_copies={}",
                encode(&entry.plugin.0),
                copies_of[entry.plugin.0.as_str()],
            );
        }

        // The admission probe (spec 13.5). One unit of each of the four work
        // kinds is exactly what the launcher asks for first, so this reports the
        // effective limit after default resolution, whether that first unit was
        // admitted, and the refusal counter it moved.
        //
        // `limit=0` is not a defect: spec 19.1 keeps an undeclared budget
        // distinct from a declared zero precisely so an author can switch a
        // surface off, and a `doctor` that called that broken would report every
        // deliberately query-only plugin as degraded. What *is* a defect is a
        // positive limit that still refuses, because then the enforcement layer
        // and the declaration disagree and the plugin will be throttled for a
        // reason its author cannot see in their own manifest.
        let budget = shared_budget_from_section(&entry.concurrency());
        for kind in BudgetKind::ALL {
            let limit = budget.limit(kind);
            let admitted = budget.try_acquire(kind).is_some();
            let surface = if limit == 0 {
                "disabled-by-declaration"
            } else if admitted {
                "enabled"
            } else {
                unhealthy = true;
                "refused-despite-limit"
            };
            println!(
                "budget={budget_line} plugin={} work={} limit={limit} admitted={admitted} \
                 refusals={} surface={surface}",
                encode(&entry.plugin.0),
                budget_kind_name(kind),
                budget.refusals(kind),
            );
            budget_line += 1;
        }

        // What the operating system will enforce on this plugin's process,
        // probed in THIS process rather than read out of a running launcher.
        // The policy is built by the same function the hosts use and prepared
        // for real, so an unavailable kernel feature or a disabled override
        // shows up here rather than being discovered when a plugin misbehaves.
        // Two limits follow from where it runs, and `probe=this-process` is
        // there to say so: `CRIKEY_PLUGIN_SANDBOX` is read from this command's
        // environment, not the launcher's, and only the baseline writable set
        // is probed, because the extra directory a legacy worker is given
        // exists per launcher instance and inventing a path here would report
        // a policy no worker uses. A legacy package declares no permissions
        // and therefore keeps the compatibility baseline, which does not
        // restrict the network.
        let sandbox = crikey_sandbox::plugin_policy(
            Vec::<std::path::PathBuf>::new(),
            entry
                .manifest
                .as_ref()
                .is_some_and(|manifest| !manifest.permissions.network),
        )
        .prepare();
        let report = sandbox.report();
        println!(
            "sandbox={index} plugin={} probe=this-process filesystem_write={} tcp_network={} reads={}",
            encode(&entry.plugin.0),
            encode(&report.filesystem_write.to_string()),
            encode(&report.tcp_network.to_string()),
            encode("unrestricted"),
        );

        // Legacy compatibility findings (spec 26.2), from the one store that
        // owns them. Anything blocking makes the plugin degraded; a `warning`
        // or `info` finding is reported without changing the verdict, because a
        // scheduling-profile note is not a defect.
        if entry.kind == PluginKind::Legacy {
            match legacy_findings(entry, interpreter.as_ref(), profile) {
                Ok(findings) => {
                    for (code, severity, message, suggestion) in findings {
                        if severity >= Severity::Blocking {
                            unhealthy = true;
                        }
                        let mut line = format!(
                            "warning={warning_line} plugin={} code={code} severity={} message={}",
                            encode(&entry.plugin.0),
                            severity.as_str(),
                            encode(&message),
                        );
                        if let Some(suggestion) = suggestion {
                            let _ = write!(line, " suggestion={}", encode(&suggestion));
                        }
                        println!("{line}");
                        warning_line += 1;
                    }
                }
                Err(problem) => {
                    unhealthy = true;
                    println!(
                        "warning={warning_line} plugin={} code=package-unreadable severity=blocking message={}",
                        encode(&entry.plugin.0),
                        encode(&problem),
                    );
                    warning_line += 1;
                }
            }
        }

        println!(
            "verdict={index} plugin={} health={}",
            encode(&entry.plugin.0),
            if unhealthy { "degraded" } else { "healthy" }
        );
        if unhealthy {
            degraded += 1;
        }
    }

    report_unreadable(&inventory);
    field("degraded", &degraded.to_string());
    field("verdict", if degraded == 0 { "healthy" } else { "degraded" });
    if degraded == 0 && inventory.unreadable.is_empty() {
        ExitCode::from(EX_OK)
    } else {
        ExitCode::from(EX_INVALID)
    }
}

/// One §26.2 finding: its stable code, how bad it is, what happened, and what
/// to do about it when there is something actionable to say.
///
/// Named rather than written inline because it is the unit `doctor` prints,
/// sorts and counts, and a bare four-tuple in a signature says nothing about
/// which position means what.
type Finding = (&'static str, Severity, String, Option<String>);

/// The §26.2 findings for one legacy package, in first-occurrence order.
fn legacy_findings(
    entry: &PluginEntry,
    interpreter: Option<&crikey_legacy_compat::Interpreter>,
    profile: SchedulingProfile,
) -> Result<Vec<Finding>, String> {
    let package = load_legacy_package(entry)?;
    let dependency = scan_windows_only_dependency(&package)?;
    let diagnostics = compatibility_diagnostics(
        &entry.plugin,
        &package,
        interpreter,
        scheduler_profile(profile),
        LegacyObservations {
            dependency: dependency.as_deref(),
            ..LegacyObservations::default()
        },
    );
    Ok(diagnostics
        .warnings_for(&entry.plugin)
        .iter()
        .map(|record| {
            let warning = &record.warning;
            (
                warning.code(),
                warning.severity(),
                warning.message(),
                warning.suggestion(),
            )
        })
        .collect())
}

/// The scheduler's profile enum for a declared manifest profile.
///
/// Two enums exist by design: `crikey-plugin-model`'s is the serialized
/// declaration and `crikey-input-scheduler`'s carries the behavioural
/// predicates, and neither crate may depend on the other. `crikey-app` maps
/// them at its own registration seam; this is the same map at the reporting
/// seam, exhaustive so a fourth profile cannot be silently reported as one of
/// these three.
fn scheduler_profile(profile: SchedulingProfile) -> crikey_input_scheduler::SchedulingProfile {
    match profile {
        SchedulingProfile::LegacyStrict => crikey_input_scheduler::SchedulingProfile::LegacyStrict,
        SchedulingProfile::LegacyOptimized => crikey_input_scheduler::SchedulingProfile::LegacyOptimized,
        SchedulingProfile::Modern => crikey_input_scheduler::SchedulingProfile::Modern,
    }
}

fn load_legacy_package(entry: &PluginEntry) -> Result<LegacyPackage, String> {
    let cache_root = crate::legacy_cache_root()?;
    PackageLoader::new(cache_root)
        .load(&entry.root)
        .map_err(|error| format!("cannot load `{}`: {error}", entry.root.display()))
}

fn budget_kind_name(kind: BudgetKind) -> &'static str {
    match kind {
        BudgetKind::Suggestion => "suggestion",
        BudgetKind::Action => "action",
        BudgetKind::Background => "background",
        BudgetKind::Catalog => "catalog",
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn field(key: &str, value: &str) {
    println!("{key}={}", encode(value));
}

/// Mirrors `package_commands::encode` exactly (spec §28 output contract).
fn encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// A completed operation that reached a bad verdict. Stdout stays empty of a
/// success report, so a reader never has to decide which of two verdicts is the
/// real one.
fn fail(message: &str) -> ExitCode {
    eprintln!("crikey: {message}");
    ExitCode::from(EX_INVALID)
}

fn refuse(message: &str) -> ExitCode {
    eprintln!("crikey: {message}\n\n{}", plugin_help());
    ExitCode::from(EX_USAGE)
}

fn print_help(command: &str) {
    match command {
        "list" => print!(
            "crikey plugin list\n\n\
             USAGE:\n    crikey plugin list\n\n\
             Reports every installed plugin and every plugin on the live discovery\n\
             roots, with its id, version, kind, enabled state and scheduling profile.\n"
        ),
        "search" => print!(
            "crikey plugin search\n\n\
             USAGE:\n    crikey plugin search <QUERY>\n\n\
             Searches every configured plugin index, best match first: an exact id, an\n\
             id prefix, an id substring, then a name or summary substring. Reports each\n\
             index's signer and whether its cached copy is fresh or stale. Exits 1 when\n\
             an index was refused, and reports that no index is configured when none is.\n"
        ),
        "show" => print!(
            "crikey plugin show\n\n\
             USAGE:\n    crikey plugin show <ID>\n\n\
             Reports every field a configured index publishes for ID: version, runtime,\n\
             licence, homepage, download URL, package digest and signer fingerprint.\n\
             Exits 1 when no configured index lists ID.\n"
        ),
        "index" => print!(
            "crikey plugin index\n\n\
             USAGE:\n    crikey plugin index update\n\n\
             Fetches every configured index, verifies its detached signature against the\n\
             trust store, and replaces the cached copy only once it verifies. Exits 1\n\
             when an index was refused or could only be served from the cache.\n"
        ),
        "install" => print!(
            "crikey plugin install\n\n\
             USAGE:\n    crikey plugin install [--unsigned-policy POLICY] <SOURCE>\n\n\
             SOURCE is a plugin directory, a `.crikey-package` archive, an `http(s)://`\n\
             URL, a `.keypirinha-package` file, or a plugin id a configured index lists.\n\
             An indexed install refuses a package that does not hash to the digest the\n\
             index published. Refused while a launcher is running.\n\n\
             OPTIONS:\n    --unsigned-policy POL   refuse (default), warn or allow, for a\n\
                 native archive with no `<package>.sig` beside it. Overrides\n\
                 `packages.unsigned-policy`. A URL install fetches `<url>.sig` to answer\n\
                 it. A source directory is packed by this command and carries no\n\
                 publisher signature, so the policy does not apply to it; nor to an\n\
                 indexed id, whose provenance is the index signature.\n"
        ),
        "remove" => print!(
            "crikey plugin remove\n\n\
             USAGE:\n    crikey plugin remove <ID>\n\n\
             Removes an installed plugin. ID is the namespaced id `crikey plugin list`\n\
             prints, or a bare id when it is unambiguous.\n"
        ),
        "enable" => print!(
            "crikey plugin enable\n\n\
             USAGE:\n    crikey plugin enable <ID>\n\n\
             Records the plugin as enabled. Applies at the next launch.\n"
        ),
        "disable" => print!(
            "crikey plugin disable\n\n\
             USAGE:\n    crikey plugin disable <ID>\n\n\
             Records the plugin as disabled. No provider loads it at the next launch:\n\
             no worker is started and it is never registered with the scheduler.\n"
        ),
        "doctor" => print!(
            "crikey plugin doctor\n\n\
             USAGE:\n    crikey plugin doctor [<ID>]\n\n\
             Reports per-plugin health: manifest validity, enabled state, scheduling\n\
             profile, the declared concurrency budgets with an admission probe of each\n\
             work kind, the operating-system confinement the plugin's process will\n\
             actually be subject to, and the compatibility findings for a legacy\n\
             package. Exits 1 when any plugin is degraded.\n"
        ),
        "scheduling-profile" => print!(
            "crikey plugin scheduling-profile\n\n\
             USAGE:\n    crikey plugin scheduling-profile <ID> [<PROFILE>]\n\n\
             Reports the profile with no PROFILE, otherwise sets it. PROFILE is\n\
             legacy-strict, legacy-optimized, modern, or default to clear an override.\n\
             Setting anything but legacy-strict on a legacy plugin is permitted and\n\
             reports the departure it makes from spec 7.2.\n"
        ),
        _ => print!("{}", plugin_help()),
    }
}

fn plugin_help() -> &'static str {
    "crikey plugin - manage plugins\n\n\
USAGE:\n\
    crikey plugin list\n\
    crikey plugin search <QUERY>\n\
    crikey plugin show <ID>\n\
    crikey plugin index update\n\
    crikey plugin install [--unsigned-policy POLICY] <SOURCE>\n\
    crikey plugin remove <ID>\n\
    crikey plugin enable <ID>\n\
    crikey plugin disable <ID>\n\
    crikey plugin doctor [<ID>]\n\
    crikey plugin scheduling-profile <ID> [<PROFILE>]\n\
\n\
OPTIONS:\n\
    -h, --help  Print this message\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two encoders that disagreed on case would make two reports diff against
    /// each other for no reason, so the spelling is pinned here as it is in
    /// every other command module.
    #[test]
    fn plugin_encoding_matches_the_frozen_spelling() {
        assert_eq!(encode("space % and ="), "space%20%25%20and%20%3D");
    }

    /// `--help` must not become a way to have a typo accepted.
    #[test]
    fn help_does_not_hide_an_unknown_plugin_option() {
        assert!(validate_help_args("list", &["--help".to_owned()]).is_ok());
        assert!(validate_help_args("list", &["--help".to_owned(), "--unknown".to_owned()]).is_err());
        assert!(validate_help_args("doctor", &["--help".to_owned(), "modern.foo".to_owned()]).is_ok());
        assert!(
            validate_help_args("list", &["--help".to_owned(), "unexpected-positional".to_owned()]).is_err()
        );
    }

    /// Every profile spelling the command documents must parse, and nothing else
    /// may: a mistyped profile silently read as the default would change how a
    /// plugin is scheduled without saying so.
    #[test]
    fn every_documented_profile_spelling_parses_and_nothing_else_does() {
        assert_eq!(
            parse_profile("legacy-strict"),
            Ok(Some(SchedulingProfile::LegacyStrict))
        );
        assert_eq!(
            parse_profile("legacy-optimized"),
            Ok(Some(SchedulingProfile::LegacyOptimized))
        );
        assert_eq!(parse_profile("modern"), Ok(Some(SchedulingProfile::Modern)));
        assert_eq!(parse_profile("default"), Ok(None));
        assert!(parse_profile("legacy_strict").is_err());
        assert!(parse_profile("").is_err());
    }

    /// The profile name round-trips, so a value printed by `list` can be handed
    /// straight back to `scheduling-profile`.
    #[test]
    fn a_printed_profile_name_parses_back_to_the_same_profile() {
        for profile in [
            SchedulingProfile::LegacyStrict,
            SchedulingProfile::LegacyOptimized,
            SchedulingProfile::Modern,
        ] {
            assert_eq!(parse_profile(profile_name(profile)), Ok(Some(profile)));
        }
    }

    /// A legacy plugin left on `legacy-strict` reports no departure; moved off
    /// it, the departure must be stated (spec 7.2).
    #[test]
    fn only_a_legacy_plugin_moved_off_legacy_strict_reports_a_departure() {
        assert!(departure(PluginKind::Legacy, SchedulingProfile::LegacyStrict).is_none());
        assert!(departure(PluginKind::Legacy, SchedulingProfile::LegacyOptimized).is_some());
        assert!(departure(PluginKind::Legacy, SchedulingProfile::Modern).is_some());
        assert!(departure(PluginKind::Modern, SchedulingProfile::Modern).is_none());
        assert!(departure(PluginKind::Native, SchedulingProfile::LegacyStrict).is_some());
    }

    /// A version-less package format must print an explicit `-`, never a blank
    /// column that reads as a parse failure.
    #[test]
    fn a_missing_version_prints_as_a_dash() {
        assert_eq!(display_version(""), "-");
        assert_eq!(display_version("   "), "-");
        assert_eq!(display_version("1.2.3"), "1.2.3");
    }

    /// The namespaced id is what every other subsystem keys on, so it must be
    /// built from the kind's own directory name rather than a second table.
    #[test]
    fn the_namespaced_id_matches_the_kind_directory_name() {
        assert_eq!(namespaced(PluginKind::Legacy, "notes").0, "legacy.notes");
        assert_eq!(namespaced(PluginKind::Modern, "notes").0, "modern.notes");
        assert_eq!(namespaced(PluginKind::Native, "notes").0, "native.notes");
    }
}
