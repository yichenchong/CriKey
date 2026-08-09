//! The plugin index: the client half of the public plugin repository (spec 2.2,
//! 23.1).
//!
//! CriKey does not host anything. What lives here is the *format* an index
//! document has, the *client* that fetches, verifies and caches one, and the
//! search and resolution the command line drives. Everything a publisher runs
//! is out of this repository entirely.
//!
//! # What an index is
//!
//! One signed JSON document listing plugins a user could install: for each, an
//! id, a display name, a version, a runtime, a summary, a homepage, a licence, a
//! download URL, the SHA-256 of the package those bytes must hash to, and the
//! fingerprint of the key its publisher signs with. The document is signed
//! detached (ADR 0012); the signature travels beside it as `<url>.sig`.
//!
//! # Two independent checks, and neither is the transport
//!
//! The document is refused unless a key in the user's trust store signs it, and
//! a downloaded package is refused unless its bytes hash to the digest the index
//! named. Neither check involves TLS, which is why a `file://` index — an
//! air-gapped mirror, a corporate share, this crate's own tests — is accepted on
//! exactly the same terms as an `https://` one. A transport is how the bytes
//! arrived; the signature is why they are believed.
//!
//! # Nothing is configured by default
//!
//! [`KEY_INDEX_URLS`] has no built-in value. With nothing configured there is no
//! index, no network traffic and no behaviour change: the commands say so rather
//! than reaching for a host this project does not run.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crikey_platform::StandardDirectories;

use crate::fetch::HttpFetcher;
use crate::index::{constant_time_hex_eq, hex_lower, is_hex_sha256};
use crate::{
    read_signature_file, verify_signed_manifest, PackageError, PackageFetcher, TrustStore, TrustedSigner,
};

// ---------------------------------------------------------------------------
// Limits and configuration keys
// ---------------------------------------------------------------------------

/// The one `index-version` this build understands.
///
/// A document declaring a higher version is refused rather than parsed
/// optimistically: the version exists precisely to let a future format change
/// something this build would misread, and guessing is how a launcher installs
/// the wrong bytes.
pub const INDEX_FORMAT_VERSION: u32 = 1;

/// Ceiling on an index document, in bytes.
///
/// Generous for a real catalogue — 8 MiB of JSON is tens of thousands of
/// entries — and small enough that a hostile or broken host cannot make the
/// client buffer a disk.
pub const INDEX_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Ceiling on a package fetched through the index, matching [`HttpFetcher`]'s
/// own default so an index install and a URL install refuse the same sizes.
pub const PACKAGE_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Ceiling on entries in one index. Bounds the allocation a parsed document
/// implies independently of the byte ceiling, because a pathological document
/// can be many small entries rather than a few large ones.
pub const MAX_INDEX_ENTRIES: usize = 20_000;

/// Ceiling on any single string an entry carries.
const MAX_FIELD_BYTES: usize = 4 * 1024;

/// Configuration key naming the index URLs, comma-separated (spec 21.2).
///
/// Deliberately absent from the configuration crate's built-in defaults: the
/// key is this crate's, and there is no default value to have — see the module
/// documentation.
pub const KEY_INDEX_URLS: &str = "launcher.plugin-index-urls";

/// Configuration key: how long a cached index is considered current, in seconds.
pub const KEY_INDEX_MAX_AGE_SECONDS: &str = "launcher.plugin-index-max-age-seconds";

/// How long a cached index is current when nothing configures it.
pub const DEFAULT_INDEX_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// The subdirectory of the cache root this client owns, so a sweeper — or a
/// person — can delete the plugin index alone.
const CACHE_SUBDIRECTORY: &str = "plugin-index";

/// The configured index URLs, in configuration order, with blanks dropped.
///
/// Comma-separated because the layered configuration store is a flat map of
/// text values and a URL cannot contain an unencoded comma. Duplicates are
/// removed: the same index listed twice is one index, and merging it with
/// itself would double every search hit.
pub fn index_urls(configured: Option<&str>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    configured
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .filter(|url| seen.insert((*url).to_owned()))
        .map(str::to_owned)
        .collect()
}

/// The configured cache lifetime, or [`DEFAULT_INDEX_MAX_AGE`].
///
/// An unparseable or absent value takes the default rather than refusing: a
/// typo in a cache lifetime must not make the plugin commands unusable, and the
/// worst it can cost is one extra fetch.
pub fn index_max_age(configured: Option<&str>) -> Duration {
    configured
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map_or(DEFAULT_INDEX_MAX_AGE, Duration::from_secs)
}

