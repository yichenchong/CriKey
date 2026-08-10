//! Remote catalog sources (spec 2.2 "distributed or remote indexing", ADR-0016).
//!
//! Catalog content that lives on another machine — a shared team index, a
//! mounted file server — searched alongside local items.
//!
//! # One more owner, not a second search path
//!
//! A remote source is an ordinary catalog owner. What arrives over the network
//! is one [`crikey_catalog::CachedSlice`] document, decoded by the same bounded
//! field-by-field decoder the on-disk cache uses, and published through
//! [`SearchService::replace_catalog`] — the single live publication edge every
//! local provider already goes through (ADR-0008). Ranking, dedup, result
//! limits, per-owner fault isolation and the persistent cache therefore apply
//! to it unchanged, and there is no query path that knows a remote source
//! exists.
//!
//! # Nothing happens on the query path
//!
//! Fetching runs on a thread this module spawns, and nothing here is reachable
//! from [`SearchService::submit_query`]. The host drives three synchronous,
//! non-blocking calls from wherever it already does background work:
//! [`RemoteCatalogService::poll`] starts due fetches,
//! [`RemoteCatalogService::apply`] admits whatever finished, and
//! [`RemoteCatalogService::request_refresh`] is what a command or a hotkey
//! calls. A launcher whose configuration names no source does none of that: the
//! service reports [`RemoteCatalogService::is_idle`] and the host skips it
//! (README invariant 2).
//!
//! # Offline first
//!
//! Every failure — an unreachable endpoint, a truncated document, a digest that
//! does not match, an untrusted signature, an item the catalog would refuse — is
//! a refusal of the *new* document. The retained slice keeps serving, including
//! across restarts, because the last good document was written to the ordinary
//! per-owner catalog cache. Failures are returned as [`RemoteReport`] values for
//! the host's diagnostics rather than swallowed (README invariant 7).
//!
//! # Everything from outside is hostile
//!
//! The manifest is capped at [`MAX_MANIFEST_BYTES`] and parsed line by line with
//! no unknown fields, no duplicates and no relative paths. The slice's length is
//! declared before it is fetched and refused before a byte is read if it exceeds
//! the source's ceiling; the read itself stops one byte past that ceiling, so a
//! lying server is bounded by the operator's number and not its own. The digest
//! is checked before the document is decoded, the signature before the document
//! is trusted, and every item before it is admitted (README invariant 8).

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crikey_catalog::{decode_slice_document, CachedSlice, CatalogError};
use crikey_core::{Item, PluginId};
use crikey_package_manager::{verify_signed_manifest, SignedManifest, TrustStore};
use sha2::{Digest, Sha256};

use crate::SearchService;

/// Namespace every remote source's catalog owner id sits under.
///
/// Distinct from `modern.`, `legacy.`, `native.` and `builtin.` so a remote
/// owner is recognisable in a health report, a cache directory listing and a
/// query trace without consulting configuration.
pub const REMOTE_OWNER_PREFIX: &str = "remote.";

/// Largest manifest this client will read.
///
/// A manifest is five short lines. Four kibibytes is generous for a
/// hand-written one with comments and small enough that fetching it costs
/// nothing worth scheduling around.
pub const MAX_MANIFEST_BYTES: u64 = 4 * 1024;

/// Largest detached signature document this client will read. One TOML table
/// with two hex fields.
pub const MAX_SIGNATURE_BYTES: u64 = 4 * 1024;

/// How long one fetch may take before it is abandoned.
///
/// A refresh that has not finished in half a minute is not going to help this
/// tick, and the retained slice is already serving.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// First line of a manifest, version included so a future layout is refused
/// rather than misread.
const MANIFEST_MAGIC: &str = "crikey-remote-catalog 1";

const FIELD_SLICE: &str = "slice";
const FIELD_BYTES: &str = "bytes";
const FIELD_SHA256: &str = "sha256";
const FIELD_SIGNATURE: &str = "signature";

/// Longest document name a manifest may name.
const MAX_DOCUMENT_NAME_BYTES: usize = 128;

/// Reserved for the first read of a document whose size is not yet known, so a
/// small document costs one allocation and a large one grows from a sane base
/// instead of from nothing.
const READ_CHUNK_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// declaration
// ---------------------------------------------------------------------------

/// One remote catalog source, as the host declares it.
///
/// A plain value type rather than `crikey_config::RemoteCatalogSource`: the
/// composition root reads configuration and hands this crate values, exactly as
/// it does for [`crate::PluginConfiguration`] (ADR-0001).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSource {
    /// Catalog owner the source publishes as, derived from its name.
    pub owner: PluginId,
    /// The operator's name for the source, used in diagnostics.
    pub name: String,
    /// URL of the source's manifest.
    pub url: String,
    /// Milliseconds between automatic refreshes; zero means manual only.
    pub interval_ms: u64,
    /// Ceiling on the bytes one refresh may read.
    pub max_bytes: u64,
    /// Whether an unsigned document is refused.
    pub require_signature: bool,
    /// The trusted key name the document must be signed by, if one was pinned.
    pub signing_key: Option<String>,
}

