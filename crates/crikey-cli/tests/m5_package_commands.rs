//! Red-first black-box tests for native package commands (spec 23.3, 23.4,
//! 28; contract §5.2; acceptance §31.29, §31.30).
//!
//! The fixture is deliberately tiny: a native manifest and one `bin/` member
//! written into a private directory at test time. Every assertion consumes only
//! the frozen percent-encoded CLI keys, never a package-manager implementation
//! type or a ZIP-library detail.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// The binary under test, as built by cargo for this integration target.
const CRIKEY: &str = env!("CARGO_BIN_EXE_crikey");

/// A valid package operation.
const EX_OK: i32 = 0;
/// A completed verification that found an invalid package or hash.
const EX_INVALID: i32 = 1;
/// A usage error.
const EX_USAGE: i32 = 64;
/// `migrate-keypirinha` remains advertised but intentionally unavailable.
const EX_UNAVAILABLE: i32 = 69;
/// The Rust runtime's status for an unwound panic.
const PANIC_STATUS: i32 = 101;

/// One completed invocation, retained so assertion failures show all output.
#[derive(Debug)]
struct Run {
    args: Vec<String>,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl fmt::Display for Run {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "\n  command: crikey {args}\n  exit:    {code:?}\n  stdout:\n{stdout}\n  stderr:\n{stderr}",
            args = self.args.join(" "),
            code = self.code,
            stdout = indent(&self.stdout),
            stderr = indent(&self.stderr),
        )
    }
}

fn indent(text: &str) -> String {
    if text.trim().is_empty() {
        return "    <empty>".to_owned();
    }
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn run(args: &[&str]) -> Run {
    let owned = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    run_owned(&owned)
}

fn run_owned(args: &[String]) -> Run {
    let output = Command::new(CRIKEY)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("could not execute `{CRIKEY}` with {args:?}: {error}"));
    Run {
        args: args.to_vec(),
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout)
            .unwrap_or_else(|_| panic!("`crikey {args:?}` wrote non-UTF-8 to stdout")),
        stderr: String::from_utf8(output.stderr)
            .unwrap_or_else(|_| panic!("`crikey {args:?}` wrote non-UTF-8 to stderr")),
    }
}

fn build_package(plugin: &Path, output: &Path) -> Run {
    let args = vec![
        "package".to_owned(),
        "build".to_owned(),
        "--plugin".to_owned(),
        display(plugin),
        "--out".to_owned(),
        display(output),
    ];
    run_owned(&args)
}

fn inspect_package(package: &Path) -> Run {
    let args = vec![
        "package".to_owned(),
        "inspect".to_owned(),
        "--package".to_owned(),
        display(package),
    ];
    run_owned(&args)
}

fn verify_package(package: &Path, expected_hash: Option<&str>) -> Run {
    let mut args = vec![
        "package".to_owned(),
        "verify".to_owned(),
        "--package".to_owned(),
        display(package),
    ];
    if let Some(expected_hash) = expected_hash {
        args.push("--expect-hash".to_owned());
        args.push(expected_hash.to_owned());
    }
    run_owned(&args)
}

fn assert_no_panic(run: &Run) {
    assert_ne!(
        run.code,
        Some(PANIC_STATUS),
        "package command must not unwind{run}"
    );
    assert!(
        !run.stderr.contains("panicked at"),
        "package command must not print a panic backtrace{run}"
    );
}

fn assert_completed(run: &Run, code: i32) {
    assert_no_panic(run);
    assert_eq!(run.code, Some(code), "unexpected package command status{run}");
    assert!(
        !run.stdout.trim().is_empty(),
        "the package command must print a report{run}"
    );
}

fn assert_usage(run: &Run) {
    assert_no_panic(run);
    assert_eq!(
        run.code,
        Some(EX_USAGE),
        "bad package arguments must exit {EX_USAGE}{run}"
    );
    assert!(
        run.stdout.trim().is_empty(),
        "usage errors must not print a package report{run}"
    );
    assert!(
        !run.stderr.trim().is_empty(),
        "usage errors need a diagnostic{run}"
    );
}

// ---------------------------------------------------------------------------
// Reading the frozen key=value output
// ---------------------------------------------------------------------------

/// One printed line, split into its whitespace-safe fields.
#[derive(Debug)]
struct Record {
    line: usize,
    fields: Vec<(String, String)>,
}

impl Record {
    fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    fn need(&self, key: &str, run: &Run) -> &str {
        self.get(key)
            .unwrap_or_else(|| panic!("line {} has no `{key}` field{run}", self.line))
    }

    fn is_detail(&self) -> bool {
        self.get("entry").is_some()
    }
}

