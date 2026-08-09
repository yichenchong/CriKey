//! The `crikey catalog` command family (spec 2.2 "distributed or remote
//! indexing"; 26; 28; ADR-0016).
//!
//! Two subcommands over the configured remote catalog sources: `sources` lists
//! them, `refresh` fetches them now.
//!
//! # Why a command and not only a timer
//!
//! A remote index changes when a colleague publishes, not on a schedule. An
//! operator who has just published wants the new rows now, and an operator
//! debugging a source wants the failure in front of them rather than in a log an
//! hour from now. `refresh` is that: one fetch per named source, every refusal
//! reported by artefact and reason on stderr, and a non-zero exit when any
//! source was refused.
//!
//! # What `refresh` actually changes
//!
//! It writes the verified slice into the persistent per-owner catalog cache —
//! the same cache `crikey run` loads during startup stage 2, and the same slice
//! the launcher's own background refresh writes. A launcher already running does
//! not see it until it restarts, and the output says so rather than implying a
//! live update this command has no channel to perform (README invariant 7).
//!
//! # The output contract
//!
//! Whitespace-separated `key=value` tokens with uppercase percent-encoded
//! values, byte-identical to `crikey config` and `crikey plugin`.

use std::fmt::Write as _;
use std::process::ExitCode;

use crikey_app::{fetch_source, DefaultCatalogFetcher, RemoteSource};
use crikey_catalog::{CatalogCache, FileCatalogCache};
use crikey_config::{remote_catalog_sources, ConfigStore, RemoteCatalogSource};
use crikey_package_manager::TrustStore;
use crikey_platform::StandardDirectories;

use crate::catalog_cache_root;

/// A completed operation that found nothing wrong.
const EX_OK: u8 = 0;
/// A completed operation that reached a bad verdict: a source was refused.
const EX_INVALID: u8 = 1;
/// An argument list this module could not parse.
const EX_USAGE: u8 = 64;

/// Runs the subcommand following `crikey catalog`.
pub(crate) fn run(args: &[String]) -> ExitCode {
    let Some(command) = args.first().map(String::as_str) else {
        return refuse("`catalog` needs sources or refresh");
    };

    if command == "-h" || command == "--help" {
        if args.len() == 1 {
            print!("{}", catalog_help());
            return ExitCode::from(EX_OK);
        }
        return refuse("`catalog --help` takes no additional arguments");
    }

    let rest = &args[1..];
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_help(command);
        return ExitCode::from(EX_OK);
    }

    match command {
        "sources" => sources(rest),
        "refresh" => refresh(rest),
        other => refuse(&format!("unknown catalog subcommand `{other}`")),
    }
}

/// The positional arguments of a subcommand, refusing options and empty values.
fn positional(command: &str, args: &[String], maximum: usize) -> Result<Vec<String>, String> {
    if let Some(option) = args.iter().find(|argument| argument.starts_with('-')) {
        return Err(format!("`catalog {command}` does not understand `{option}`"));
    }
    if args.len() > maximum {
        return Err(format!(
            "`catalog {command}` takes at most {maximum} argument(s), got {}",
            args.len()
        ));
    }
    if args.iter().any(String::is_empty) {
        return Err(format!("`catalog {command}` was given an empty argument"));
    }
    Ok(args.to_vec())
}

/// The declared sources, in configuration order.
///
/// Plugin schemas are deliberately not registered: a remote source is a host
/// setting, no plugin declares one, and starting a discovery scan would make
/// this command depend on the state of every installed package.
fn declared() -> Result<(StandardDirectories, Vec<RemoteCatalogSource>), String> {
    let directories = StandardDirectories::for_process()
        .map_err(|error| format!("cannot resolve the standard directories: {error}"))?;
    let store =
        ConfigStore::load(&directories).map_err(|error| format!("cannot load the configuration: {error}"))?;
    let sources = remote_catalog_sources(&store)
        .map_err(|error| format!("cannot read the remote catalog sources: {error}"))?;
    Ok((directories, sources))
}

/// Converts a declaration into the value the app crate fetches with.
fn runtime_source(declared: &RemoteCatalogSource) -> RemoteSource {
    let mut source = RemoteSource::new(&declared.name, &declared.url);
    source.interval_ms = declared.interval_ms;
    source.max_bytes = declared.max_bytes;
    source.require_signature = declared.require_signature;
    source.signing_key = declared.signing_key.clone();
    source
}

