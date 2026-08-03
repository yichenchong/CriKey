# Synthetic legacy test plugins

CriKey's **own** hand-written Keypirinha-API plugins. Nothing here is a
redistributed third-party package: every file in every directory below was
written for this repository, is covered by this repository's licence, and exists
solely to be run by CriKey's own tests. Real third-party packages are described
— never vendored — in [`../real-plugin-corpus/`](../real-plugin-corpus/), and
the API surface they depend on is classified in
[`../api-matrix/`](../api-matrix/).

Each package proves one thing. Four of them are pointed at by
`crikey dev test-legacy-compat` and one by `crikey dev inspect-catalog`; both
commands are driven end to end by
`crates/crikey-cli/tests/m3_legacy_commands.rs`. A missing directory is a test
*failure*, never a skip: a conformance suite that quietly tests nothing when its
packages are absent is worse than no suite, because it is green.

## Layout

A package is a directory. Its id is the directory name, verbatim. The loader
chooses the top-level `.py` file whose stem equals that id when one exists; if
not, it chooses the first top-level module in deterministic order (spec 14.3).
There is no manifest: a directory with no top-level module is not an empty
package, it is a broken one, and the loader refuses it by name.

The ids are hyphenated, so some module stems are too — `well-behaved.py` is not
a valid Python identifier. The worker loads such a file through
`importlib.util.spec_from_file_location` against the package-relative path and
uses the declared package id separately, so the sanitized import key does not
change the package identity.

Every `.py` file is an importable package-local module; everything else, `.ini`
configuration included, is a package resource. Two packages carry a resource in
a subdirectory as well, so that split is exercised by real data the plugins
actually read rather than by a file nobody opens.

| Package | Module | Other files |
| --- | --- | --- |
| `well-behaved` | `well-behaved.py` | `well-behaved.ini`, `data/catalog.txt` |
| `ignores-should-terminate` | `ignores-should-terminate.py` | `ignores-should-terminate.ini` |
| `caches-dynamic-suggestions` | `caches-dynamic-suggestions.py` | `caches-dynamic-suggestions.ini` |
| `windows-only` | `windows-only.py` | `windows-only.ini` |
| `catalog-only` | `catalog-only.py` | `catalog-only.ini`, `data/descriptions.txt` |

## What each fixture proves

### `well-behaved` — the control

Publishes a catalog read from `data/catalog.txt`, answers suggestions promptly,
and reads `should_terminate()` unconditionally at the top of every iteration of
its work loop, abandoning without publishing when the flag is raised. Its
suggestions are recomputed from the query on every request.

Defends spec 14.5 and 14.8 as a whole: it must pass all thirteen core
conformance checks, report `portable=true`, and exit 0. When a check fails here,
the check is wrong, not the plugin.

### `ignores-should-terminate` — the named M3 exit criterion

Its `on_suggest` runs a long loop and contains no call to `should_terminate()`
anywhere in the module, so a host that marks the request obsolete mid-flight
observes zero polls. It still answers — late, over a loop bounded by a constant
rather than by a clock — because the report must never depend on a timeout
firing.

Defends spec 9.2 and 27.3, acceptance 31.17, and the roadmap M3 criterion "the
synthetic legacy test-plugin suite passes, including a plugin that ignores
`should_terminate()`". It must fail `should_terminate_observed` and **only**
that check.

The misbehaviour is behavioural, not declared. There is no
`ignores_termination = True` marker for a harness to read, and no configuration
key that switches the polling off: a defect behind a flag tests the flag, and
the check would then pass on a plugin that merely lied politely.

### `caches-dynamic-suggestions` — staleness by memoization

Fills a module-level memo keyed by plugin id on its first `on_suggest` and
republishes it verbatim for every later query, so two different queries produce
a byte-identical payload. It polls `should_terminate()` before consulting the
memo, so an obsolete request abandons rather than serving a warm answer.

Defends spec 14.9 and acceptance 31.18, which forbid caching dynamic legacy
suggestions under the default profile: a cached answer is indistinguishable from
a stale one (spec 8.5) and the user cannot tell which they are looking at. It
must fail `dynamic_suggestions_not_cached` and only that check.

The memo is bounded — sixteen ids by default, configurable up to a hard ceiling
— and on overflow stops admitting new ids rather than growing. A defect fixture
is still not licensed to leak, and the suite must fail this package for caching,
not for leaking.

### `windows-only` — correct, and simply not portable

Imports the Windows-only compatibility module at module scope and reaches a
Win32 entry point only from behind `if …is_available():`. The import succeeds on
every host, so the package loads and every scheduling check runs and passes; the
Win32 branch cannot run here, so the report says `unavailable`.

Defends spec 14.2, 14.12 and acceptance 31.31, plus roadmap principle 7: a run
with no failures but some unavailable checks is `incomplete`, not `pass`, and a
package that needs Win32 is never presented as cross-platform on any host. It
reports `checks_failed=0`, `checks_unavailable=1`, `portable=false`, and exits
non-zero off Windows.

The guard spelling is load-bearing. `hasattr(kpwt, "kernel32")` and
`getattr(kpwt, "kernel32", None)` would **not** work: attribute access on a
Win32 entry point raises `WindowsOnlyError`, which is a `RuntimeError` and
deliberately not an `AttributeError`, precisely so those two probes cannot
launder a Win32 access into a silent `False` or `None`. A fixture written either
way would fail to load off Windows instead of loading and honestly reporting the
check unavailable — which is the behaviour under test.

### `catalog-only` — the catalog under a microscope

Publishes exactly three items, in a fixed order, and answers no suggestions. One
label is exactly

```text
Deterministic Fixture Item #2 (50% = half)
```

which holds a space, an `=` and a `%` — the three characters that each break
`crikey dev inspect-catalog`'s `key=value` output format in a different way.

Defends spec 26.3 and 10.1, and the losslessness of the report encoding. Legacy
item labels are written by plugin authors, not by us, so the encoding is
exercised by a value a plugin really published rather than only by an assertion.
The three targets are distinct, which is what keeps the host-derived item
identities distinct (spec 10.2).

Item labels live in the module and nowhere else; only the *descriptions* come
from `data/descriptions.txt`, so a resource that failed to load can never
disturb the string the encoding test depends on.

## House rules for anything added here

* **Deterministic.** No wall clock, no randomness, no network, no absolute
  paths, no environment sniffing. Two runs of one invocation must be
  byte-identical, the failing ones included, or the report cannot be diffed
  against the last release — which is the only use a compatibility corpus has.
* **Bounded.** Every loop, every retained collection has an explicit cap and a
  documented overflow behaviour. A fixture that could hang or grow without bound
  stops testing the thing it was written for.
* **Genuinely misbehaving.** A fixture that demonstrates a defect must contain
  the defect. Simulating one with a flag the harness reads turns a behavioural
  check into a self-report.
* **One rule each.** A fixture that breaks two rules cannot distinguish a
  precise conformance suite from one that reports blanket failures.