impl RemoteSource {
    /// A source with the given name and manifest URL, refreshed only when asked.
    ///
    /// The host overwrites the remaining fields from configuration; they are
    /// spelled out here so a source constructed in a test is complete.
    pub fn new(name: &str, url: &str) -> Self {
        Self {
            owner: remote_owner(name),
            name: name.to_owned(),
            url: url.to_owned(),
            interval_ms: 0,
            max_bytes: 32 * 1024 * 1024,
            require_signature: false,
            signing_key: None,
        }
    }
}

/// The catalog owner id a source named `name` publishes as.
pub fn remote_owner(name: &str) -> PluginId {
    PluginId(format!("{REMOTE_OWNER_PREFIX}{name}"))
}

// ---------------------------------------------------------------------------
// failures
// ---------------------------------------------------------------------------

/// Why one refresh did not produce a slice the launcher will serve.
///
/// Every variant names the artefact it is about, because a shared index has
/// several documents and "the remote catalog failed" would not tell an operator
/// which file on which server to look at.
#[derive(Debug)]
pub enum RemoteCatalogError {
    /// The document could not be retrieved at all.
    Unreachable { url: String, reason: String },
    /// A URL this client will not fetch.
    UnsupportedUrl { url: String, reason: &'static str },
    /// The manifest is not a manifest.
    Manifest { url: String, reason: &'static str },
    /// The declared length exceeds the source's ceiling, so nothing was read.
    Oversized { url: String, declared: u64, limit: u64 },
    /// More bytes arrived than the ceiling allows.
    TooLong { url: String, limit: u64 },
    /// The document's length disagrees with the manifest.
    LengthMismatch {
        url: String,
        declared: u64,
        actual: usize,
    },
    /// The document's digest disagrees with the manifest.
    DigestMismatch {
        url: String,
        declared: String,
        actual: String,
    },
    /// A signature is required and the manifest names none.
    Unsigned { url: String },
    /// The signature is present and does not hold, or its signer is not trusted.
    Signature { url: String, reason: String },
    /// The signature holds for a key other than the one the operator pinned.
    PinnedSigner {
        url: String,
        expected: String,
        signer: String,
    },
    /// The slice document itself could not be trusted.
    Document { url: String },
    /// One item is not one this launcher will admit.
    Item {
        url: String,
        position: usize,
        reason: &'static str,
    },
    /// The live catalog refused the slice.
    Refused { url: String, error: CatalogError },
}

impl fmt::Display for RemoteCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable { url, reason } => write!(formatter, "{url} could not be fetched: {reason}"),
            Self::UnsupportedUrl { url, reason } => write!(formatter, "{url} {reason}"),
            Self::Manifest { url, reason } => write!(formatter, "{url} is not a usable manifest: {reason}"),
            Self::Oversized { url, declared, limit } => write!(
                formatter,
                "{url} declares {declared} bytes; this source may read {limit}"
            ),
            Self::TooLong { url, limit } => {
                write!(formatter, "{url} delivered more than the {limit} byte ceiling")
            }
            Self::LengthMismatch {
                url,
                declared,
                actual,
            } => write!(
                formatter,
                "{url} is {actual} bytes; its manifest declares {declared}"
            ),
            Self::DigestMismatch {
                url,
                declared,
                actual,
            } => write!(
                formatter,
                "{url} has digest sha256:{actual}; its manifest declares sha256:{declared}"
            ),
            Self::Unsigned { url } => write!(
                formatter,
                "{url} is unsigned and this source requires a signature"
            ),
            Self::Signature { url, reason } => write!(formatter, "{url} signature refused: {reason}"),
            Self::PinnedSigner {
                url,
                expected,
                signer,
            } => write!(
                formatter,
                "{url} is signed by trusted key `{signer}`; this source requires `{expected}`"
            ),
            Self::Document { url } => write!(
                formatter,
                "{url} is not a readable catalog slice document for this schema version"
            ),
            Self::Item {
                url,
                position,
                reason,
            } => write!(formatter, "{url} item {position} {reason}"),
            Self::Refused { url, error } => write!(formatter, "{url} was refused by the catalog: {error}"),
        }
    }
}

impl std::error::Error for RemoteCatalogError {}

// ---------------------------------------------------------------------------
// transport
// ---------------------------------------------------------------------------

