//! Native package commands (spec 23.3, 23.4, 28; contract §5.2).
//!
//! Archive creation, inspection and verification stay in
//! `crikey-package-manager`; this module is only argument validation and the
//! frozen whitespace-safe report surface.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crikey_package_manager::{build_package, inspect_package, verify_package, NativePackageReport};

const EX_OK: u8 = 0;
const EX_INVALID: u8 = 1;
const EX_USAGE: u8 = 64;
const EX_UNAVAILABLE: u8 = 69;

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
        return if command == "migrate-keypirinha" {
            ExitCode::from(EX_UNAVAILABLE)
        } else {
            ExitCode::from(EX_OK)
        };
    }

    match command {
        "build" => build(&args[1..]),
        "verify" => verify(&args[1..]),
        "inspect" => inspect(&args[1..]),
        "migrate-keypirinha" => ExitCode::from(EX_UNAVAILABLE),
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
            "migrate-keypirinha" => false,
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
        "migrate-keypirinha" => {
            eprintln!("crikey: `package migrate-keypirinha` is not implemented")
        }
        _ => print!("{}", package_help()),
    }
}

fn package_help() -> &'static str {
    "crikey package - package native plugins\n\n\
USAGE:\n\
    crikey package build --plugin DIR [--out FILE]\n\
    crikey package verify --package FILE [--expect-hash HEX]\n\
    crikey package inspect --package FILE\n\
    crikey package migrate-keypirinha\n\
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