// ---------------------------------------------------------------------------
// The document
// ---------------------------------------------------------------------------

/// A parsed index document.
///
/// Unknown members are kept in [`Self::extra`] rather than refused, so an index
/// generated for a newer client is still usable by this one: the format's
/// compatibility promise is that new members may be added at any time within
/// one `index-version`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PluginIndexDocument {
    /// The format version. Must be [`INDEX_FORMAT_VERSION`].
    pub index_version: u32,
    /// When the publisher generated this document, as RFC 3339 text.
    ///
    /// Text rather than a parsed instant: it is displayed, never compared. The
    /// freshness this client acts on is the age of the *cached copy*, which is
    /// a fact about this machine, not a claim by a remote host.
    pub generated_at: String,
    /// The listed plugins, in publisher order.
    #[serde(default)]
    pub plugins: Vec<IndexEntry>,
    /// Every member this build does not know about, preserved.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// One plugin an index lists.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct IndexEntry {
    /// The plugin's own id, unnamespaced, as its manifest declares it.
    pub id: String,
    /// The display name.
    pub name: String,
    /// The published version.
    pub version: String,
    /// The declared runtime, as text.
    ///
    /// Not [`crikey_plugin_model::Runtime`]: an index that lists a runtime this
    /// build has never heard of must still parse, still be searchable, and
    /// simply not be installable by this build. Use [`Self::parsed_runtime`].
    pub runtime: String,
    /// One line describing the plugin.
    #[serde(default)]
    pub summary: String,
    /// The project's page, if it has one.
    #[serde(default)]
    pub homepage: Option<String>,
    /// The licence identifier, if the publisher declares one.
    #[serde(default)]
    pub licence: Option<String>,
    /// Where the package is fetched from.
    pub download_url: String,
    /// Lowercase hex SHA-256 the downloaded package must hash to.
    pub package_digest: String,
    /// The fingerprint of the key the publisher signs with.
    ///
    /// Reported, not enforced: this client verifies the *index* signature and
    /// the *package digest*, and the digest is what binds these bytes. The
    /// fingerprint is the publisher's attribution, and calling it a verified
    /// package signature would be a claim nothing here checks.
    pub signer_fingerprint: String,
    /// Every member this build does not know about, preserved.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl IndexEntry {
    /// The runtime as this build's enum, or `None` for one it does not know.
    pub fn parsed_runtime(&self) -> Option<crikey_plugin_model::Runtime> {
        serde_json::from_value(Value::String(self.runtime.clone())).ok()
    }

    fn validate(&self) -> Result<(), PackageError> {
        check_field("id", &self.id)?;
        if self
            .id
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')))
        {
            return Err(PackageError::MalformedIndex(format!(
                "plugin id `{}` holds a character an id may not hold",
                self.id
            )));
        }
        check_field("name", &self.name)?;
        check_field("version", &self.version)?;
        check_field("runtime", &self.runtime)?;
        bound_field("summary", &self.summary)?;
        if let Some(homepage) = &self.homepage {
            bound_field("homepage", homepage)?;
        }
        if let Some(licence) = &self.licence {
            bound_field("licence", licence)?;
        }
        check_field("download-url", &self.download_url)?;
        if !is_supported_url(&self.download_url) {
            return Err(PackageError::MalformedIndex(format!(
                "`{}` names download-url `{}`, which is neither https:// nor file://",
                self.id, self.download_url
            )));
        }
        if !is_hex_sha256(&self.package_digest)
            || self.package_digest.bytes().any(|byte| byte.is_ascii_uppercase())
        {
            return Err(PackageError::MalformedIndex(format!(
                "`{}` names package-digest `{}`, which is not a 64-character lowercase hex SHA-256",
                self.id, self.package_digest
            )));
        }
        check_field("signer-fingerprint", &self.signer_fingerprint)?;
        if self.signer_fingerprint.len() != FINGERPRINT_LENGTH
            || !self
                .signer_fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(PackageError::MalformedIndex(format!(
                "`{}` names signer-fingerprint `{}`, which is not a {FINGERPRINT_LENGTH}-character lowercase hex fingerprint",
                self.id, self.signer_fingerprint
            )));
        }
        Ok(())
    }
}

