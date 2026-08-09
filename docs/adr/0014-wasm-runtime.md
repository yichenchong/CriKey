# ADR-0014: Out-of-process WebAssembly runtime

Status: Accepted
Spec: §2.2, §16, §19, §20, §24; acceptance §30, §31

## Context

CriKey must run third-party WebAssembly plugins without executing guest code in
the UI process. A module is hostile input: validation, memory growth, imports,
traps and runaway loops all need bounded failure. The host machine has two
cores and 7.4 GB RAM, and the repository already limits dependency build size
because workspace debug output has exhausted disk. CI denies warnings.

A WASM plugin must remain indistinguishable from a native plugin after its
process boundary: the existing native supervisor and native protocol already
provide restart, timeout, crash diagnostics and sibling containment.

## Decision

Use `wasmi` 1.1 with `default-features = false, features = ["std"]`. The
interpreter is loaded only by the new `crikey-wasm-host` executable. The CriKey
application never links or instantiates a module. `wasmi`'s `wat` feature is
not enabled in production, so a text file cannot be mistaken for a `.wasm`
binary. The production dependency has about eight indirect normal packages;
the default `wat` tree adds roughly seven more.

The host accepts ABI version 1. A guest exports `memory`,
`crikey_abi_version() -> i32`, `crikey_alloc(i32) -> i32`, and any of
`crikey_suggest(i32,i32) -> i64`, `crikey_catalog() -> i64`, and
`crikey_execute(i32,i32) -> i32`. Blobs use `CKW1`, little-endian integers,
bounded length-prefixed UTF-8 strings and count-prefixed collections. The
item batch maps directly to `crikey_core::Item` and `Action`; the host supplies
the owning plugin id rather than trusting the guest.

There is no WASI linker. `crikey::log` is always available. Filesystem reads
and environment reads are defined only when the manifest grants the narrow
corresponding permission. Filesystem reads are package-directory confined and
bounded; wider manifest scopes are not claimed as honoured. Memory is bounded
with `StoreLimits`. Fuel is derived from the hard suggestion deadline and
interrupts spinning code. Since wasmi 1.1 has no epoch interruption, a
wall-clock watchdog is also armed; an extreme overrun aborts this worker and
lets the supervisor restart it.

## Consequences

A guest trap poisons only its instance; the process remains available for the
next request, while a watchdog overrun is contained by the supervisor. A
sibling plugin is a separate process and continues independently. Protocol
payloads do not change. The interpreter is slower than native compilation and
adds startup and memory cost, but the small build is appropriate for the host.

The guest ABI is intentionally narrow: configuration, network, clipboard,
process control, native libraries and other capabilities are unavailable,
not reported available. The conformance fixture proves a third-party-shaped
module can build and answer without permissions.

## Alternatives rejected

- **wasmtime.** Faster JIT execution, but its Cranelift build and dependency
  footprint are disproportionate on the two-core, disk-constrained host.
- **In-process wasmi.** Lower IPC cost, but violates the isolation invariant:
  interpreter faults and guest resource abuse would share the UI process.
- **WASI by default.** Filesystem, environment and network would be reachable
  without an explicit manifest grant; narrow linker imports make denial
  structural instead.
- **Epoch-only interruption.** wasmi 1.1 does not expose epoch interruption;
  fuel plus the supervisor/watchdog is the available bounded design.

Revisit this ADR if cold-start and query latency fail the measured §25 budget,
if wasmi gains epoch interruption, or if disk/build constraints are relaxed
enough that wasmtime's speed justifies its footprint.
