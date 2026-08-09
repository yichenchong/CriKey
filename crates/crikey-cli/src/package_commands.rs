//! Native package commands and Keypirinha migration (spec 23.3, 23.4, 28;
//! contract §5.2).
//!
//! Archive creation, inspection and verification stay in
//! `crikey-package-manager`, and the Keypirinha translation stays in
//! `crikey-legacy-compat` beside the archive reader it needs; this module is
//! only argument validation and the frozen whitespace-safe report surface.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crikey_config::ConfigStore;
use crikey_legacy_compat::{migrate_keypirinha_package, MigrationReport};
use crikey_package_manager::{
    build_package, inspect_package, sign_package, signature_path_for, verify_package_with_policy,
    NativePackageReport, PackageError, PackageSignatureReport, PackageSigningKey, PublicKey, SignatureError,
    SignaturePolicy, SignatureState, TrustStore, UnsignedPolicy, KEY_UNSIGNED_POLICY,
};
use crikey_platform::StandardDirectories;

use crate::legacy_commands::PrivatePackageCache;

const EX_OK: u8 = 0;
const EX_INVALID: u8 = 1;
const EX_USAGE: u8 = 64;

/// Runs the subcommand following `crikey package`.
pub(crate) fn run(args: &[String]) -> ExitCode {
    let Some(command) = args.first().map(String::as_str) else {
        return refuse(
            "`package` needs build, verify, inspect, sign, keygen, trust-add, trust-list, \
             trust-remove or migrate-keypirinha",
        );
    };

    if command == "-h" || command == "--help" {
        if args.len() == 1 {
            print!("{}", package_help());
            return ExitCode::from(EX_OK);
        }
        return refuse("`package --help` takes no additional arguments");
    }

    if args[1..].iter().any(|arg| arg == "-h" || arg == "--help") {
        if let Err(message) = validate_help_args(command, &args[1..]) {
            return refuse(&message);
        }
        print_help(command);
        return ExitCode::from(EX_OK);
    }

    match command {
        "build" => build(&args[1..]),
        "verify" => verify(&args[1..]),
        "inspect" => inspect(&args[1..]),
        "sign" => sign(&args[1..]),
        "keygen" => keygen(&args[1..]),
        "trust-add" => trust_add(&args[1..]),
        "trust-list" => trust_list(&args[1..]),
        "trust-remove" => trust_remove(&args[1..]),
        "migrate-keypirinha" => migrate_keypirinha(&args[1..]),
        other => refuse(&format!("unknown package subcommand `{other}`")),
    }
}

/// The flags each subcommand takes a value for.
///
/// One table rather than a `match` per parser: the flag set is the same fact
/// whether it is being parsed or being skipped past while a `--help` line is
/// checked for a flag nobody recognises, and two copies of it would disagree
/// the first time a flag was added to one of them.
fn value_flags(command: &str) -> Option<&'static [&'static str]> {
    Some(match command {
        "build" => &["--plugin", "--out"],
        "verify" => &["--package", "--expect-hash", "--unsigned-policy"],
        "inspect" => &["--package"],
        "sign" => &["--package", "--key", "--key-env", "--out"],
        "keygen" => &["--out", "--public-out"],
        "trust-add" => &["--name", "--key", "--key-file"],
        "trust-list" => &[],
        "trust-remove" => &["--name"],
        "migrate-keypirinha" => &["--package", "--out"],
        _ => return None,
    })
}

fn validate_help_args(command: &str, args: &[String]) -> Result<(), String> {
    let flags = value_flags(command).ok_or_else(|| format!("unknown package subcommand `{command}`"))?;
    let mut position = 0;
    while position < args.len() {
        let argument = args[position].as_str();
        if argument == "-h" || argument == "--help" {
            position += 1;
            continue;
        }
        if flags.iter().any(|flag| **flag == *argument) {
            // The value may be absent, which `--help` does not care about; a
            // following `--flag` is the next flag, not this one's value.
            position += 1;
            if args.get(position).is_some_and(|value| !value.starts_with("--")) {
                position += 1;
            }
            continue;
        }
        if flags.iter().any(|flag| {
            argument
                .strip_prefix(*flag)
                .is_some_and(|rest| rest.starts_with('='))
        }) {
            position += 1;
            continue;
        }
        return Err(format!("package command does not understand `{argument}`"));
    }
    Ok(())
}

