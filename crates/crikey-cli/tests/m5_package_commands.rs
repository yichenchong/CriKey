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
    capture(args, output)
}

/// Runs the binary with the CriKey directory overrides pointed inside `root`.
///
/// Every command that consults the trust store reads the config root. A test
/// that left that pointing at the developer's own home directory would trust
/// whatever keys that machine happens to hold, and would pass or fail depending
/// on whose machine it ran on.
fn run_owned_in(root: &Path, args: &[String]) -> Run {
    let output = Command::new(CRIKEY)
        .args(args)
        .env("HOME", root)
        .env("CRIKEY_CONFIG_DIR", root.join("config"))
        .env("CRIKEY_DATA_DIR", root.join("data"))
        .env("CRIKEY_CACHE_DIR", root.join("cache"))
        .env("CRIKEY_STATE_DIR", root.join("state"))
        .output()
        .unwrap_or_else(|error| panic!("could not execute `{CRIKEY}` with {args:?}: {error}"));
    capture(args, output)
}

fn capture(args: &[String], output: std::process::Output) -> Run {
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

/// Verifies inside `scratch`'s own config root, so the trust store under test is
/// the one the test wrote and nothing else.
fn verify_package(
    scratch: &Scratch,
    package: &Path,
    expected_hash: Option<&str>,
    policy: Option<&str>,
) -> Run {
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
    if let Some(policy) = policy {
        args.push("--unsigned-policy".to_owned());
        args.push(policy.to_owned());
    }
    run_owned_in(&scratch.path, &args)
}

/// Generates a signing key pair and returns `(key file, public key, fingerprint)`.
fn keygen(scratch: &Scratch, label: &str) -> (PathBuf, String, String) {
    let key = scratch.path.join(format!("{label}.key"));
    let run = run_owned_in(
        &scratch.path,
        &[
            "package".to_owned(),
            "keygen".to_owned(),
            "--out".to_owned(),
            display(&key),
        ],
    );
    assert_completed(&run, EX_OK);
    let report = summary(&parse(&run), &run);
    assert_eq!(field(&report, "verdict", &run), "generated");
    let public = field(&report, "public-key", &run).to_owned();
    let fingerprint = field(&report, "fingerprint", &run).to_owned();
    assert_eq!(public.len(), 64, "a public key is 64 hex characters{run}");
    assert_eq!(fingerprint.len(), 32, "a fingerprint is 32 hex characters{run}");
    (key, public, fingerprint)
}

fn trust_add(scratch: &Scratch, name: &str, public: &str) -> Run {
    run_owned_in(
        &scratch.path,
        &[
            "package".to_owned(),
            "trust-add".to_owned(),
            "--name".to_owned(),
            name.to_owned(),
            "--key".to_owned(),
            public.to_owned(),
        ],
    )
}

fn sign(scratch: &Scratch, package: &Path, key: &Path) -> Run {
    run_owned_in(
        &scratch.path,
        &[
            "package".to_owned(),
            "sign".to_owned(),
            "--package".to_owned(),
            display(package),
            "--key".to_owned(),
            display(key),
        ],
    )
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

    /// Whether this line is one repeated row rather than part of the summary.
    ///
    /// `entry=` numbers archive members; `key=` numbers trusted keys. Both repeat,
    /// so folding either into the summary would make a second row look like a
    /// duplicated summary field.
    fn is_detail(&self) -> bool {
        self.get("entry").is_some() || self.get("key").is_some()
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
    // Decoded before comparing, like every other path assertion here: the
    // output contract percent-encodes, and a Windows path is full of
    // backslashes, so comparing the raw field passes only where the path
    // happens to need no escaping.
    assert_eq!(decode(field(&report, "package", &run)), display(&package));
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

/// The `bin/*.sig` marker and package provenance are separate facts.
///
/// `unsigned_binary` has always meant "an executable under `bin/` has no sibling
/// `<name>.sig`", which is a note about the payload. `signature=` is the new
/// fact: whether a detached signature over the *whole package* verified against
/// a trusted key. A freshly built package has neither, and the report says so in
/// both words rather than conflating them.
#[test]
fn package_verify_marks_unsigned_binary_and_reports_the_package_as_unsigned() {
    let scratch = Scratch::new("verify");
    let (_plugin, package, build, build_report) = build_fixture(&scratch, "verify");
    let valid = verify_package(&scratch, &package, None, Some("allow"));
    assert_completed(&valid, EX_OK);
    let valid_records = parse(&valid);
    let valid_report = summary(&valid_records, &valid);
    assert_eq!(field(&valid_report, "verdict", &valid), "valid");
    assert_eq!(field(&valid_report, "signature", &valid), "unsigned");
    assert_eq!(
        field(&valid_report, "fingerprint", &valid),
        "",
        "an unsigned package has no signer to name{valid}"
    );
    assert_eq!(field(&valid_report, "unsigned_binary", &valid), "true");
    assert_eq!(
        field(&valid_report, "hash", &valid),
        field(&build_report, "hash", &build),
        "verify must report the archive hash emitted by build"
    );

    let wrong = wrong_hash(field(&build_report, "hash", &build));
    let invalid = verify_package(&scratch, &package, Some(&wrong), Some("allow"));
    assert_completed(&invalid, EX_INVALID);
    let invalid_report = summary(&parse(&invalid), &invalid);
    assert_eq!(field(&invalid_report, "verdict", &invalid), "invalid");
}

/// A signed package names its signer, and the fingerprint is the same one
/// `keygen` printed and `trust-add` confirmed.
///
/// This is the whole point of the feature: before it, a party who rebuilt the
/// archive and rewrote the embedded lock to match produced a package that
/// verified, because nothing in the package said who built it.
#[test]
fn package_verify_reports_a_trusted_signature_and_its_fingerprint() {
    let scratch = Scratch::new("signed");
    let (_plugin, package, _build, _report) = build_fixture(&scratch, "signed");
    let (key, public, fingerprint) = keygen(&scratch, "publisher");

    let trusted = trust_add(&scratch, "publisher", &public);
    assert_completed(&trusted, EX_OK);
    let trusted_report = summary(&parse(&trusted), &trusted);
    assert_eq!(field(&trusted_report, "verdict", &trusted), "trusted");
    assert_eq!(field(&trusted_report, "fingerprint", &trusted), fingerprint);

    let signed = sign(&scratch, &package, &key);
    assert_completed(&signed, EX_OK);
    let signed_report = summary(&parse(&signed), &signed);
    assert_eq!(field(&signed_report, "verdict", &signed), "signed");
    assert_eq!(field(&signed_report, "fingerprint", &signed), fingerprint);

    // Refuse is the default, and a signed package satisfies it without a flag.
    let verified = verify_package(&scratch, &package, None, None);
    assert_completed(&verified, EX_OK);
    let report = summary(&parse(&verified), &verified);
    assert_eq!(field(&report, "verdict", &verified), "valid");
    assert_eq!(field(&report, "signature", &verified), "trusted");
    assert_eq!(field(&report, "signer", &verified), "publisher");
    assert_eq!(field(&report, "fingerprint", &verified), fingerprint);
}

/// All three unsigned-package policies, and the default.
#[test]
fn package_verify_applies_every_unsigned_policy_and_defaults_to_refusing() {
    let scratch = Scratch::new("policy");
    let (_plugin, package, _build, _report) = build_fixture(&scratch, "policy");

    for policy in [None, Some("refuse")] {
        let refused = verify_package(&scratch, &package, None, policy);
        assert_completed(&refused, EX_INVALID);
        let report = summary(&parse(&refused), &refused);
        assert_eq!(field(&report, "verdict", &refused), "invalid");
        assert_eq!(
            field(&report, "signature", &refused),
            "unsigned",
            "the refusal must say why{refused}"
        );
        assert!(
            refused.stderr.contains("no detached signature"),
            "the refusal must name the missing signature{refused}"
        );
    }

    let warned = verify_package(&scratch, &package, None, Some("warn"));
    assert_completed(&warned, EX_OK);
    assert_eq!(
        field(&summary(&parse(&warned), &warned), "verdict", &warned),
        "valid"
    );
    assert!(
        warned.stderr.contains("no detached signature"),
        "`warn` must actually warn{warned}"
    );

    let allowed = verify_package(&scratch, &package, None, Some("allow"));
    assert_completed(&allowed, EX_OK);
    assert_eq!(
        field(&summary(&parse(&allowed), &allowed), "verdict", &allowed),
        "valid"
    );
    assert!(
        !allowed.stderr.contains("no detached signature"),
        "`allow` is the silent one{allowed}"
    );

    let unknown = verify_package(&scratch, &package, None, Some("maybe"));
    assert_completed(&unknown, EX_INVALID);
    assert!(
        unknown.stderr.contains("maybe"),
        "an unknown policy must be named, not silently treated as the default{unknown}"
    );
}

/// An unknown signer is refused as *untrusted*, which is a different answer
/// from *invalid* and has a different remedy.
#[test]
fn package_verify_refuses_an_unknown_signer_as_untrusted_rather_than_invalid() {
    let scratch = Scratch::new("untrusted");
    let (_plugin, package, _build, _report) = build_fixture(&scratch, "untrusted");
    let (key, _public, fingerprint) = keygen(&scratch, "stranger");
    assert_completed(&sign(&scratch, &package, &key), EX_OK);

    let run = verify_package(&scratch, &package, None, None);
    assert_completed(&run, EX_INVALID);
    let report = summary(&parse(&run), &run);
    assert_eq!(field(&report, "verdict", &run), "invalid");
    assert_eq!(
        field(&report, "signature", &run),
        "untrusted",
        "a valid signature by an unknown key is untrusted, not invalid{run}"
    );
    assert_eq!(
        field(&report, "fingerprint", &run),
        fingerprint,
        "the refusal must name the key the operator would have to trust{run}"
    );
    assert!(
        run.stderr.contains(&fingerprint) && run.stderr.contains("trust store"),
        "the diagnostic must name the key and the store{run}"
    );
}

/// A signature over one package does not carry over to another.
///
/// The signature covers every member's name and digest, so rebuilding the
/// package with different bytes and keeping the old `.sig` is refused even
/// though the key is trusted and the embedded lock is internally consistent.
#[test]
fn package_verify_refuses_a_signature_that_no_longer_covers_the_package() {
    let scratch = Scratch::new("tampered");
    let (plugin, package, _build, _report) = build_fixture(&scratch, "tampered");
    let (key, public, fingerprint) = keygen(&scratch, "publisher");
    assert_completed(&trust_add(&scratch, "publisher", &public), EX_OK);
    assert_completed(&sign(&scratch, &package, &key), EX_OK);
    assert_completed(&verify_package(&scratch, &package, None, None), EX_OK);

    // Rebuild with a different payload, leaving the old signature in place. The
    // rebuilt package's own lock matches its own bytes, which is exactly the
    // attack the embedded lock alone could not see.
    write(&plugin.join("bin/m5-plugin"), b"HOSTILE REPLACEMENT PAYLOAD\n");
    assert_completed(&build_package(&plugin, &package), EX_OK);

    let run = verify_package(&scratch, &package, None, None);
    assert_completed(&run, EX_INVALID);
    let report = summary(&parse(&run), &run);
    assert_eq!(field(&report, "verdict", &run), "invalid");
    assert_eq!(field(&report, "signature", &run), "invalid");
    assert_eq!(field(&report, "fingerprint", &run), fingerprint);
    assert!(
        run.stderr.contains(&fingerprint) && run.stderr.contains(&display(&package)),
        "the refusal must name the artefact and the key{run}"
    );
}

/// A signature file that is truncated, oversized or not a signature at all is
/// refused, and refused without reading it into memory whole.
#[test]
fn package_verify_refuses_malformed_signature_files_without_a_large_allocation() {
    let scratch = Scratch::new("sigfile");
    let (_plugin, package, _build, _report) = build_fixture(&scratch, "sigfile");
    let (key, public, _fingerprint) = keygen(&scratch, "publisher");
    assert_completed(&trust_add(&scratch, "publisher", &public), EX_OK);
    assert_completed(&sign(&scratch, &package, &key), EX_OK);

    let signature = PathBuf::from(format!("{}.sig", display(&package)));
    let good = fs::read(&signature).expect("the signature was written");

    // Truncated: the TOML no longer parses, or the hex is short.
    write(&signature, &good[..good.len() / 2]);
    let truncated = verify_package(&scratch, &package, None, None);
    assert_completed(&truncated, EX_INVALID);
    assert_eq!(
        field(&summary(&parse(&truncated), &truncated), "verdict", &truncated),
        "invalid"
    );

    // Oversized: five megabytes of padding inside an otherwise valid document.
    // Refused on its length, so the process never allocates the file.
    let mut oversized = good.clone();
    oversized.extend(std::iter::repeat_n(b'#', 5 * 1024 * 1024));
    write(&signature, &oversized);
    let too_big = verify_package(&scratch, &package, None, None);
    assert_completed(&too_big, EX_INVALID);
    assert!(
        too_big.stderr.contains("limit"),
        "an oversized signature must be refused on its size{too_big}"
    );

    // A public key of the wrong length is not a key.
    write(
        &signature,
        b"version = 1\npublic-key = \"abcd\"\nsignature = \"beef\"\n",
    );
    let short_key = verify_package(&scratch, &package, None, None);
    assert_completed(&short_key, EX_INVALID);
}

/// `keygen` never overwrites and never prints the private half.
#[test]
fn package_keygen_refuses_to_overwrite_and_never_prints_the_signing_key() {
    let scratch = Scratch::new("keygen");
    let (key, public, fingerprint) = keygen(&scratch, "author");
    let secret = fs::read_to_string(&key).expect("the signing key was written");
    let secret = secret.trim().to_owned();
    assert_eq!(secret.len(), 64, "a signing key is 64 hex characters");
    assert_ne!(secret, public, "the private half is not the public half");

    let again = run_owned_in(
        &scratch.path,
        &[
            "package".to_owned(),
            "keygen".to_owned(),
            "--out".to_owned(),
            display(&key),
        ],
    );
    assert_no_panic(&again);
    assert_eq!(again.code, Some(EX_INVALID));
    assert!(
        again.stderr.contains("overwrite"),
        "an existing signing key must never be replaced{again}"
    );
    assert_eq!(
        fs::read_to_string(&key).expect("still readable").trim(),
        secret,
        "the original key must be untouched"
    );

    // A second key must be a different key, and neither half of it may reach
    // stdout or stderr except the public one.
    let second_path = scratch.path.join("second.key");
    let first = run_owned_in(
        &scratch.path,
        &[
            "package".to_owned(),
            "keygen".to_owned(),
            "--out".to_owned(),
            display(&second_path),
        ],
    );
    assert_completed(&first, EX_OK);
    let written = fs::read_to_string(&second_path).expect("written");
    let written = written.trim();
    assert_ne!(written, secret, "two keygen runs must not produce one key");
    assert!(
        !first.stdout.contains(written) && !first.stderr.contains(written),
        "keygen must never print the signing key{first}"
    );
    let second_report = summary(&parse(&first), &first);
    assert_ne!(
        field(&second_report, "fingerprint", &first),
        fingerprint,
        "a different key has a different fingerprint{first}"
    );

    // `--out` has no default: a private key must never land somewhere unnamed.
    let no_out = run_owned_in(&scratch.path, &["package".to_owned(), "keygen".to_owned()]);
    assert_usage(&no_out);
}

/// A signing key may come from the environment, which is how CI signs without
/// ever writing a private key to a checkout.
#[test]
fn package_sign_reads_a_signing_key_from_the_environment() {
    let scratch = Scratch::new("keyenv");
    let (key, public, fingerprint) = keygen(&scratch, "ci");
    let (_plugin, package, _build, _report) = build_fixture(&scratch, "keyenv");
    let secret = fs::read_to_string(&key).expect("readable");
    let secret = secret.trim().to_owned();
    assert_completed(&trust_add(&scratch, "ci", &public), EX_OK);

    let args = vec![
        "package".to_owned(),
        "sign".to_owned(),
        "--package".to_owned(),
        display(&package),
        "--key-env".to_owned(),
        "CRIKEY_TEST_SIGNING_KEY".to_owned(),
    ];
    let output = Command::new(CRIKEY)
        .args(&args)
        .env("HOME", &scratch.path)
        .env("CRIKEY_CONFIG_DIR", scratch.path.join("config"))
        .env("CRIKEY_TEST_SIGNING_KEY", &secret)
        .output()
        .expect("crikey runs");
    let signed = capture(&args, output);
    assert_completed(&signed, EX_OK);
    let report = summary(&parse(&signed), &signed);
    assert_eq!(field(&report, "verdict", &signed), "signed");
    assert_eq!(field(&report, "fingerprint", &signed), fingerprint);
    assert!(
        !signed.stdout.contains(&secret) && !signed.stderr.contains(&secret),
        "the signing key must never be echoed{signed}"
    );

    assert_completed(&verify_package(&scratch, &package, None, None), EX_OK);

    // An unset variable is a named refusal, not a fall back to some other key.
    let unset = run_owned_in(
        &scratch.path,
        &[
            "package".to_owned(),
            "sign".to_owned(),
            "--package".to_owned(),
            display(&package),
            "--key-env".to_owned(),
            "CRIKEY_TEST_SIGNING_KEY_THAT_IS_UNSET".to_owned(),
        ],
    );
    assert_no_panic(&unset);
    assert_eq!(unset.code, Some(EX_INVALID));
    assert!(
        unset.stderr.contains("CRIKEY_TEST_SIGNING_KEY_THAT_IS_UNSET"),
        "the refusal must name the variable{unset}"
    );

    // `--key` and `--key-env` together is a usage error, not a silent preference.
    let both = run_owned_in(
        &scratch.path,
        &[
            "package".to_owned(),
            "sign".to_owned(),
            "--package".to_owned(),
            display(&package),
            "--key".to_owned(),
            display(&key),
            "--key-env".to_owned(),
            "CRIKEY_TEST_SIGNING_KEY".to_owned(),
        ],
    );
    assert_usage(&both);
}

/// Trusting, listing and untrusting a key round-trips through the config root.
#[test]
fn package_trust_add_list_and_remove_round_trip_through_the_config_root() {
    let scratch = Scratch::new("trust");
    let (_key, public, fingerprint) = keygen(&scratch, "publisher");

    let empty = run_owned_in(&scratch.path, &["package".to_owned(), "trust-list".to_owned()]);
    assert_completed(&empty, EX_OK);
    assert_eq!(
        field(&summary(&parse(&empty), &empty), "keys", &empty),
        "0",
        "an operator who has trusted nobody has an empty store, not an error{empty}"
    );

    assert_completed(&trust_add(&scratch, "publisher", &public), EX_OK);

    let listed = run_owned_in(&scratch.path, &["package".to_owned(), "trust-list".to_owned()]);
    assert_completed(&listed, EX_OK);
    let records = parse(&listed);
    assert_eq!(field(&summary(&records, &listed), "keys", &listed), "1");
    let entry = records
        .iter()
        .find(|record| record.get("key").is_some())
        .unwrap_or_else(|| panic!("the trusted key must be listed{listed}"));
    assert_eq!(entry.need("name", &listed), "publisher");
    assert_eq!(entry.need("fingerprint", &listed), fingerprint);
    assert_eq!(entry.need("public-key", &listed), public);

    // One key, one name: trusting it twice would make revoking it once a
    // half-revocation.
    let duplicate = trust_add(&scratch, "another-name", &public);
    assert_no_panic(&duplicate);
    assert_eq!(duplicate.code, Some(EX_INVALID));
    assert!(duplicate.stderr.contains(&fingerprint), "{duplicate}");

    let removed = run_owned_in(
        &scratch.path,
        &[
            "package".to_owned(),
            "trust-remove".to_owned(),
            "--name".to_owned(),
            "publisher".to_owned(),
        ],
    );
    assert_completed(&removed, EX_OK);
    let report = summary(&parse(&removed), &removed);
    assert_eq!(field(&report, "verdict", &removed), "removed");
    assert_eq!(
        field(&report, "fingerprint", &removed),
        fingerprint,
        "removal must name the key that went{removed}"
    );
    assert_eq!(field(&report, "keys", &removed), "0");

    let absent = run_owned_in(
        &scratch.path,
        &[
            "package".to_owned(),
            "trust-remove".to_owned(),
            "--name".to_owned(),
            "publisher".to_owned(),
        ],
    );
    assert_no_panic(&absent);
    assert_eq!(absent.code, Some(EX_INVALID));
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

    let run = verify_package(&scratch, &corrupt, None, Some("allow"));
    assert_completed(&run, EX_INVALID);
    let report = summary(&parse(&run), &run);
    assert_eq!(field(&report, "verdict", &run), "invalid");
}

#[test]
fn package_commands_reject_nonexistent_paths_with_diagnostics() {
    let scratch = Scratch::new("missing-path");
    let absent_plugin = scratch.path.join("missing-plugin");
    let absent_package = scratch.path.join("missing.crikey-package");

    let build = build_package(&absent_plugin, &absent_package);
    assert_completed(&build, EX_INVALID);
    assert!(
        build.stderr.contains("missing-plugin"),
        "build diagnostics must name the missing plugin directory{build}"
    );

    let inspect = inspect_package(&absent_package);
    assert_completed(&inspect, EX_INVALID);
    assert!(
        inspect.stderr.contains("missing.crikey-package"),
        "inspect diagnostics must name the missing archive{inspect}"
    );

    let verify = verify_package(&scratch, &absent_package, None, Some("allow"));
    assert_completed(&verify, EX_INVALID);
    assert!(
        verify.stderr.contains("missing.crikey-package"),
        "verify diagnostics must name the missing archive{verify}"
    );
}

// ---------------------------------------------------------------------------
// Status distinctions and required arguments
// ---------------------------------------------------------------------------

/// No `package` subcommand answers `EX_UNAVAILABLE` any more.
///
/// That status is reserved for a command that is advertised and unbuilt, and
/// `migrate-keypirinha` is now built. A script that treats 69 as "not
/// implemented" must never see it from this family again — its own behaviour is
/// pinned in `m7_plugin_commands.rs`, which owns the migration report.
#[test]
fn package_statuses_distinguish_unknown_subcommands_and_missing_flags() {
    let migrate = run(&["package", "migrate-keypirinha"]);
    assert_no_panic(&migrate);
    assert_ne!(
        migrate.code,
        Some(EX_UNAVAILABLE),
        "migration is implemented, so it may not report itself unavailable{migrate}"
    );
    assert_usage(&migrate);

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
