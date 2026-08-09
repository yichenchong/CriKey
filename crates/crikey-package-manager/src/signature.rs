//! Ed25519 detached signatures, the named-key trust store, and the unsigned
//! package policy (spec §2.2, §23.3).
//!
//! # What a signature here covers
//!
//! Not one file. The embedded `crikey-package.lock` already binds every archive
//! member to a SHA-256 digest, and [`crate::native`] refuses any package whose
//! members disagree with it — but a lock is only as trustworthy as the person
//! who wrote it, so a hostile party can rebuild an archive, rewrite the lock to
//! match, and the result verifies. What was missing was provenance.
//!
//! A signature therefore covers a *canonical manifest* of the whole package:
//! the plugin identity plus the name and digest of every member, in one
//! unambiguous byte string ([`crate::native::canonical_manifest`]). One
//! signature consequently authenticates the entire package, and changing any
//! byte of any member — including the lock — invalidates it.
//!
//! # Hostile input
//!
//! The crypto material is input too. A signature file, a key file and the trust
//! store are each read through [`read_capped`], which refuses a file larger
//! than its ceiling *before* allocating rather than truncating it, and every
//! decoded key and signature is length-checked to exactly 32 and 64 bytes. A
//! truncated, padded, oversized or non-hexadecimal artefact is refused with an
//! error that names the path.
//!
//! # Trust
//!
//! There is no certificate authority and no key server. An operator names the
//! keys they trust, one line each, in `<config_dir>/trusted-keys.toml`. A
//! signature by a key that is not in that file is refused as *untrusted*, which
//! is a different answer from *invalid* and gets a different error: the first
//! is a decision the operator has not made yet, the second is an attack or a
//! corrupt download.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crikey_platform::StandardDirectories;
use ed25519_dalek::{Signer, SigningKey as DalekSigningKey, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::index::constant_time_hex_eq;

/// File name of the trust store, inside `config_dir()`.
pub const TRUST_STORE_FILE: &str = "trusted-keys.toml";

/// Configuration key selecting the [`UnsignedPolicy`].
///
/// Defaulted here rather than in `crikey-config`'s `BUILT_IN_DEFAULTS`, which
/// deliberately holds only keys that crate itself owns; a second copy of the
/// default beside the code that enforces it is how the two drift apart.
pub const KEY_UNSIGNED_POLICY: &str = "packages.unsigned-policy";

/// Raw byte length of an Ed25519 public key and of a private key seed.
const KEY_BYTES: usize = 32;

/// Raw byte length of an Ed25519 signature.
const SIGNATURE_BYTES: usize = 64;

/// A fingerprint is the first 16 bytes of the digest, rendered as hex.
const FINGERPRINT_BYTES: usize = 16;

/// Ceiling on a detached signature file. The TOML document holds a version, a
/// 64-character key and a 128-character signature; 4 KiB is two orders of
/// magnitude of slack and still refuses a multi-gigabyte "signature".
const MAX_SIGNATURE_FILE_BYTES: u64 = 4 * 1024;

/// Ceiling on the trust store. Roughly a thousand keys, far past what any
/// operator curates by hand.
const MAX_TRUST_STORE_BYTES: u64 = 64 * 1024;

/// Ceiling on a bare key file, which holds one line of hexadecimal.
const MAX_KEY_FILE_BYTES: u64 = 1024;

/// The version tag inside a detached signature file.
const SIGNATURE_FORMAT_VERSION: u32 = 1;

/// Everything that can go wrong establishing or checking provenance.
///
/// Every variant names the artefact or the path it is about, and the two
/// refusals a signature can produce are separate variants: an operator who has
/// simply not trusted a publisher yet needs a different next action from one
/// looking at a payload that does not match its signature.
#[derive(Debug, thiserror::Error)]
pub enum SignatureError {
    #[error("malformed public key: {0}")]
    MalformedKey(String),
    #[error("malformed private key: {0}")]
    MalformedPrivateKey(String),
    #[error("malformed signature: {0}")]
    MalformedSignature(String),
    /// The maths failed: these bytes were not signed by this key.
    #[error("signature for `{artefact}` does not verify against key {fingerprint}")]
    Verification { artefact: String, fingerprint: String },
    /// The signature may well be valid; nobody said this signer is trusted.
    #[error(
        "`{artefact}` is signed by key {fingerprint}, which is not in the trust store; \
         trust it with `crikey package trust-add --name NAME --key <PUBLIC-KEY-HEX>`"
    )]
    UntrustedSigner { artefact: String, fingerprint: String },
    #[error("`{artefact}` carries no detached signature and the unsigned-package policy is `refuse`")]
    Unsigned { artefact: String },
    #[error("trust store {path}: {reason}")]
    TrustStore { path: PathBuf, reason: String },
    /// A key or signature file that could not be read or made sense of.
    #[error("{path}: {reason}")]
    Material { path: PathBuf, reason: String },
    #[error("unknown unsigned-package policy `{0}`; expected `refuse`, `warn` or `allow`")]
    UnknownPolicy(String),
    #[error("no random bytes available for key generation: {0}")]
    Entropy(String),
}

