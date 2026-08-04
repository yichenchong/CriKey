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

use crikey_legacy_compat::{migrate_keypirinha_package, MigrationReport};
use crikey_package_manager::{build_package, inspect_package, verify_package, NativePackageReport};

use crate::legacy_commands::PrivatePackageCache;

const EX_OK: u8 = 0;
const EX_INVALID: u8 = 1;
const EX_USAGE: u8 = 64;

/// Runs the subcommand following `crikey package`.
pub(crate) fn run(args: &[String]) -> ExitCode {
    let Some(command) = args.first().map(String::as_str) else {
        return refuse("`package` needs build, verify, inspect or migrate-keypirinha");
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
        "migrate-keypirinha" => migrate_keypirinha(&args[1..]),
        other => refuse(&format!("unknown package subcommand `{other}`")),
    }
}

fn validate_help_args(command: &str, args: &[String]) -> Result<(), String> {
    let mut position = 0;
    while position < args.len() {
        let argument = args[position].as_str();
        if argument == "-h" || argument == "--help" {
            position += 1;
            continue;
        }
        let consumes_value = match command {
            "build" => {
                matches!(argument, "--plugin" | "--out")
                    || argument.starts_with("--plugin=")
                    || argument.starts_with("--out=")
            }
            "verify" => {
                matches!(argument, "--package" | "--expect-hash")
                    || argument.starts_with("--package=")
                    || argument.starts_with("--expect-hash=")
            }
            "inspect" => matches!(argument, "--package") || argument.starts_with("--package="),
            "migrate-keypirinha" => {
                matches!(argument, "--package" | "--out")
                    || argument.starts_with("--package=")
                    || argument.starts_with("--out=")
            }
            _ => return Err(format!("unknown package subcommand `{command}`")),
        };
        if consumes_value {
            if argument == "--plugin"
                || argument == "--out"
                || argument == "--package"
                || argument == "--expect-hash"
            {
                position += 1;
                if args.get(position).is_some_and(|value| !value.starts_with("--")) {
                    position += 1;
                }
            } else {
                position += 1;
            }
        } else {
            return Err(format!("package command does not understand `{argument}`"));
        }
    }
    Ok(())
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

fn verify(args: &[String]) -> ExitCode {
    let mut package: Option<PathBuf> = None;
    let mut expected_hash: Option<PathBuf> = None;
    if let Err(message) = parse_verify_flags(args, &mut package, &mut expected_hash) {
        return refuse(&message);
    }
    let Some(package) = package else {
        return refuse("`package verify` needs `--package FILE`");
    };
    if package.as_os_str().is_empty() {
        return refuse("`package verify --package` was given an empty path");
    }
    let expected_hash = expected_hash.as_deref().and_then(Path::to_str);
    match verify_package(&package, expected_hash) {
        Ok(report) => {
            print_report(&package, &report, true);
            ExitCode::from(EX_OK)
        }
        Err(error) => {
            print_invalid(&package);
            eprintln!(
                "crikey: package verification failed for `{}`: {error}",
                package.display()
            );
            ExitCode::from(EX_INVALID)
        }
    }
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

fn parse_verify_flags(
    args: &[String],
    package: &mut Option<PathBuf>,
    expected_hash: &mut Option<PathBuf>,
) -> Result<(), String> {
    let mut position = 0;
    while position < args.len() {
        let argument = args[position].as_str();
        if let Some(value) = argument.strip_prefix("--package=") {
            if value.is_empty() {
                return Err("`package verify --package` was given an empty path".to_owned());
            }
            *package = Some(PathBuf::from(value));
            position += 1;
        } else if argument == "--package" {
            let value = args
                .get(position + 1)
                .ok_or_else(|| "`package verify` needs a path after `--package`".to_owned())?;
            if value.is_empty() {
                return Err("`package verify --package` was given an empty path".to_owned());
            }
            *package = Some(PathBuf::from(value));
            position += 2;
        } else if let Some(value) = argument.strip_prefix("--expect-hash=") {
            if value.is_empty() {
                return Err("`package verify --expect-hash` was given an empty value".to_owned());
            }
            *expected_hash = Some(PathBuf::from(value));
            position += 1;
        } else if argument == "--expect-hash" {
            let value = args
                .get(position + 1)
                .ok_or_else(|| "`package verify` needs HEX after `--expect-hash`".to_owned())?;
            if value.is_empty() {
                return Err("`package verify --expect-hash` was given an empty value".to_owned());
            }
            *expected_hash = Some(PathBuf::from(value));
            position += 2;
        } else {
            return Err(format!("`package verify` does not understand `{argument}`"));
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
    field("signed", if report.signed { "true" } else { "false" });
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
             USAGE:\n    crikey package verify --package FILE [--expect-hash HEX]\n\n\
             OPTIONS:\n    --package FILE       Archive to verify.\n\
                 --expect-hash HEX  Expected SHA-256 hash.\n\
                 -h, --help          Print this message without verifying.\n"
        ),
        "inspect" => print!(
            "crikey package inspect\n\n\
             USAGE:\n    crikey package inspect --package FILE\n\n\
             OPTIONS:\n    --package FILE  Archive to inspect.\n\
                 -h, --help      Print this message without inspecting.\n"
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
    "crikey package - package native plugins and migrate Keypirinha packages\n\n\
USAGE:\n\
    crikey package build --plugin DIR [--out FILE]\n\
    crikey package verify --package FILE [--expect-hash HEX]\n\
    crikey package inspect --package FILE\n\
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
    fn empty_output_and_hash_values_are_rejected() {
        let mut plugin = None;
        let mut output = None;
        assert!(parse_flags(
            &["--plugin".to_owned(), "plugin".to_owned(), "--out=".to_owned()],
            &mut plugin,
            &mut output,
            false,
        )
        .is_err());

        let mut package = None;
        let mut expected_hash = None;
        assert!(parse_verify_flags(
            &[
                "--package".to_owned(),
                "package".to_owned(),
                "--expect-hash=".to_owned()
            ],
            &mut package,
            &mut expected_hash,
        )
        .is_err());
    }
}
