# ADR-0007: Lossless platform paths

Status: Accepted
Spec: §18.3

## Context

Windows paths are potentially ill-formed UTF-16; Unix paths are arbitrary byte
strings. A launcher that normalizes paths to `String` will eventually fail to
launch a file it displayed. Display formatting and path identity are different
concerns and must not share a representation.

## Decision

- Platform discovery and launch APIs use `PlatformPath`, a newtype over
  `OsString`. Identity comparisons and backend launch calls use it directly.
- Core catalog items intentionally carry a `String` target field. The platform
  layer encodes a native target into that field with `encode_target`, using a
  readable form for valid UTF-8 and a platform tag plus escapes for units that
  UTF-8 cannot represent. `decode_target` reconstructs the native path before
  launch and rejects a foreign-platform tag instead of guessing.
- Catalog archives and plugin IPC therefore carry the encoded target string,
  not an `OsString` or a raw byte buffer. A caller must use the platform
  conversion helpers when putting a native path into an item; display text
  must never be fed back into identity or launch.
- SDK target fields remain strings because they carry logical plugin targets;
  native application paths use the host's encoded representation.

## Alternatives

- **UTF-8 everywhere, lossy on ingest.** Simplest; silently loses items and
  breaks execution for a real minority of files.
- **Platform-tagged enum in core.** Adds branches to the hot path for a case the
  `OsString` representation already handles.