/// An Ed25519 public key, identified in every message by its fingerprint.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PublicKey(VerifyingKey);

impl PublicKey {
    /// Decodes 64 lowercase or uppercase hexadecimal characters.
    pub fn from_hex(text: &str) -> Result<Self, SignatureError> {
        let bytes = decode_fixed_hex(text, KEY_BYTES).map_err(SignatureError::MalformedKey)?;
        let mut key = [0_u8; KEY_BYTES];
        key.copy_from_slice(&bytes);
        Self::from_bytes(&key)
    }

    /// Decodes the 32 raw bytes, rejecting a point that is not on the curve.
    pub fn from_bytes(bytes: &[u8; KEY_BYTES]) -> Result<Self, SignatureError> {
        VerifyingKey::from_bytes(bytes)
            .map(Self)
            .map_err(|error| SignatureError::MalformedKey(format!("not a valid curve point: {error}")))
    }

    /// Reads a key from a file holding one line of hexadecimal.
    pub fn from_file(path: &Path) -> Result<Self, SignatureError> {
        let text = read_hex_line(path, MAX_KEY_FILE_BYTES)?;
        Self::from_hex(&text).map_err(|error| SignatureError::Material {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })
    }

    /// Writes the key to a new file, refusing to overwrite one.
    pub fn write_new(&self, path: &Path) -> Result<(), SignatureError> {
        write_new_file(path, format!("{}\n", self.to_hex()).as_bytes(), 0o644)
    }

    /// The 32 key bytes as 64 lowercase hexadecimal characters.
    pub fn to_hex(&self) -> String {
        encode_hex(self.0.as_bytes())
    }

    /// A short, stable name for this key: the first 16 bytes of the SHA-256 of
    /// its 32 raw bytes, as 32 lowercase hexadecimal characters.
    ///
    /// Short enough to read out loud and compare by eye, which is the only way
    /// a key ever gets verified out of band, and long enough that producing a
    /// second key with the same fingerprint is not a thing an attacker does.
    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(self.0.as_bytes());
        encode_hex(&digest[..FINGERPRINT_BYTES])
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("PublicKey")
            .field(&self.fingerprint())
            .finish()
    }
}

/// A detached Ed25519 signature.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Signature([u8; SIGNATURE_BYTES]);

impl Signature {
    /// Decodes 128 lowercase or uppercase hexadecimal characters.
    pub fn from_hex(text: &str) -> Result<Self, SignatureError> {
        let bytes = decode_fixed_hex(text, SIGNATURE_BYTES).map_err(SignatureError::MalformedSignature)?;
        let mut signature = [0_u8; SIGNATURE_BYTES];
        signature.copy_from_slice(&bytes);
        Ok(Self(signature))
    }

    /// The 64 signature bytes as 128 lowercase hexadecimal characters.
    pub fn to_hex(&self) -> String {
        encode_hex(&self.0)
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("Signature").field(&self.to_hex()).finish()
    }
}

