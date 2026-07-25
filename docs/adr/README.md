# Architecture decision records

One file per decision, numbered, never edited in place once accepted: a
superseding ADR is added and the old one is marked `Superseded by ADR-nnnn`.

An ADR is required for any choice that constrains other subsystems, contradicts
a default a reader would assume, or pulls a load-bearing dependency into the
tree. Provisional ADRs must state their revisit trigger.

| ADR | Decision | Status |
| --- | --- | --- |
| [0001](0001-workspace-layout.md) | Workspace layout and dependency direction | Accepted |
| [0002](0002-ui-stack.md) | UI stack: winit + wgpu + egui behind `crikey-ui` traits | Accepted |
| [0003](0003-concurrency-model.md) | Threading and async model | Accepted |
| [0004](0004-plugin-ipc.md) | Protobuf over named pipes / UDS / stdio | Accepted |
| [0005](0005-python-hosting.md) | Out-of-process CPython workers only | Accepted |
| [0006](0006-legacy-scheduling.md) | Obsolete-work replacement for `legacy-strict` | Accepted |
| [0007](0007-path-representation.md) | Lossless platform paths | Accepted |
| [0008](0008-catalog-persistence.md) | Zero-copy memory-mapped catalog cache | Provisional |
| [0009](0009-branding-and-attribution.md) | Branding and attribution | Accepted |