/// Parses `--flag VALUE` and `--flag=VALUE` for the flags `command` accepts.
///
/// Refuses an empty value, because an empty path reaches the filesystem as the
/// current directory and an empty key reaches the parser as a malformed one,
/// and refuses a repeated flag rather than letting the last one silently win.
fn parse_values(command: &str, args: &[String]) -> Result<Vec<(String, String)>, String> {
    let flags = value_flags(command).ok_or_else(|| format!("unknown package subcommand `{command}`"))?;
    let mut parsed: Vec<(String, String)> = Vec::new();
    let mut position = 0;
    while position < args.len() {
        let argument = args[position].as_str();
        let (flag, value) = match flags.iter().find(|flag| **flag == argument) {
            Some(flag) => {
                let value = args
                    .get(position + 1)
                    .ok_or_else(|| format!("`package {command}` needs a value after `{flag}`"))?;
                position += 2;
                (*flag, value.as_str())
            }
            None => {
                let split = flags.iter().find_map(|flag| {
                    argument
                        .strip_prefix(*flag)
                        .and_then(|rest| rest.strip_prefix('='))
                        .map(|value| (*flag, value))
                });
                let Some((flag, value)) = split else {
                    return Err(format!("`package {command}` does not understand `{argument}`"));
                };
                position += 1;
                (flag, value)
            }
        };
        if value.is_empty() {
            return Err(format!("`package {command} {flag}` was given an empty value"));
        }
        if parsed.iter().any(|(seen, _)| seen == flag) {
            return Err(format!("`package {command}` was given `{flag}` twice"));
        }
        parsed.push((flag.to_owned(), value.to_owned()));
    }
    Ok(parsed)
}

fn value<'a>(parsed: &'a [(String, String)], flag: &str) -> Option<&'a str> {
    parsed
        .iter()
        .find(|(seen, _)| seen == flag)
        .map(|(_, value)| value.as_str())
}

fn build(args: &[String]) -> ExitCode {
    let mut plugin: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    if let Err(message) = parse_flags(args, &mut plugin, &mut output, false) {
        return refuse(&message);
    }
    let Some(plugin) = plugin else {
        return refuse("`package build` needs `--plugin DIR`");
    };
    if plugin.as_os_str().is_empty() {
        return refuse("`package build --plugin` was given an empty path");
    }
    let output = output.unwrap_or_else(|| {
        let mut path = plugin.clone();
        path.set_extension("crikey-package");
        path
    });

    match build_package(&plugin, &output) {
        Ok(report) => {
            print_report(&output, &report, true);
            ExitCode::from(EX_OK)
        }
        Err(error) => {
            print_invalid(&output);
            eprintln!("crikey: could not build package `{}`: {error}", plugin.display());
            ExitCode::from(EX_INVALID)
        }
    }
}

/// Authenticates a package's members *and* its provenance (spec 2.2, 23.3).
///
/// The embedded per-member lock only ever answered "are these the bytes someone
/// packaged"; a party who rebuilds the archive rewrites the lock to match and
/// passes. So this command now also asks who signed it, under the operator's
/// [`UnsignedPolicy`], and prints the answer and the signer's fingerprint
/// whichever way it comes out. A refusal names the artefact and the key.
fn verify(args: &[String]) -> ExitCode {
    let parsed = match parse_values("verify", args) {
        Ok(parsed) => parsed,
        Err(message) => return refuse(&message),
    };
    let Some(package) = value(&parsed, "--package").map(PathBuf::from) else {
        return refuse("`package verify` needs `--package FILE`");
    };
    let policy = match signature_policy(value(&parsed, "--unsigned-policy")) {
        Ok(policy) => policy,
        Err(message) => {
            print_invalid(&package);
            eprintln!("crikey: {message}");
            return ExitCode::from(EX_INVALID);
        }
    };
    match verify_package_with_policy(&package, value(&parsed, "--expect-hash"), &policy) {
        Ok(report) => {
            print_report(&package, &report, true);
            if report.signature == SignatureState::Unsigned && policy.unsigned() == Some(UnsignedPolicy::Warn)
            {
                eprintln!(
                    "crikey: `{}` carries no detached signature; \
                     its provenance is unknown (policy `warn`)",
                    package.display()
                );
            }
            ExitCode::from(EX_OK)
        }
        Err(error) => {
            print_refusal(&package, &error);
            eprintln!(
                "crikey: package verification failed for `{}`: {error}",
                package.display()
            );
            ExitCode::from(EX_INVALID)
        }
    }
}