/// Verifies `signature` over `payload` under `key`.
///
/// `verify_strict` rather than `verify`: it refuses small-order public keys and
/// the signature malleability that lets one signed message be re-encoded, so a
/// signature that verifies here verifies for everyone who checks it the same
/// way. This is the whole of the crate's public verification surface — the
/// plugin index consumes exactly this function rather than carrying a second
/// copy of the decision.
pub fn verify_detached(payload: &[u8], signature: &Signature, key: &PublicKey) -> Result<(), SignatureError> {
    let parsed = ed25519_dalek::Signature::from_bytes(&signature.0);
    key.0
        .verify_strict(payload, &parsed)
        .map_err(|_| SignatureError::Verification {
            artefact: "payload".to_owned(),
            fingerprint: key.fingerprint(),
        })
}

/// The trusted key a payload turned out to be signed by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedSigner {
    /// The operator's name for the key, from the trust store.
    pub name: String,
    /// The key's fingerprint, which is what every message quotes.
    pub fingerprint: String,
}

/// A detached signature file's contents: the signature and the public key that
/// produced it.
///
/// The key travels with the signature so verification is a lookup by
/// fingerprint rather than a trial against every trusted key, and so the
/// refusal for an untrusted signer can name the key the operator would have to
/// trust. Shipping the key alongside grants it nothing: only the trust store
/// decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedManifest {
    pub key: PublicKey,
    pub signature: Signature,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct SignatureDocument {
    version: u32,
    public_key: String,
    signature: String,
}

impl SignedManifest {
    /// Renders the sidecar document this crate writes and reads.
    pub fn to_toml(&self) -> String {
        // Hand-rolled rather than `toml::to_string`: three fields whose values
        // are hexadecimal by construction cannot fail to serialise, and a
        // `Result` here would be an error path no caller could ever hit.
        format!(
            "version = {SIGNATURE_FORMAT_VERSION}\npublic-key = \"{}\"\nsignature = \"{}\"\n",
            self.key.to_hex(),
            self.signature.to_hex()
        )
    }

    /// Parses a sidecar document.
    pub fn parse(text: &str) -> Result<Self, SignatureError> {
        let document: SignatureDocument =
            toml::from_str(text).map_err(|error| SignatureError::MalformedSignature(error.to_string()))?;
        if document.version != SIGNATURE_FORMAT_VERSION {
            return Err(SignatureError::MalformedSignature(format!(
                "signature format version {} is not {SIGNATURE_FORMAT_VERSION}",
                document.version
            )));
        }
        Ok(Self {
            key: PublicKey::from_hex(&document.public_key)?,
            signature: Signature::from_hex(&document.signature)?,
        })
    }
}

/// Reads a detached signature file, refusing anything over
/// [`MAX_SIGNATURE_FILE_BYTES`] without allocating it.
pub fn read_signature_file(path: &Path) -> Result<SignedManifest, SignatureError> {
    let bytes = read_capped(path, MAX_SIGNATURE_FILE_BYTES)?;
    let text = std::str::from_utf8(&bytes).map_err(|error| SignatureError::Material {
        path: path.to_path_buf(),
        reason: format!("not UTF-8: {error}"),
    })?;
    SignedManifest::parse(text).map_err(|error| SignatureError::Material {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })
}

/// Writes a detached signature file, in one rename.
pub fn write_signature_file(path: &Path, manifest: &SignedManifest) -> Result<(), SignatureError> {
    write_atomically(path, manifest.to_toml().as_bytes(), 0o644)
}

/// A private signing key, held only as long as one signing operation needs it.
///
/// Never serialised by [`fmt::Debug`], which prints the *public* fingerprint:
/// a diagnostic that leaked a signing key would be the single worst defect this
/// module could have, and the derive would have done exactly that.
pub struct PackageSigningKey(DalekSigningKey);

impl PackageSigningKey {
    /// A fresh key from the operating system's CSPRNG.
    pub fn generate() -> Result<Self, SignatureError> {
        let mut seed = [0_u8; KEY_BYTES];
        getrandom::fill(&mut seed).map_err(|error| SignatureError::Entropy(error.to_string()))?;
        Ok(Self(DalekSigningKey::from_bytes(&seed)))
    }

