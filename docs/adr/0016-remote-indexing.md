# ADR-0016: Remote catalog indexing

Status: Accepted
Spec: §2.2 (distributed or remote indexing), §10.1, §22.1, §22.4, §25.1, §26

## Context

Spec §2.2 lists distributed or remote indexing as later scope: catalog content
that lives on another machine — a shared team index or a file server — searched
alongside local items. A remote document is hostile input that decides what a
keystroke launches, and a network fetch is unbounded in time and size. The
launcher already has the right seams: `SearchService::replace_catalog` is the
single per-owner publication edge (ADR-0008), and `PackageFetcher` established
that network access belongs behind a trait.

## Decision

- A source is one more catalog owner, `remote.<name>`, publishing through
  `SearchService::replace_catalog`. There is no second search path, ranking rule
  or cache; the ordinary per-owner slice keeps serving while it is unreachable.
- The wire format is the catalog archive. `encode_slice_document` and
  `decode_slice_document` expose the same bounded, field-by-field codec used by
  the on-disk cache. A second format would mean a second validation boundary.
- A manifest (`crikey-remote-catalog 1`, `slice`, `bytes`, `sha256`, optional
  `signature`) is capped at 4 KiB. It names one plain file beside itself; the
  declared length is refused before the document is requested.
- Refusal order is length, digest, optional signature, decode, item checks,
  then catalog admission. Signature checks use the package manager's
  `verify_signed_manifest` and operator trust store (ADR-0012); no crypto is
  written here.
- Only `https` and `file` are accepted. `file://` is the mounted-share case.
- `poll` starts fetches on worker threads, `apply` admits completed documents,
  and repeated triggers coalesce to one fetch. `crikey catalog refresh` runs the
  same verification and writes the verified slice to the persistent cache.
- No configuration means no sources, threads or sockets. Queries never fetch.

## Consequences

Items are re-owned to `remote.<name>` on the fetch thread. Unsigned sources are transport-checked, not authenticated; `require-signature` buys authenticity. The CLI refresh command cannot update an already-running launcher and says so.

## Alternatives rejected

- **JSON/TOML index.** Readable, but it adds another parser, bounds and hostile
  input validation model where the archive codec already exists.
- **Fetch on the query path.** One slow server would violate §25.1.
- **A remote provider plugin.** Data does not need plugin scheduling or another
  worker boundary; it should publish through the catalog seam directly.
- **Trusting server `Content-Length`.** The client manifest and bounded read are
  controlled by the operator's limit, not by the party being bounded.