/// Characters in the fingerprint spelling ADR 0012 fixes: the first 16 bytes of
/// the SHA-256 over a key, in lowercase hex.
const FINGERPRINT_LENGTH: usize = 32;

impl PluginIndexDocument {
    /// Parses and validates `bytes`.
    ///
    /// Everything an index says about itself is checked here, before any caller
    /// can act on it: the version, the entry count, every field's length, the
    /// digest's shape, the download URL's scheme, and the absence of duplicate
    /// ids. A document that fails any of them is rejected whole — a partially
    /// trusted catalogue is how a user installs the one entry an attacker got
    /// past the parser.
    pub fn parse(bytes: &[u8]) -> Result<Self, PackageError> {
        let document: Self = serde_json::from_slice(bytes)
            .map_err(|error| PackageError::MalformedIndex(format!("index is not valid JSON: {error}")))?;
        document.validate()?;
        Ok(document)
    }

    fn validate(&self) -> Result<(), PackageError> {
        if self.index_version != INDEX_FORMAT_VERSION {
            return Err(PackageError::MalformedIndex(format!(
                "index-version {} is not the version {INDEX_FORMAT_VERSION} this build understands",
                self.index_version
            )));
        }
        check_field("generated-at", &self.generated_at)?;
        if self.plugins.len() > MAX_INDEX_ENTRIES {
            return Err(PackageError::MalformedIndex(format!(
                "index lists {} plugins, more than the {MAX_INDEX_ENTRIES} entry limit",
                self.plugins.len()
            )));
        }
        let mut seen = BTreeSet::new();
        for entry in &self.plugins {
            entry.validate()?;
            if !seen.insert(entry.id.as_str()) {
                return Err(PackageError::MalformedIndex(format!(
                    "index lists `{}` twice",
                    entry.id
                )));
            }
        }
        Ok(())
    }

    /// The entry for `id`, if this document lists it.
    pub fn entry(&self, id: &str) -> Option<&IndexEntry> {
        self.plugins.iter().find(|entry| entry.id == id)
    }
}

/// A required field: non-empty, bounded, and free of control characters that
/// would corrupt a `key=value` report or a terminal.
fn check_field(name: &str, value: &str) -> Result<(), PackageError> {
    if value.is_empty() {
        return Err(PackageError::MalformedIndex(format!(
            "index entry has an empty `{name}`"
        )));
    }
    bound_field(name, value)
}

fn bound_field(name: &str, value: &str) -> Result<(), PackageError> {
    if value.len() > MAX_FIELD_BYTES {
        return Err(PackageError::MalformedIndex(format!(
            "index entry's `{name}` is {} bytes, over the {MAX_FIELD_BYTES} byte field limit",
            value.len()
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(PackageError::MalformedIndex(format!(
            "index entry's `{name}` holds a control character"
        )));
    }
    Ok(())
}

fn is_supported_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("file://")
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// How well an entry answered a query. Lower is better; the ordering is the
/// ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchQuality {
    /// The query is the entry's whole id.
    ExactId,
    /// The entry's id starts with the query.
    IdPrefix,
    /// The entry's id contains the query.
    IdSubstring,
    /// The entry's display name contains the query.
    Name,
    /// The entry's summary contains the query.
    Summary,
}

impl MatchQuality {
    /// The stable spelling a report prints.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExactId => "exact-id",
            Self::IdPrefix => "id-prefix",
            Self::IdSubstring => "id-substring",
            Self::Name => "name",
            Self::Summary => "summary",
        }
    }

    /// How `entry` answers `query`, which is already lowercase.
    fn of(entry: &IndexEntry, query: &str) -> Option<Self> {
        let id = entry.id.to_lowercase();
        if id == query {
            return Some(Self::ExactId);
        }
        if id.starts_with(query) {
            return Some(Self::IdPrefix);
        }
        if id.contains(query) {
            return Some(Self::IdSubstring);
        }
        if entry.name.to_lowercase().contains(query) {
            return Some(Self::Name);
        }
        if entry.summary.to_lowercase().contains(query) {
            return Some(Self::Summary);
        }
        None
    }
}

/// One search result: which index it came from, and how well it matched.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// The entry, copied out of the snapshot it came from.
    pub entry: IndexEntry,
    /// The index URL that listed it.
    pub index_url: String,
    /// Why it matched.
    pub quality: MatchQuality,
}

