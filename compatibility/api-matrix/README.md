# Legacy API compatibility matrix

Version-controlled **and tested** classification of every documented Keypirinha
API surface CriKey's Legacy Compatibility Layer implements (spec 14.10). CriKey
is an independent project; this is its compatibility layer, not a Keypirinha
component.

`matrix.toml` is data, not prose. It is loaded by
`crates/crikey-legacy-compat/src/matrix.rs`, printed by
`crikey dev compatibility-report`, and checked against the shipped Python shim
by `crates/crikey-legacy-compat/tests/compatibility_matrix.rs`. A row that
overstates the layer, a documented API that is missing, a duplicate key or an
unexplained caveat fails that suite.

## Schema

```toml
matrix-version = 1     # schema version; unknown versions are rejected

[[api]]
module = "keypirinha"       # required; one of the four documented modules
symbol = "Plugin.on_start"  # required; a documented name, or "*" (see below)
status = "full"             # required; one of the six values below
notes  = ""                 # required for behavioural-difference and partial
```

Entries are read in file order and `(module, symbol)` is unique. The parser
rejects unknown module names and future schema versions instead of interpreting
them with today's rules.

| `status` | meaning |
| --- | --- |
| `full` | the documented behaviour, reproduced |
| `behavioural-difference` | present and usable, but observably different — `notes` mandatory |
| `windows-only` | backed by Win32; never advertised as portable (acceptance 31.31) |
| `partial` | present with a documented gap — `notes` mandatory |
| `unsupported` | deliberately not reproduced; a spec 14.12 non-goal |
| `planned` | a documented Keypirinha API this milestone does not ship |

Modules are limited to the four documented ones: `keypirinha`,
`keypirinha_util`, `keypirinha_net`, `keypirinha_wintypes`. Private shim
internals (`_set_host`, `_clear_host`, `_install_stdout_guard`) are CriKey
plumbing, not claimed legacy API, and are deliberately absent.

## Configuration interpolation

The settings rows are deliberately marked `behavioural-difference`. The Rust
reader returns values as stored and does not implement Python
`configparser`'s extended interpolation, including `${section:key}` and
`${env:VARIABLE}` references. A plugin that relies on those references must
resolve them itself or change its configuration.

## `symbol = "*"`

A literal catch-all row for the rest of a module, not a glob. Storage and exact
lookup treat it as ordinary text; only classification consults it, and only
after an exact `(module, symbol)` match fails. A module with no `"*"` row
reports an unknown symbol as unclassified rather than guessing. This is how
`keypirinha_wintypes` classifies its whole Win32 surface in one row without the
matrix becoming a pattern-matching engine.

## Unknown spellings

Every `status` value must be one of the six above, exactly, in kebab-case. An
unrecognised spelling is a typed error naming the offending row; it is never
defaulted to `planned`, because a silently defaulted status is indistinguishable
from an honest gap.

## Permissions posture of a legacy package

Keypirinha plugins were written for a host with no permission model, and a
legacy package ships no `crikey.toml`. "No manifest" therefore has to resolve
to a decision the host writes down, not to a skipped check.

That decision is `Permissions::legacy_compatibility_baseline()` in
`crates/crikey-plugin-model/src/permissions.rs`. A legacy owner is registered
with it in the plugin action router and travels the same host-mediated gate as
a manifest-governed plugin. It grants:

| grant | why |
| --- | --- |
| `process` | `keypirinha_util`'s execution helpers are part of what this layer promises; refusing them would break the compatibility contract rather than confine anything |
| `filesystem`, package scope, read | the host reads a package's own shipped files for icons and other resources |

Nothing else is granted, and `crikey plugin doctor` prints the posture for
every legacy entry as
`legacy_permission_posture=compatibility-baseline`.

What the baseline does **not** do is confine the CPython child. The legacy
worker reaches the clipboard, the network and the filesystem through its own
interpreter, without asking the host, so no value here can stop it. The doctor
line says so as well (`unconfined=`). Read the baseline as a statement about
what the *host* will do on a legacy plugin's behalf, never as a sandbox.
