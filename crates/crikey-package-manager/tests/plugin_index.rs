//! The plugin index format and client (spec 2.2, 23.1; ADR-0013).
//!
//! Every fixture is a signed document on disk reached through a `file://` URL,
//! so the whole client — transport, signature check, parser, cache and digest
//! check — runs exactly as it does in production without a socket. "The network
//! is unavailable" is modelled by deleting the file the URL names, which is the
//! same [`PackageError::SourceUnavailable`] a refused connection produces.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use crikey_package_manager::{
    index_max_age, index_urls, package_digest, search, Freshness, IndexEntry, IndexSnapshot, IndexTransport,
    MatchQuality, PackageError, PackageFetcher, PackageSigningKey, PluginIndexClient, PluginIndexDocument,
    TrustStore, TrustedSigner, INDEX_MAX_BYTES, PACKAGE_MAX_BYTES,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A private directory tree removed when the test that made it ends.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "crikey-plugin-index-{label}-{}-{}",
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
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The `file://` URL naming `path`, in the one spelling this client accepts.
fn file_url(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{text}")
    }
}

/// One index entry as JSON text, with an unknown member on every entry so that
/// forward compatibility is exercised by every test rather than only the one
/// named after it.
fn entry_json(id: &str, name: &str, summary: &str, digest: &str) -> String {
    format!(
        r#"{{
      "id": "{id}",
      "name": "{name}",
      "version": "1.2.3",
      "runtime": "python",
      "summary": "{summary}",
      "homepage": "https://example.test/{id}",
      "licence": "Apache-2.0",
      "download-url": "https://example.test/{id}.crikey-package",
      "package-digest": "{digest}",
      "signer-fingerprint": "0123456789abcdef0123456789abcdef",
      "publisher-note": "a member this build has never heard of"
    }}"#
    )
}

/// A whole index document, given already-rendered entry text.
fn index_json(entries: &[String]) -> String {
    format!(
        "{{\n  \"index-version\": 1,\n  \"generated-at\": \"2026-08-08T00:00:00Z\",\n  \
         \"catalogue-note\": \"another unknown member\",\n  \"plugins\": [\n    {}\n  ]\n}}\n",
        entries.join(",\n    ")
    )
}

/// The 64-character digest of the empty file, which is what every fixture
/// package below actually is.
const EMPTY_DIGEST: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Writes `index.json` and its detached signature into `directory`, signed by
/// `key`, and returns the URL naming the document.
fn publish(directory: &Path, body: &str, key: &PackageSigningKey) -> String {
    let document = directory.join("index.json");
    fs::write(&document, body).expect("index is writable");
    let manifest = key.detached(body.as_bytes());
    fs::write(directory.join("index.json.sig"), manifest.to_toml()).expect("signature is writable");
    file_url(&document)
}

/// A client over `urls`, caching under `cache`, trusting exactly `trusted`.
fn client(
    cache: &Path,
    urls: &[String],
    max_age: Duration,
    trusted: &[&PackageSigningKey],
) -> PluginIndexClient {
    let mut trust = TrustStore::empty();
    for (position, key) in trusted.iter().enumerate() {
        trust
            .add(&format!("publisher-{position}"), key.public_key())
            .expect("each fixture key is distinct");
    }
    PluginIndexClient::with_parts(
        cache,
        urls.to_vec(),
        max_age,
        Box::new(IndexTransport::new(INDEX_MAX_BYTES)),
        Box::new(IndexTransport::new(PACKAGE_MAX_BYTES)),
        trust,
    )
}

/// The single outcome of a one-index client, as a `Result`.
fn only(client: &PluginIndexClient, refresh: bool) -> Result<IndexSnapshot, PackageError> {
    let mut outcomes = client.load(refresh);
    assert_eq!(outcomes.len(), 1, "the fixture configures exactly one index");
    outcomes.remove(0).snapshot
}

fn a_day() -> Duration {
    Duration::from_secs(24 * 60 * 60)
}

// ---------------------------------------------------------------------------
// The document format
// ---------------------------------------------------------------------------

