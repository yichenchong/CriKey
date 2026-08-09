//! Black-box tests for `crikey plugin search`, `show`, `index update` and
//! installation by an indexed id (spec 2.2, 21.2, 23.1; ADR-0013).
//!
//! Every assertion consumes only the frozen percent-encoded `key=value` surface
//! and the exit status, exactly as `m7_plugin_commands.rs` does. Every index is
//! a signed document on disk reached through a `file://` URL: no test opens a
//! socket, and "the network is unavailable" is the file the URL named being
//! deleted.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use crikey_package_manager::{package_digest, PackageSigningKey, TrustStore};

/// The binary under test, as built by cargo for this integration target.
const CRIKEY: &str = env!("CARGO_BIN_EXE_crikey");

/// A completed operation that found nothing wrong.
const EX_OK: i32 = 0;
/// A completed operation that reached a bad verdict.
const EX_INVALID: i32 = 1;
/// The Rust runtime's status for an unwound panic.
const PANIC_STATUS: i32 = 101;

/// The 64-character SHA-256 of the empty file. Published by fixtures whose
/// package bytes are deliberately something else.
const EMPTY_DIGEST: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

// ---------------------------------------------------------------------------
// Running the binary
// ---------------------------------------------------------------------------

/// A private directory tree removed when the test that made it ends.
struct Host {
    path: PathBuf,
}

impl Host {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "crikey-plugin-index-cli-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");
        Self { path }
    }

    fn subdir(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::create_dir_all(&path).expect("fixture subdirectory is creatable");
        path
    }

    fn config_dir(&self) -> PathBuf {
        self.subdir("config")
    }

    /// Writes the user configuration layer this host runs with.
    fn configure(&self, urls: &[String], max_age_seconds: Option<u64>) {
        let mut body = format!(
            "[launcher]\nplugin-index-urls = \"{}\"\n",
            urls.join(",").replace('\\', "\\\\")
        );
        if let Some(seconds) = max_age_seconds {
            body.push_str(&format!("plugin-index-max-age-seconds = \"{seconds}\"\n"));
        }
        fs::write(self.config_dir().join("config.toml"), body).expect("configuration is writable");
    }

    /// Records `key` as a trusted publisher in this host's trust store.
    fn trust(&self, name: &str, key: &PackageSigningKey) {
        let mut store = TrustStore::empty();
        store
            .add(name, key.public_key())
            .expect("the first key is distinct");
        store
            .save_to(&self.config_dir().join("trusted-keys.toml"))
            .expect("the trust store is writable");
    }

    fn run(&self, args: &[&str]) -> Run {
        let mut command = Command::new(CRIKEY);
        command.args(args);
        command.env("CRIKEY_CONFIG_DIR", self.path.join("config"));
        command.env("CRIKEY_DATA_DIR", self.path.join("data"));
        command.env("CRIKEY_CACHE_DIR", self.path.join("cache"));
        command.env("CRIKEY_STATE_DIR", self.path.join("state"));
        command.env("CRIKEY_LEGACY_CACHE_ROOT", self.path.join("legacy-cache"));
        command.env_remove("CRIKEY_LEGACY_PACKAGE_ROOTS");
        command.env_remove("CRIKEY_MODERN_PLUGIN_ROOTS");
        command.env_remove("CRIKEY_NATIVE_PLUGIN_ROOTS");
        let output = command.output().expect("the crikey binary runs");
        Run {
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// One completed invocation, retained so an assertion failure shows all output.
#[derive(Debug)]
struct Run {
    args: Vec<String>,
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl fmt::Display for Run {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "crikey {}\n  status: {:?}\n  stdout:\n{}\n  stderr:\n{}",
            self.args.join(" "),
            self.status,
            indent(&self.stdout),
            indent(&self.stderr)
        )
    }
}

fn indent(text: &str) -> String {
    if text.is_empty() {
        return "    <empty>".to_owned();
    }
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Asserts the process exited with `code` and did not panic.
fn assert_completed(run: &Run, code: i32) {
    assert_ne!(
        run.status,
        Some(PANIC_STATUS),
        "the command must never panic; {run}"
    );
    assert!(
        !run.stderr.contains("panicked at"),
        "the command must never panic; {run}"
    );
    assert_eq!(run.status, Some(code), "unexpected exit status; {run}");
}

// ---------------------------------------------------------------------------
// Reading the frozen key=value output
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Record {
    fields: BTreeMap<String, String>,
}

impl Record {
    fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    fn field(&self, key: &str, run: &Run) -> &str {
        self.get(key)
            .unwrap_or_else(|| panic!("line is missing `{key}`: {:?}; {run}", self.fields))
    }
}

fn parse(run: &Run) -> Vec<Record> {
    run.stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = BTreeMap::new();
            for token in line.split_whitespace() {
                let (key, value) = token
                    .split_once('=')
                    .unwrap_or_else(|| panic!("token `{token}` is not key=value; {run}"));
                fields.insert(key.to_owned(), decode(value));
            }
            Record { fields }
        })
        .collect()
}