    /// Decodes a 32-byte seed from 64 hexadecimal characters.
    pub fn from_hex(text: &str) -> Result<Self, SignatureError> {
        let bytes = decode_fixed_hex(text, KEY_BYTES).map_err(SignatureError::MalformedPrivateKey)?;
        let mut seed = [0_u8; KEY_BYTES];
        seed.copy_from_slice(&bytes);
        Ok(Self(DalekSigningKey::from_bytes(&seed)))
    }

    /// Reads a signing key from a file holding one line of hexadecimal.
    pub fn from_file(path: &Path) -> Result<Self, SignatureError> {
        let text = read_hex_line(path, MAX_KEY_FILE_BYTES)?;
        Self::from_hex(&text).map_err(|error| SignatureError::Material {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })
    }

    /// Reads a signing key from an environment variable.
    ///
    /// The variable's name, never its value, appears in any error: an error
    /// message quoting a malformed private key would write it into whatever
    /// captured the process's stderr.
    pub fn from_env(variable: &str) -> Result<Self, SignatureError> {
        let value = std::env::var(variable).map_err(|_| {
            SignatureError::MalformedPrivateKey(format!("`{variable}` is not set in the environment"))
        })?;
        Self::from_hex(value.trim()).map_err(|error| {
            SignatureError::MalformedPrivateKey(format!("`{variable}` does not hold a signing key: {error}"))
        })
    }

    /// The public half.
    pub fn public_key(&self) -> PublicKey {
        PublicKey(self.0.verifying_key())
    }

    /// The 32-byte seed as hexadecimal, for writing a newly generated key out
    /// once. Named to make a careless use read badly at the call site.
    pub fn secret_hex(&self) -> String {
        encode_hex(&self.0.to_bytes())
    }

    /// Writes the seed to a *new* file readable only by its owner.
    ///
    /// `create_new` rather than a check followed by a write: the check-then-write
    /// would let `crikey package keygen` overwrite an existing signing key in the
    /// window between them, and a destroyed signing key is not recoverable.
    pub fn write_new(&self, path: &Path) -> Result<(), SignatureError> {
        write_new_file(path, format!("{}\n", self.secret_hex()).as_bytes(), 0o600)
    }

    /// Signs `payload`.
    pub fn sign(&self, payload: &[u8]) -> Signature {
        Signature(self.0.sign(payload).to_bytes())
    }

    /// Signs `payload` and pairs the signature with the public half.
    pub fn detached(&self, payload: &[u8]) -> SignedManifest {
        SignedManifest {
            key: self.public_key(),
            signature: self.sign(payload),
        }
    }
}

impl fmt::Debug for PackageSigningKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackageSigningKey")
            .field("fingerprint", &self.public_key().fingerprint())
            .finish_non_exhaustive()
    }
}