/// The provenance policy this invocation enforces.
///
/// `--unsigned-policy` beats the configured `packages.unsigned-policy`, which
/// beats [`UnsignedPolicy::default`] — refusing an unsigned package, because the
/// alternative default is installing third-party native code of unknown origin
/// and that is not a decision a launcher makes on an operator's behalf.
///
/// The trust store always comes from the config root. A flag can loosen what
/// happens to a package with *no* signature; nothing on the command line can
/// add a trusted key, because trusting a key is a decision that outlives one
/// command.
///
/// Shared with `crikey plugin install` rather than duplicated there: a launcher
/// where the command that *verifies* a package and the command that *installs*
/// it resolve the operator's policy in two places is a launcher where the two
/// eventually disagree, and the one that disagrees quietly is the installer.
pub(crate) fn signature_policy(override_value: Option<&str>) -> Result<SignaturePolicy, String> {
    let directories = StandardDirectories::for_process()
        .map_err(|error| format!("cannot resolve the standard directories: {error}"))?;
    let unsigned = match override_value {
        Some(text) => UnsignedPolicy::parse(text).map_err(|error| error.to_string())?,
        None => {
            let store = ConfigStore::load(&directories)
                .map_err(|error| format!("cannot load the configuration: {error}"))?;
            match store.get(KEY_UNSIGNED_POLICY) {
                Some(text) => UnsignedPolicy::parse(text)
                    .map_err(|error| format!("`{KEY_UNSIGNED_POLICY}`: {error}"))?,
                None => UnsignedPolicy::default(),
            }
        }
    };
    let trust = TrustStore::load(&directories).map_err(|error| error.to_string())?;
    Ok(SignaturePolicy::enforced(unsigned, trust))
}

/// The trust store, for the commands that only manage it.
fn open_trust_store() -> Result<TrustStore, String> {
    let directories = StandardDirectories::for_process()
        .map_err(|error| format!("cannot resolve the standard directories: {error}"))?;
    TrustStore::load(&directories).map_err(|error| error.to_string())
}

fn inspect(args: &[String]) -> ExitCode {
    let mut package: Option<PathBuf> = None;
    let mut ignored: Option<PathBuf> = None;
    if let Err(message) = parse_flags(args, &mut package, &mut ignored, true) {
        return refuse(&message);
    }
    let Some(package) = package else {
        return refuse("`package inspect` needs `--package FILE`");
    };
    if package.as_os_str().is_empty() {
        return refuse("`package inspect --package` was given an empty path");
    }
    match inspect_package(&package) {
        Ok(report) => {
            print_report(&package, &report, true);
            ExitCode::from(EX_OK)
        }
        Err(error) => {
            print_invalid(&package);
            eprintln!("crikey: cannot inspect package `{}`: {error}", package.display());
            ExitCode::from(EX_INVALID)
        }
    }
}