/// Every record carrying `key`, in printed order.
fn records<'a>(parsed: &'a [Record], key: &str) -> Vec<&'a Record> {
    parsed.iter().filter(|record| record.get(key).is_some()).collect()
}

/// The single-field summary lines, as one map.
fn summary(parsed: &[Record]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for record in parsed {
        if record.fields.len() == 1 {
            let (key, value) = record.fields.iter().next().expect("one field");
            map.insert(key.clone(), value.clone());
        }
    }
    map
}

/// Decodes the uppercase percent escapes the commands emit.
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                decoded.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(decoded).expect("decoded output is UTF-8")
}

// ---------------------------------------------------------------------------
// Index fixtures
// ---------------------------------------------------------------------------

fn file_url(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{text}")
    }
}

fn entry_json(id: &str, name: &str, summary: &str, download_url: &str, digest: &str) -> String {
    format!(
        r#"{{
      "id": "{id}",
      "name": "{name}",
      "version": "1.2.3",
      "runtime": "python",
      "summary": "{summary}",
      "homepage": "https://example.test/{id}",
      "licence": "Apache-2.0",
      "download-url": "{download_url}",
      "package-digest": "{digest}",
      "signer-fingerprint": "0123456789abcdef0123456789abcdef",
      "publisher-note": "a member this build has never heard of"
    }}"#
    )
}

fn index_json(entries: &[String]) -> String {
    format!(
        "{{\n  \"index-version\": 1,\n  \"generated-at\": \"2026-08-08T00:00:00Z\",\n  \
         \"plugins\": [\n    {}\n  ]\n}}\n",
        entries.join(",\n    ")
    )
}

/// Writes a signed index into `directory` and returns its URL.
fn publish(directory: &Path, body: &str, key: &PackageSigningKey) -> String {
    let document = directory.join("index.json");
    fs::write(&document, body).expect("index is writable");
    fs::write(
        directory.join("index.json.sig"),
        key.detached(body.as_bytes()).to_toml(),
    )
    .expect("signature is writable");
    file_url(&document)
}

/// A catalogue whose entries all point at a package that does not exist. Enough
/// for search, show and index update, none of which download anything.
fn catalogue() -> String {
    index_json(&[
        entry_json(
            "stopwatch",
            "Stopwatch",
            "a clock you start",
            "https://example.test/stopwatch.crikey-package",
            EMPTY_DIGEST,
        ),
        entry_json(
            "world-clock",
            "World times",
            "every zone",
            "https://example.test/world-clock.crikey-package",
            EMPTY_DIGEST,
        ),
        entry_json(
            "clock",
            "Clock",
            "the time",
            "https://example.test/clock.crikey-package",
            EMPTY_DIGEST,
        ),
        entry_json(
            "notes",
            "Notes",
            "write things down",
            "https://example.test/notes.crikey-package",
            EMPTY_DIGEST,
        ),
    ])
}