/// The named public keys an operator has decided to trust.
///
/// Ordered by name so [`Self::save`] is byte-stable and a diff of the file
/// shows only what the operator changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustStore {
    keys: BTreeMap<String, PublicKey>,
    path: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustStoreDocument {
    #[serde(default, rename = "key")]
    keys: Vec<TrustStoreEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct TrustStoreEntry {
    name: String,
    public_key: String,
}

impl TrustStore {
    /// A store trusting nobody, not backed by a file.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Loads `<config_dir>/trusted-keys.toml` through the standard directory
    /// resolution every other CriKey file goes through.
    ///
    /// An absent file is an empty store, not an error: an operator who has
    /// never trusted a key has an empty trust store, and refusing to start
    /// because of it would be refusing the default state.
    pub fn load(directories: &StandardDirectories) -> Result<Self, SignatureError> {
        Self::load_from(&directories.config_dir().join(TRUST_STORE_FILE))
    }

    /// Loads a trust store from an explicit path.
    pub fn load_from(path: &Path) -> Result<Self, SignatureError> {
        let bytes = match read_capped(path, MAX_TRUST_STORE_BYTES) {
            Ok(bytes) => bytes,
            Err(SignatureError::Material { reason, .. }) if reason == ABSENT => {
                return Ok(Self {
                    keys: BTreeMap::new(),
                    path: Some(path.to_path_buf()),
                });
            }
            Err(error) => return Err(error),
        };
        let trust_store = |reason: String| SignatureError::TrustStore {
            path: path.to_path_buf(),
            reason,
        };
        let text = std::str::from_utf8(&bytes).map_err(|error| trust_store(format!("not UTF-8: {error}")))?;
        let document: TrustStoreDocument =
            toml::from_str(text).map_err(|error| trust_store(error.to_string()))?;
        let mut keys = BTreeMap::new();
        for entry in document.keys {
            validate_key_name(&entry.name).map_err(trust_store)?;
            let key = PublicKey::from_hex(&entry.public_key)
                .map_err(|error| trust_store(format!("key `{}`: {error}", entry.name)))?;
            let fingerprint = key.fingerprint();
            if let Some((existing, _)) = find_fingerprint(&keys, &fingerprint) {
                return Err(trust_store(format!(
                    "keys `{existing}` and `{}` are the same key {fingerprint}; one key, one name, \
                     or revoking it later would only half revoke it",
                    entry.name
                )));
            }
            if keys.insert(entry.name.clone(), key).is_some() {
                return Err(trust_store(format!("key `{}` is listed twice", entry.name)));
            }
        }
        Ok(Self {
            keys,
            path: Some(path.to_path_buf()),
        })
    }

    /// The file this store was loaded from, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether no key is trusted.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// How many keys are trusted.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// The key an operator filed under `name`.
    pub fn key(&self, name: &str) -> Option<&PublicKey> {
        self.keys.get(name)
    }

    /// The trusted key with this fingerprint, and the name it is filed under.
    pub fn find_by_fingerprint(&self, fingerprint: &str) -> Option<(&str, &PublicKey)> {
        find_fingerprint(&self.keys, fingerprint)
    }

    /// Every trusted key, by name, in name order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &PublicKey)> {
        self.keys.iter().map(|(name, key)| (name.as_str(), key))
    }

    /// Trusts `key` under `name`.
    ///
    /// Refuses to reuse a name and refuses to file one key under two names: a
    /// key trusted twice is a key an operator can only partly stop trusting.
    pub fn add(&mut self, name: &str, key: PublicKey) -> Result<(), SignatureError> {
        let store = |reason: String| SignatureError::TrustStore {
            path: self.path.clone().unwrap_or_default(),
            reason,
        };
        validate_key_name(name).map_err(store)?;
        if self.keys.contains_key(name) {
            return Err(store(format!(
                "`{name}` is already trusted; remove it first to replace it"
            )));
        }
        let fingerprint = key.fingerprint();
        if let Some((existing, _)) = self.find_by_fingerprint(&fingerprint) {
            return Err(store(format!(
                "key {fingerprint} is already trusted as `{existing}`"
            )));
        }
        self.keys.insert(name.to_owned(), key);
        Ok(())
    }

    /// Stops trusting `name`. Returns whether anything was removed.
    pub fn remove(&mut self, name: &str) -> bool {
        self.keys.remove(name).is_some()
    }

    /// Writes the store back to the file it was loaded from.
    pub fn save(&self) -> Result<(), SignatureError> {
        let path = self.path.as_deref().ok_or_else(|| SignatureError::TrustStore {
            path: PathBuf::new(),
            reason: "this trust store is not backed by a file".to_owned(),
        })?;
        self.save_to(path)
    }

    /// Writes the store to `path`, in one rename.
    pub fn save_to(&self, path: &Path) -> Result<(), SignatureError> {
        let mut text = String::from(
            "# Public keys this installation trusts to sign CriKey plugin packages.\n\
             # Managed by `crikey package trust-add` and `crikey package trust-remove`.\n",
        );
        for (name, key) in self.entries() {
            text.push_str(&format!(
                "\n[[key]]\nname = \"{name}\"\npublic-key = \"{}\"\n",
                key.to_hex()
            ));
        }
        write_atomically(path, text.as_bytes(), 0o600)
    }
}

