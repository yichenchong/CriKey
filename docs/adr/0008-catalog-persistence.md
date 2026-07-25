# ADR-0008: Catalog persistence

Status: Provisional — revisit when the M1 benchmark exists
Spec: §22.1, §25.1, §25.6, §31.27

## Context

Startup must reach "user can type" quickly with a persisted catalog of up to
500,000 items, and main-process idle memory should stay under 100 MiB. Parsing
half a million items from JSON at every launch is not compatible with that.

## Decision (provisional)

- The persisted core catalog is a single memory-mapped, zero-copy archive per
  plugin slice, loaded without a parse step during startup stage 2.
- Candidate format: `rkyv`. Selection is confirmed by an M1 benchmark measuring
  load time, resident memory and search latency at 500k items against a
  `bincode`-decode baseline.
- Strings are stored as bytes with an encoding tag (ADR-0007), not `String`.
- Each plugin owns a slice; invalidation, rebuild and rollback are per slice, so
  one plugin's rebuild never invalidates the whole catalog.
- A schema version is embedded; a version mismatch discards the cache and
  rebuilds rather than attempting migration.

## Consequences

- Startup cost approaches an `mmap` plus index fix-up rather than a parse.
- Resident memory is dominated by page cache the OS can reclaim.
- Zero-copy formats constrain the data model: variable-length and optional
  fields need care, and the archive layout becomes part of the compatibility
  surface guarded by the schema version.

## Revisit trigger

If the M1 benchmark shows a plain `bincode` decode meeting both the startup and
the memory targets, take the simpler format and drop the zero-copy constraint.

## Alternatives

- **SQLite.** Excellent for incremental updates and queries; adds a dependency
  and a per-query cost the in-memory index does not have. Reconsider if
  incremental catalog updates dominate.
- **JSON/TOML.** Debuggable, far too slow at this scale.