/// Every entry in `snapshots` matching `query`, best match first.
///
/// Ties break on the id so that two runs over the same catalogue print the same
/// order; an unstable ranking makes a diff between two reports meaningless. A
/// plugin listed by two configured indexes appears once per index, because they
/// are different publishers offering different bytes and collapsing them would
/// hide which one an install would take.
pub fn search(snapshots: &[IndexSnapshot], query: &str) -> Vec<SearchHit> {
    let needle = query.to_lowercase();
    let mut hits: Vec<SearchHit> = snapshots
        .iter()
        .flat_map(|snapshot| {
            snapshot.document.plugins.iter().filter_map(|entry| {
                MatchQuality::of(entry, &needle).map(|quality| SearchHit {
                    entry: entry.clone(),
                    index_url: snapshot.url.clone(),
                    quality,
                })
            })
        })
        .collect();
    hits.sort_by(|left, right| {
        left.quality
            .cmp(&right.quality)
            .then_with(|| left.entry.id.cmp(&right.entry.id))
            .then_with(|| left.index_url.cmp(&right.index_url))
    });
    hits
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// The client's transport: HTTPS through the existing [`HttpFetcher`], plus
/// local files.
///
/// One type rather than two so that a caller never has to decide which fetcher
/// a configured URL needs. `file://` is not a weakening: see the module
/// documentation on why the transport is not the trust boundary.
#[derive(Debug, Clone, Copy)]
pub struct IndexTransport {
    max_bytes: u64,
}

impl IndexTransport {
    /// A transport refusing anything over `max_bytes`.
    pub fn new(max_bytes: u64) -> Self {
        Self { max_bytes }
    }
}

impl PackageFetcher for IndexTransport {
    fn fetch(&self, url: &str, destination: &Path) -> Result<(), PackageError> {
        let Some(path) = local_path(url) else {
            return HttpFetcher::with_max_bytes(self.max_bytes).fetch(url, destination);
        };
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| PackageError::SourceUnavailable(format!("{url} could not be read: {error}")))?;
        if !metadata.is_file() {
            return Err(PackageError::SourceUnavailable(format!(
                "{url} does not name a regular file"
            )));
        }
        if metadata.len() > self.max_bytes {
            return Err(PackageError::SourceUnavailable(format!(
                "{url} is larger than the {} byte limit",
                self.max_bytes
            )));
        }
        // Stream local mirrors through the same one-byte-over-limit guard as
        // HTTPS. A size check followed by `fs::copy` is racy when a publisher
        // replaces the file between the two operations.
        let mut source = fs::File::open(&path)
            .map_err(|error| PackageError::SourceUnavailable(format!("{url} could not be read: {error}")))?;
        let mut output = fs::File::create(destination)?;
        let written = std::io::copy(
            &mut Read::by_ref(&mut source).take(self.max_bytes + 1),
            &mut output,
        )?;
        output.flush()?;
        if written > self.max_bytes {
            let _ = fs::remove_file(destination);
            return Err(PackageError::SourceUnavailable(format!(
                "{url} is larger than the {} byte limit",
                self.max_bytes
            )));
        }
        Ok(())
    }
}

/// The filesystem path a `file://` URL names, or `None` for any other scheme.
///
/// Only the local form is accepted: `file://host/share` names another machine's
/// share, which is not something this client knows how to read, and silently
/// treating the host as a path component would read the wrong file.
fn local_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let decoded = percent_decode(rest);
    // `file:///C:/x` on Windows: the leading separator is the URL's, not the
    // path's.
    let bytes = decoded.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        return Some(PathBuf::from(&decoded[1..]));
    }
    Some(PathBuf::from(decoded))
}

/// Decodes `%XX` escapes, leaving anything malformed exactly as written.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = char::from(bytes[index + 1]).to_digit(16);
            let low = char::from(bytes[index + 2]).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// Whether a snapshot came from a current copy, and why not when it did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    /// Fetched during this call, or cached within the configured lifetime.
    Fresh,
    /// Served from a cached copy the client could not refresh.
    Stale {
        /// How old the cached copy is, in seconds.
        age_seconds: u64,
        /// Why the refresh did not happen.
        reason: String,
    },
}

impl Freshness {
    /// The stable spelling a report prints.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Stale { .. } => "stale",
        }
    }
}

