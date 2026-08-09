# ADR-0012: Ed25519 detached package signatures

Status: Accepted
Spec: §2.2, §21.2, §23.3, §23.4

## Context

`package verify` checked only the embedded `crikey-package.lock`. A hostile party
could rebuild an archive, rewrite its lock, and pass verification. The existing
per-`bin/` `.sig` marker had no defined contents or verifier. Native installation
executes third-party code, so integrity without provenance is insufficient.
## Decision
A signature covers a versioned canonical manifest: plugin id, version, member
count, and every member name plus SHA-256 digest in sorted order. Length-prefixed
fields prevent ambiguity; changing any member, manifest or lock changes the
signed bytes. The domain-separated version prevents future layout reinterpretation.

Ed25519 uses `ed25519-dalek` and `verify_strict`; key generation uses the OS CSPRNG
through `getrandom`. The sole verification API is
`verify_detached(payload, signature, key)`. Detached `<package>.sig` contains
version, public key and signature. Shipping the key grants it no trust.

Named keys live in `trusted-keys.toml` beneath existing
`StandardDirectories::config_dir()`. A fingerprint is the first 16 SHA-256 bytes
of the raw key, rendered as 32 lowercase hexadecimal characters. Trust is checked
before signature mathematics: an unknown key is `UntrustedSigner`; bad mathematics
under a trusted key is `Verification`. Both refuse and name artefact and
fingerprint. Commands provide `sign`, `keygen`, `trust-add`, `trust-list` and
`trust-remove`; private keys are never implicit or overwritten.

Unsigned packages use `packages.unsigned-policy`: `refuse` is the default, with
explicit `warn` and `allow`. `crikey plugin install` resolves it — from
`--unsigned-policy`, else the configured key, else the default — through the
same resolver `crikey package verify` uses, and hands it to the installer. The
decision precedes stopping a plugin or moving an install directory.
`SignatureState::Unchecked` is distinct from `Unsigned`: policy-free inspection
says provenance was not examined.

The policy governs a native package that arrives as a distributed archive: a
local `.crikeypkg`, or a URL, whose sibling `<url>.sig` is fetched into the same
scratch directory so the question can be answered at all. It does not govern two
sources, and both exclusions are properties of those sources rather than
concessions. A native *source directory* is packed into an archive by the
install call itself, so no publisher could have signed it and `refuse` would ban
source-tree installs instead of establishing anything. An *indexed* id already
has provenance: the index document was verified against the trust store and
pins the package digest, so a trusted key has vouched for the exact bytes, and
`--unsigned-policy` is refused there rather than accepted and ignored. Modern
and legacy packages are outside the scheme entirely — they have no canonical
member manifest to sign.

## Consequences

- Verify reports `signature`, `signer` and `fingerprint`; hardcoded `signed=false`
  is removed. Existing installers retain an explicit unchecked policy.
- Crypto material, signatures and trust stores have bounded reads and exact
  lengths; writes are atomic and private keys are owner-only.
- The old `unsigned_binary` shape marker remains separate from provenance.

## Alternatives rejected

- Signing archive bytes: harmless re-zips would invalidate signatures.
- Extending per-file `.sig`: it cannot authenticate the package as a whole.
- Trusting the shipped public key: an attacker can ship any key.
- X.509/PGP: unnecessary chain and revocation machinery for named local trust.
- Defaulting to `warn`: unknown native code must not be an implicit decision.
