//! The `crikey config` command family (spec 21.2, 21.3; 26; 28).
//!
//! Three subcommands over one store: `list`, `get` and `layers`.
//!
//! # Why this family exists
//!
//! Spec 21.2 gives configuration seven layers. A precedence order nobody can
//! inspect is not diagnosable (spec 26): when a user's setting appears not to
//! take effect, the only useful answer is *which layer won*, and
//! [`ConfigStore::layer_of`] is exactly that answer. `crikey config layers` is
//! that command; `get` reports the winning layer alongside the value, so the
//! narrow question needs no second invocation.
//!
//! # Secrets
//!
//! Nothing here prints a value through any path but
//! [`ConfigStore::display_value`], which substitutes `<redacted>` for a field its
//! plugin declared `secret` (spec 21.3). A flag that implies protection and then
//! leaks the value in a listing is worse than no flag at all, so the redaction is
//! a property of the one rendering function rather than a rule each subcommand
//! remembers.
//!
//! Knowing which keys are secret requires the plugin schemas, so every subcommand
//! registers them before printing — read straight off disk, with no plugin
//! process started.
//!
//! # The output contract
//!
//! Whitespace-separated `key=value` tokens with uppercase percent-encoded values,
//! byte-identical to `crikey plugin` and `crikey package`, so `cut`, `grep` and
//! `sort` are a complete reader. Configuration keys and values are third-party
//! strings that may hold a space, an `=` or a `%`; every value is therefore
//! encoded rather than quoted.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use crikey_config::{discover_plugin_schemas, ConfigStore};
use crikey_platform::{PluginKind, StandardDirectories};

use crate::{discovery_roots, legacy_package_roots, modern_plugin_roots, native_plugin_roots};

/// A completed operation that found nothing wrong.
const EX_OK: u8 = 0;
/// A completed operation that reached a bad verdict: no such key.
const EX_INVALID: u8 = 1;
/// An argument list this module could not parse.
const EX_USAGE: u8 = 64;

/// Runs the subcommand following `crikey config`.
pub(crate) fn run(args: &[String]) -> ExitCode {
    let Some(command) = args.first().map(String::as_str) else {
        return refuse("`config` needs list, get or layers");
    };

    if command == "-h" || command == "--help" {
        if args.len() == 1 {
            print!("{}", config_help());
            return ExitCode::from(EX_OK);
        }
        return refuse("`config --help` takes no additional arguments");
    }

    let rest = &args[1..];
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_help(command);
        return ExitCode::from(EX_OK);
    }

    match command {
        "list" => list(rest),
        "get" => get(rest),
        "layers" => layers(rest),
        other => refuse(&format!("unknown config subcommand `{other}`")),
    }
}

/// The positional arguments of a subcommand, refusing options and empty values.
fn positional(command: &str, args: &[String], maximum: usize) -> Result<Vec<String>, String> {
    if let Some(option) = args.iter().find(|argument| argument.starts_with('-')) {
        return Err(format!("`config {command}` does not understand `{option}`"));
    }
    if args.len() > maximum {
        return Err(format!(
            "`config {command}` takes at most {maximum} argument(s), got {}",
            args.len()
        ));
    }
    if args.iter().any(String::is_empty) {
        return Err(format!("`config {command}` was given an empty argument"));
    }
    Ok(args.to_vec())
}

/// Loads the store with every installed and discoverable plugin schema
/// registered.
///
/// Schema registration is not optional decoration: without it the store cannot
/// know which keys are secret, and cannot report the plugin-defaults layer that a
/// user's setting is competing with. Schema problems are reported on stderr and do
/// not stop the listing — a broken plugin must not make the whole configuration
/// uninspectable, which is when inspecting it matters most.
pub(crate) fn load() -> Result<ConfigStore, String> {
    let directories = StandardDirectories::for_process()
        .map_err(|error| format!("cannot resolve the standard directories: {error}"))?;
    let mut store =
        ConfigStore::load(&directories).map_err(|error| format!("cannot load the configuration: {error}"))?;
    for problem in register_schemas(&mut store, &directories) {
        eprintln!("crikey: {problem}");
    }
    Ok(store)
}

/// Registers every discoverable plugin's `[configuration]` schema against
/// `store`, returning one line per problem.
///
/// Shared with `crikey run` (see `crate::register_plugin_schemas`) in intent but
/// not in code: this one scans the same roots the launcher scans, so the two see
/// the same plugins and therefore agree about which keys are secret.
pub(crate) fn register_schemas(store: &mut ConfigStore, directories: &StandardDirectories) -> Vec<String> {
    let mut messages = Vec::new();
    let (schemas, problems) = discover_plugin_schemas(&schema_roots(directories));
    let had_discovery_problem = !problems.is_empty();
    for problem in problems {
        messages.push(format!(
            "plugin schema unavailable ({}): {}",
            problem.package.display(),
            problem.reason
        ));
    }
    for schema in schemas {
        for error in store.register_plugin_schema(&schema.plugin, &schema.section) {
            messages.push(error.to_string());
        }
    }
    if had_discovery_problem {
        // A failed manifest may have declared secrets we cannot inspect. Keep
        // every key in an unregistered namespace redacted rather than allowing
        // an unrelated parse error to turn `config get` into a token dump.
        store.redact_unregistered_plugins();
    }
    messages
}

