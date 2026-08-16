# Proposal: a local file and folder index

Status: proposal, not a decision. Written to be argued with; the decision
belongs in an ADR once the open questions at the end are settled.

Scope: searching the user's files and folders *as results*. Launching a file
manager is a separate, already-solved thing.

## 1. What the tree can do today

Nothing. All three backends report `Capability::FileSearch` as `Unavailable`
(`crikey-platform-linux/src/lib.rs:509-518`, `windows/src/lib.rs:130-137`,
`macos/src/lib.rs:465-472`), and no host code ever calls `backend.capability`,
so the value is a diagnostic rather than a switch. Nothing in the workspace
walks a filesystem.

What already exists and is worth reusing:

- `Category::File` and `Category::Directory` in the item model
  (`crikey-core/src/item.rs:51-65`).
- A matcher, prefilter and ranker that are agnostic about where items came
  from (`crikey-query`, `crikey-ranking`).
- Spec requirements that anticipate exactly this work: §18.7 already demands
  filesystem-event debounce, coalescing, rename correlation, overflow
  detection and a full-rescan fallback (`docs/spec/crikey-spec-v1.md:1488-1505`),
  and §18.5 notes macOS "Spotlight metadata where useful" (`:1459-1473`).

There is no file-index ADR, roadmap milestone, or TODO. This document is the
first.

## 2. The finding that reframes the request

**Flow Launcher does not have an indexer.** Its Explorer plugin delegates
entirely. `Settings.cs` exposes an `IndexProvider` choosing between
`EverythingSearchManager` and `WindowsIndexSearchManager`, defaulting to the
latter. The Everything path P/Invokes a bundled `Everything.dll`
(`Everything_SetSearchW`, `Everything_QueryW`); the Windows path builds an AQS
query and runs OLE DB SQL against `SystemIndex`. There is no crawl, no
watcher, and no cold-index build in Flow Launcher itself; its own
`StringMatcher.FuzzyMatch` is used only for highlighting, with ranking left to
the engine. When Everything is absent it does not fall back — it offers to
download and start it.

So the quality being admired is **Everything's** (voidtools), and Everything
achieves it by shipping *an elevated Windows service* that reads the NTFS MFT
and tails the USN change journal. That is the entire trick, and it is
Windows-and-NTFS-only.

This matters because it sets a realistic bar. Everything's own FAQ quotes
~5 s to index a fresh Windows install (~250k files), ~1 minute for 1M files,
and ~100 MB RAM / 45 MB disk for 1M files. Those are the numbers to be
measured against — vendor figures, not guarantees.

## 3. Is a cross-platform indexer possible?

Partly, and the split is not where one might hope.

**Collection cannot be cross-platform.** The three platforms have
categorically different primitives:

| | Bulk enumeration | Change feed | Privilege |
| --- | --- | --- | --- |
| Windows/NTFS | `FSCTL_ENUM_USN_DATA` over the MFT | `FSCTL_READ_USN_JOURNAL` | **Administrator** |
| macOS/APFS | `searchfs(2)` — whole-volume, ~100× `find` | `FSEvents`, replayable via `sinceWhen` | TCC prompts |
| Linux/ext4 | none — no MFT equivalent | inotify (per-dir) / fanotify (whole-fs) | fanotify FS-wide needs `CAP_SYS_ADMIN` |

Three hard constraints fell out of the research:

1. **Windows: a non-elevated process cannot build a fast whole-volume index.**
   Microsoft states change-journal operations require system-Administrator
   privileges; opening the volume with `FILE_READ_ATTRIBUTES` does not
   authorise the FSCTLs. This is precisely why Everything installs a service.
2. **macOS: `searchfs(2)` exists and is fast, but is not usable.** Apple's own
   man page carries a compatibility note saying it has been undocumented for
   over two years and that volume implementations vary; recovering paths from
   its results needs the private `fsgetpath` SPI. Fast, and a liability.
3. **Linux: there is no shortcut at all.** ext4 has inodes and directory
   blocks but no global name table, so a baseline is a tree walk. inotify is
   per-directory and capped by `fs.inotify.max_user_watches`; fanotify's
   `FAN_MARK_FILESYSTEM` avoids per-directory watches but needs
   `CAP_SYS_ADMIN`.

**Everything above collection *is* cross-platform**, and most of it already
exists here: normalisation, the trigram-ish prefilter, the matcher, ranking,
persistence. So the honest architecture is *one cross-platform core with three
thin collectors*, not a cross-platform indexer.

## 4. The constraint that shapes everything: the catalog will not hold it

`MemoryCatalog` is capped at 500,000 items and is measured, at that size, at
**506,626,048 bytes resident** with a 125 MB archive and 13.1 ms p95 query
(ADR-0008:50-56). The archive path caps at 256 MiB
(`crikey-catalog/src/lib.rs:1132-1140`).

A typical home directory is 100k–1M entries, so a naive "publish files as
catalog items" design either hits the item cap or spends half a gigabyte to do
worse than Everything does in 100 MB. The generic `Item` — owned label,
description, target, search terms, `BTreeMap` metadata, actions — is simply
the wrong representation for a file, where the only distinguishing data is a
parent directory and a basename.