/// A host with one signed, trusted index already published on disk.
fn host_with_index(label: &str) -> (Host, PackageSigningKey, PathBuf) {
    let host = Host::new(label);
    let key = PackageSigningKey::generate().expect("entropy");
    let served = host.subdir("served");
    let url = publish(&served, &catalogue(), &key);
    host.configure(&[url], None);
    host.trust("publisher", &key);
    (host, key, served)
}

// ---------------------------------------------------------------------------
// Nothing configured
// ---------------------------------------------------------------------------

/// The whole family says the same thing when no index is configured, and none
/// of them reaches for a host this project does not run. The verdict is a
/// completed operation that reached a bad one, not a usage error: the command
/// line was fine, the configuration is what is missing.
#[test]
fn every_index_command_reports_that_no_index_is_configured() {
    let host = Host::new("unconfigured");

    for args in [
        vec!["plugin", "search", "clock"],
        vec!["plugin", "show", "clock"],
        vec!["plugin", "index", "update"],
        vec!["plugin", "install", "clock"],
    ] {
        let run = host.run(&args);
        assert_completed(&run, EX_INVALID);
        assert!(
            run.stderr.contains("no plugin index is configured"),
            "the refusal explains itself; {run}"
        );
        assert!(
            run.stderr.contains("launcher.plugin-index-urls"),
            "the refusal names the key to set; {run}"
        );
    }
}

/// A path that does not exist must still be reported as a path, not silently
/// re-interpreted as an index lookup: adding the index cannot change what an
/// existing invocation means.
#[test]
fn a_missing_path_is_still_reported_as_a_path() {
    let host = Host::new("path-not-id");
    let missing = host.path.join("no-such-thing");

    let run = host.run(&["plugin", "install", missing.to_str().expect("utf-8 path")]);
    assert_completed(&run, EX_INVALID);
    assert!(run.stderr.contains("no-such-thing"), "{run}");
    assert!(
        !run.stderr.contains("no plugin index is configured"),
        "a path is not an indexed id; {run}"
    );
}

// ---------------------------------------------------------------------------
// index update
// ---------------------------------------------------------------------------

#[test]
fn index_update_verifies_the_signature_and_names_the_signer() {
    let (host, key, _served) = host_with_index("update");

    let run = host.run(&["plugin", "index", "update"]);
    assert_completed(&run, EX_OK);
    let parsed = parse(&run);
    assert_eq!(
        summary(&parsed).get("indexes").map(String::as_str),
        Some("1"),
        "{run}"
    );

    let indexes = records(&parsed, "index");
    let entry = indexes.first().expect("one index line is printed");
    assert_eq!(entry.field("status", &run), "ok");
    assert_eq!(entry.field("signer", &run), "publisher");
    assert_eq!(entry.field("fingerprint", &run), key.public_key().fingerprint());
    assert_eq!(entry.field("freshness", &run), "fresh");
    assert_eq!(entry.field("plugins", &run), "4");
}

/// "Nobody signed it" is not a weaker claim than "the wrong person signed it".
#[test]
fn an_unsigned_index_is_refused() {
    let (host, _key, served) = host_with_index("unsigned");
    fs::remove_file(served.join("index.json.sig")).expect("the signature is removable");

    let run = host.run(&["plugin", "index", "update"]);
    assert_completed(&run, EX_INVALID);
    let parsed = parse(&run);
    let entry = records(&parsed, "index");
    let entry = entry.first().expect("one index line is printed");
    assert_eq!(entry.field("status", &run), "refused");
    assert!(
        entry.field("reason", &run).contains("unsigned"),
        "the reason says the index is unsigned; {run}"
    );
}

