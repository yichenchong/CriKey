# ADR-0001: Workspace layout and dependency direction

Status: Accepted
Spec: §5.3, §29

## Context

The specification prescribes ~20 crates. Left unconstrained, a Rust workspace of
that size drifts into a cycle-free-on-paper but conceptually tangled graph, and
platform code leaks into "portable" crates through a convenience import.

## Decision

- One Cargo workspace, resolver 2, with shared `[workspace.package]` and
  `[workspace.dependencies]`. Crates never pin their own versions of shared deps.
- `crikey-core` has no intra-workspace dependencies. It holds generations, the
  item/action model, ids, lossless paths and the error type.
- Platform-independent crates must not depend on `crikey-platform-*`. They depend
  on `crikey-platform` traits only.
- Only `crikey-app` names a backend crate, and only through
  `[target.'cfg(...)'.dependencies]`. Each backend crate is additionally
  `#![cfg(...)]`-gated so a mistaken dependency yields an empty crate rather than
  a build that silently links the wrong platform.
- Only production `crikey-cli` names `crikey-app`; the `benchmarks` crate also
  depends on it as an out-of-band harness. Production code reaches the
  composition root only through the CLI.
- SDK crates (`sdk/rust`) depend on the protocol and core model, never on host crates.

## Consequences

- The dependency graph is a DAG that mirrors the spec's component diagram, so a
  reviewer can check a `Cargo.toml` diff against §5 directly.
- Adding a desktop API call to, say, `crikey-catalog` requires adding a
  dependency that review will reject — the rule is mechanically visible.
- Cross-cutting types must earn their place in `crikey-core`; anything
  subsystem-specific stays in the subsystem crate.

## Alternatives

- **Fewer, larger crates.** Faster to build, but the platform-separation rule
  becomes a comment instead of a compiler-checked boundary.
- **Separate workspaces per subsystem.** Adds path/version friction with no
  isolation benefit at this size.