**Therefore: files must not go through `MemoryCatalog`.** This is the one place
the proposal deliberately breaks with precedent — ADR-0016 established that
remote catalogs reuse `replace_catalog` specifically to avoid a second search
path. The justification for diverging is scale, and it should be argued
explicitly in the ADR rather than assumed.

## 5. Proposed design

### 5.1 A dedicated store: `crikey-file-index`

Compact by construction, modelled on plocate (proven: an inverted trigram
index over path names, 10–100× faster than mlocate on a smaller database):

- **Paths as edges, not strings.** `(parent_id, name)` with a string arena, so
  a deep path costs one basename, not its full text. This is also what makes
  rename-a-directory an O(1) edit rather than a subtree rewrite.
- **Trigram posting lists over lowercased basenames** for candidate
  generation, then the *existing* `crikey-query` matcher for scoring, so file
  results rank on the same evidence as everything else.
- **Names only.** Content search is explicitly out of scope; where an OS
  service offers it, delegate (§5.4).
- Budget target: ≤150 MB resident and ≤60 MB on disk for 1M entries, i.e.
  within sight of Everything. **This is a target to be measured, not a claim.**

### 5.2 Per-OS collectors behind one trait

A `FileCollector` service beside `ApplicationDiscovery`, following the existing
optional-service pattern (`crikey-platform/src/window.rs:8-20`: a backend hands
out a service only when it has one, rather than being forced to lie).

- **Baseline**: parallel walk (`jwalk`/`getdents64`-style) over configured
  roots, everywhere. Identical semantics on all three OSes.
- **Freshness**: `notify` for the common case; per-OS upgrades where they pay —
  FSEvents with `sinceWhen` replay on macOS (it can recover events missed while
  the launcher was closed, which inotify cannot), inotify with explicit
  `IN_Q_OVERFLOW` → reconcile on Linux.
- **Checkpoint identity per OS**, persisted: USN journal id + next USN
  (Windows), FSEvents event id + volume UUID (macOS), scan generation + mount
  identity (Linux). Any discontinuity → rebuild that root. This is how §18.7's
  overflow/full-rescan requirement is satisfied.

### 5.3 Capability reporting becomes real

`Capability::FileSearch` finally means something, and the states already in the
enum map cleanly: `Available`, `Partial` (scoped roots only),
`PermissionGated` (macOS TCC not granted; Windows helper not installed),
`Unavailable`. Each backend gets a deliberate arm, and the CLI can report it.

### 5.4 OS services as accelerators, never as the contract

Offer adapters, clearly labelled, never as the default:

- **Windows `SystemIndex`** — works unelevated, but only over configured
  scopes, security-trimmed, with documented staleness and service-health
  failure modes.
- **macOS Spotlight** (`MDQuery` via `objc2-core-services` 0.3.2) — fast, but
  three traps: `kMDItemPath` is *retrievable only* and cannot be queried or
  sorted on; TCC-denied scopes return **silently empty** rather than erroring;
  and a user with Spotlight off gets nothing (Alfred documents exactly this
  failure, because Alfred *is* a Spotlight front-end).
- **Linux plocate / Tracker (TinySPARQL) / Baloo** — plocate is names-only and
  only as fresh as the last `updatedb`; Tracker has a documented D-Bus/SPARQL
  endpoint and is the one genuinely reusable desktop index; Baloo has no stable
  third-party query API.

### 5.5 Opening a result

There is no generic open action today — host-mediated execution accepts only
`APPLICATION_LAUNCH_ACTION_ID` (`crikey-platform/src/lib.rs:87-88,111-137`).
A file result needs a host-mediated "open path" action with a reveal-in-file-
manager sibling, plus defined behaviour when the platform refuses.

## 6. Phasing

| Phase | Content | Ships |
| --- | --- | --- |
| **P0** | `crikey-file-index`, walk-based collector, `notify` freshness, configurable roots + exclusions, open action, capability reporting | Identical behaviour on all three OSes |
| **P1** | Per-OS freshness: FSEvents replay, inotify overflow handling, checkpointing | Correct across restarts |
| **P2** | Optional adapters: Spotlight, `SystemIndex`, Tracker | Coverage where the OS beats us |
| **P3** | Optional elevated Windows helper: MFT baseline + USN tail | Everything-class on Windows, opt-in |

P0 is deliberately the unglamorous one: it is the only phase that behaves the
same everywhere, and it is a prerequisite for honestly measuring whether P2/P3
are worth their cost.

## 7. Evidence plan

Extend the existing 500k harness (`benchmarks/src/lib.rs:188-258,364-395`),
which already reports candidates examined, p50/p95, archive bytes and RSS.
Add: realistic path-length distributions, a file/directory mix, churn during
indexing, and cold-start time. Publish against Everything's figures.

## 8. Open questions for the decision

1. **Scope default.** Home directory only, or every mounted volume? Everything
   indexes all local NTFS by default; Raycast defaults to home + `/Applications`.
2. **Is the second search path acceptable?** §4 argues scale forces it; ADR-0016
   argues one path. This needs an explicit ruling.
3. **Windows elevation.** Do we ever ship a privileged helper, or is
   "Everything-class Windows search" simply out of scope and delegated to
   Everything itself if the user has it?
4. **Hidden and ignored files.** Indexed, or excluded by default?
5. **macOS TCC.** Prompt on first run, or index only what is readable and
   report `Partial` until the user grants access?