#[test]
fn an_index_signed_by_an_untrusted_key_is_refused() {
    let host = Host::new("untrusted");
    let publisher = PackageSigningKey::generate().expect("entropy");
    let stranger = PackageSigningKey::generate().expect("entropy");
    let url = publish(&host.subdir("served"), &catalogue(), &stranger);
    host.configure(&[url], None);
    // A trust store that holds a key, just not the one that signed this.
    host.trust("publisher", &publisher);

    let run = host.run(&["plugin", "index", "update"]);
    assert_completed(&run, EX_INVALID);
    let parsed = parse(&run);
    let indexes = records(&parsed, "index");
    let entry = indexes.first().expect("one index line is printed");
    assert_eq!(entry.field("status", &run), "refused");
    assert!(
        entry
            .field("reason", &run)
            .contains(&stranger.public_key().fingerprint()),
        "the refusal names the key that signed it; {run}"
    );
}

/// An index edited after signing is refused even though the signature is a
/// genuine signature: it is a signature over other bytes.
#[test]
fn an_index_edited_after_signing_is_refused() {
    let (host, _key, served) = host_with_index("tampered");
    let tampered = index_json(&[entry_json(
        "evil",
        "Evil",
        "added after signing",
        "https://example.test/evil.crikey-package",
        EMPTY_DIGEST,
    )]);
    fs::write(served.join("index.json"), tampered).expect("the document is rewritable");

    let run = host.run(&["plugin", "index", "update"]);
    assert_completed(&run, EX_INVALID);
    assert!(
        run.stdout.contains("status=refused"),
        "the edited index is refused; {run}"
    );
}

// ---------------------------------------------------------------------------
// search and show
// ---------------------------------------------------------------------------

#[test]
fn search_filters_and_ranks_from_the_id_outwards() {
    let (host, _key, _served) = host_with_index("search");

    let run = host.run(&["plugin", "search", "clock"]);
    assert_completed(&run, EX_OK);
    let parsed = parse(&run);
    let found = summary(&parsed);
    assert_eq!(found.get("query").map(String::as_str), Some("clock"), "{run}");
    assert_eq!(found.get("matches").map(String::as_str), Some("3"), "{run}");

    let ranked: Vec<(&str, &str)> = records(&parsed, "match")
        .iter()
        .map(|record| (record.field("id", &run), record.field("quality", &run)))
        .collect();
    assert_eq!(
        ranked,
        vec![
            ("clock", "exact-id"),
            ("world-clock", "id-substring"),
            ("stopwatch", "summary"),
        ],
        "`notes` matches nothing and must not appear; {run}"
    );
}

/// A query nothing answers is a completed operation that found nothing wrong,
/// not a bad verdict: an empty catalogue and an empty result are different.
#[test]
fn a_query_that_matches_nothing_still_succeeds() {
    let (host, _key, _served) = host_with_index("search-empty");

    let run = host.run(&["plugin", "search", "zzzzz"]);
    assert_completed(&run, EX_OK);
    assert_eq!(
        summary(&parse(&run)).get("matches").map(String::as_str),
        Some("0"),
        "{run}"
    );
}

#[test]
fn show_reports_every_published_field() {
    let (host, _key, _served) = host_with_index("show");

    let run = host.run(&["plugin", "show", "clock"]);
    assert_completed(&run, EX_OK);
    let parsed = parse(&run);
    let listings = records(&parsed, "listing");
    let listing = listings.first().expect("one listing is printed");

    assert_eq!(listing.field("id", &run), "clock");
    assert_eq!(listing.field("name", &run), "Clock");
    assert_eq!(listing.field("version", &run), "1.2.3");
    assert_eq!(listing.field("runtime", &run), "python");
    assert_eq!(listing.field("licence", &run), "Apache-2.0");
    assert_eq!(listing.field("homepage", &run), "https://example.test/clock");
    assert_eq!(listing.field("package_digest", &run), EMPTY_DIGEST);
    assert_eq!(
        listing.field("signer_fingerprint", &run),
        "0123456789abcdef0123456789abcdef"
    );
    assert_eq!(listing.field("summary", &run), "the time");
}

