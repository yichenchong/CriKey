# ADR-0019: Plugin processes are write-confined with Landlock on Linux

Status: Accepted
Spec: §5.1, §16.1, §20.2, §24.4, §31.30

## Context

Every runtime that executes third-party code already runs out of process:
native plugins, the WASM and C-ABI hosts, modern Python workers and legacy
Keypirinha workers. That contains a crash and nothing else. A plugin that runs
at all runs with the user's full authority — it can overwrite the user's
documents, rewrite another plugin's code inside the installation root, or
replace the launcher's own catalog cache. §20.2 asks for operating-system
sandboxing "where practical", and until now the honest answer in the README was
that there was none.

The three obvious mechanisms are not equally practical:

- **Landlock** (Linux 5.13+, filesystem; 6.7+, TCP) needs no privilege, no
  helper daemon and no policy language. A ruleset is built from open
  descriptors and applied by the process to itself.
- **seccomp** filters syscalls. Getting a filter right for CPython plus
  arbitrary C extensions is a research project, and a wrong filter kills the
  interpreter with `SIGSYS` at a point no diagnostic can explain.
- **Windows AppContainer / macOS App Sandbox** both require the *child* to be
  built and signed for the sandbox, which is not something a host can impose on
  a third-party executable after the fact.

## Decision

A new crate, `crikey-sandbox`, builds a Landlock ruleset in the parent and
applies it in the child between `fork` and `exec`, alongside
`PR_SET_NO_NEW_PRIVS`. It is installed by all three spawn paths:
`crikey-native-host` (which covers native, WASM and C-ABI plugins),
`crikey-python-host` and `crikey-legacy-compat`.

The policy is a **write allowlist**:

- Handled rights are the write-shaped ones only — `WRITE_FILE`, the `MAKE_*`
  and `REMOVE_*` family, plus `REFER` (ABI 2) and `TRUNCATE` (ABI 3). Read and
  execute rights are not handled at all, so they are not restricted.
- Granted paths are the system temporary directory, `/dev/shm`, the usual
  writable character devices (`/dev/null`, `/dev/zero`, `/dev/full`,
  `/dev/random`, `/dev/urandom`, `/dev/tty`), and the one directory the host
  tells that plugin to write to: for a legacy package, the
  `package_cache_path()` directory. Nothing else. That deliberately excludes
  the installation cache root behind `package_cache_dir()`, which is where
  archive packages are extracted and therefore holds every plugin's code, and
  CriKey's own configuration directory, which decides which plugins load at
  all. Both stay readable, which is all the compatibility API promises.
- A manifest without `permissions.network = true` additionally gets
  `LANDLOCK_ACCESS_NET_BIND_TCP` and `CONNECT_TCP` handled and granted to
  nothing, on kernels implementing ABI 4 or later.

`CRIKEY_PLUGIN_SANDBOX=off` disables it process-wide. An unrecognised value
means `enforce`, so a typo fails closed.

Every mechanism reports itself separately through `SandboxReport`, and
`crikey plugin doctor` prints the result per plugin. A kernel without Landlock,
a kernel below ABI 4 asked for TCP denial, and an operator override are three
different lines, none of which reads as "enforced".

## Consequences

What this buys: integrity. A plugin cannot corrupt the user's files, another
plugin's code, or the launcher's state, and cannot escape by exec'ing something
else — the restriction is inherited.

What it does not buy, and what the documentation must keep saying:

- **No confidentiality.** Reads are unrestricted. A read allowlist wide enough
  for CPython is close to "everything the user can read" anyway, and a narrow
  one that breaks an interpreter is worse than none.
- **No syscall confinement.** There is no seccomp filter.
- **Network confinement is TCP-only and Landlock-only.** UDP, Unix sockets,
  netlink and an inherited connected socket are untouched.
- **Linux only.** Windows and macOS children are spawned exactly as before.
  The Windows job object is a resource limit, not a sandbox, and is not
  relabelled as one.

Costs accepted: a legacy plugin that wrote to a directory nobody gave it now
fails on Linux where it previously succeeded. That is the point of the change,
and the override exists for the operator who disagrees in a specific case.

## Alternatives rejected

- **A read allowlist as well.** Rejected above: it breaks interpreters and buys
  a confidentiality claim this project cannot honestly defend.
- **Using the `landlock` crate.** Its ergonomics are good, but the restriction
  must happen in a `pre_exec` closure where allocation is illegal; the split
  this crate uses — build in the parent, apply in the child — is not the shape
  that crate's API encourages, and the two syscalls it wraps are twenty lines.
- **Confining the launcher process too.** The launcher legitimately writes
  wherever the user points it and launches arbitrary applications. Confining it
  would need a policy that is nearly "allow everything", which is a claim
  without a property behind it.
