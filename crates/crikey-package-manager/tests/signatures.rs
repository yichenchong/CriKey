//! Provenance for native plugin packages (spec §2.2, §23.3; ADR-0012).
//!
//! The embedded `crikey-package.lock` authenticates an archive against itself,
//! which a hostile party defeats by rebuilding the archive and rewriting the
//! lock to match. These tests pin the thing that closes that hole: an Ed25519
//! detached signature over a canonical manifest of every member, checked against
//! an operator-curated trust store.
//!
//! Every fixture generates its own key pair at test time. No private key is
//! committed, and no test reads the developer's own trust store: the store under
//! test is always a file this test wrote.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crikey_package_manager::{
    build_package, inspect_package, install_native_with_policy, read_signature_file, sign_package,
    signature_path_for, verify_detached, verify_package, verify_package_with_policy, verify_signed_manifest,
    PackageError, PackageSigningKey, PublicKey, Signature, SignatureError, SignaturePolicy, SignatureState,
    SignedManifest, TrustStore, UnsignedPolicy, TRUST_STORE_FILE,
};
use crikey_platform::{DirectoryConvention, DirectoryEnvironment, StandardDirectories};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "crikey-signature-{pid}-{ordinal}-{label}",
            pid = std::process::id(),
            ordinal = NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&path).expect("scratch directory");
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Writes a minimal native plugin source tree and returns its directory.
fn plugin_source(scratch: &Scratch, id: &str, payload: &[u8]) -> PathBuf {
    let directory = scratch.join(id);
    fs::create_dir_all(directory.join("bin")).expect("bin directory");
    let manifest = format!(
        "manifest-version = 1\n\n\
         [plugin]\n\
         id = \"{id}\"\n\
         name = \"Signature Fixture\"\n\
         version = \"1.0.0\"\n\
         runtime = \"native\"\n\
         entrypoint = \"bin/fixture\"\n\n\
         [platform]\n\
         os = [\"{os}\"]\n\
         arch = [\"{arch}\"]\n",
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
    );
    fs::write(directory.join("crikey.toml"), manifest).expect("manifest");
    fs::write(directory.join("bin/fixture"), payload).expect("payload");
    directory
}

/// Builds a package from a fresh source tree and returns the archive path.
fn package(scratch: &Scratch, id: &str, payload: &[u8]) -> PathBuf {
    let source = plugin_source(scratch, id, payload);
    let archive = scratch.join(&format!("{id}.crikey-package"));
    build_package(&source, &archive).expect("the fixture packages");
    archive
}

fn trusting(key: &PublicKey) -> TrustStore {
    let mut store = TrustStore::empty();
    store.add("publisher", *key).expect("a first key is trusted");
    store
}

