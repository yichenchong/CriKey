# Architecture decision records

One file per decision, numbered. Accepted decisions are not rewritten to hide
their history; a later ADR records an amendment or supersession and the index
points readers to the current decision.
For a superseding decision, add the later ADR and mark the old one
`Superseded by ADR-nnnn`.

An ADR is required for any choice that constrains other subsystems, contradicts
a default a reader would assume, or pulls a load-bearing dependency into the
tree. Provisional ADRs must state their revisit trigger.

| ADR | Decision | Status |
| --- | --- | --- |
| [0001](0001-workspace-layout.md) | Workspace layout and dependency direction | Accepted |
| [0002](0002-ui-stack.md) | UI stack: winit + wgpu + egui behind `crikey-ui` traits | Accepted |
| [0003](0003-concurrency-model.md) | Threading and async model | Accepted |
| [0004](0004-plugin-ipc.md) | Protobuf wire format and local transports; encoding amended by ADR-0010; shared-memory deferral upheld by ADR-0017 | Accepted, amended |
| [0005](0005-python-hosting.md) | Out-of-process CPython workers only | Accepted; discovery order amended by ADR-0018 |
| [0006](0006-legacy-scheduling.md) | Obsolete-work replacement for `legacy-strict` | Accepted |
| [0007](0007-path-representation.md) | Lossless platform paths | Accepted |
| [0008](0008-catalog-persistence.md) | Versioned per-plugin catalog archive with owned decode | Accepted for M1; production integration landed |
| [0009](0009-branding-and-attribution.md) | Branding and attribution | Accepted |
| [0010](0010-protobuf-codec.md) | Hand-written proto3 codec instead of generated bindings | Accepted; amends ADR-0004 |
| [0011](0011-wayland-backend.md) | Wayland global shortcuts through the GlobalShortcuts portal | Accepted |
| [0012](0012-package-signing.md) | Ed25519 detached package signatures over a canonical member manifest | Accepted |
| [0013](0013-plugin-index.md) | Signed JSON plugin index and the client that consumes it | Accepted |
| [0014](0014-wasm-runtime.md) | Out-of-process WebAssembly runtime using wasmi | Accepted |
| [0015](0015-restricted-c-abi.md) | Restricted C-ABI libraries loaded by supervised `crikey-cabi-host` | Accepted |
| [0016](0016-remote-indexing.md) | Remote catalog sources publish through the ordinary per-owner slice edge | Accepted |
| [0017](0017-shared-memory-transport.md) | Shared-memory transport stays deferred; ADR-0004's profiling gate measured and not met | Rejected for v1 |
| [0018](0018-interpreter-precedence.md) | Interpreter discovery order; a bundled runtime outranks the search path | Accepted; amends ADR-0005 |
| [0019](0019-plugin-sandbox.md) | Plugin processes are write-confined with Landlock on Linux; reads, syscalls and non-TCP networking stay unrestricted | Accepted |
| [0020](0020-plugin-pages.md) | Plugin pages cross the boundary as a display list; pixel canvas and embedded webview rejected | Accepted |