/// `crikey catalog sources` — every declared remote source and its policy.
fn sources(args: &[String]) -> ExitCode {
    if let Err(message) = positional("sources", args, 0) {
        return refuse(&message);
    }
    let (_, declared) = match declared() {
        Ok(declared) => declared,
        Err(message) => return fail(&message),
    };
    for source in &declared {
        println!(
            "source={} owner={} url={} interval-ms={} max-bytes={} require-signature={} signing-key={}",
            encode(&source.name),
            encode(&crikey_app::remote_owner(&source.name).0),
            encode(&source.url),
            source.interval_ms,
            source.max_bytes,
            source.require_signature,
            encode(source.signing_key.as_deref().unwrap_or("")),
        );
    }
    println!("sources={}", declared.len());
    ExitCode::from(EX_OK)
}

/// `crikey catalog refresh [<name>]` — fetch, verify and cache now.
fn refresh(args: &[String]) -> ExitCode {
    let arguments = match positional("refresh", args, 1) {
        Ok(arguments) => arguments,
        Err(message) => return refuse(&message),
    };
    let (directories, declared) = match declared() {
        Ok(declared) => declared,
        Err(message) => return fail(&message),
    };
    let selected: Vec<&RemoteCatalogSource> = match arguments.first() {
        Some(name) => {
            let matched: Vec<&RemoteCatalogSource> =
                declared.iter().filter(|source| &source.name == name).collect();
            if matched.is_empty() {
                return fail(&format!("no remote catalog source is named `{name}`"));
            }
            matched
        }
        None => declared.iter().collect(),
    };
    // Nothing configured is the default state, not a failure: a launcher with no
    // remote source is the launcher this command was added to.
    if selected.is_empty() {
        println!("refreshed=0 refused=0 sources=0");
        return ExitCode::from(EX_OK);
    }

    // An absent trust store is an empty one, so a source that requires a
    // signature is refused by name rather than by a missing-file error.
    let trust = match TrustStore::load(&directories) {
        Ok(trust) => trust,
        Err(error) => return fail(&format!("cannot load the trusted key store: {error}")),
    };
    let cache = match catalog_cache_root() {
        Ok(root) => FileCatalogCache::new(root),
        Err(message) => return fail(&message),
    };
    let fetcher = DefaultCatalogFetcher;

    let mut refreshed = 0;
    let mut refused = 0;
    for source in selected {
        let runtime = runtime_source(source);
        match fetch_source(&runtime, &fetcher, &trust) {
            Ok(remote) => {
                if let Err(error) = cache.store_slice(&remote.slice) {
                    eprintln!(
                        "crikey: remote catalog `{}` could not be cached: {error}",
                        source.name
                    );
                    refused += 1;
                    continue;
                }
                refreshed += 1;
                println!(
                    "source={} owner={} published-by={} items={} signer={}",
                    encode(&source.name),
                    encode(&remote.slice.plugin.0),
                    encode(&remote.published_by.0),
                    remote.slice.items.len(),
                    encode(remote.signer.as_deref().unwrap_or("")),
                );
            }
            Err(error) => {
                eprintln!(
                    "crikey: remote catalog `{}` refused a refresh: {error}",
                    source.name
                );
                refused += 1;
            }
        }
    }
    println!("refreshed={refreshed} refused={refused}");
    if refreshed > 0 {
        // Said plainly rather than implied: this process has no channel into a
        // running launcher, so the rows become searchable when one next starts.
        println!("note=cached-for-next-start");
    }
    if refused > 0 {
        return ExitCode::from(EX_INVALID);
    }
    ExitCode::from(EX_OK)
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Mirrors `config_commands::encode` exactly (spec §28 output contract).
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

fn fail(message: &str) -> ExitCode {
    eprintln!("crikey: {message}");
    ExitCode::from(EX_INVALID)
}

fn refuse(message: &str) -> ExitCode {
    eprintln!("crikey: {message}\n\n{}", catalog_help());
    ExitCode::from(EX_USAGE)
}

fn print_help(command: &str) {
    match command {
        "sources" => print!(
            "crikey catalog sources\n\n\
             USAGE:\n    crikey catalog sources\n\n\
             Reports every configured remote catalog source, the catalog owner it\n\
             publishes as, its refresh interval, its size ceiling and its signature\n\
             policy. No source is configured by default.\n"
        ),
        "refresh" => print!(
            "crikey catalog refresh\n\n\
             USAGE:\n    crikey catalog refresh [<NAME>]\n\n\
             Fetches each configured remote catalog source now, verifies its digest\n\
             and signature, and writes the verified slice into the persistent catalog\n\
             cache. Exits 1 when any source was refused. A launcher already running\n\
             keeps serving its retained slice until it restarts.\n"
        ),
        _ => print!("{}", catalog_help()),
    }
}

fn catalog_help() -> String {
    "crikey catalog - inspect and refresh remote catalog sources\n\n\
     USAGE:\n    crikey catalog <SUBCOMMAND>\n\n\
     SUBCOMMANDS:\n\
     \x20   sources           every configured remote source and its policy\n\
     \x20   refresh [<NAME>]  fetch, verify and cache every source, or one\n"
        .to_owned()
}
