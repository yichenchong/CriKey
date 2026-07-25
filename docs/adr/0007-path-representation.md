# ADR-0007: Lossless platform paths

Status: Accepted
Spec: §18.3

## Context

Windows paths are potentially ill-formed UTF-16; Unix paths are arbitrary byte
strings. A launcher that normalizes paths to `String` will eventually fail to
launch a file it displayed. Display formatting and path identity are different
concerns and must not share a representation.

## Decision

- Internally, paths are `PlatformPath`, a newtype over `OsString`. Identity
  comparisons, hashing and catalog keys use it directly.
- Display uses an explicitly lossy rendering, never fed back into identity.
- Across IPC, paths are transported as raw bytes plus an encoding tag: WTF-8 for
  Windows-origin paths, raw bytes for Unix. The SDKs expose a path type that
  round-trips those bytes rather than a plain string.
- Any conversion to `String` in host code is a deliberate, reviewed act.

## Consequences

- Files with non-UTF-8 names remain launchable and indexable.
- Plugin SDKs carry a path type instead of using their language's string type,
  which is friction for plugin authors — mitigated with helpers that make the
  lossy conversion explicit and obvious.
- Catalog serialization stores bytes, not strings.

## Alternatives

- **UTF-8 everywhere, lossy on ingest.** Simplest; silently loses items and
  breaks execution for a real minority of files.
- **Platform-tagged enum in core.** Adds branches to the hot path for a case the
  `OsString` representation already handles.