/// Every root a plugin schema could be declared in.
///
/// All three kinds, including legacy: `discover_plugin_schemas` skips a
/// `legacy-python` manifest by RUNTIME rather than by path, so a legacy package
/// sitting in a modern root is still left to the Legacy Compatibility Layer's own
/// configuration contract (spec 21.1, 14). Scanning by path instead would make
/// that boundary depend on where an operator happened to put a package.
pub(crate) fn schema_roots(directories: &StandardDirectories) -> Vec<PathBuf> {
    let mut roots = discovery_roots(PluginKind::Modern, modern_plugin_roots(), directories);
    roots.extend(discovery_roots(
        PluginKind::Native,
        native_plugin_roots(),
        directories,
    ));
    roots.extend(discovery_roots(
        PluginKind::Legacy,
        legacy_package_roots(),
        directories,
    ));
    let mut unique = Vec::with_capacity(roots.len());
    for root in roots {
        if !unique.contains(&root) {
            unique.push(root);
        }
    }
    unique
}

/// `crikey config list` — every key, its winning layer and its value.
fn list(args: &[String]) -> ExitCode {
    if let Err(message) = positional("list", args, 0) {
        return refuse(&message);
    }
    let store = match load() {
        Ok(store) => store,
        Err(message) => return fail(&message),
    };
    for key in store.keys() {
        let Some(layer) = store.layer_of(key) else {
            continue;
        };
        // `display_value` and never `get`: this is the redaction choke point.
        let value = store.display_value(key).unwrap_or_default();
        println!(
            "key={} layer={} value={} secret={}",
            encode(key),
            encode(layer.as_str()),
            encode(value),
            store.is_secret(key)
        );
    }
    ExitCode::from(EX_OK)
}

/// `crikey config get <key>` — one key's value and the layer that supplied it.
fn get(args: &[String]) -> ExitCode {
    let arguments = match positional("get", args, 1) {
        Ok(arguments) => arguments,
        Err(message) => return refuse(&message),
    };
    let Some(key) = arguments.first() else {
        return refuse("`config get` needs a key");
    };
    let store = match load() {
        Ok(store) => store,
        Err(message) => return fail(&message),
    };
    let Some(layer) = store.layer_of(key) else {
        return fail(&format!("no configuration layer supplies `{key}`"));
    };
    let value = store.display_value(key).unwrap_or_default();
    println!(
        "key={} layer={} value={} secret={}",
        encode(key),
        encode(layer.as_str()),
        encode(value),
        store.is_secret(key)
    );
    ExitCode::from(EX_OK)
}

/// `crikey config layers [<key>]` — which layer wins, and what the order is.
///
/// With no key it prints the layer order itself, lowest precedence first, so an
/// operator can read the precedence without the specification to hand. With a key
/// it prints that key's winning layer.
fn layers(args: &[String]) -> ExitCode {
    let arguments = match positional("layers", args, 1) {
        Ok(arguments) => arguments,
        Err(message) => return refuse(&message),
    };
    if let Some(key) = arguments.first() {
        let store = match load() {
            Ok(store) => store,
            Err(message) => return fail(&message),
        };
        let Some(layer) = store.layer_of(key) else {
            return fail(&format!("no configuration layer supplies `{key}`"));
        };
        println!("key={} layer={}", encode(key), encode(layer.as_str()));
        return ExitCode::from(EX_OK);
    }
    for (index, layer) in crikey_config::ConfigLayer::ALL.iter().enumerate() {
        println!("precedence={} layer={}", index + 1, encode(layer.as_str()));
    }
    ExitCode::from(EX_OK)
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Mirrors `plugin_commands::encode` exactly (spec §28 output contract).
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
/// success report, so a reader never has to decide which of two verdicts is real.
fn fail(message: &str) -> ExitCode {
    eprintln!("crikey: {message}");
    ExitCode::from(EX_INVALID)
}

fn refuse(message: &str) -> ExitCode {
    eprintln!("crikey: {message}\n\n{}", config_help());
    ExitCode::from(EX_USAGE)
}

fn print_help(command: &str) {
    match command {
        "list" => print!(
            "crikey config list\n\n\
             USAGE:\n    crikey config list\n\n\
             Reports every configuration key, the layer that supplied its winning\n\
             value, and that value. A field a plugin declared `secret` is reported\n\
             as <redacted>.\n"
        ),
        "get" => print!(
            "crikey config get\n\n\
             USAGE:\n    crikey config get <KEY>\n\n\
             Reports one key's winning value and the layer it came from. Exits 1 when\n\
             no layer supplies the key. A secret field's value is <redacted>.\n"
        ),
        "layers" => print!(
            "crikey config layers\n\n\
             USAGE:\n    crikey config layers [<KEY>]\n\n\
             With a KEY, reports which of the seven layers supplied its winning value.\n\
             Without one, reports the layer precedence order, lowest first.\n"
        ),
        _ => print!("{}", config_help()),
    }
}

fn config_help() -> String {
    "crikey config - inspect the layered configuration\n\n\
     USAGE:\n    crikey config <SUBCOMMAND>\n\n\
     SUBCOMMANDS:\n\
     \x20   list              every key, its winning layer and its value\n\
     \x20   get <KEY>         one key's winning value and layer\n\
     \x20   layers [<KEY>]    which layer won a key, or the precedence order\n"
        .to_owned()
}