/// Retrieves the bytes a URL names, reading no more than it is allowed to.
///
/// Behind a trait for the same reason package fetching is: a test that opens a
/// socket is not a test of this module. Every test in this workspace supplies a
/// fetcher that reads from disk, and the production fetcher's `file://` half is
/// exercised directly.
pub trait CatalogFetcher: Send + Sync {
    /// Reads `url`, refusing rather than truncating past `max_bytes`.
    fn fetch(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, RemoteCatalogError>;
}

/// The production fetcher: one HTTPS GET, or one read of a mounted path.
///
/// `file://` is not a convenience for tests. "A remote file server" is a
/// mounted share once the operating system has done its job, and a launcher
/// that could only speak HTTPS would leave that case to a plugin.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultCatalogFetcher;

impl CatalogFetcher for DefaultCatalogFetcher {
    fn fetch(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, RemoteCatalogError> {
        if let Some(rest) = url.strip_prefix("file://") {
            return read_file_url(url, rest, max_bytes);
        }
        if url.starts_with("https://") {
            return https_get(url, max_bytes);
        }
        Err(RemoteCatalogError::UnsupportedUrl {
            url: url.to_owned(),
            reason: "is not an `https://` or `file://` URL",
        })
    }
}

fn https_get(url: &str, max_bytes: u64) -> Result<Vec<u8>, RemoteCatalogError> {
    let agent = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(FETCH_TIMEOUT))
        .build()
        .new_agent();
    let mut response = agent
        .get(url)
        .call()
        .map_err(|error| RemoteCatalogError::Unreachable {
            url: url.to_owned(),
            reason: error.to_string(),
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(RemoteCatalogError::Unreachable {
            url: url.to_owned(),
            reason: format!("the server answered {status}"),
        });
    }
    // One byte past the ceiling, so an over-long body is refused rather than
    // silently truncated into a document that might still parse.
    let mut reader = response.body_mut().as_reader().take(max_bytes.saturating_add(1));
    read_capped(url, &mut reader, max_bytes)
}

/// Decodes a `file://` URL and reads it, refusing before allocating when the
/// file on disk is already too large.
fn read_file_url(url: &str, rest: &str, max_bytes: u64) -> Result<Vec<u8>, RemoteCatalogError> {
    // `file://host/path` names a share this client has no way to mount itself.
    let Some(encoded) = rest.strip_prefix('/') else {
        return Err(RemoteCatalogError::UnsupportedUrl {
            url: url.to_owned(),
            reason: "must be `file:///` with an empty host and an absolute path",
        });
    };
    let Some(decoded) = percent_decode(encoded) else {
        return Err(RemoteCatalogError::UnsupportedUrl {
            url: url.to_owned(),
            reason: "contains a percent escape that is not two hexadecimal digits",
        });
    };
    let path = local_path(&decoded);
    let file = File::open(&path).map_err(|error| RemoteCatalogError::Unreachable {
        url: url.to_owned(),
        reason: error.to_string(),
    })?;
    // The cheapest possible refusal of an oversized document: the length is
    // already recorded, so nothing is read and nothing is reserved.
    if let Ok(metadata) = file.metadata() {
        if metadata.len() > max_bytes {
            return Err(RemoteCatalogError::Oversized {
                url: url.to_owned(),
                declared: metadata.len(),
                limit: max_bytes,
            });
        }
    }
    let mut reader = file.take(max_bytes.saturating_add(1));
    read_capped(url, &mut reader, max_bytes)
}

/// The filesystem path a `file:///` URL's decoded body names.
///
/// The body arrives with its leading `/` already stripped, because that slash
/// is the URL's empty-authority separator rather than part of the path. On a
/// POSIX host putting it back is the whole job. On Windows it must not go
/// back: RFC 8089 spells a local drive path `file:///C:/dir/file`, so the body
/// is `C:/dir/file`, and `/C:/dir/file` is not a path Windows can open. A URL
/// naming a rooted path with no drive (`file:///Windows/win.ini`) still keeps
/// its slash, since that is what the host means by an absolute path.
fn local_path(decoded: &str) -> PathBuf {
    #[cfg(windows)]
    {
        let mut characters = decoded.chars();
        let drive = characters.next().is_some_and(|first| first.is_ascii_alphabetic())
            && characters.next() == Some(':')
            && matches!(characters.next(), Some('/') | Some('\\') | None);
        if drive {
            return PathBuf::from(decoded);
        }
    }
    PathBuf::from(format!("/{decoded}"))
}

/// Reads a reader already limited to `max_bytes + 1` and refuses at the ceiling.
///
/// The reservation is the smaller of the ceiling and one chunk, so a source
/// permitted 256 MiB does not reserve 256 MiB to read a manifest.
fn read_capped(url: &str, reader: &mut impl Read, max_bytes: u64) -> Result<Vec<u8>, RemoteCatalogError> {
    let reserve = usize::try_from(max_bytes.saturating_add(1))
        .unwrap_or(READ_CHUNK_BYTES)
        .min(READ_CHUNK_BYTES);
    let mut bytes = Vec::with_capacity(reserve);
    reader
        .read_to_end(&mut bytes)
        .map_err(|error| RemoteCatalogError::Unreachable {
            url: url.to_owned(),
            reason: error.to_string(),
        })?;
    if bytes.len() as u64 > max_bytes {
        return Err(RemoteCatalogError::TooLong {
            url: url.to_owned(),
            limit: max_bytes,
        });
    }
    Ok(bytes)
}

/// Decodes `%XX` escapes, or `None` for an escape that is not two hex digits.
///
/// Implemented rather than refused because a mounted share is routinely called
/// `Team Documents`, and a launcher that could not name it would push the
/// operator into renaming their fileserver.
fn percent_decode(text: &str) -> Option<String> {
    if !text.contains('%') {
        return Some(text.to_owned());
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_value(*bytes.get(index + 1)?)?;
            let low = hex_value(*bytes.get(index + 2)?)?;
            out.push(high * 16 + low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// manifest
// ---------------------------------------------------------------------------

/// What a source's manifest says about the document it publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteManifest {
    /// Document name, relative to the manifest's own directory.
    pub slice: String,
    /// The document's length in bytes.
    pub bytes: u64,
    /// The document's SHA-256, lowercase hex.
    pub sha256: String,
    /// Detached signature document name, when the publisher signed the slice.
    pub signature: Option<String>,
}

impl RemoteManifest {
    /// Parses a manifest, refusing anything it does not fully understand.
    ///
    /// Blank lines and `#` comments are allowed so the file can be maintained by
    /// hand. Everything else is exact: the version line comes first, each field
    /// appears at most once, an unknown field is a refusal rather than a line to
    /// skip, and a document name is one plain file name so a manifest cannot
    /// point the client at another directory or another host.
    pub fn parse(url: &str, bytes: &[u8]) -> Result<Self, RemoteCatalogError> {
        let refuse = |reason: &'static str| RemoteCatalogError::Manifest {
            url: url.to_owned(),
            reason,
        };
        let text = std::str::from_utf8(bytes).map_err(|_| refuse("is not valid UTF-8"))?;
        let mut lines = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'));
        if lines.next() != Some(MANIFEST_MAGIC) {
            return Err(refuse("does not begin with `crikey-remote-catalog 1`"));
        }

        let mut slice = None;
        let mut declared_bytes = None;
        let mut sha256 = None;
        let mut signature = None;
        for line in lines {
            let (field, value) = line
                .split_once(' ')
                .ok_or_else(|| refuse("has a field with no value"))?;
            let value = value.trim();
            let slot = match field {
                FIELD_SLICE => &mut slice,
                FIELD_SIGNATURE => &mut signature,
                FIELD_BYTES => {
                    if declared_bytes.is_some() {
                        return Err(refuse("declares a field twice"));
                    }
                    declared_bytes = Some(
                        value
                            .parse::<u64>()
                            .map_err(|_| refuse("declares a length that is not a whole number"))?,
                    );
                    continue;
                }
                FIELD_SHA256 => {
                    if sha256.is_some() {
                        return Err(refuse("declares a field twice"));
                    }
                    if value.len() != 64
                        || !value
                            .bytes()
                            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
                    {
                        return Err(refuse(
                            "declares a digest that is not 64 lowercase hexadecimal digits",
                        ));
                    }
                    sha256 = Some(value.to_owned());
                    continue;
                }
                _ => return Err(refuse("names a field this client does not understand")),
            };
            if slot.is_some() {
                return Err(refuse("declares a field twice"));
            }
            if !is_document_name(value) {
                return Err(refuse(
                    "names a document that is not one plain file name beside the manifest",
                ));
            }
            *slot = Some(value.to_owned());
        }

        Ok(Self {
            slice: slice.ok_or_else(|| refuse("names no slice document"))?,
            bytes: declared_bytes.ok_or_else(|| refuse("declares no length"))?,
            sha256: sha256.ok_or_else(|| refuse("declares no digest"))?,
            signature,
        })
    }
}

/// Accepts one plain file name: no directories, no schemes, no traversal.
///
/// This is what confines a source to the directory its manifest sits in. A
/// manifest that could name an absolute URL would let whoever controls the
/// index redirect the client to a host the operator never configured.
fn is_document_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_DOCUMENT_NAME_BYTES
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

/// The manifest's directory, including its trailing slash.
///
/// The manifest URL is validated to name a document rather than a directory
/// before it reaches here, so there is always a slash to cut at; a URL without
/// one resolves to itself, which then fails to fetch under its own name.
fn base_of(url: &str) -> &str {
    match url.rfind('/') {
        Some(at) => &url[..=at],
        None => url,
    }
}

// ---------------------------------------------------------------------------
// one refresh
// ---------------------------------------------------------------------------

/// A verified remote document, owned by the local source it arrived for.
#[derive(Debug)]
pub struct RemoteSlice {
    /// The slice, with every item already re-owned to the source's owner id.
    pub slice: CachedSlice,
    /// The owner the publisher stamped into the document, retained for
    /// diagnostics: it says which index the items came from, which the local
    /// owner id deliberately does not.
    pub published_by: PluginId,
    /// The trusted key name the document was signed by, when it was signed.
    pub signer: Option<String>,
}

/// Fetches, verifies and re-owns one source's document.
///
/// Synchronous and free of shared state on purpose: this is the whole rule, so
/// every refusal below is reachable from a test without a thread or a socket.
/// The caller decides where it runs; [`RemoteCatalogService`] runs it on a
/// thread of its own.
pub fn fetch_source(
    source: &RemoteSource,
    fetcher: &dyn CatalogFetcher,
    trust: &TrustStore,
) -> Result<RemoteSlice, RemoteCatalogError> {
    let manifest_bytes = fetcher.fetch(&source.url, MAX_MANIFEST_BYTES)?;
    let manifest = RemoteManifest::parse(&source.url, &manifest_bytes)?;

    let base = base_of(&source.url);
    let slice_url = format!("{base}{}", manifest.slice);
    // Refused on the manifest's own word, before a byte of the document is
    // requested: the cheap refusal comes first so an index that has grown past
    // what this launcher will hold costs one four-kilobyte read.
    if manifest.bytes > source.max_bytes {
        return Err(RemoteCatalogError::Oversized {
            url: slice_url,
            declared: manifest.bytes,
            limit: source.max_bytes,
        });
    }

    let document = fetcher.fetch(&slice_url, manifest.bytes)?;
    if document.len() as u64 != manifest.bytes {
        return Err(RemoteCatalogError::LengthMismatch {
            url: slice_url,
            declared: manifest.bytes,
            actual: document.len(),
        });
    }
    let digest = hex_digest(&document);
    if digest != manifest.sha256 {
        return Err(RemoteCatalogError::DigestMismatch {
            url: slice_url,
            declared: manifest.sha256,
            actual: digest,
        });
    }

    let signer = verify_signature(source, &slice_url, &manifest, &document, fetcher, trust)?;

    let Some(mut slice) = decode_slice_document(&document) else {
        return Err(RemoteCatalogError::Document { url: slice_url });
    };
    validate_items(&slice_url, &slice.items)?;

    let published_by = std::mem::replace(&mut slice.plugin, source.owner.clone());
    // Ownership is rewritten here, on the fetching thread, so the publication
    // edge on the host's thread is the same one-move call a local provider
    // makes. A shared index does not know the name each subscriber filed it
    // under, so it stamps its own; the document's own decoder has already
    // proved every item agreed with that stamp.
    for item in &mut slice.items {
        item.plugin_id = source.owner.clone();
    }
    Ok(RemoteSlice {
        slice,
        published_by,
        signer,
    })
}

/// Applies the source's signature policy and returns the trusted signer.
fn verify_signature(
    source: &RemoteSource,
    slice_url: &str,
    manifest: &RemoteManifest,
    document: &[u8],
    fetcher: &dyn CatalogFetcher,
    trust: &TrustStore,
) -> Result<Option<String>, RemoteCatalogError> {
    let Some(name) = manifest.signature.as_deref() else {
        // Unsigned is a refusal only where the operator asked for signatures.
        // A source with no policy is transport-checked, not authenticated, and
        // this module does not pretend otherwise.
        return if source.require_signature {
            Err(RemoteCatalogError::Unsigned {
                url: slice_url.to_owned(),
            })
        } else {
            Ok(None)
        };
    };

    let signature_url = format!("{}{name}", base_of(&source.url));
    let bytes = fetcher.fetch(&signature_url, MAX_SIGNATURE_BYTES)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| RemoteCatalogError::Manifest {
        url: signature_url.clone(),
        reason: "is not valid UTF-8",
    })?;
    let signed = SignedManifest::parse(text).map_err(|error| RemoteCatalogError::Signature {
        url: signature_url.clone(),
        reason: error.to_string(),
    })?;
    let signer = verify_signed_manifest(document, &signed, trust, slice_url).map_err(|error| {
        RemoteCatalogError::Signature {
            url: slice_url.to_owned(),
            reason: error.to_string(),
        }
    })?;
    if let Some(expected) = source.signing_key.as_deref() {
        if signer.name != expected {
            return Err(RemoteCatalogError::PinnedSigner {
                url: slice_url.to_owned(),
                expected: expected.to_owned(),
                signer: signer.name,
            });
        }
    }
    Ok(Some(signer.name))
}

/// Refuses an item a local publisher would have been refused, and two a remote
/// one is additionally held to.
///
/// The catalog's own admission rules — ownership, counts, payload sizes — are
/// applied by [`SearchService::replace_catalog`] and are not restated here. What
/// is checked here is what those rules do not cover and a remote publisher can
/// get wrong: an item with no identity cannot be deduplicated or executed, and
/// an item with no visible label is a row a user cannot read.
fn validate_items(url: &str, items: &[Item]) -> Result<(), RemoteCatalogError> {
    for (position, item) in items.iter().enumerate() {
        let reason = if item.stable_id.0.is_empty() {
            Some("has an empty stable id")
        } else if item.label.trim().is_empty() {
            Some("has no visible label")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(RemoteCatalogError::Item {
                url: url.to_owned(),
                position,
                reason,
            });
        }
    }
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
    hex
}

const HEX_DIGITS: [u8; 16] = *b"0123456789abcdef";

// ---------------------------------------------------------------------------
// scheduling
// ---------------------------------------------------------------------------

/// Per-source refresh state: what is owed, what is running, what happened last.
#[derive(Debug)]
struct SourceState {
    source: RemoteSource,
    /// A refresh has been asked for and not yet started.
    pending: bool,
    /// A refresh is running, so another one is not started.
    in_flight: bool,
    /// When the interval next comes due, absent while a source has never run
    /// and for a source with no interval.
    next_due_ms: Option<u64>,
    /// Configuration generation. Outcomes carry this token so a result from a
    /// source removed and re-added with the same name cannot replace its newer
    /// slice.
    generation: u64,
    last_success_ms: Option<u64>,
    last_items: usize,
    last_error: Option<String>,
}

/// What a host can report about one source without knowing how it works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSourceStatus {
    pub name: String,
    pub url: String,
    pub interval_ms: u64,
    /// Whether a refresh is running right now.
    pub refreshing: bool,
    /// Host clock value of the last document this source successfully admitted.
    pub last_success_ms: Option<u64>,
    /// Items that document contributed.
    pub items: usize,
    /// The most recent failure, retained until a refresh succeeds.
    pub last_error: Option<String>,
}

/// One completed refresh, ready to be admitted or reported.
#[derive(Debug)]
pub struct RemoteOutcome {
    pub name: String,
    pub generation: u64,
    pub result: Result<RemoteSlice, RemoteCatalogError>,
}

/// What one call to [`RemoteCatalogService::apply`] did, per source.
#[derive(Debug)]
pub enum RemoteReport {
    /// A document was verified and admitted.
    Published {
        name: String,
        published_by: PluginId,
        items: usize,
        signer: Option<String>,
    },
    /// A refresh was refused. The retained slice is still serving.
    Refused { name: String, error: RemoteCatalogError },
}

impl fmt::Display for RemoteReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Published {
                name,
                published_by,
                items,
                signer,
            } => {
                write!(
                    formatter,
                    "remote catalog `{name}` published {items} items from {}",
                    published_by.0
                )?;
                match signer {
                    Some(signer) => write!(formatter, ", signed by `{signer}`"),
                    None => write!(formatter, ", unsigned"),
                }
            }
            Self::Refused { name, error } => {
                write!(formatter, "remote catalog `{name}` refused a refresh: {error}")
            }
        }
    }
}