/// The compatibility promise: a member this build does not know is kept, not
/// refused. An index generated for a newer client must stay usable by an older
/// one, or every client in the field breaks the day a member is added.
#[test]
fn an_unknown_member_is_preserved_rather_than_refused() {
    let body = index_json(&[entry_json("clock", "Clock", "the time", EMPTY_DIGEST)]);
    let document = PluginIndexDocument::parse(body.as_bytes()).expect("unknown members are not fatal");

    assert!(
        document.extra.contains_key("catalogue-note"),
        "an unknown top-level member is preserved: {:?}",
        document.extra.keys().collect::<Vec<_>>()
    );
    let entry = document.entry("clock").expect("the entry parsed");
    assert!(
        entry.extra.contains_key("publisher-note"),
        "an unknown entry member is preserved: {:?}",
        entry.extra.keys().collect::<Vec<_>>()
    );
    assert_eq!(entry.licence.as_deref(), Some("Apache-2.0"));
    assert_eq!(entry.homepage.as_deref(), Some("https://example.test/clock"));
}

/// A runtime this build has never heard of is data, not a parse error: the
/// entry must stay listable and searchable, and simply not be installable here.
#[test]
fn an_unknown_runtime_is_listed_and_is_not_a_runtime_this_build_knows() {
    let entry = entry_json("gadget", "Gadget", "something new", EMPTY_DIGEST)
        .replace("\"runtime\": \"python\"", "\"runtime\": \"quantum\"");
    let document = PluginIndexDocument::parse(index_json(&[entry]).as_bytes()).expect("parses");

    let entry = document.entry("gadget").expect("the entry is listed");
    assert_eq!(entry.runtime, "quantum");
    assert_eq!(entry.parsed_runtime(), None);
}

/// The version exists so a future format may change something this build would
/// misread. Guessing is how a launcher installs the wrong bytes.
#[test]
fn a_newer_index_version_is_refused() {
    let body = index_json(&[]).replace("\"index-version\": 1", "\"index-version\": 2");
    let error = PluginIndexDocument::parse(body.as_bytes()).expect_err("a future version is refused");

    let message = error.to_string();
    assert!(
        message.contains('2') && message.contains("index-version"),
        "{message}"
    );
}

/// Rejected whole, never in part: a partially trusted catalogue is how a user
/// installs the one entry that got past the parser.
#[test]
fn one_bad_entry_rejects_the_whole_document() {
    let good = entry_json("clock", "Clock", "the time", EMPTY_DIGEST);
    let bad = entry_json("timer", "Timer", "counts down", "not-a-digest");
    let error = PluginIndexDocument::parse(index_json(&[good, bad]).as_bytes())
        .expect_err("a malformed digest is refused");

    let message = error.to_string();
    assert!(
        message.contains("timer") && message.contains("package-digest"),
        "{message}"
    );
}

#[test]
fn a_download_url_that_is_neither_https_nor_file_is_refused() {
    let entry = entry_json("clock", "Clock", "the time", EMPTY_DIGEST).replace(
        "https://example.test/clock.crikey-package",
        "http://example.test/clock.crikey-package",
    );
    let error =
        PluginIndexDocument::parse(index_json(&[entry]).as_bytes()).expect_err("plain http is refused");
    assert!(error.to_string().contains("download-url"), "{error}");
}

/// Two entries under one id make `show` and `install` ambiguous inside a single
/// document, where there is no publisher to disambiguate between.
#[test]
fn a_duplicate_id_is_refused() {
    let first = entry_json("clock", "Clock", "the time", EMPTY_DIGEST);
    let second = entry_json("clock", "Clock Two", "also the time", EMPTY_DIGEST);
    let error = PluginIndexDocument::parse(index_json(&[first, second]).as_bytes()).expect_err("refused");
    assert!(error.to_string().contains("twice"), "{error}");
}