fn find_fingerprint<'a>(
    keys: &'a BTreeMap<String, PublicKey>,
    fingerprint: &str,
) -> Option<(&'a str, &'a PublicKey)> {
    keys.iter().find_map(|(name, key)| {
        constant_time_hex_eq(&key.fingerprint(), fingerprint).then_some((name.as_str(), key))
    })
}

/// A trust-store name is a filing label, not a path or a display string.
fn validate_key_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a key name cannot be empty".to_owned());
    }
    if name.len() > 64 {
        return Err(format!(
            "key name is {} bytes, over the 64 byte limit",
            name.len()
        ));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'@'))
    {
        return Err(format!(
            "key name `{name}` may hold only letters, digits, `-`, `.`, `_` and `@`"
        ));
    }
    Ok(())
}

/// What to do about a package that carries no signature at all.
///
/// [`Self::Refuse`] is the default because the alternative default is "install
/// third-party native code of unknown origin", and a launcher that chooses that
/// for the operator has made a security decision that was never its to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnsignedPolicy {
    /// An unsigned package is not installed.
    #[default]
    Refuse,
    /// An unsigned package is installed and the operator is told.
    Warn,
    /// An unsigned package is installed silently.
    Allow,
}

impl UnsignedPolicy {
    /// Parses the configured spelling.
    pub fn parse(text: &str) -> Result<Self, SignatureError> {
        match text.trim() {
            "refuse" => Ok(Self::Refuse),
            "warn" => Ok(Self::Warn),
            "allow" => Ok(Self::Allow),
            other => Err(SignatureError::UnknownPolicy(other.to_owned())),
        }
    }

    /// The configured spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Refuse => "refuse",
            Self::Warn => "warn",
            Self::Allow => "allow",
        }
    }
}

/// Whether and how provenance is checked.
///
/// [`Self::Unchecked`] exists so that adding provenance changed no existing
/// behaviour by accident: every code path that verified packages before this
/// module landed still passes `Unchecked` and still means exactly what it meant,
/// and a report from such a path says [`SignatureState::Unchecked`] rather than
/// claiming a package is unsigned when nobody looked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignaturePolicy {
    /// Provenance is not examined. The report says so.
    Unchecked,
    /// Provenance is examined against `trust`, with `unsigned` deciding what
    /// happens to a package that carries no signature.
    Enforced {
        unsigned: UnsignedPolicy,
        trust: TrustStore,
    },
}

impl SignaturePolicy {
    /// Provenance is not examined.
    pub fn unchecked() -> Self {
        Self::Unchecked
    }

    /// Provenance is examined.
    pub fn enforced(unsigned: UnsignedPolicy, trust: TrustStore) -> Self {
        Self::Enforced { unsigned, trust }
    }

    /// The unsigned-package policy, or `None` when provenance is not examined.
    pub fn unsigned(&self) -> Option<UnsignedPolicy> {
        match self {
            Self::Unchecked => None,
            Self::Enforced { unsigned, .. } => Some(*unsigned),
        }
    }
}

/// What provenance turned out to be. Not an outcome that can be a refusal:
/// an untrusted signer and a signature that does not verify are both errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureState {
    /// Nobody looked. Reported by the entry points that take no policy.
    Unchecked,
    /// Looked, and there is no detached signature beside the artefact.
    Unsigned,
    /// Verified against a key in the trust store.
    Trusted { name: String, fingerprint: String },
}

impl SignatureState {
    /// The frozen one-word spelling used in command output.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unchecked => "unchecked",
            Self::Unsigned => "unsigned",
            Self::Trusted { .. } => "trusted",
        }
    }

    /// The signer's fingerprint, when there is one.
    pub fn fingerprint(&self) -> Option<&str> {
        match self {
            Self::Unchecked | Self::Unsigned => None,
            Self::Trusted { fingerprint, .. } => Some(fingerprint),
        }
    }

    /// The operator's name for the signer, when there is one.
    pub fn signer(&self) -> Option<&str> {
        match self {
            Self::Unchecked | Self::Unsigned => None,
            Self::Trusted { name, .. } => Some(name),
        }
    }
}

