# ADR-0005: Python hosting

Status: Accepted
Spec: §4.2, §14.11, §15.3, §15.6, §24.1

## Context

CriKey must run legacy Keypirinha plugins, modern Python plugins, and plugins
with binary extension modules — some of which require different interpreter
versions and conflicting dependency sets. A crashing interpreter must not take
CriKey down, and Python must never execute on the UI thread.

## Decision

CPython runs only in supervised worker processes. The host embeds no
interpreter: there is no PyO3 interpreter in the main process, ever.

- A worker is keyed by runtime profile, interpreter, content-addressed
  dependency environment, entrypoint, and plugin source path. Identical keys
  may share a process; distinct source paths or entrypoints receive separate
  workers because the protocol has no per-call plugin routing.
- The environment store may reuse a materialized dependency environment, but
  that reuse does not imply that unrelated plugin processes share an address
  space. Plugins with unstable native extensions still receive a dedicated
  worker.
- The import path is assembled explicitly: plugin source, packaged modules,
  managed dependencies, CriKey SDK, standard library. System-wide
  `site-packages` is excluded by default.
- Native workers speak the native v1 proto3 protocol. Modern and legacy Python
  workers use their own bounded newline-delimited JSON protocols; the legacy
  worker additionally hosts the Keypirinha-compatible shim modules.

## Consequences

- Interpreter segfaults, C-extension crashes and version conflicts are contained
  in a worker; recovery is a restart (§31.10, §31.20).
- No GIL interacts with the UI or the query hot path.
- Per-worker process and startup cost is real: mitigated by lazy start, reuse
  for identical worker keys, and serving cached catalog results while workers
  boot.
- Any host API a plugin needs must be exposed as a protocol message, which
  provides the boundary at which permission enforcement can be added.

## Alternatives

- **Embedded CPython via PyO3.** Lowest call overhead; rejected because one
  faulty extension terminates CriKey and multiple interpreter versions cannot
  coexist in one process.
- **Sub-interpreters.** Insufficient isolation for native extensions and poor
  compatibility with existing legacy plugins.