/// Every remote source, its refresh schedule and the thread that fetches it.
///
/// Hand-written `Debug` because a [`CatalogFetcher`] is not required to be
/// `Debug`: the trait exists so a host can supply its own, and demanding
/// `Debug` of them would buy nothing but a bound to satisfy.
pub struct RemoteCatalogService {
    sources: Vec<SourceState>,
    fetcher: Arc<dyn CatalogFetcher>,
    trust: Arc<TrustStore>,
    sender: Sender<RemoteOutcome>,
    receiver: Receiver<RemoteOutcome>,
}

impl fmt::Debug for RemoteCatalogService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteCatalogService")
            .field("sources", &self.sources)
            .finish_non_exhaustive()
    }
}

impl RemoteCatalogService {
    /// Builds a service for `sources`, each owed one refresh.
    ///
    /// Every source starts pending so a launcher picks up whatever the index
    /// published while it was not running. That first fetch still happens on the
    /// fetching thread and still cannot delay startup or a query.
    pub fn new(sources: Vec<RemoteSource>, fetcher: Arc<dyn CatalogFetcher>, trust: Arc<TrustStore>) -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            sources: sources
                .into_iter()
                .map(|source| SourceState {
                    source,
                    pending: true,
                    in_flight: false,
                    next_due_ms: None,
                    generation: 0,
                    last_success_ms: None,
                    last_items: 0,
                    last_error: None,
                })
                .collect(),
            fetcher,
            trust,
            sender,
            receiver,
        }
    }

    /// Whether this service has nothing to do, ever.
    ///
    /// True exactly when no source is configured, which is the default. A host
    /// that checks this once can skip every other call and stay bit-for-bit the
    /// launcher it was before remote indexing existed.
    pub fn is_idle(&self) -> bool {
        self.sources.is_empty()
    }

    /// Asks every source to refresh at the next [`Self::poll`].
    ///
    /// This is the coalescing point. A burst of triggers sets one pending flag
    /// per source. A worker already running is logically cancelled: it may
    /// finish its transport call, but its generation can no longer publish.
    pub fn request_refresh(&mut self) {
        for state in &mut self.sources {
            state.generation = state.generation.wrapping_add(1);
            state.pending = true;
            state.in_flight = false;
        }
    }

    /// Starts every fetch that is due and returns how many began.
    ///
    /// Non-blocking: each fetch runs on its own thread and reports back through
    /// a channel. A thread that cannot be spawned is recorded as that source's
    /// failure, so a host under thread pressure loses a refresh rather than a
    /// launcher.
    pub fn poll(&mut self, now_ms: u64) -> usize {
        let mut started = 0;
        for state in &mut self.sources {
            if state.in_flight {
                continue;
            }
            let interval_due = state.next_due_ms.is_some_and(|due| now_ms >= due);
            if !state.pending && !interval_due {
                continue;
            }
            state.pending = false;
            state.in_flight = true;
            let generation = state.generation;
            let source = state.source.clone();
            let fetcher = Arc::clone(&self.fetcher);
            let trust = Arc::clone(&self.trust);
            let sender = self.sender.clone();
            let name = source.name.clone();
            let spawned = thread::Builder::new()
                .name(format!("crikey-remote-{name}"))
                .spawn(move || {
                    let result = fetch_source(&source, fetcher.as_ref(), trust.as_ref());
                    // A closed channel means the host is gone; there is nothing
                    // to report to and nothing to clean up.
                    let _ = sender.send(RemoteOutcome {
                        name: source.name.clone(),
                        generation,
                        result,
                    });
                });
            match spawned {
                Ok(_) => started += 1,
                Err(error) => {
                    state.in_flight = false;
                    state.last_error = Some(format!("refresh thread could not start: {error}"));
                    state.next_due_ms = next_due(&state.source, now_ms);
                }
            }
        }
        started
    }

    /// Admits every finished refresh and returns what happened.
    ///
    /// The only call that touches the catalog, and the only one that has to run
    /// where [`SearchService`] lives. It never blocks: a refresh still running
    /// is simply not in this batch.
    pub fn apply(&mut self, search: &mut SearchService, now_ms: u64) -> Vec<RemoteReport> {
        let mut reports = Vec::new();
        while let Ok(outcome) = self.receiver.try_recv() {
            let Some(state) = self
                .sources
                .iter_mut()
                .find(|state| state.source.name == outcome.name)
            else {
                // A source removed by a configuration change while its refresh
                // was in the air. Its document is simply dropped.
                continue;
            };
            if outcome.generation != state.generation {
                // A newer trigger superseded this worker. Do not clear the
                // newer request's in-flight state or let stale bytes replace
                // the retained slice.
                continue;
            }
            state.in_flight = false;
            state.next_due_ms = next_due(&state.source, now_ms);
            let url = state.source.url.clone();
            match outcome.result {
                Ok(remote) => {
                    let RemoteSlice {
                        slice,
                        published_by,
                        signer,
                    } = remote;
                    match search.replace_catalog(&state.source.owner, slice.instance, slice.items) {
                        Ok(items) => {
                            state.last_success_ms = Some(now_ms);
                            state.last_items = items;
                            state.last_error = None;
                            reports.push(RemoteReport::Published {
                                name: state.source.name.clone(),
                                published_by,
                                items,
                                signer,
                            });
                        }
                        Err(error) => {
                            let error = RemoteCatalogError::Refused { url, error };
                            state.last_error = Some(error.to_string());
                            reports.push(RemoteReport::Refused {
                                name: state.source.name.clone(),
                                error,
                            });
                        }
                    }
                }
                Err(error) => {
                    state.last_error = Some(error.to_string());
                    reports.push(RemoteReport::Refused {
                        name: state.source.name.clone(),
                        error,
                    });
                }
            }
        }
        reports
    }

    /// What each source is doing, for a diagnostic command.
    pub fn status(&self) -> Vec<RemoteSourceStatus> {
        self.sources
            .iter()
            .map(|state| RemoteSourceStatus {
                name: state.source.name.clone(),
                url: state.source.url.clone(),
                interval_ms: state.source.interval_ms,
                refreshing: state.in_flight,
                last_success_ms: state.last_success_ms,
                items: state.last_items,
                last_error: state.last_error.clone(),
            })
            .collect()
    }
}