/// Signs a built package for publication (spec 2.2; ADR 0012).
///
/// The key comes from a file or from the environment and from nowhere else. No
/// key is generated here, and none is looked for in the working directory: a
/// command that quietly produced a signing key would produce one that ends up
/// committed, and a command that quietly found one would sign with whatever a
/// hostile working tree left lying around.
fn sign(args: &[String]) -> ExitCode {
    let parsed = match parse_values("sign", args) {
        Ok(parsed) => parsed,
        Err(message) => return refuse(&message),
    };
    let Some(package) = value(&parsed, "--package").map(PathBuf::from) else {
        return refuse("`package sign` needs `--package FILE`");
    };
    let key = match (value(&parsed, "--key"), value(&parsed, "--key-env")) {
        (Some(path), None) => PackageSigningKey::from_file(Path::new(path)),
        (None, Some(variable)) => PackageSigningKey::from_env(variable),
        (None, None) => {
            return refuse("`package sign` needs `--key FILE` or `--key-env VARIABLE`");
        }
        (Some(_), Some(_)) => {
            return refuse("`package sign` takes `--key` or `--key-env`, not both");
        }
    };
    let key = match key {
        Ok(key) => key,
        Err(error) => {
            // The message names the file or the variable, never the value: a
            // diagnostic that quoted a malformed private key would write it
            // into whatever captured this process's stderr.
            field("package", &package.display().to_string());
            field("verdict", "unsigned");
            eprintln!("crikey: cannot read the signing key: {error}");
            return ExitCode::from(EX_INVALID);
        }
    };
    let out = value(&parsed, "--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| signature_path_for(&package));
    match sign_package(&package, &key, &out) {
        Ok(report) => {
            print_signature(&package, &report);
            ExitCode::from(EX_OK)
        }
        Err(error) => {
            field("package", &package.display().to_string());
            field("verdict", "unsigned");
            eprintln!("crikey: cannot sign `{}`: {error}", package.display());
            ExitCode::from(EX_INVALID)
        }
    }
}

/// Generates a signing key pair for a plugin author (spec 2.2; ADR 0012).
///
/// `--out` is required and is never defaulted. A default would put a private key
/// somewhere the author did not name, and the somewhere most authors run this
/// from is a repository. The file is created with `create_new` and owner-only
/// permissions, so an existing key is never overwritten.
fn keygen(args: &[String]) -> ExitCode {
    let parsed = match parse_values("keygen", args) {
        Ok(parsed) => parsed,
        Err(message) => return refuse(&message),
    };
    let Some(out) = value(&parsed, "--out").map(PathBuf::from) else {
        return refuse(
            "`package keygen` needs `--out FILE`; a signing key is never written to a path you did not name",
        );
    };
    let key = match PackageSigningKey::generate() {
        Ok(key) => key,
        Err(error) => {
            eprintln!("crikey: cannot generate a signing key: {error}");
            return ExitCode::from(EX_INVALID);
        }
    };
    if let Err(error) = key.write_new(&out) {
        eprintln!("crikey: cannot write the signing key: {error}");
        return ExitCode::from(EX_INVALID);
    }
    let public = key.public_key();
    if let Some(path) = value(&parsed, "--public-out") {
        if let Err(error) = public.write_new(Path::new(path)) {
            // The private half is already on disk under a path the author named.
            // Saying so is the difference between "rerun it" and "rerun it after
            // deleting the key you did not know existed".
            eprintln!(
                "crikey: the signing key was written to `{}`, but the public key could not be: {error}",
                out.display()
            );
            return ExitCode::from(EX_INVALID);
        }
    }
    // `key-file`, not `key`: `trust-list` uses `key=<index>` for its repeated
    // rows, and one spelling meaning two things across one command family is how
    // a script that greps output gets it wrong.
    field("key-file", &out.display().to_string());
    field("public-key", &public.to_hex());
    field("fingerprint", &public.fingerprint());
    field("verdict", "generated");
    ExitCode::from(EX_OK)
}

/// Trusts a named public key (spec 2.2; ADR 0012).
fn trust_add(args: &[String]) -> ExitCode {
    let parsed = match parse_values("trust-add", args) {
        Ok(parsed) => parsed,
        Err(message) => return refuse(&message),
    };
    let Some(name) = value(&parsed, "--name") else {
        return refuse("`package trust-add` needs `--name NAME`");
    };
    let key = match (value(&parsed, "--key"), value(&parsed, "--key-file")) {
        (Some(hex), None) => PublicKey::from_hex(hex),
        (None, Some(path)) => PublicKey::from_file(Path::new(path)),
        (None, None) => return refuse("`package trust-add` needs `--key HEX` or `--key-file FILE`"),
        (Some(_), Some(_)) => {
            return refuse("`package trust-add` takes `--key` or `--key-file`, not both");
        }
    };
    let key = match key {
        Ok(key) => key,
        Err(error) => {
            eprintln!("crikey: {error}");
            return ExitCode::from(EX_INVALID);
        }
    };
    let fingerprint = key.fingerprint();
    let mut store = match open_trust_store() {
        Ok(store) => store,
        Err(message) => {
            eprintln!("crikey: {message}");
            return ExitCode::from(EX_INVALID);
        }
    };
    if let Err(error) = store.add(name, key) {
        eprintln!("crikey: {error}");
        return ExitCode::from(EX_INVALID);
    }
    if let Err(error) = store.save() {
        eprintln!("crikey: cannot write the trust store: {error}");
        return ExitCode::from(EX_INVALID);
    }
    print_trust_store_path(&store);
    field("name", name);
    field("fingerprint", &fingerprint);
    field("keys", &store.len().to_string());
    field("verdict", "trusted");
    ExitCode::from(EX_OK)
}

/// Lists the trusted keys, fingerprint first: the fingerprint is what an
/// operator compares against what a publisher advertises.
fn trust_list(args: &[String]) -> ExitCode {
    if let Err(message) = parse_values("trust-list", args) {
        return refuse(&message);
    }
    let store = match open_trust_store() {
        Ok(store) => store,
        Err(message) => {
            eprintln!("crikey: {message}");
            return ExitCode::from(EX_INVALID);
        }
    };
    print_trust_store_path(&store);
    for (index, (name, key)) in store.entries().enumerate() {
        println!(
            "key={index} name={} fingerprint={} public-key={}",
            encode(name),
            encode(&key.fingerprint()),
            encode(&key.to_hex())
        );
    }
    field("keys", &store.len().to_string());
    field("verdict", "listed");
    ExitCode::from(EX_OK)
}

/// Stops trusting a named key.
fn trust_remove(args: &[String]) -> ExitCode {
    let parsed = match parse_values("trust-remove", args) {
        Ok(parsed) => parsed,
        Err(message) => return refuse(&message),
    };
    let Some(name) = value(&parsed, "--name") else {
        return refuse("`package trust-remove` needs `--name NAME`");
    };
    let mut store = match open_trust_store() {
        Ok(store) => store,
        Err(message) => {
            eprintln!("crikey: {message}");
            return ExitCode::from(EX_INVALID);
        }
    };
    // The fingerprint is read before the removal, because after it there is
    // nothing left to name and an operator needs to see which key went.
    let fingerprint = store.key(name).map(PublicKey::fingerprint);
    if !store.remove(name) {
        eprintln!("crikey: no trusted key is named `{name}`");
        return ExitCode::from(EX_INVALID);
    }
    if let Err(error) = store.save() {
        eprintln!("crikey: cannot write the trust store: {error}");
        return ExitCode::from(EX_INVALID);
    }
    print_trust_store_path(&store);
    field("name", name);
    field("fingerprint", fingerprint.as_deref().unwrap_or(""));
    field("keys", &store.len().to_string());
    field("verdict", "removed");
    ExitCode::from(EX_OK)
}

fn print_trust_store_path(store: &TrustStore) {
    field(
        "store",
        &store
            .path()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
    );
}

/// Converts a Keypirinha package into a CriKey package directory (spec 23.3).
///
/// The report names every fact the source format does not carry, because the
/// generated manifest deliberately does not claim any of them: an operator who
/// publishes a migrated package without reading this list ships a `version` of
/// `0.0.0+keypirinha-migrated`. The migration is therefore a *success* with a
/// non-empty `limitation.*` list rather than a warning-free conversion.
fn migrate_keypirinha(args: &[String]) -> ExitCode {
    let mut package: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    if let Err(message) = parse_migrate_flags(args, &mut package, &mut output) {
        return refuse(&message);
    }
    let Some(package) = package else {
        return refuse("`package migrate-keypirinha` needs `--package PATH`");
    };
    let Some(output) = output else {
        return refuse("`package migrate-keypirinha` needs `--out DIR`");
    };

    // Archive extraction happens under a directory only this process can write,
    // for the reason `PrivatePackageCache` documents: the loader trusts an
    // already extracted content-addressed directory, and the migration copies
    // whatever it finds there into the package it is about to hand back.
    let cache = match PrivatePackageCache::new("migrate-keypirinha") {
        Ok(cache) => cache,
        Err(message) => {
            print_migration_failed(&package, &output);
            eprintln!("crikey: {message}");
            return ExitCode::from(EX_INVALID);
        }
    };

    match migrate_keypirinha_package(&package, &output, &cache.path) {
        Ok(report) => {
            print_migration(&package, &report);
            ExitCode::from(EX_OK)
        }
        Err(error) => {
            print_migration_failed(&package, &output);
            eprintln!(
                "crikey: cannot migrate Keypirinha package `{}`: {error}",
                package.display()
            );
            ExitCode::from(EX_INVALID)
        }
    }
}

/// A migration that did not happen, naming both paths.
///
/// `print_invalid` would report the destination under the `package` key, and a
/// reader who saw a path they never named as `--package` would go looking for a
/// package that does not exist.
fn print_migration_failed(source: &Path, destination: &Path) {
    field("package", &source.display().to_string());
    field("destination", &destination.display().to_string());
    field("verdict", "invalid");
}

fn parse_migrate_flags(
    args: &[String],
    package: &mut Option<PathBuf>,
    output: &mut Option<PathBuf>,
) -> Result<(), String> {
    let mut position = 0;
    while position < args.len() {
        let argument = args[position].as_str();
        if let Some(value) = argument.strip_prefix("--package=") {
            *package = Some(non_empty(value, "--package")?);
            position += 1;
        } else if argument == "--package" {
            let value = args
                .get(position + 1)
                .ok_or_else(|| "`package migrate-keypirinha` needs a path after `--package`".to_owned())?;
            *package = Some(non_empty(value, "--package")?);
            position += 2;
        } else if let Some(value) = argument.strip_prefix("--out=") {
            *output = Some(non_empty(value, "--out")?);
            position += 1;
        } else if argument == "--out" {
            let value = args
                .get(position + 1)
                .ok_or_else(|| "`package migrate-keypirinha` needs a path after `--out`".to_owned())?;
            *output = Some(non_empty(value, "--out")?);
            position += 2;
        } else {
            return Err(format!(
                "`package migrate-keypirinha` does not understand `{argument}`"
            ));
        }
    }
    Ok(())
}

/// A path flag's value, refusing the empty string.
///
/// An empty path is the one value that would otherwise reach the filesystem as
/// the current directory, which is never what a caller who typed `--out=` meant.
fn non_empty(value: &str, flag: &str) -> Result<PathBuf, String> {
    if value.is_empty() {
        return Err(format!(
            "`package migrate-keypirinha {flag}` was given an empty path"
        ));
    }
    Ok(PathBuf::from(value))
}

/// The migration report as frozen `key=value` lines.
///
/// The limitation lines carry the stable code in the key and the prose in the
/// value, exactly as the §26.2 diagnostics rendering does, so a script greps a
/// code it can act on and a human reads the sentence beside it.
fn print_migration(source: &Path, report: &MigrationReport) {
    field("package", &source.display().to_string());
    field("destination", &report.destination.display().to_string());
    field("plugin", &report.id);
    field("version", crikey_legacy_compat::MIGRATED_VERSION);
    field("runtime", "legacy-python");
    field("scheduling_profile", "legacy-strict");
    field("entrypoint", &report.entrypoint);
    field("modules", &report.modules.len().to_string());
    field("resources", &report.resources.len().to_string());
    for (index, module) in report.modules.iter().enumerate() {
        println!("module={index} import={}", encode(module));
    }
    for (index, resource) in report.resources.iter().enumerate() {
        println!(
            "resource={index} path={}",
            encode(&resource.display().to_string())
        );
    }
    field("limitations", &report.limitations.len().to_string());
    for limitation in &report.limitations {
        println!(
            "limitation.{}={}",
            limitation.code(),
            encode(&limitation.message())
        );
    }
    field("verdict", "migrated");
}

fn parse_flags(
    args: &[String],
    plugin_or_package: &mut Option<PathBuf>,
    output: &mut Option<PathBuf>,
    inspect_mode: bool,
) -> Result<(), String> {
    let mut position = 0;
    while position < args.len() {
        let argument = args[position].as_str();
        if let Some(value) = argument.strip_prefix("--plugin=") {
            if inspect_mode {
                return Err("`package inspect` accepts `--package`, not `--plugin`".to_owned());
            }
            if value.is_empty() {
                return Err("`package build --plugin` was given an empty path".to_owned());
            }
            *plugin_or_package = Some(PathBuf::from(value));
            position += 1;
        } else if argument == "--plugin" {
            if inspect_mode {
                return Err("`package inspect` accepts `--package`, not `--plugin`".to_owned());
            }
            let value = args
                .get(position + 1)
                .ok_or_else(|| "`package build` needs a path after `--plugin`".to_owned())?;
            if value.is_empty() {
                return Err("`package build --plugin` was given an empty path".to_owned());
            }
            *plugin_or_package = Some(PathBuf::from(value));
            position += 2;
        } else if let Some(value) = argument.strip_prefix("--package=") {
            if !inspect_mode {
                return Err("`package build` accepts `--plugin`, not `--package`".to_owned());
            }
            if value.is_empty() {
                return Err("`package inspect --package` was given an empty path".to_owned());
            }
            *plugin_or_package = Some(PathBuf::from(value));
            position += 1;
        } else if argument == "--package" {
            if !inspect_mode {
                return Err("`package build` accepts `--plugin`, not `--package`".to_owned());
            }
            let value = args
                .get(position + 1)
                .ok_or_else(|| "the package command needs a path after `--package`".to_owned())?;
            if value.is_empty() {
                return Err("`package inspect --package` was given an empty path".to_owned());
            }
            *plugin_or_package = Some(PathBuf::from(value));
            position += 2;
        } else if let Some(value) = argument.strip_prefix("--out=") {
            if inspect_mode {
                return Err("`package inspect` does not accept `--out`".to_owned());
            }
            if value.is_empty() {
                return Err("`package build --out` was given an empty path".to_owned());
            }
            *output = Some(PathBuf::from(value));
            position += 1;
        } else if argument == "--out" {
            if inspect_mode {
                return Err("`package inspect` does not accept `--out`".to_owned());
            }
            let value = args
                .get(position + 1)
                .ok_or_else(|| "`package build` needs a path after `--out`".to_owned())?;
            if value.is_empty() {
                return Err("`package build --out` was given an empty path".to_owned());
            }
            *output = Some(PathBuf::from(value));
            position += 2;
        } else {
            return Err(format!("package command does not understand `{argument}`"));
        }
    }
    Ok(())
}

fn print_report(package: &Path, report: &NativePackageReport, valid: bool) {
    field("package", &package.display().to_string());
    field("entries", &report.entries.len().to_string());
    field("hash", &report.hash);
    field("plugin", &report.plugin);
    field("version", &report.version);
    field("runtime", "native");
    field("platform", &report.os.join(","));
    field("arch", &report.arch.join(","));
    field("signature", report.signature.label());
    field("signer", report.signature.signer().unwrap_or(""));
    field("fingerprint", report.signature.fingerprint().unwrap_or(""));
    field(
        "unsigned_binary",
        if report.unsigned_binary { "true" } else { "false" },
    );
    for (index, (path, bytes)) in report.entries.iter().enumerate() {
        println!("entry={} path={} bytes={}", index, encode(path), bytes);
    }
    field("verdict", if valid { "valid" } else { "invalid" });
}

fn print_invalid(package: &Path) {
    field("package", &package.display().to_string());
    field("verdict", "invalid");
}

/// A refusal, with the signature words on stdout beside the verdict.
///
/// The prose on stderr already names the artefact and the key, but a script
/// reading the report would otherwise see only `verdict=invalid` and could not
/// tell an untrusted publisher from a tampered download — two situations with
/// completely different next actions.
fn print_refusal(package: &Path, error: &PackageError) {
    field("package", &package.display().to_string());
    if let PackageError::Signature(signature) = error {
        let (state, fingerprint) = match signature {
            SignatureError::Unsigned { .. } => ("unsigned", None),
            SignatureError::UntrustedSigner { fingerprint, .. } => ("untrusted", Some(fingerprint)),
            SignatureError::Verification { fingerprint, .. } => ("invalid", Some(fingerprint)),
            _ => ("unreadable", None),
        };
        field("signature", state);
        field("fingerprint", fingerprint.map_or("", String::as_str));
    }
    field("verdict", "invalid");
}

/// What `crikey package sign` produced.
fn print_signature(package: &Path, report: &PackageSignatureReport) {
    field("package", &package.display().to_string());
    field("signature", &report.signature.display().to_string());
    field("plugin", &report.plugin);
    field("version", &report.version);
    field("hash", &report.hash);
    field("fingerprint", &report.fingerprint);
    field("verdict", "signed");
}

fn field(key: &str, value: &str) {
    println!("{key}={}", encode(value));
}

/// Mirrors `modern_commands::encode` exactly (spec §28 output contract).
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

fn refuse(message: &str) -> ExitCode {
    eprintln!("crikey: {message}\n\n{}", package_help());
    ExitCode::from(EX_USAGE)
}

fn print_help(command: &str) {
    match command {
        "build" => print!(
            "crikey package build\n\n\
             USAGE:\n    crikey package build --plugin DIR [--out FILE]\n\n\
             OPTIONS:\n    --plugin DIR   Native plugin directory to package.\n\
                 --out FILE     Archive to write (defaults beside the plugin).\n\
                 -h, --help     Print this message without building.\n"
        ),
        "verify" => print!(
            "crikey package verify\n\n\
             USAGE:\n    crikey package verify --package FILE [--expect-hash HEX] \
             [--unsigned-policy POLICY]\n\n\
             OPTIONS:\n    --package FILE          Archive to verify.\n\
                 --expect-hash HEX       Expected SHA-256 hash of the archive.\n\
                 --unsigned-policy POL   refuse (default), warn or allow, for a\n\
                                         package with no detached signature.\n\
                 -h, --help              Print this message without verifying.\n\n\
             The embedded lock authenticates the archive against itself. The\n\
             detached `<package>.sig` beside it authenticates who built it, against\n\
             the keys in `trusted-keys.toml`. `signature=` and `fingerprint=` report\n\
             the answer; an untrusted signer and a signature that does not verify\n\
             are both refusals, and are reported differently.\n"
        ),
        "inspect" => print!(
            "crikey package inspect\n\n\
             USAGE:\n    crikey package inspect --package FILE\n\n\
             OPTIONS:\n    --package FILE  Archive to inspect.\n\
                 -h, --help      Print this message without inspecting.\n"
        ),
        "sign" => print!(
            "crikey package sign\n\n\
             USAGE:\n    crikey package sign --package FILE (--key FILE | --key-env VARIABLE) \
             [--out FILE]\n\n\
             OPTIONS:\n    --package FILE      Archive to sign; authenticated first.\n\
                 --key FILE          File holding one line: the signing key as 64\n\
                                     hexadecimal characters.\n\
                 --key-env VARIABLE  Environment variable holding the same, for CI.\n\
                 --out FILE          Signature to write (defaults to <package>.sig).\n\
                 -h, --help          Print this message without signing.\n\n\
             The signature covers every member's name and digest, so it is a claim\n\
             about the whole package. No key is generated and none is searched for:\n\
             use `crikey package keygen` once, and keep the result out of the\n\
             repository.\n"
        ),
        "keygen" => print!(
            "crikey package keygen\n\n\
             USAGE:\n    crikey package keygen --out FILE [--public-out FILE]\n\n\
             OPTIONS:\n    --out FILE         Signing key to create; required, and never\n\
                                    overwritten.\n\
                 --public-out FILE  Public key to create beside it.\n\
                 -h, --help         Print this message without generating a key.\n\n\
             `--out` has no default on purpose: a private key must not appear at a\n\
             path you did not name. The file is created readable only by you, and\n\
             the key itself is never printed.\n"
        ),
        "trust-add" => print!(
            "crikey package trust-add\n\n\
             USAGE:\n    crikey package trust-add --name NAME (--key HEX | --key-file FILE)\n\n\
             OPTIONS:\n    --name NAME       Your name for this publisher.\n\
                 --key HEX         Public key as 64 hexadecimal characters.\n\
                 --key-file FILE   File holding the same on one line.\n\
                 -h, --help        Print this message without trusting anything.\n\n\
             Compare the printed fingerprint against the one the publisher\n\
             advertises before you rely on it. Nothing else establishes that this\n\
             is their key.\n"
        ),
        "trust-list" => print!(
            "crikey package trust-list\n\n\
             USAGE:\n    crikey package trust-list\n\n\
             OPTIONS:\n    -h, --help  Print this message.\n"
        ),
        "trust-remove" => print!(
            "crikey package trust-remove\n\n\
             USAGE:\n    crikey package trust-remove --name NAME\n\n\
             OPTIONS:\n    --name NAME  Trusted key to stop trusting.\n\
                 -h, --help   Print this message without removing anything.\n"
        ),
        "migrate-keypirinha" => print!(
            "crikey package migrate-keypirinha\n\n\
             USAGE:\n    crikey package migrate-keypirinha --package PATH --out DIR\n\n\
             OPTIONS:\n    --package PATH  Keypirinha package archive or directory.\n\
                 --out DIR       CriKey package directory to create; must not exist.\n\
                 -h, --help      Print this message without migrating.\n\n\
             The generated `crikey.toml` declares only what the Keypirinha format\n\
             carries. Everything it does not is reported as a `limitation.*` line;\n\
             run `crikey plugin doctor` afterwards for compatibility findings.\n"
        ),
        _ => print!("{}", package_help()),
    }
}

fn package_help() -> &'static str {
    "crikey package - package, sign and verify native plugins\n\n\
USAGE:\n\
    crikey package build --plugin DIR [--out FILE]\n\
    crikey package verify --package FILE [--expect-hash HEX] [--unsigned-policy POLICY]\n\
    crikey package inspect --package FILE\n\
    crikey package sign --package FILE (--key FILE | --key-env VARIABLE) [--out FILE]\n\
    crikey package keygen --out FILE [--public-out FILE]\n\
    crikey package trust-add --name NAME (--key HEX | --key-file FILE)\n\
    crikey package trust-list\n\
    crikey package trust-remove --name NAME\n\
    crikey package migrate-keypirinha --package PATH --out DIR\n\
\n\
OPTIONS:\n\
    -h, --help  Print this message\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_encoding_matches_frozen_spelling() {
        assert_eq!(encode("space % and ="), "space%20%25%20and%20%3D");
    }

    #[test]
    fn help_does_not_hide_unknown_package_options() {
        let args = vec!["--help".to_owned(), "--unknown".to_owned()];
        assert!(validate_help_args("build", &args).is_err());
        assert!(validate_help_args(
            "build",
            &["--help".to_owned(), "--plugin".to_owned(), "--unknown".to_owned()]
        )
        .is_err());
    }

    #[test]
    fn empty_flag_values_are_rejected() {
        let mut plugin = None;
        let mut output = None;
        assert!(parse_flags(
            &["--plugin".to_owned(), "plugin".to_owned(), "--out=".to_owned()],
            &mut plugin,
            &mut output,
            false,
        )
        .is_err());

        assert!(parse_values(
            "verify",
            &[
                "--package".to_owned(),
                "package".to_owned(),
                "--expect-hash=".to_owned()
            ],
        )
        .is_err());
    }

    #[test]
    fn a_repeated_flag_is_refused_rather_than_letting_the_last_one_win() {
        // Silently preferring one of two `--key`s would let a stray shell
        // expansion sign with a key the author did not name.
        assert!(parse_values(
            "sign",
            &["--key".to_owned(), "a".to_owned(), "--key=b".to_owned()],
        )
        .is_err());
    }

    #[test]
    fn prefix_flags_do_not_swallow_longer_flags() {
        // `--key` is a prefix of `--key-env`; the parser must file
        // `--key-env=X` under `--key-env` and not under `--key` with a value of
        // `-env=X`.
        let parsed = parse_values("sign", &["--key-env=CRIKEY_TEST_KEY".to_owned()]).expect("parses");
        assert_eq!(value(&parsed, "--key-env"), Some("CRIKEY_TEST_KEY"));
        assert_eq!(value(&parsed, "--key"), None);
    }

    #[test]
    fn every_dispatched_subcommand_has_a_flag_table() {
        // `value_flags` returning `None` is how both the parser and the help
        // checker report an unknown subcommand, so a command wired into `run`
        // without a table would refuse every one of its own flags.
        for command in [
            "build",
            "verify",
            "inspect",
            "sign",
            "keygen",
            "trust-add",
            "trust-list",
            "trust-remove",
            "migrate-keypirinha",
        ] {
            assert!(value_flags(command).is_some(), "{command} has no flag table");
        }
        assert!(value_flags("no-such-command").is_none());
    }
}
