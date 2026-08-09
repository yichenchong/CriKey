# ADR-0017: Shared-memory transport stays deferred

Status: Rejected for v1 — ADR-0004's deferral upheld on measurement; revisit
trigger tightened below
Spec: §16.8, §2.2, §25.1

## Context

ADR-0004 deferred a shared-memory transport behind an explicit gate: "added
only if profiling proves local IPC insufficient." Nobody had run that profile,
so this ADR runs it before deciding rather than building first and justifying
afterwards.

`benchmarks/transport/native_transport_probe.rs` measures the shipping path
against a prototype shared-memory data plane (one `MAP_SHARED` region, blocking
doorbell, no spinning). It compiles standalone — `rustc -O --edition 2021`, no
cargo, no dependencies — but includes
`crates/crikey-native-protocol/src/{wire,message,frame}.rs` verbatim, so the
codec and framing under test are the shipped ones, and the catalog is the
500,000 synthetic items of `benchmarks/src/lib.rs` encoded as `CatalogBatch`
envelopes. Both transports carry identical bytes and are followed by identical
decoding, so the transport is the only variable.

The first result reshaped the workload: a batch sized only against
`MAX_FRAME_BYTES` (8 MiB) encodes and then fails to decode, because
`DECODE_ALLOCATION_BUDGET` separately bounds decoded repeated fields. The real
maximum batch is 7,841 items / 1.761 MiB, making a 500k catalog 64 frames.

Intel N150, 2 cores, Linux 7.0, rustc 1.86.0, best of three runs, two
independent 500k invocations:

| stage | 500,000 items | 50,000 items |
| --- | --- | --- |
| encode | 887.9 / 942.3 ms | 85.0 ms |
| socket transfer (shipping) | 46.4 / 44.6 ms | 6.8 ms |
| shared-memory transfer | 18.5 / 19.9 ms | 2.6 ms |
| decode | 618.6 / 652.5 ms | 47.2 ms |
| end to end | 1552.9 / 1639.4 ms | 139.0 ms |

117,750,704 wire bytes (235.5 B/item). Socket throughput 2,420–2,516 MB/s;
fixed per-frame cost 8.6–9.9 µs, so all 64 frames cost under 0.7 ms of framing.

## Decision

Do not build a shared-memory transport for v1. `TransportKind` keeps its three
variants, no capability advertises shared memory, and nothing new is opt-in
because nothing new exists. Local IPC is not insufficient: it is 2.7–3.0 % of
the time a 500k catalog costs, and a *perfect* shared region — the measured
prototype, already free of the socket's two copies — returns 1.5–1.8 %.

## Consequences

- The honest capability report is unchanged: no code claims a transport it does
  not have, and `endpoint_vocabulary_names_no_shared_memory_transport` in
  `crates/crikey-native-protocol/tests/transport.rs` pins that no endpoint
  spelling names one.
- The 97 % of catalog transfer cost that is encode and decode is where a future
  optimisation belongs; that is ADR-0008's territory, not ADR-0004's.
- Reopen when either holds: sustained local-IPC throughput for large batches
  falls below ~400 MB/s (a 5–6× regression on the numbers above), or the codec
  gets fast enough that encode plus decode for 500,000 items drops under
  ~200 ms — at which point transport is ~19 % of end to end and a shared region
  returns ~11 %. Rerunning the probe is the evidence either claim needs.

## Alternatives rejected

- **Build it anyway, strictly opt-in.** A transport nobody can justify enabling
  is still two platform implementations, an attacker-controlled shared region
  to validate on every read, and a lifecycle to prove on crash — permanent cost
  for a 1.8 % ceiling.
- **Raise `MAX_FRAME_BYTES` instead.** The batch is bounded by the decode
  allocation budget, not the frame cap, so this changes nothing.
- **Report shared memory as `Unavailable` at negotiation.** Advertising a
  capability slot for a decision that was rejected invites a future silent
  fallback; absence is the truthful report.