#[test]
fn show_refuses_an_id_no_index_lists() {
    let (host, _key, _served) = host_with_index("show-unknown");

    let run = host.run(&["plugin", "show", "nonexistent"]);
    assert_completed(&run, EX_INVALID);
    assert!(
        run.stderr.contains("nonexistent") && run.stderr.contains("not listed"),
        "{run}"
    );
}

// ---------------------------------------------------------------------------
// Installing by id
// ---------------------------------------------------------------------------

/// The digest is the only thing binding the downloaded bytes to the entry the
/// operator read. A mismatch names both digests so they can tell a corrupted
/// download from a stale index from a substituted package.
#[test]
fn installing_by_id_refuses_a_package_whose_digest_does_not_match() {
    let host = Host::new("digest-mismatch");
    let key = PackageSigningKey::generate().expect("entropy");
    let served = host.subdir("served");
    let package = served.join("clock.crikey-package");
    fs::write(&package, b"substituted bytes").expect("the package is writable");
    let actual = package_digest(&package).expect("the fixture hashes");

    let body = index_json(&[entry_json(
        "clock",
        "Clock",
        "the time",
        &file_url(&package),
        EMPTY_DIGEST,
    )]);
    let url = publish(&served, &body, &key);
    host.configure(&[url], None);
    host.trust("publisher", &key);

    let run = host.run(&["plugin", "install", "clock"]);
    assert_completed(&run, EX_INVALID);
    assert!(
        run.stderr.contains(EMPTY_DIGEST),
        "the refusal names the published digest; {run}"
    );
    assert!(
        run.stderr.contains(&actual),
        "the refusal names the download's digest; {run}"
    );
    assert!(
        !host
            .path
            .join("data")
            .join("plugins")
            .join("modern")
            .join("clock")
            .exists(),
        "nothing is installed when the digest is refused; {run}"
    );
}

#[test]
fn installing_an_id_no_index_lists_is_refused() {
    let (host, _key, _served) = host_with_index("install-unknown");

    let run = host.run(&["plugin", "install", "nonexistent"]);
    assert_completed(&run, EX_INVALID);
    assert!(run.stderr.contains("not listed"), "{run}");
}

// ---------------------------------------------------------------------------
// Offline fallback
// ---------------------------------------------------------------------------

/// The last good copy answers when the source is unreachable, and says so. A
/// command that served yesterday's catalogue as though it were today's is how
/// an operator installs a version that no longer exists.
#[test]
fn a_stale_cached_index_answers_offline_and_is_reported_stale() {
    let host = Host::new("stale");
    let key = PackageSigningKey::generate().expect("entropy");
    let served = host.subdir("served");
    let url = publish(&served, &catalogue(), &key);
    // A zero lifetime makes every command attempt a refresh.
    host.configure(&[url], Some(0));
    host.trust("publisher", &key);

    let warm = host.run(&["plugin", "index", "update"]);
    assert_completed(&warm, EX_OK);

    fs::remove_file(served.join("index.json")).expect("the document is removable");
    fs::remove_file(served.join("index.json.sig")).expect("the signature is removable");

    let offline = host.run(&["plugin", "search", "clock"]);
    assert_completed(&offline, EX_OK);
    let parsed = parse(&offline);
    let indexes = records(&parsed, "index");
    let entry = indexes.first().expect("one index line is printed");
    assert_eq!(entry.field("status", &offline), "ok");
    assert_eq!(entry.field("freshness", &offline), "stale");
    assert!(
        !entry.field("reason", &offline).is_empty() && entry.field("reason", &offline) != "-",
        "a stale copy says why it could not be refreshed; {offline}"
    );
    assert_eq!(
        summary(&parsed).get("matches").map(String::as_str),
        Some("3"),
        "the cached catalogue still answers; {offline}"
    );

    // An update that could only serve the cache did not update.
    let update = host.run(&["plugin", "index", "update"]);
    assert_completed(&update, EX_INVALID);
    assert!(update.stdout.contains("freshness=stale"), "{update}");
}
