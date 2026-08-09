# ADR-0008: Catalog persistence

Status: Accepted for M1; production integration landed
Spec: §22.1, §25.1, §25.6, §31.27

## Context

Startup stage 2 must recover cached catalog slices without trusting plugin-owned
files, and the stress path must keep 500,000 items searchable. The specification
also asks main-process idle memory to remain below 100 MiB where practical.
Those constraints require a measured format decision rather than an assumed
zero-copy design.

## Decision

- M1 uses the versioned `crikey-catalog-archive-v1` binary format implemented by
  `FileCatalogCache`.
- Each plugin owns one checksummed slice. Writes replace that slice atomically;
  corruption, truncation, a foreign schema, or a hostile element count turns
  only that slice into a cache miss.
- Loads decode into owned `Item` values and build the in-memory presence,
  ordered-pair, and label-prefix indexes. M1 does not claim mmap or zero-copy
  loading.
- Platform paths retain their ADR-0007 encoding tag.
- A schema mismatch discards and rebuilds the slice; the cache is never migrated
  in place.

This is the boring format whose behavior is already bounded and tested. An mmap
format remains an optimization option, not an architectural prerequisite.

The codec and `FileCatalogCache` are implemented and covered by library and
integration tests. The current `crikey run` composition root constructs the
cache, loads persisted slices before the persisted-catalog startup stage, and
writes nonempty refreshed slices after successful catalog replacement. Torn,
foreign, unreadable, or otherwise invalid individual slices are reported as
rebuildable misses; cache write failures are reported, while a failure to
enumerate the cache root is returned as a startup error.

## Measured evidence

Release measurements on the reference Intel N150 machine, using 64 complete
labels typed one character at a time through `SearchService::submit_query`.
They are one dated run on an otherwise idle host: `crikey dev benchmark`
reproduces them, but only under the same conditions -- a re-run on a loaded
machine has measured six times the load time and fifty times the query p95,
which says nothing about the codec.

| Items | Archive | Load | Query p95 | Resident after load | Peak RSS |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 50,000 | 12,527,569 B | 147,756,924 ns | 1,213,171 ns | 65,806,336 B | 65,871,872 B |
| 500,000 | 125,275,069 B | 1,800,725,150 ns | 13,099,462 ns | 506,626,048 B | 630,509,568 B |

The 500,000-item run meets the 16 ms cached-local-result target. The 50,000-item
working set remains below 100 MiB. The stress-scale resident footprint and load
time expose the cost of full decoding; they do not justify relabelling the
current codec as zero-copy.

The harness measures the shipped archive only. No `rkyv`, `bincode`, or mmap
baseline exists in this workspace, so this ADR makes no comparative claim about
one.

## Consequences

- The archive and decoder are small, dependency-free, and covered against
  truncation, checksum failures, path escape, hostile counts, and schema drift.
- Search owns prepared labels and indexes, so query latency is decoupled from
  the on-disk representation.
- Full decode duplicates the file-backed payload in owned heap state and makes
  stress-scale startup and memory materially more expensive.
- A future mmap representation may replace the codec behind `CatalogCache`
  without changing query, ranking, or application APIs.

## Revisit trigger

Re-open this ADR when a product requirement places a hard bound on stage-2 load
time or requires a 500,000-item steady-state footprint near 100 MiB. Any
replacement must benchmark the same end-to-end query path, preserve per-slice
fault isolation, and show a material improvement over the measurements above.

## Alternatives

- **Memory-mapped zero-copy archive (`rkyv` or bespoke).** Plausibly lowers
  decode cost and owned memory, but adds layout, validation, and alignment
  complexity. Deferred until the revisit trigger fires and a measured prototype
  exists.
- **SQLite.** Strong incremental-update and query behavior; adds a dependency
  and a second indexing model. Reconsider if incremental catalog churn dominates.
- **JSON/TOML.** Debuggable, but unsuitable for this scale and trust boundary.