/// Applies `policy` to the signature that does or does not sit beside an
/// artefact, given the exact bytes a signature over it must cover.
///
/// The one place the decision tree lives, so the package path and the index
/// path cannot drift apart:
///
/// 1. No policy — [`SignatureState::Unchecked`].
/// 2. No signature file — the [`UnsignedPolicy`] decides.
/// 3. A signature by a key not in the trust store — refused as untrusted.
/// 4. A signature that does not verify — refused as invalid.
pub fn evaluate(
    artefact: &str,
    signature_path: &Path,
    payload: &[u8],
    policy: &SignaturePolicy,
) -> Result<SignatureState, SignatureError> {
    let SignaturePolicy::Enforced { unsigned, trust } = policy else {
        return Ok(SignatureState::Unchecked);
    };
    // Absent is a state the policy decides about. Present-but-not-a-file is not
    // absence: something is sitting where a signature belongs, and silently
    // calling that "unsigned" under a tolerant policy would let a directory or a
    // symlink hide the question rather than answer it.
    match fs::symlink_metadata(signature_path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(SignatureError::Material {
                path: signature_path.to_path_buf(),
                reason: "is not a regular file, so it is not a signature".to_owned(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return match unsigned {
                UnsignedPolicy::Refuse => Err(SignatureError::Unsigned {
                    artefact: artefact.to_owned(),
                }),
                UnsignedPolicy::Warn | UnsignedPolicy::Allow => Ok(SignatureState::Unsigned),
            };
        }
        Err(error) => {
            return Err(SignatureError::Material {
                path: signature_path.to_path_buf(),
                reason: error.to_string(),
            });
        }
    }
    let manifest = read_signature_file(signature_path)?;
    let signer = verify_signed_manifest(payload, &manifest, trust, artefact)?;
    Ok(SignatureState::Trusted {
        name: signer.name,
        fingerprint: signer.fingerprint,
    })
}

/// Resolves a detached signature's key against `store` and then verifies it.
///
/// Trust is decided *before* the maths, so a valid signature by an unknown key
/// is [`SignatureError::UntrustedSigner`] and never
/// [`SignatureError::Verification`].
pub fn verify_signed_manifest(
    payload: &[u8],
    manifest: &SignedManifest,
    store: &TrustStore,
    artefact: &str,
) -> Result<TrustedSigner, SignatureError> {
    let fingerprint = manifest.key.fingerprint();
    let Some((name, trusted)) = store.find_by_fingerprint(&fingerprint) else {
        return Err(SignatureError::UntrustedSigner {
            artefact: artefact.to_owned(),
            fingerprint,
        });
    };
    verify_detached(payload, &manifest.signature, trusted).map_err(|_| SignatureError::Verification {
        artefact: artefact.to_owned(),
        fingerprint: fingerprint.clone(),
    })?;
    Ok(TrustedSigner {
        name: name.to_owned(),
        fingerprint,
    })
}

/// The sidecar path for an artefact: the artefact's own name with `.sig`
/// appended, so a package and its signature sort together and neither can be
/// mistaken for the other.
pub fn signature_path_for(artefact: &Path) -> PathBuf {
    let mut name = artefact.as_os_str().to_owned();
    name.push(".sig");
    PathBuf::from(name)
}

/// The marker reason [`read_capped`] uses for an absent file, so callers that
/// treat absence as a legitimate state can recognise it without a second stat.
const ABSENT: &str = "no such file";

/// Reads at most `cap` bytes, refusing rather than truncating.
///
/// The size is checked from the directory entry before anything is allocated,
/// and then again against what was actually read, because a file can grow
/// between the two and the first check alone would be a bound an attacker
/// chooses.
fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>, SignatureError> {
    let material = |reason: String| SignatureError::Material {
        path: path.to_path_buf(),
        reason,
    };
    let file = File::open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            material(ABSENT.to_owned())
        } else {
            material(error.to_string())
        }
    })?;
    let metadata = file.metadata().map_err(|error| material(error.to_string()))?;
    if !metadata.is_file() {
        return Err(material("not a regular file".to_owned()));
    }
    if metadata.len() > cap {
        return Err(material(format!(
            "{} bytes, over the {cap} byte limit",
            metadata.len()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let read = file
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| material(error.to_string()))?;
    if read as u64 > cap {
        return Err(material(format!(
            "grew past the {cap} byte limit while being read"
        )));
    }
    Ok(bytes)
}

/// Reads a file holding exactly one line of hexadecimal and nothing else.
fn read_hex_line(path: &Path, cap: u64) -> Result<String, SignatureError> {
    let bytes = read_capped(path, cap)?;
    let text = std::str::from_utf8(&bytes).map_err(|error| SignatureError::Material {
        path: path.to_path_buf(),
        reason: format!("not UTF-8: {error}"),
    })?;
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let first = lines.next().ok_or_else(|| SignatureError::Material {
        path: path.to_path_buf(),
        reason: "holds no key".to_owned(),
    })?;
    if lines.next().is_some() {
        return Err(SignatureError::Material {
            path: path.to_path_buf(),
            reason: "holds more than one line; a key file holds one key".to_owned(),
        });
    }
    Ok(first.trim().to_owned())
}

/// Decodes hexadecimal of exactly `expected` bytes.
///
/// Length is checked before decoding, so a hostile 400 MB "key" is refused on
/// its length rather than after being parsed.
fn decode_fixed_hex(text: &str, expected: usize) -> Result<Vec<u8>, String> {
    let text = text.trim();
    if text.len() != expected * 2 {
        return Err(format!(
            "{} hexadecimal characters, expected {}",
            text.len(),
            expected * 2
        ));
    }
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(expected);
    for pair in bytes.chunks_exact(2) {
        let high = hex_value(pair[0])?;
        let low = hex_value(pair[1])?;
        decoded.push((high << 4) | low);
    }
    Ok(decoded)
}

fn hex_value(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        other => Err(format!(
            "`{}` is not a hexadecimal digit",
            char::from(other).escape_debug()
        )),
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

/// Creates `path` with `mode` and refuses to touch it if it already exists.
///
/// Key material, unlike a signature or a trust store, is never rewritten in
/// place: `create_new` is the only check that cannot be raced, and losing a
/// signing key to a clobbering write is unrecoverable in a way that losing a
/// signature file is not.
fn write_new_file(path: &Path, bytes: &[u8], mode: u32) -> Result<(), SignatureError> {
    let material = |reason: String| SignatureError::Material {
        path: path.to_path_buf(),
        reason,
    };
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|error| SignatureError::Material {
            path: parent.to_path_buf(),
            reason: error.to_string(),
        })?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            material("already exists; refusing to overwrite key material".to_owned())
        } else {
            material(error.to_string())
        }
    })?;
    file.write_all(bytes)
        .map_err(|error| material(error.to_string()))?;
    file.sync_all().map_err(|error| material(error.to_string()))
}

/// Writes `bytes` to `path` in one rename, so a reader never sees a partial
/// trust store or a half-written signature.
///
/// `mode` is applied to the temporary file before the rename, so the final path
/// is never briefly world-readable when it should not be.
fn write_atomically(path: &Path, bytes: &[u8], mode: u32) -> Result<(), SignatureError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let material = |target: &Path, error: std::io::Error| SignatureError::Material {
        path: target.to_path_buf(),
        reason: error.to_string(),
    };
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent).map_err(|error| material(parent, error))?;
    }
    let directory = parent.unwrap_or_else(|| Path::new("."));
    let base = path.file_name().map_or_else(
        || "signature".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let temporary = loop {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let candidate = directory.join(format!(".{base}.tmp-{}-{sequence}", std::process::id()));
        if !candidate.exists() {
            break candidate;
        }
    };
    let outcome = (|| -> Result<(), std::io::Error> {
        let mut file = File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        fs::rename(&temporary, path)
    })();
    if let Err(error) = outcome {
        let _ = fs::remove_file(&temporary);
        return Err(material(path, error));
    }
    Ok(())
}
