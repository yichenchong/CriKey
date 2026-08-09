# ADR-0018: Interpreter discovery precedence

Status: Accepted; amends ADR-0005
Spec: §14.11, §15.4, §15.3

## Context

CriKey resolves a CPython interpreter in two places — the modern Python host
and the Legacy Compatibility Layer — and until now both looked only at the
host: `CRIKEY_PYTHON`, then an external runtime profile, then `python3` on the
search path. That makes an installed CriKey silently dependent on whatever
interpreter the machine happens to carry, which §15.4 forbids for a shipped
artefact, and it means a modern plugin declaring `requires-python` fails on a
user's machine for reasons the user cannot act on.

Adding a runtime to the installed tree is not by itself a decision worth
recording. The ordering is, because it contradicts what a reader would assume.
The obvious order puts the system interpreter first and treats a bundled one as
a fallback for machines with no Python. That is backwards for a launcher: the
bundled runtime is the one the release was tested against, and a newer
interpreter found on `PATH` is an untested substitute that can differ in
extension-module ABI. Two subsystems must also agree, or a legacy package and a
modern package on the same machine can end up on different interpreters for no
reason a user could predict.

## Decision

Discovery is ordered and every rule is decisive — a candidate that is selected
and then fails to run, or fails `requires-python` or `MINIMUM_SUPPORTED_PYTHON`,
is an error and never a fall-through to the next rule:

1. `CRIKEY_PYTHON` — the operator override.
2. The interpreter named by `RuntimeProfile::External`.
3. The runtime staged beside the installed binary, at `python-runtime/` or at
   `../lib/crikey/python-runtime/` for a prefix install.
4. `python3` on the search path.

The bundled runtime therefore outranks a newer interpreter on `PATH`. Both the
modern host and the legacy layer consume one implementation of this policy
rather than each carrying their own.

Failing loudly at rule 3 is the load-bearing half. A broken or half-staged
runtime that quietly degraded to the host's Python would produce a release that
works on the maintainer's machine and fails on a user's, which is the exact
class of defect bundling exists to remove.

## Consequences

- A released artefact runs plugins without a system-wide interpreter (§15.4).
- A developer build, which stages nothing, discovers exactly what it did
  before; rule 3 finds nothing and costs one `stat`.
- Testing a plugin against a different interpreter is an explicit act:
  `CRIKEY_PYTHON`, not luck about `PATH` order.
- The staging script must verify relocatability, because a runtime whose
  `sys.prefix` escapes the staged tree would satisfy rule 3 and then load the
  host's standard library.

## Alternatives rejected

- **Bundled last, as a fallback.** Ships a tested runtime and then declines to
  use it whenever the machine has any Python at all, which is the opposite of
  what shipping it was for.
- **Bundled first, ahead of `CRIKEY_PYTHON`.** Removes the operator's ability
  to point CriKey at a specific interpreter, which is the one thing the
  override exists to do.
- **A manifest field naming an interpreter.** Rejected already in M6.5:
  `requires-python` states the portable constraint, and anything more specific
  is a plugin author dictating host layout.