/// One configured index, as this client currently sees it.
#[derive(Debug, Clone)]
pub struct IndexSnapshot {
    /// The configured URL it was read from.
    pub url: String,
    /// The verified document.
    pub document: PluginIndexDocument,
    /// The trusted key that signed it.
    pub signer: TrustedSigner,
    /// Whether the copy is current.
    pub freshness: Freshness,
}

/// What one configured index produced.
#[derive(Debug)]
pub struct IndexOutcome {
    /// The configured URL.
    pub url: String,
    /// The snapshot, or why there is none.
    pub snapshot: Result<IndexSnapshot, PackageError>,
}

/// Fetches, verifies, caches and searches plugin indexes.
///
/// Holds two transports because an index and a package have wildly different
/// sizes and therefore wildly different ceilings: an 8 MiB cap on a package
/// would refuse every real plugin, and a 256 MiB cap on an index would let a
/// hostile host make the client buffer a disk before the parser ever sees it.
pub struct PluginIndexClient {
    cache_root: PathBuf,
    urls: Vec<String>,
    max_age: Duration,
    index_transport: Box<dyn PackageFetcher>,
    package_transport: Box<dyn PackageFetcher>,
    trust: TrustStore,
}

/// Prints the shape and none of the trusted material: a trust store in a
/// diagnostic is a list of public keys nobody reads and everybody scrolls past.
impl std::fmt::Debug for PluginIndexClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginIndexClient")
            .field("cache_root", &self.cache_root)
            .field("urls", &self.urls)
            .field("max_age", &self.max_age)
            .finish_non_exhaustive()
    }
}

impl PluginIndexClient {
    /// A client over `urls`, caching under `directories` and trusting the keys
    /// the user's trust store names.
    pub fn new(
        directories: &StandardDirectories,
        urls: Vec<String>,
        max_age: Duration,
    ) -> Result<Self, PackageError> {
        let trust = TrustStore::load(directories)
            .map_err(|error| PackageError::IndexSignature(format!("trust store: {error}")))?;
        Ok(Self::with_parts(
            directories.cache_dir(),
            urls,
            max_age,
            Box::new(IndexTransport::new(INDEX_MAX_BYTES)),
            Box::new(IndexTransport::new(PACKAGE_MAX_BYTES)),
            trust,
        ))
    }

    /// A client with injected transports and trust store.
    ///
    /// The seam every unit test uses: a test that reached the network would not
    /// be testing this crate.
    pub fn with_parts(
        cache_dir: &Path,
        urls: Vec<String>,
        max_age: Duration,
        index_transport: Box<dyn PackageFetcher>,
        package_transport: Box<dyn PackageFetcher>,
        trust: TrustStore,
    ) -> Self {
        Self {
            cache_root: cache_dir.join(CACHE_SUBDIRECTORY),
            urls,
            max_age,
            index_transport,
            package_transport,
            trust,
        }
    }

    /// Every configured index.
    ///
    /// With `refresh`, each is fetched afresh; without it, a cached copy within
    /// the configured lifetime is used as it stands and anything older is
    /// refreshed. A refresh that fails *for transport reasons* falls back to the
    /// last good cached copy and reports it [`Freshness::Stale`]; a refresh that
    /// fails because the document was malformed, unsigned, or signed by an
    /// untrusted key is a hard refusal that neither falls back nor overwrites
    /// the cache — a rejected document must not be able to displace a good one,
    /// and a rejection must not be softened into an older answer.
    pub fn load(&self, refresh: bool) -> Vec<IndexOutcome> {
        self.urls
            .iter()
            .map(|url| IndexOutcome {
                url: url.clone(),
                snapshot: self.load_one(url, refresh),
            })
            .collect()
    }

    fn load_one(&self, url: &str, refresh: bool) -> Result<IndexSnapshot, PackageError> {
        let cache = CacheEntry::new(&self.cache_root, url);
        let age = cache.age();
        // `>=`, so a configured lifetime of zero means "always refresh" rather
        // than "refresh once the clock's resolution notices".
        let stale = age.is_none_or(|age| age >= self.max_age);
        if !refresh && !stale {
            return self.read_cached(url, &cache, Freshness::Fresh);
        }

        match self.fetch_into_cache(url, &cache) {
            Ok(snapshot) => Ok(snapshot),
            Err(error) if !is_transport_failure(&error) => Err(error),
            Err(error) => {
                let Some(age) = age else {
                    return Err(error);
                };
                self.read_cached(
                    url,
                    &cache,
                    Freshness::Stale {
                        age_seconds: age.as_secs(),
                        reason: error.to_string(),
                    },
                )
            }
        }
    }

