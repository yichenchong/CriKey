# Real plugin corpus

A maintained corpus of published Keypirinha packages classified against the
Legacy Compatibility Layer (spec 27.4, acceptance 31.13, 31.31).

Packages are **referenced, never vendored**. This directory contains exactly
`corpus.toml` and this README; `crates/crikey-legacy-compat/tests/compatibility_matrix.rs`
fails if anything else appears. Vendoring third-party plugin source here would
import its licence into this repository and let a classification drift away from
the revision it claims to describe.

## Schema

```toml
corpus-version = 1     # schema version; a bump is a breaking change

[[package]]
id             = "prayzzz.keypirinha-epoch"                    # required, unique
source         = "https://github.com/prayzzz/keypirinha-epoch" # required, https
revision       = "ea2177868d078bf614e1c7a841f95a2aee5111bd"    # required, 40 hex chars
licence        = "MIT"                                         # required
classification = "works-unchanged"                             # required
notes          = "evidence for the classification"              # required, non-empty
```

`revision` is a full 40-character commit hash. Tags and branch names move, so a
result pinned to one cannot be reproduced. `source` must be an `https://` URL to
the upstream repository, and `licence` and `notes` are mandatory. A reference
without evidence is not a classification.

| `classification` | meaning |
| --- | --- |
| `works-unchanged` | loads and runs with no source or configuration edits |
| `works-with-configuration-changes` | no source edits; its own settings must be re-pointed |
| `works-with-minimal-source-changes` | a documented, small, enumerated source edit |
| `windows-only-but-compatible` | uses only delivered APIs but depends on Windows |
| `blocked-missing-apis` | calls an API classified `planned` or `unsupported` |
| `blocked-python-version` | needs a Python older or newer than the layer supports |
| `blocked-undocumented-behaviour` | depends on behaviour spec 14.12 does not reproduce |
| `works-only-under-legacy-optimized` | breaks under `legacy-strict` scheduling |
| `requires-legacy-strict` | breaks under `legacy-optimized` scheduling |
| `untested` | referenced, not yet classified — counted, never hidden |

An unrecognised spelling is a typed error, never a silent default.

## Evidence

A classification without evidence is a defect, so every entry's `notes` say what
produced it and how strong that is. At M3 the evidence is a **static API audit**:
the package's plugin sources were read at the pinned revision and every
`keypirinha*` name they touch was looked up in `../api-matrix/matrix.toml`. That
proves a package cannot work when it calls something `planned`, and shows nothing
stands in its way otherwise — but it is not an execution record. The CI host is
headless Linux with no Win32 and no desktop session, so no entry claims to have
been run. Packages whose sources have not been audited are `untested` and stay in
the published totals, because a hidden gap is worse than a small number.

`crikey dev test-legacy-compat` replaces static audits with observed results as
the worker gains the ability to drive these packages, and
`crikey dev compatibility-report` prints the totals.