fn parse(run: &Run) -> Vec<Record> {
    run.stdout
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            let number = index + 1;
            let mut seen = BTreeSet::new();
            let fields = line
                .split_whitespace()
                .map(|token| {
                    let (key, value) = token
                        .split_once('=')
                        .unwrap_or_else(|| panic!("line {number}: `{token}` is not `key=value`{run}"));
                    assert!(!key.is_empty(), "line {number}: empty output key{run}");
                    assert!(
                        !value.contains('='),
                        "line {number}: bare `=` in `{token}`; values must use `%3D`{run}"
                    );
                    assert!(
                        seen.insert(key.to_owned()),
                        "line {number}: repeated key `{key}` makes the line ambiguous{run}"
                    );
                    (key.to_owned(), value.to_owned())
                })
                .collect::<Vec<_>>();
            assert!(!fields.is_empty(), "line {number}: no fields{run}");
            Record { line: number, fields }
        })
        .collect()
}

fn summary(records: &[Record], run: &Run) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    for record in records.iter().filter(|record| !record.is_detail()) {
        for (key, value) in &record.fields {
            assert!(
                fields.insert(key.clone(), value.clone()).is_none(),
                "line {}: summary key `{key}` was reported twice{run}",
                record.line
            );
        }
    }
    fields
}

fn field<'a>(summary: &'a BTreeMap<String, String>, key: &str, run: &Run) -> &'a str {
    summary
        .get(key)
        .unwrap_or_else(|| panic!("the report has no summary field `{key}`{run}"))
        .as_str()
}

fn number(summary: &BTreeMap<String, String>, key: &str, run: &Run) -> u64 {
    let raw = field(summary, key, run);
    raw.parse()
        .unwrap_or_else(|_| panic!("summary `{key}={raw}` is not a whole number{run}"))
}

fn entries(records: &[Record]) -> Vec<&Record> {
    records
        .iter()
        .filter(|record| record.get("entry").is_some())
        .collect()
}

/// Decodes uppercase percent escapes used for paths and summary values.
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value
                .get(index + 1..index + 3)
                .unwrap_or_else(|| panic!("`{value}` ends inside a percent escape"));
            assert!(
                hex.chars()
                    .all(|digit| digit.is_ascii_digit() || digit.is_ascii_uppercase()),
                "`{value}` uses lowercase percent escapes"
            );
            let byte = u8::from_str_radix(hex, 16)
                .unwrap_or_else(|_| panic!("`{value}` contains invalid escape `%{hex}`"));
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| panic!("`{value}` is not UTF-8 after decoding"))
}

fn assert_sha256(value: &str, run: &Run) {
    assert_eq!(value.len(), 64, "`hash=` must contain 64 characters{run}");
    assert!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "`hash={value}` is not lowercase hexadecimal{run}"
    );
}

fn wrong_hash(actual: &str) -> String {
    let replacement = if actual.starts_with('0') { '1' } else { '0' };
    let mut wrong = actual.to_owned();
    wrong.replace_range(0..1, &replacement.to_string());
    wrong
}

// ---------------------------------------------------------------------------
// Fixture on disk
// ---------------------------------------------------------------------------

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "crikey-m5-package-cli-{pid}-{ordinal}-{label}",
            pid = std::process::id(),
            ordinal = NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&path)
            .unwrap_or_else(|error| panic!("could not create {}: {error}", path.display()));
        Self { path }
    }

    fn subdir(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::create_dir_all(&path)
            .unwrap_or_else(|error| panic!("could not create {}: {error}", path.display()));
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn write(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("could not create {}: {error}", parent.display()));
    }
    fs::write(path, contents).unwrap_or_else(|error| panic!("could not write {}: {error}", path.display()));
}

fn display(path: &Path) -> String {
    path.to_str()
        .unwrap_or_else(|| panic!("{} is not valid UTF-8", path.display()))
        .to_owned()
}

/// Writes a native package manifest and one unsigned binary entry.
fn native_plugin(scratch: &Scratch) -> (PathBuf, PathBuf) {
    let plugin = scratch.subdir("native-plugin");
    let manifest = format!(
        "manifest-version = 1\n\n\
         [plugin]\n\
         id = \"dev.crikey.m5.native-package\"\n\
         name = \"M5 Native Package\"\n\
         version = \"1.2.3\"\n\
         runtime = \"native\"\n\
         entrypoint = \"bin/m5-plugin\"\n\n\
         [platform]\n\
         os = [\"{}\"]\n\
         arch = [\"{}\"]\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
    write(&plugin.join("crikey.toml"), manifest.as_bytes());
    let binary = plugin.join("bin/m5-plugin");
    write(&binary, b"CRIKEY M5 NATIVE FIXTURE\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&binary)
            .unwrap_or_else(|error| panic!("could not stat {}: {error}", binary.display()))
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&binary, permissions)
            .unwrap_or_else(|error| panic!("could not mark {} executable: {error}", binary.display()));
    }
    let package = scratch.path.join("m5-native.crikey-package");
    (plugin, package)
}

fn build_fixture(scratch: &Scratch, label: &str) -> (PathBuf, PathBuf, Run, BTreeMap<String, String>) {
    let (plugin, package) = native_plugin(scratch);
    let run = build_package(&plugin, &package);
    assert_completed(&run, EX_OK);
    let records = parse(&run);
    let report = summary(&records, &run);
    assert_eq!(decode(field(&report, "package", &run)), display(&package));
    assert!(
        number(&report, "entries", &run) >= 2,
        "manifest and bin entries are required{run}"
    );
    assert_sha256(field(&report, "hash", &run), &run);
    assert!(
        package.is_file(),
        "package build did not create {} ({label})",
        package.display()
    );
    (plugin, package, run, report)
}