    fn read_cached(
        &self,
        url: &str,
        cache: &CacheEntry,
        freshness: Freshness,
    ) -> Result<IndexSnapshot, PackageError> {
        let bytes = read_capped(&cache.document, INDEX_MAX_BYTES)?;
        let (document, signer) = self.verify(url, &bytes, &cache.signature)?;
        Ok(IndexSnapshot {
            url: url.to_owned(),
            document,
            signer,
            freshness,
        })
    }

    /// Fetches, verifies, and only then replaces the cached copy.
    ///
    /// Order matters: a document is written into the cache after it has been
    /// verified, never before, so a host that starts serving garbage cannot
    /// destroy the copy the user could still install from.
    fn fetch_into_cache(&self, url: &str, cache: &CacheEntry) -> Result<IndexSnapshot, PackageError> {
        let scratch = cache.scratch()?;
        let document_path = scratch.join("index.json");
        let signature_path = scratch.join("index.json.sig");
        let result = self
            .fetch_verified(url, &document_path, &signature_path)
            .and_then(|(document, signer)| {
                cache.publish(&document_path, &signature_path, url)?;
                Ok(IndexSnapshot {
                    url: url.to_owned(),
                    document,
                    signer,
                    freshness: Freshness::Fresh,
                })
            });
        let _ = fs::remove_dir_all(&scratch);
        result
    }

    fn fetch_verified(
        &self,
        url: &str,
        document_path: &Path,
        signature_path: &Path,
    ) -> Result<(PluginIndexDocument, TrustedSigner), PackageError> {
        self.index_transport.fetch(url, document_path)?;
        // A missing sidecar is an unsigned index, which is a refusal and not a
        // transport failure: falling back to a cached copy here would let a host
        // that dropped its signature keep serving through the cache forever.
        self.index_transport
            .fetch(&format!("{url}.sig"), signature_path)
            .map_err(|error| {
                PackageError::IndexSignature(format!(
                    "{url} is unsigned: no detached signature at {url}.sig ({error})"
                ))
            })?;
        let bytes = read_capped(document_path, INDEX_MAX_BYTES)?;
        self.verify(url, &bytes, signature_path)
    }

    /// Verifies `bytes` against the detached signature at `signature_path`.
    fn verify(
        &self,
        url: &str,
        bytes: &[u8],
        signature_path: &Path,
    ) -> Result<(PluginIndexDocument, TrustedSigner), PackageError> {
        let signed = read_signature_file(signature_path).map_err(|error| {
            PackageError::IndexSignature(format!("{url} has no usable detached signature: {error}"))
        })?;
        let signer = verify_signed_manifest(bytes, &signed, &self.trust, url)
            .map_err(|error| PackageError::IndexSignature(error.to_string()))?;
        // Parsed only after the signature holds, so a hostile document never
        // reaches the parser on the strength of having been served.
        let document = PluginIndexDocument::parse(bytes)?;
        Ok((document, signer))
    }

    /// Downloads the package `entry` names into `destination`, refusing bytes
    /// that do not hash to the digest the index published.
    ///
    /// The refusal names both digests, because "hash mismatch" without them
    /// tells an operator nothing about whether they are looking at a corrupted
    /// download, a stale index, or an attack. The rejected file is deleted
    /// rather than left behind for a later command to mistake for a package.
    pub fn download_package(&self, entry: &IndexEntry, destination: &Path) -> Result<(), PackageError> {
        self.package_transport.fetch(&entry.download_url, destination)?;
        let actual = match package_digest(destination) {
            Ok(digest) => digest,
            Err(error) => {
                let _ = fs::remove_file(destination);
                return Err(error);
            }
        };
        if !constant_time_hex_eq(&actual, &entry.package_digest) {
            let _ = fs::remove_file(destination);
            return Err(PackageError::HashMismatch(format!(
                "`{}` from {}: the index names package-digest {}, the download hashes to {actual}",
                entry.id, entry.download_url, entry.package_digest
            )));
        }
        Ok(())
    }
}

/// Whether an error means "the bytes did not arrive" rather than "the bytes
/// were unacceptable". Only the former may fall back to a cached copy.
fn is_transport_failure(error: &PackageError) -> bool {
    matches!(error, PackageError::SourceUnavailable(_) | PackageError::Io(_))
}