#[test]
fn configuration_defaults_to_no_index_at_all() {
    assert!(index_urls(None).is_empty());
    assert_eq!(index_max_age(None), Duration::from_secs(24 * 60 * 60));
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

fn snapshot_of(body: &str, url: &str) -> IndexSnapshot {
    IndexSnapshot {
        url: url.to_owned(),
        document: PluginIndexDocument::parse(body.as_bytes()).expect("fixture parses"),
        signer: TrustedSigner {
            name: "publisher".to_owned(),
            fingerprint: "0123456789abcdef0123456789abcdef".to_owned(),
        },
        freshness: Freshness::Fresh,
    }
}

/// Filtering and ranking are one contract: an operator scanning `search` output
/// reads the first line, so the entry whose id *is* the query has to be there.
#[test]
fn search_filters_and_ranks_from_the_id_outwards() {
    let body = index_json(&[
        entry_json("stopwatch", "Stopwatch", "a clock you start", EMPTY_DIGEST),
        entry_json("world-clock", "World times", "every zone", EMPTY_DIGEST),
        entry_json("clock", "Clock", "the time", EMPTY_DIGEST),
        entry_json("timer", "Clock timer", "counts down", EMPTY_DIGEST),
        entry_json("clockwork", "Clockwork", "gears", EMPTY_DIGEST),
        entry_json("notes", "Notes", "write things down", EMPTY_DIGEST),
    ]);
    let snapshots = vec![snapshot_of(&body, "file:///fixture")];

    let hits = search(&snapshots, "clock");

    let ranked: Vec<(&str, MatchQuality)> = hits
        .iter()
        .map(|hit| (hit.entry.id.as_str(), hit.quality))
        .collect();
    assert_eq!(
        ranked,
        vec![
            ("clock", MatchQuality::ExactId),
            ("clockwork", MatchQuality::IdPrefix),
            ("world-clock", MatchQuality::IdSubstring),
            ("timer", MatchQuality::Name),
            ("stopwatch", MatchQuality::Summary),
        ],
        "`notes` matches nothing and must not appear"
    );
}

#[test]
fn search_ignores_case_on_both_sides() {
    let body = index_json(&[entry_json("clock", "Clock", "the time", EMPTY_DIGEST)]);
    let snapshots = vec![snapshot_of(&body, "file:///fixture")];

    assert_eq!(search(&snapshots, "CLOCK").len(), 1);
    assert_eq!(search(&snapshots, "zzz").len(), 0);
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

#[test]
fn a_signed_index_verifies_and_names_its_signer() {
    let scratch = Scratch::new("signed");
    let key = PackageSigningKey::generate().expect("entropy");
    let served = scratch.subdir("served");
    let body = index_json(&[entry_json("clock", "Clock", "the time", EMPTY_DIGEST)]);
    let url = publish(&served, &body, &key);

    let client = client(&scratch.subdir("cache"), &[url], a_day(), &[&key]);
    let snapshot = only(&client, true).expect("a trusted signature verifies");

    assert_eq!(snapshot.signer.fingerprint, key.public_key().fingerprint());
    assert_eq!(snapshot.freshness, Freshness::Fresh);
    assert!(snapshot.document.entry("clock").is_some());
}

/// An index with no detached signature is refused rather than trusted for
/// having been served: "nobody signed it" is not a weaker claim than "the wrong
/// person signed it", it is the same claim.
#[test]
fn an_unsigned_index_is_refused() {
    let scratch = Scratch::new("unsigned");
    let key = PackageSigningKey::generate().expect("entropy");
    let served = scratch.subdir("served");
    let body = index_json(&[]);
    let url = publish(&served, &body, &key);
    fs::remove_file(served.join("index.json.sig")).expect("the signature is removable");

    let client = client(&scratch.subdir("cache"), &[url], a_day(), &[&key]);
    let error = only(&client, true).expect_err("an unsigned index is refused");

    assert!(
        matches!(error, PackageError::IndexSignature(_)),
        "an unsigned index is a signature refusal, not a transport failure: {error}"
    );
    assert!(error.to_string().contains("unsigned"), "{error}");
}

#[test]
fn an_index_signed_by_an_untrusted_key_is_refused() {
    let scratch = Scratch::new("untrusted");
    let publisher = PackageSigningKey::generate().expect("entropy");
    let stranger = PackageSigningKey::generate().expect("entropy");
    let url = publish(&scratch.subdir("served"), &index_json(&[]), &stranger);

    // The trust store holds a key, just not the one that signed this.
    let client = client(&scratch.subdir("cache"), &[url], a_day(), &[&publisher]);
    let error = only(&client, true).expect_err("an untrusted signer is refused");

    assert!(matches!(error, PackageError::IndexSignature(_)), "{error}");
    assert!(
        error.to_string().contains(&stranger.public_key().fingerprint()),
        "the refusal names the key that signed it: {error}"
    );
}

/// The signature is checked against the bytes actually served, so an index
/// edited after signing is refused even though the signature itself is genuine.
#[test]
fn an_index_edited_after_signing_is_refused() {
    let scratch = Scratch::new("tampered");
    let key = PackageSigningKey::generate().expect("entropy");
    let served = scratch.subdir("served");
    let url = publish(&served, &index_json(&[]), &key);
    let tampered = index_json(&[entry_json("evil", "Evil", "added later", EMPTY_DIGEST)]);
    fs::write(served.join("index.json"), &tampered).expect("the document is rewritable");

    let client = client(&scratch.subdir("cache"), &[url], a_day(), &[&key]);
    let error = only(&client, true).expect_err("edited bytes are refused");

    assert!(matches!(error, PackageError::IndexSignature(_)), "{error}");
}

/// A refused document must not be able to destroy the good copy an operator
/// could still install from: rejection is not a reason to lose a catalogue.
#[test]
fn a_refused_refresh_leaves_the_cached_copy_intact() {
    let scratch = Scratch::new("cache-intact");
    let key = PackageSigningKey::generate().expect("entropy");
    let served = scratch.subdir("served");
    let cache = scratch.subdir("cache");
    let good = index_json(&[entry_json("clock", "Clock", "the time", EMPTY_DIGEST)]);
    let url = publish(&served, &good, &key);

    let client = client(&cache, &[url], a_day(), &[&key]);
    only(&client, true).expect("the first fetch verifies");

    // The host starts serving something nobody signed.
    fs::write(served.join("index.json"), index_json(&[])).expect("the document is rewritable");
    let error = only(&client, true).expect_err("the refresh is refused");
    assert!(matches!(error, PackageError::IndexSignature(_)), "{error}");

    let cached = only(&client, false).expect("the cached copy still verifies");
    assert!(
        cached.document.entry("clock").is_some(),
        "the good catalogue survived a refused refresh"
    );
}

// ---------------------------------------------------------------------------
// Offline fallback
// ---------------------------------------------------------------------------

/// A transport failure is the one case that may fall back, and the fallback is
/// reported: a command that served yesterday's catalogue as though it were
/// today's is how an operator installs a version that no longer exists.
#[test]
fn a_stale_cached_index_is_used_when_the_source_is_unavailable_and_reported_stale() {
    let scratch = Scratch::new("stale");
    let key = PackageSigningKey::generate().expect("entropy");
    let served = scratch.subdir("served");
    let cache = scratch.subdir("cache");
    let body = index_json(&[entry_json("clock", "Clock", "the time", EMPTY_DIGEST)]);
    let url = publish(&served, &body, &key);

    let warm = client(&cache, &[url.clone()], a_day(), &[&key]);
    only(&warm, true).expect("the first fetch verifies");

    // The network is unavailable: the URL names nothing any more.
    fs::remove_file(served.join("index.json")).expect("the document is removable");
    fs::remove_file(served.join("index.json.sig")).expect("the signature is removable");

    // A zero lifetime makes every load attempt a refresh, which now fails.
    let cold = client(&cache, &[url], Duration::ZERO, &[&key]);
    let snapshot = only(&cold, false).expect("the last good copy answers");

    match &snapshot.freshness {
        Freshness::Stale { reason, .. } => {
            assert!(
                reason.contains("could not be read") || reason.contains("unavailable"),
                "the reason says why the refresh failed: {reason}"
            );
        }
        Freshness::Fresh => panic!("a copy served because the source vanished is not fresh"),
    }
    assert_eq!(snapshot.freshness.as_str(), "stale");
    assert!(snapshot.document.entry("clock").is_some());
}

/// With no cached copy there is nothing to fall back to, and the transport
/// failure is reported as itself rather than as an empty catalogue.
#[test]
fn an_unavailable_source_with_no_cache_reports_the_transport_failure() {
    let scratch = Scratch::new("cold");
    let key = PackageSigningKey::generate().expect("entropy");
    let url = file_url(&scratch.path.join("nothing-here").join("index.json"));

    let client = client(&scratch.subdir("cache"), &[url], a_day(), &[&key]);
    let error = only(&client, false).expect_err("there is nothing to serve");

    assert!(matches!(error, PackageError::SourceUnavailable(_)), "{error}");
}

// ---------------------------------------------------------------------------
// Package download
// ---------------------------------------------------------------------------

/// One entry pointing at `package`, with `digest` as its published digest.
fn entry_for(package: &Path, digest: &str) -> IndexEntry {
    let body = index_json(&[entry_json("clock", "Clock", "the time", digest)
        .replace("https://example.test/clock.crikey-package", &file_url(package))]);
    PluginIndexDocument::parse(body.as_bytes())
        .expect("fixture parses")
        .entry("clock")
        .expect("the entry is listed")
        .clone()
}

#[test]
fn a_download_matching_the_published_digest_is_kept() {
    let scratch = Scratch::new("digest-ok");
    let key = PackageSigningKey::generate().expect("entropy");
    let package = scratch.subdir("served").join("clock.crikey-package");
    fs::write(&package, b"pretend package bytes").expect("the package is writable");
    let digest = package_digest(&package).expect("the fixture hashes");
    let entry = entry_for(&package, &digest);

    let client = client(&scratch.subdir("cache"), &[], a_day(), &[&key]);
    let destination = scratch.path.join("downloaded");
    client
        .download_package(&entry, &destination)
        .expect("bytes that hash to the published digest are kept");

    assert_eq!(
        fs::read(&destination).expect("the download landed"),
        b"pretend package bytes"
    );
}

/// The refusal names both digests, because "hash mismatch" alone cannot tell an
/// operator whether they are looking at a corrupted download, a stale index or
/// a substituted package. The rejected file is deleted rather than left behind
/// for a later command to mistake for a package.
#[test]
fn a_digest_mismatch_refuses_the_download_and_names_both_digests() {
    let scratch = Scratch::new("digest-bad");
    let key = PackageSigningKey::generate().expect("entropy");
    let package = scratch.subdir("served").join("clock.crikey-package");
    fs::write(&package, b"substituted bytes").expect("the package is writable");
    let actual = package_digest(&package).expect("the fixture hashes");
    let entry = entry_for(&package, EMPTY_DIGEST);

    let client = client(&scratch.subdir("cache"), &[], a_day(), &[&key]);
    let destination = scratch.path.join("downloaded");
    let error = client
        .download_package(&entry, &destination)
        .expect_err("a substituted package is refused");

    let message = error.to_string();
    assert!(
        message.contains(EMPTY_DIGEST),
        "the published digest is named: {message}"
    );
    assert!(
        message.contains(&actual),
        "the download's digest is named: {message}"
    );
    assert!(
        !destination.exists(),
        "a refused download is deleted, not left where a later command could find it"
    );
}

#[test]
fn a_local_response_over_the_transport_limit_is_refused_and_removed() {
    let scratch = Scratch::new("local-cap");
    let served = scratch.subdir("served").join("index.json");
    fs::write(&served, b"12345").expect("the fixture is writable");
    let destination = scratch.path.join("downloaded");
    let error = IndexTransport::new(4)
        .fetch(&file_url(&served), &destination)
        .expect_err("a response over the configured cap is refused");

    assert!(error.to_string().contains("larger than"), "{error}");
    assert!(
        !destination.exists(),
        "an oversized response is not left as a usable download"
    );
}