// ---------------------------------------------------------------------------
// Build and inspect
// ---------------------------------------------------------------------------

#[test]
fn package_build_emits_archive_path_entry_count_and_sha256() {
    let scratch = Scratch::new("build");
    let (_plugin, package, run, report) = build_fixture(&scratch, "build");
    assert_eq!(field(&report, "package", &run), display(&package));
    assert!(number(&report, "entries", &run) >= 2);
    assert_sha256(field(&report, "hash", &run), &run);
}

#[test]
fn package_inspect_reports_manifest_identity_and_each_archive_member() {
    let scratch = Scratch::new("inspect");
    let (_plugin, package, _build, _build_report) = build_fixture(&scratch, "inspect");
    let run = inspect_package(&package);
    assert_completed(&run, EX_OK);
    let records = parse(&run);
    let report = summary(&records, &run);
    assert_eq!(field(&report, "plugin", &run), "dev.crikey.m5.native-package");
    assert_eq!(field(&report, "version", &run), "1.2.3");
    assert_eq!(field(&report, "runtime", &run), "native");
    assert_eq!(field(&report, "platform", &run), std::env::consts::OS);
    assert_eq!(field(&report, "arch", &run), std::env::consts::ARCH);

    let members = entries(&records);
    assert_eq!(members.len() as u64, number(&report, "entries", &run));
    let mut indexes = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for member in members {
        let index = member
            .need("entry", &run)
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("entry index is not numeric{run}"));
        assert!(indexes.insert(index), "archive entry index {index} repeats{run}");
        let bytes = member
            .need("bytes", &run)
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("entry byte count is not numeric{run}"));
        assert!(bytes > 0, "archive member bytes must be positive{run}");
        paths.insert(decode(member.need("path", &run)));
    }
    assert!(
        paths.contains("crikey.toml"),
        "manifest must be an archive member{run}"
    );
    assert!(
        paths.contains("bin/m5-plugin"),
        "native binary must be an archive member{run}"
    );
}

// ---------------------------------------------------------------------------
// Verify, hash mismatch and corruption
// ---------------------------------------------------------------------------

#[test]
fn package_verify_marks_unsigned_binary_but_accepts_it_as_valid() {
    let scratch = Scratch::new("verify");
    let (_plugin, package, build, build_report) = build_fixture(&scratch, "verify");
    let valid = verify_package(&package, None);
    assert_completed(&valid, EX_OK);
    let valid_records = parse(&valid);
    let valid_report = summary(&valid_records, &valid);
    assert_eq!(field(&valid_report, "verdict", &valid), "valid");
    assert_eq!(field(&valid_report, "signed", &valid), "false");
    assert_eq!(field(&valid_report, "unsigned_binary", &valid), "true");
    assert_eq!(
        field(&valid_report, "hash", &valid),
        field(&build_report, "hash", &build),
        "verify must report the archive hash emitted by build"
    );

    let wrong = wrong_hash(field(&build_report, "hash", &build));
    let invalid = verify_package(&package, Some(&wrong));
    assert_completed(&invalid, EX_INVALID);
    let invalid_report = summary(&parse(&invalid), &invalid);
    assert_eq!(field(&invalid_report, "verdict", &invalid), "invalid");
}

#[test]
fn package_verify_rejects_a_truncated_archive_without_panicking() {
    let scratch = Scratch::new("corrupt");
    let (_plugin, package, _build, _build_report) = build_fixture(&scratch, "corrupt");
    let bytes =
        fs::read(&package).unwrap_or_else(|error| panic!("could not read {}: {error}", package.display()));
    assert!(
        bytes.len() > 8,
        "the package fixture must produce a non-trivial archive"
    );
    let corrupt = scratch.path.join("truncated.crikey-package");
    write(&corrupt, &bytes[..8]);

    let run = verify_package(&corrupt, None);
    assert_completed(&run, EX_INVALID);
    let report = summary(&parse(&run), &run);
    assert_eq!(field(&report, "verdict", &run), "invalid");
}

// ---------------------------------------------------------------------------
// Status distinctions and required arguments
// ---------------------------------------------------------------------------

#[test]
fn package_statuses_distinguish_migration_unknown_subcommands_and_missing_flags() {
    let migrate = run(&["package", "migrate-keypirinha"]);
    assert_no_panic(&migrate);
    assert_eq!(
        migrate.code,
        Some(EX_UNAVAILABLE),
        "migration remains unimplemented{migrate}"
    );

    let unknown = run(&["package", "not-a-command"]);
    assert_usage(&unknown);

    let missing = [
        vec!["package", "build"],
        vec!["package", "build", "--plugin"],
        vec!["package", "verify"],
        vec!["package", "verify", "--package"],
        vec!["package", "inspect"],
        vec!["package", "inspect", "--package"],
    ];
    for args in missing {
        let run = run(&args);
        assert_usage(&run);
    }
}