// ---------------------------------------------------------------------------
// The on-disk cache
// ---------------------------------------------------------------------------

/// Where one index URL's last good copy lives.
///
/// The directory name is a digest of the URL rather than the URL itself: a URL
/// holds `/`, `:` and `%`, and a scheme for escaping them into a filename is a
/// second thing to get wrong. The URL is written beside the copy so a person
/// reading the cache can still tell what a directory is.
#[derive(Debug)]
struct CacheEntry {
    directory: PathBuf,
    document: PathBuf,
    signature: PathBuf,
}

impl CacheEntry {
    fn new(root: &Path, url: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(url.as_bytes());
        let digest = hex_lower(&hasher.finalize());
        let directory = root.join(&digest[..32]);
        Self {
            document: directory.join("index.json"),
            signature: directory.join("index.json.sig"),
            directory,
        }
    }

    /// How long ago the cached document was written, or `None` when there is no
    /// cached document or the filesystem reports no usable timestamp.
    fn age(&self) -> Option<Duration> {
        let modified = fs::metadata(&self.document).ok()?.modified().ok()?;
        SystemTime::now().duration_since(modified).ok()
    }

    /// A scratch directory beside the cached copy, emptied first.
    fn scratch(&self) -> Result<PathBuf, PackageError> {
        let scratch = self.directory.join("incoming");
        let _ = fs::remove_dir_all(&scratch);
        fs::create_dir_all(&scratch)?;
        Ok(scratch)
    }

    /// Moves a verified document and its signature into place.
    fn publish(&self, document: &Path, signature: &Path, url: &str) -> Result<(), PackageError> {
        fs::create_dir_all(&self.directory)?;
        fs::rename(document, &self.document)?;
        fs::rename(signature, &self.signature)?;
        fs::write(self.directory.join("url"), url.as_bytes())?;
        Ok(())
    }
}

/// Reads a whole file, refusing anything over `cap` rather than truncating it.
fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>, PackageError> {
    let file = fs::File::open(path).map_err(|error| {
        PackageError::SourceUnavailable(format!("{} could not be read: {error}", path.display()))
    })?;
    let mut bytes = Vec::new();
    // One byte past the ceiling, so an over-long file is detected rather than
    // silently truncated into a document that parses.
    file.take(cap + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(PackageError::MalformedIndex(format!(
            "{} is larger than the {cap} byte limit",
            path.display()
        )));
    }
    Ok(bytes)
}

/// The lowercase hex SHA-256 of a file, read in bounded chunks.
///
/// Published because a publisher generating an index needs exactly the digest
/// this client will recompute, and two implementations of "the SHA-256 of a
/// package" are two chances to disagree about it.
pub fn package_digest(path: &Path) -> Result<String, PackageError> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_urls_drop_blanks_and_duplicates() {
        assert_eq!(
            index_urls(Some(" https://a/i.json , ,https://b/i.json,https://a/i.json ")),
            vec!["https://a/i.json".to_owned(), "https://b/i.json".to_owned()]
        );
        assert!(index_urls(None).is_empty());
        assert!(index_urls(Some("  ")).is_empty());
    }

    /// A typo in a cache lifetime must cost a fetch, never the command.
    #[test]
    fn an_unreadable_max_age_takes_the_default() {
        assert_eq!(index_max_age(Some("60")), Duration::from_secs(60));
        assert_eq!(index_max_age(Some("soon")), DEFAULT_INDEX_MAX_AGE);
        assert_eq!(index_max_age(None), DEFAULT_INDEX_MAX_AGE);
    }

    #[test]
    fn a_file_url_names_a_local_path() {
        assert_eq!(
            local_path("file:///tmp/i.json"),
            Some(PathBuf::from("/tmp/i.json"))
        );
        assert_eq!(
            local_path("file://localhost/tmp/i.json"),
            Some(PathBuf::from("/tmp/i.json"))
        );
        assert_eq!(
            local_path("file:///tmp/with%20space/i.json"),
            Some(PathBuf::from("/tmp/with space/i.json"))
        );
        assert_eq!(
            local_path("file:///C:/tmp/i.json"),
            Some(PathBuf::from("C:/tmp/i.json"))
        );
        // Another machine's share is not a path this client knows how to read.
        assert_eq!(local_path("file://server/share/i.json"), None);
        assert_eq!(local_path("https://example.test/i.json"), None);
    }
}