fn enforced(unsigned: UnsignedPolicy, trust: TrustStore) -> SignaturePolicy {
    SignaturePolicy::enforced(unsigned, trust)
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

#[test]
fn a_signature_by_a_trusted_key_verifies_and_names_the_signer_and_fingerprint() {
    let scratch = Scratch::new("valid");
    let archive = package(&scratch, "dev.example.valid", b"payload");
    let key = PackageSigningKey::generate().expect("entropy");
    let fingerprint = key.public_key().fingerprint();

    let signed = sign_package(&archive, &key, &signature_path_for(&archive)).expect("signs");
    assert_eq!(signed.fingerprint, fingerprint);
    assert_eq!(signed.plugin, "dev.example.valid");
    assert!(signature_path_for(&archive).is_file());

    let report = verify_package_with_policy(
        &archive,
        None,
        &enforced(UnsignedPolicy::Refuse, trusting(&key.public_key())),
    )
    .expect("a signature by a trusted key verifies");
    assert_eq!(
        report.signature,
        SignatureState::Trusted {
            name: "publisher".to_owned(),
            fingerprint: fingerprint.clone(),
        }
    );
    assert_eq!(report.signature.signer(), Some("publisher"));
    assert_eq!(report.signature.fingerprint(), Some(fingerprint.as_str()));
    assert_eq!(report.signature.label(), "trusted");
}

/// The signature covers the whole package, not one member.
///
/// The attack this closes: rebuild the archive with different bytes so its own
/// embedded lock is internally consistent, then keep the old signature. Every
/// pre-signature check passes; only the canonical member manifest notices.
#[test]
fn a_tampered_package_fails_verification_even_though_its_own_lock_agrees_with_it() {
    let scratch = Scratch::new("tampered");
    let source = plugin_source(&scratch, "dev.example.tampered", b"authentic");
    let archive = scratch.join("tampered.crikey-package");
    build_package(&source, &archive).expect("builds");

    let key = PackageSigningKey::generate().expect("entropy");
    let fingerprint = key.public_key().fingerprint();
    sign_package(&archive, &key, &signature_path_for(&archive)).expect("signs");

    // The rebuilt archive is a perfectly well-formed, internally consistent
    // package. `verify_package` — which checks the lock and nothing else — still
    // accepts it, which is precisely why the signature has to exist.
    fs::write(source.join("bin/fixture"), b"hostile").expect("replace payload");
    build_package(&source, &archive).expect("rebuilds");
    verify_package(&archive, None).expect("the rebuilt package is internally consistent");

    let error = verify_package_with_policy(
        &archive,
        None,
        &enforced(UnsignedPolicy::Refuse, trusting(&key.public_key())),
    )
    .expect_err("a signature over the old members must not cover the new ones");
    let PackageError::Signature(SignatureError::Verification {
        artefact,
        fingerprint: named,
    }) = error
    else {
        panic!("expected a verification refusal, got {error}");
    };
    assert!(
        artefact.contains("tampered.crikey-package"),
        "the refusal must name the artefact, got {artefact}"
    );
    assert_eq!(named, fingerprint, "the refusal must name the key");
}

/// Untrusted and invalid are different answers with different remedies.
#[test]
fn a_valid_signature_from_an_untrusted_key_is_refused_as_untrusted_not_as_invalid() {
    let scratch = Scratch::new("untrusted");
    let archive = package(&scratch, "dev.example.untrusted", b"payload");
    let stranger = PackageSigningKey::generate().expect("entropy");
    let known = PackageSigningKey::generate().expect("entropy");
    sign_package(&archive, &stranger, &signature_path_for(&archive)).expect("signs");

    // The store trusts somebody, just not this signer: an empty store would let
    // the test pass for the wrong reason.
    let error = verify_package_with_policy(
        &archive,
        None,
        &enforced(UnsignedPolicy::Refuse, trusting(&known.public_key())),
    )
    .expect_err("an unknown signer is refused");
    let PackageError::Signature(SignatureError::UntrustedSigner {
        artefact,
        fingerprint,
    }) = error
    else {
        panic!("an unknown key must be reported as untrusted, not invalid; got {error}");
    };
    assert_eq!(fingerprint, stranger.public_key().fingerprint());
    assert!(artefact.contains("dev.example.untrusted"));

    // And the same signature verifies once that key is trusted, which proves the
    // refusal above was about trust and not about the maths.
    verify_package_with_policy(
        &archive,
        None,
        &enforced(UnsignedPolicy::Refuse, trusting(&stranger.public_key())),
    )
    .expect("trusting the signer is the only thing that was missing");
}

#[test]
fn an_unsigned_package_follows_each_of_the_three_configured_policies() {
    let scratch = Scratch::new("policy");
    let archive = package(&scratch, "dev.example.policy", b"payload");
    assert!(
        !signature_path_for(&archive).exists(),
        "the fixture must be unsigned"
    );

    let error = verify_package_with_policy(
        &archive,
        None,
        &enforced(UnsignedPolicy::Refuse, TrustStore::empty()),
    )
    .expect_err("`refuse` refuses");
    let PackageError::Signature(SignatureError::Unsigned { artefact }) = error else {
        panic!("expected an unsigned refusal, got {error}");
    };
    assert!(artefact.contains("dev.example.policy"));

    for tolerated in [UnsignedPolicy::Warn, UnsignedPolicy::Allow] {
        let report = verify_package_with_policy(&archive, None, &enforced(tolerated, TrustStore::empty()))
            .unwrap_or_else(|error| panic!("`{}` accepts: {error}", tolerated.as_str()));
        assert_eq!(report.signature, SignatureState::Unsigned);
        assert_eq!(report.signature.fingerprint(), None);
    }

    assert_eq!(UnsignedPolicy::default(), UnsignedPolicy::Refuse);
    assert_eq!(
        UnsignedPolicy::parse("warn").expect("parses"),
        UnsignedPolicy::Warn
    );
    assert!(UnsignedPolicy::parse("maybe").is_err());
}

/// Not looking is reported as not having looked.
///
/// `inspect_package` and the policy-free `verify_package` establish no
/// provenance. Reporting `unsigned` there would be a claim about the package
/// that nothing had established — the honesty invariant, in the smallest place
/// it applies.
#[test]
fn the_policy_free_entry_points_report_unchecked_rather_than_unsigned() {
    let scratch = Scratch::new("unchecked");
    let archive = package(&scratch, "dev.example.unchecked", b"payload");
    let key = PackageSigningKey::generate().expect("entropy");
    sign_package(&archive, &key, &signature_path_for(&archive)).expect("signs");

    assert_eq!(
        inspect_package(&archive).expect("inspects").signature,
        SignatureState::Unchecked
    );
    assert_eq!(
        verify_package(&archive, None)
            .expect("verifies members")
            .signature,
        SignatureState::Unchecked
    );
    assert_eq!(SignatureState::Unchecked.label(), "unchecked");
    assert_eq!(SignatureState::Unchecked.fingerprint(), None);
}

// ---------------------------------------------------------------------------
// Hostile crypto material
// ---------------------------------------------------------------------------

/// A signature file is input, and input is hostile.
#[test]
fn a_truncated_or_oversized_signature_file_is_refused_without_a_large_allocation() {
    let scratch = Scratch::new("sigfile");
    let archive = package(&scratch, "dev.example.sigfile", b"payload");
    let key = PackageSigningKey::generate().expect("entropy");
    let signature = signature_path_for(&archive);
    sign_package(&archive, &key, &signature).expect("signs");
    let good = fs::read(&signature).expect("readable");
    let policy = enforced(UnsignedPolicy::Refuse, trusting(&key.public_key()));

    // Empty: a zero-byte file is not a signature.
    fs::write(&signature, b"").expect("write");
    assert!(verify_package_with_policy(&archive, None, &policy).is_err());

    // Truncated mid-document.
    fs::write(&signature, &good[..good.len() / 2]).expect("write");
    assert!(verify_package_with_policy(&archive, None, &policy).is_err());

    // Oversized. The refusal quotes the ceiling, which only the metadata check
    // that runs *before* the read can produce: a size-checked-after-reading
    // implementation would have allocated the eight megabytes first.
    let mut oversized = good.clone();
    oversized.extend(std::iter::repeat_n(b'#', 8 * 1024 * 1024));
    fs::write(&signature, &oversized).expect("write");
    let error = verify_package_with_policy(&archive, None, &policy)
        .expect_err("an oversized signature file is refused");
    let text = error.to_string();
    assert!(
        text.contains("over the 4096 byte limit"),
        "the refusal must be on the declared size, got: {text}"
    );

    // A directory where a signature file belongs is not a signature file, and
    // is not an unsigned package either.
    fs::remove_file(&signature).expect("remove");
    fs::create_dir(&signature).expect("directory in its place");
    assert!(verify_package_with_policy(&archive, None, &policy).is_err());
}

#[test]
fn a_key_or_signature_of_the_wrong_length_is_refused_before_it_is_decoded() {
    // Length first, so a hostile megabyte of hexadecimal is refused on its
    // length rather than after being parsed into a megabyte of bytes.
    assert!(PublicKey::from_hex("").is_err());
    assert!(PublicKey::from_hex(&"ab".repeat(31)).is_err());
    assert!(PublicKey::from_hex(&"ab".repeat(33)).is_err());
    assert!(PublicKey::from_hex(&"z".repeat(64)).is_err());
    assert!(Signature::from_hex(&"ab".repeat(63)).is_err());
    assert!(Signature::from_hex(&"ab".repeat(65)).is_err());

    let key = PackageSigningKey::generate().expect("entropy");
    let public = key.public_key();
    let round_tripped = PublicKey::from_hex(&public.to_hex()).expect("hex round-trips");
    assert_eq!(round_tripped.fingerprint(), public.fingerprint());

    let signature = key.sign(b"payload");
    let parsed = Signature::from_hex(&signature.to_hex()).expect("hex round-trips");
    verify_detached(b"payload", &parsed, &public).expect("verifies");
    assert!(verify_detached(b"payloae", &parsed, &public).is_err());
}

#[test]
fn a_signature_document_with_the_wrong_version_or_shape_is_refused() {
    let key = PackageSigningKey::generate().expect("entropy");
    let manifest = key.detached(b"payload");
    let good = manifest.to_toml();
    assert_eq!(SignedManifest::parse(&good).expect("parses"), manifest);

    assert!(SignedManifest::parse("").is_err());
    assert!(SignedManifest::parse(&good.replace("version = 1", "version = 2")).is_err());
    assert!(SignedManifest::parse(&good.replace("public-key", "publickey")).is_err());
    // An unknown field is refused rather than ignored: a document this crate
    // does not fully understand is a document it has no business trusting.
    assert!(SignedManifest::parse(&format!("{good}extra = true\n")).is_err());
}

// ---------------------------------------------------------------------------
// Trust store
// ---------------------------------------------------------------------------

#[test]
fn the_trust_store_round_trips_through_the_config_root() {
    let scratch = Scratch::new("store");
    let config = scratch.join("config");
    let directories = StandardDirectories::resolve(
        DirectoryConvention::Xdg,
        &DirectoryEnvironment::new()
            .set("HOME", &scratch.path)
            .set("CRIKEY_CONFIG_DIR", &config),
    )
    .expect("directories resolve");
    assert_eq!(directories.config_dir(), config);

    // An operator who has never trusted a key has an empty store, not an error.
    let mut store = TrustStore::load(&directories).expect("an absent store loads empty");
    assert!(store.is_empty());
    assert_eq!(store.path(), Some(config.join(TRUST_STORE_FILE).as_path()));

    let first = PackageSigningKey::generate().expect("entropy").public_key();
    let second = PackageSigningKey::generate().expect("entropy").public_key();
    store.add("alpha", first).expect("first key");
    store.add("beta", second).expect("second key");
    store.save().expect("saves");

    let reloaded = TrustStore::load(&directories).expect("reloads");
    assert_eq!(reloaded.len(), 2);
    assert_eq!(reloaded.key("alpha").map(PublicKey::to_hex), Some(first.to_hex()));
    assert_eq!(
        reloaded
            .find_by_fingerprint(&second.fingerprint())
            .map(|(name, _)| name),
        Some("beta")
    );
    assert_eq!(reloaded.find_by_fingerprint(&"0".repeat(32)), None);
    // Ordered by name, so the file is byte-stable and a diff shows only what the
    // operator changed.
    assert_eq!(
        reloaded.entries().map(|(name, _)| name).collect::<Vec<_>>(),
        vec!["alpha", "beta"]
    );

    let mut store = reloaded;
    assert!(store.remove("alpha"));
    assert!(!store.remove("alpha"));
    store.save().expect("saves");
    assert_eq!(TrustStore::load(&directories).expect("reloads").len(), 1);
}

#[test]
fn the_trust_store_refuses_a_reused_name_a_reused_key_and_an_unusable_name() {
    let key = PackageSigningKey::generate().expect("entropy").public_key();
    let other = PackageSigningKey::generate().expect("entropy").public_key();
    let mut store = TrustStore::empty();
    store.add("publisher", key).expect("first");

    assert!(store.add("publisher", other).is_err(), "a name is not reusable");
    // One key, one name: trusting it twice would make revoking it once a
    // half-revocation.
    let duplicate = store
        .add("publisher-again", key)
        .expect_err("the same key twice is refused");
    assert!(
        duplicate.to_string().contains(&key.fingerprint()),
        "the refusal must name the key: {duplicate}"
    );

    assert!(store.add("", other).is_err());
    assert!(store.add("../escape", other).is_err());
    assert!(store.add(&"a".repeat(65), other).is_err());
    assert_eq!(store.len(), 1);
}

#[test]
fn an_oversized_or_malformed_trust_store_is_refused_rather_than_partly_applied() {
    let scratch = Scratch::new("badstore");
    let path = scratch.join("trusted-keys.toml");
    let key = PackageSigningKey::generate().expect("entropy").public_key();

    fs::write(&path, "[[key]]\nname = \"a\"\n").expect("write");
    assert!(TrustStore::load_from(&path).is_err(), "a key needs a public key");

    fs::write(&path, "[[key]]\nname = \"a\"\npublic-key = \"nope\"\n").expect("write");
    assert!(TrustStore::load_from(&path).is_err());

    fs::write(
        &path,
        format!(
            "[[key]]\nname = \"a\"\npublic-key = \"{hex}\"\n\
             [[key]]\nname = \"b\"\npublic-key = \"{hex}\"\n",
            hex = key.to_hex()
        ),
    )
    .expect("write");
    assert!(
        TrustStore::load_from(&path).is_err(),
        "one key under two names is refused at load, not silently deduplicated"
    );

    // Oversized: refused on its declared size before it is read.
    let filler = "#".repeat(128 * 1024);
    fs::write(&path, filler).expect("write");
    let error = TrustStore::load_from(&path).expect_err("an oversized store is refused");
    assert!(
        error.to_string().contains("over the 65536 byte limit"),
        "got: {error}"
    );
}

#[test]
fn a_key_file_holds_exactly_one_key() {
    let scratch = Scratch::new("keyfile");
    let key = PackageSigningKey::generate().expect("entropy");
    let path = scratch.join("publisher.pub");
    key.public_key().write_new(&path).expect("writes");
    assert_eq!(
        PublicKey::from_file(&path).expect("reads").fingerprint(),
        key.public_key().fingerprint()
    );

    // Never overwritten: a clobbered key file is a silently changed trust
    // decision.
    let error = key
        .public_key()
        .write_new(&path)
        .expect_err("refuses to overwrite");
    assert!(error.to_string().contains("already exists"), "got: {error}");

    let two = scratch.join("two-keys.pub");
    fs::write(
        &two,
        format!("{}\n{}\n", key.public_key().to_hex(), key.public_key().to_hex()),
    )
    .expect("write");
    assert!(
        PublicKey::from_file(&two).is_err(),
        "two keys in one file is ambiguous, so it is refused"
    );

    let empty = scratch.join("empty.pub");
    fs::write(&empty, "\n\n").expect("write");
    assert!(PublicKey::from_file(&empty).is_err());

    assert!(PublicKey::from_file(&scratch.join("absent.pub")).is_err());
}

#[test]
fn a_signing_key_comes_from_a_file_or_the_environment_and_never_from_a_default_path() {
    let scratch = Scratch::new("signkey");
    let key = PackageSigningKey::generate().expect("entropy");
    let path = scratch.join("signing.key");
    key.write_new(&path).expect("writes");

    let loaded = PackageSigningKey::from_file(&path).expect("reads");
    assert_eq!(loaded.public_key().fingerprint(), key.public_key().fingerprint());
    assert_eq!(loaded.secret_hex(), key.secret_hex());

    // The seed is 32 bytes; anything else is not a signing key.
    assert!(PackageSigningKey::from_hex("").is_err());
    assert!(PackageSigningKey::from_hex(&"ab".repeat(31)).is_err());

    // An unset variable is named, and its value is never quoted back.
    let error = PackageSigningKey::from_env("CRIKEY_TEST_SIGNING_KEY_THAT_IS_UNSET")
        .expect_err("an unset variable is refused");
    assert!(error
        .to_string()
        .contains("CRIKEY_TEST_SIGNING_KEY_THAT_IS_UNSET"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The property is "nobody else can read it", not an exact mode: the
        // process umask can only ever remove bits, so pinning 0o600 exactly
        // would make this test depend on the umask of whoever ran it.
        let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(
            mode & 0o077,
            0,
            "a signing key must not be readable by group or other, got {mode:o}"
        );
        assert_ne!(mode & 0o400, 0, "the owner must be able to read it back");
    }
}

// ---------------------------------------------------------------------------
// Installation
// ---------------------------------------------------------------------------

/// A refused package costs the operator nothing.
///
/// The signature decision happens before `stop_running`, so a package that will
/// not be installed has not stopped a running plugin and has not touched the
/// installed directory.
#[test]
fn installation_refuses_an_unsigned_package_before_stopping_anything() {
    let scratch = Scratch::new("install");
    let archive = package(&scratch, "dev.example.install", b"payload");
    let root = scratch.join("installed");
    let mut stopped = Vec::new();
    let mut stop = |id: &str| {
        stopped.push(id.to_owned());
        Ok(())
    };

    let error = install_native_with_policy(
        &archive,
        &root,
        std::env::consts::OS,
        std::env::consts::ARCH,
        &mut stop,
        None,
        &enforced(UnsignedPolicy::Refuse, TrustStore::empty()),
    )
    .expect_err("an unsigned package is not installed under `refuse`");
    assert!(matches!(
        error,
        PackageError::Signature(SignatureError::Unsigned { .. })
    ));
    assert!(
        stopped.is_empty(),
        "a package that will be refused must not stop a running plugin first"
    );
    assert!(!root.exists(), "nothing may be written for a refused package");
}

#[test]
fn installation_records_the_signer_of_a_trusted_package() {
    let scratch = Scratch::new("install-signed");
    let archive = package(&scratch, "dev.example.installsigned", b"payload");
    let key = PackageSigningKey::generate().expect("entropy");
    sign_package(&archive, &key, &signature_path_for(&archive)).expect("signs");
    let root = scratch.join("installed");
    let mut stop = |_: &str| Ok(());

    let install = install_native_with_policy(
        &archive,
        &root,
        std::env::consts::OS,
        std::env::consts::ARCH,
        &mut stop,
        None,
        &enforced(UnsignedPolicy::Refuse, trusting(&key.public_key())),
    )
    .expect("a trusted package installs");
    assert_eq!(
        install.report.signature,
        SignatureState::Trusted {
            name: "publisher".to_owned(),
            fingerprint: key.public_key().fingerprint(),
        }
    );
    assert!(root.join("bin").join("fixture").is_file());
}

// ---------------------------------------------------------------------------
// The detached-signature seam other slices consume
// ---------------------------------------------------------------------------

/// The plugin index and the remote catalog verify their own documents through
/// this seam, so the trust-before-maths ordering is pinned here too.
#[test]
fn verify_signed_manifest_distinguishes_an_unknown_key_from_a_bad_signature() {
    let key = PackageSigningKey::generate().expect("entropy");
    let stranger = PackageSigningKey::generate().expect("entropy");
    let manifest = key.detached(b"index document");

    let trusted = trusting(&key.public_key());
    let signer =
        verify_signed_manifest(b"index document", &manifest, &trusted, "index.toml").expect("verifies");
    assert_eq!(signer.name, "publisher");
    assert_eq!(signer.fingerprint, key.public_key().fingerprint());

    let error = verify_signed_manifest(
        b"index document",
        &manifest,
        &trusting(&stranger.public_key()),
        "index.toml",
    )
    .expect_err("an unknown signer is refused");
    assert!(
        matches!(error, SignatureError::UntrustedSigner { .. }),
        "got {error}"
    );

    let error = verify_signed_manifest(b"other document", &manifest, &trusted, "index.toml")
        .expect_err("a signature over other bytes is refused");
    assert!(
        matches!(error, SignatureError::Verification { .. }),
        "got {error}"
    );
}

/// The sidecar file is the only channel between signing and verifying, so what
/// `sign_package` wrote must be exactly what a verifier reads back.
#[test]
fn a_written_signature_file_reads_back_as_the_manifest_that_was_written() {
    let scratch = Scratch::new("roundtrip");
    let archive = package(&scratch, "dev.example.roundtrip", b"payload");
    let key = PackageSigningKey::generate().expect("entropy");
    let path = signature_path_for(&archive);
    sign_package(&archive, &key, &path).expect("signs");

    let manifest = read_signature_file(&path).expect("reads");
    assert_eq!(manifest.key.fingerprint(), key.public_key().fingerprint());
    assert_eq!(
        SignedManifest::parse(&manifest.to_toml()).expect("re-parses"),
        manifest
    );

    // Rewriting the file from the parsed manifest changes nothing a verifier
    // sees, which is what makes the document format a stable interface rather
    // than an accident of how it happened to be written.
    fs::write(&path, manifest.to_toml()).expect("rewrite");
    verify_package_with_policy(
        &archive,
        None,
        &enforced(UnsignedPolicy::Refuse, trusting(&key.public_key())),
    )
    .expect("the rewritten sidecar still verifies");
}

#[test]
fn the_default_signature_path_sits_beside_the_artefact() {
    assert_eq!(
        signature_path_for(Path::new("/tmp/plugin.crikey-package")),
        PathBuf::from("/tmp/plugin.crikey-package.sig")
    );
}