/// When a source that just finished comes due again, or `None` for a source
/// refreshed only on request.
fn next_due(source: &RemoteSource, now_ms: u64) -> Option<u64> {
    if source.interval_ms == 0 {
        None
    } else {
        Some(now_ms.saturating_add(source.interval_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owner_id_is_namespaced_so_it_cannot_collide_with_a_plugin() {
        assert_eq!(remote_owner("team"), PluginId("remote.team".to_owned()));
    }

    /// `file:///` names an absolute local path, and what "absolute" spells
    /// differs by host. Getting this wrong is invisible on the host that
    /// happens to work and total on the other: every `file://` source refuses.
    #[test]
    fn a_file_url_body_becomes_the_absolute_path_this_host_understands() {
        if cfg!(windows) {
            // RFC 8089's local-drive form. `/C:/srv/index.txt` is not a path
            // Windows can open, so the separator must not be put back.
            assert_eq!(local_path("C:/srv/index.txt"), PathBuf::from("C:/srv/index.txt"));
            assert_eq!(local_path("c:/srv"), PathBuf::from("c:/srv"));
            // A rooted path with no drive still means what the host means by
            // absolute, so it keeps the slash the URL's authority separator ate.
            assert_eq!(local_path("Windows/win.ini"), PathBuf::from("/Windows/win.ini"));
        } else {
            assert_eq!(local_path("srv/index.txt"), PathBuf::from("/srv/index.txt"));
            // A drive letter is not special here: it is an ordinary file name,
            // and silently dropping the root would escape the named directory.
            assert_eq!(local_path("C:/srv"), PathBuf::from("/C:/srv"));
        }
    }

    #[test]
    fn a_manifest_directory_is_the_url_up_to_its_last_slash() {
        assert_eq!(base_of("https://host/a/b/index.txt"), "https://host/a/b/");
        assert_eq!(base_of("file:///srv/index.txt"), "file:///srv/");
    }

    #[test]
    fn a_document_name_is_one_plain_file_name() {
        assert!(is_document_name("catalog.slice"));
        assert!(!is_document_name("../catalog.slice"));
        assert!(!is_document_name("nested/catalog.slice"));
        assert!(!is_document_name("https://elsewhere/catalog.slice"));
        assert!(!is_document_name(""));
        assert!(!is_document_name(".."));
    }

    #[test]
    fn a_complete_manifest_parses() {
        let text = format!(
            "{MANIFEST_MAGIC}\n# a comment\n\nslice catalog.slice\nbytes 42\nsha256 {}\nsignature catalog.slice.sig\n",
            "a".repeat(64)
        );
        let manifest =
            RemoteManifest::parse("file:///srv/index.txt", text.as_bytes()).expect("it is a manifest");
        assert_eq!(
            manifest,
            RemoteManifest {
                slice: "catalog.slice".to_owned(),
                bytes: 42,
                sha256: "a".repeat(64),
                signature: Some("catalog.slice.sig".to_owned()),
            }
        );
    }

    #[test]
    fn a_manifest_without_the_version_line_is_refused() {
        let text = format!("slice catalog.slice\nbytes 1\nsha256 {}\n", "a".repeat(64));
        assert!(RemoteManifest::parse("file:///srv/index.txt", text.as_bytes()).is_err());
    }

    #[test]
    fn a_manifest_naming_an_unknown_field_is_refused_rather_than_skipped() {
        let text = format!(
            "{MANIFEST_MAGIC}\nslice catalog.slice\nbytes 1\nsha256 {}\ncompression zstd\n",
            "a".repeat(64)
        );
        assert!(RemoteManifest::parse("file:///srv/index.txt", text.as_bytes()).is_err());
    }

    #[test]
    fn a_manifest_declaring_a_field_twice_is_refused() {
        let text = format!(
            "{MANIFEST_MAGIC}\nslice one.slice\nslice two.slice\nbytes 1\nsha256 {}\n",
            "a".repeat(64)
        );
        assert!(RemoteManifest::parse("file:///srv/index.txt", text.as_bytes()).is_err());
    }

    #[test]
    fn a_manifest_pointing_outside_its_own_directory_is_refused() {
        let text = format!(
            "{MANIFEST_MAGIC}\nslice ../../etc/passwd\nbytes 1\nsha256 {}\n",
            "a".repeat(64)
        );
        assert!(RemoteManifest::parse("file:///srv/index.txt", text.as_bytes()).is_err());
    }

    #[test]
    fn an_uppercase_digest_is_refused_so_comparison_needs_no_folding() {
        let text = format!(
            "{MANIFEST_MAGIC}\nslice catalog.slice\nbytes 1\nsha256 {}\n",
            "A".repeat(64)
        );
        assert!(RemoteManifest::parse("file:///srv/index.txt", text.as_bytes()).is_err());
    }

    #[test]
    fn percent_escapes_decode_and_a_broken_escape_does_not() {
        assert_eq!(
            percent_decode("srv/Team%20Docs/i.txt").as_deref(),
            Some("srv/Team Docs/i.txt")
        );
        assert_eq!(
            percent_decode("srv/plain/i.txt").as_deref(),
            Some("srv/plain/i.txt")
        );
        assert_eq!(percent_decode("srv/%zz/i.txt"), None);
        assert_eq!(percent_decode("srv/trailing%2"), None);
    }

    #[test]
    fn the_digest_is_the_one_sha256_everyone_else_computes() {
        // The published SHA-256 of the empty input.
        assert_eq!(
            hex_digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn an_interval_of_zero_never_comes_due_on_its_own() {
        let source = RemoteSource::new("team", "file:///srv/index.txt");
        assert_eq!(next_due(&source, 1_000), None);
    }

    #[test]
    fn an_interval_schedules_the_next_refresh_from_when_the_last_one_finished() {
        let mut source = RemoteSource::new("team", "file:///srv/index.txt");
        source.interval_ms = 500;
        assert_eq!(next_due(&source, 1_000), Some(1_500));
    }

    #[test]
    fn an_item_with_no_identity_or_no_label_is_refused() {
        let owner = remote_owner("team");
        let mut item = Item {
            stable_id: crikey_core::ItemId("one".to_owned()),
            plugin_id: owner.clone(),
            category: crikey_core::Category::Application,
            label: "Editor".to_owned(),
            description: String::new(),
            target: "/usr/bin/editor".to_owned(),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: crikey_core::ArgumentPolicy::Forbidden,
            hit_policy: crikey_core::HitPolicy::Recorded,
            score_hint: 0,
            metadata: std::collections::BTreeMap::new(),
            actions: Vec::new(),
        };
        assert!(validate_items("file:///srv/catalog.slice", std::slice::from_ref(&item)).is_ok());

        item.label = "   ".to_owned();
        assert!(validate_items("file:///srv/catalog.slice", std::slice::from_ref(&item)).is_err());

        item.label = "Editor".to_owned();
        item.stable_id = crikey_core::ItemId(String::new());
        assert!(validate_items("file:///srv/catalog.slice", std::slice::from_ref(&item)).is_err());
    }
}
