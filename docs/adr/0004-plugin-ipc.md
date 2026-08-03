# ADR-0004: Plugin IPC transport and encoding

Status: Accepted — encoding mechanism amended by ADR-0010; proto3 wire and
transport decisions remain accepted
Spec: §16.2–16.5, §12.3, §12.4

## Context

Out-of-process plugins in several languages must exchange catalog batches,
suggestion batches, cancellations and lifecycle messages with the host. The
protocol has to survive version skew between host and third-party SDKs, and must
not cost one IPC round trip per candidate.

## Decision

- **Encoding**: Protocol Buffers (proto3) wire format, schema in
  `sdk/protocol/crikey/v1/`. The native host uses the hand-written codec
  selected by ADR-0010. Evolution is additive only; unknown fields round-trip;
  tags are never renumbered or reused.
- **Framing**: length-delimited messages with a hard `MAX_FRAME_BYTES` cap
  (8 MiB). An oversized frame is a protocol violation and disconnects the plugin.
- **Transport**: Windows named pipes, Unix domain sockets, and stdio for
  development. The protocol layer is transport-agnostic; `Endpoint` selects one.
- **Envelope**: every message carries connection id, request id, optional query
  generation and optional deadline. Responses reference the request id.
- **Batching**: catalog and suggestion messages are batches, never single items.

## Consequences

- Third-party SDKs in C, C++, Go, Zig or C# need only a protobuf implementation
  and a socket, which is the point of §4.3.
- Schema-first development means the wire contract is reviewable independently
  of either side's implementation.
- The hand-written native codec avoids a `protoc` or `prost` build dependency;
  the Rust SDK carries the protocol implementation needed by plugin authors.

## Alternatives

- **JSON-RPC.** Human-readable and dependency-light, but parsing cost and size
  for 500k-item catalog transfers are unacceptable.
- **Cap'n Proto / FlatBuffers.** Zero-copy reads are attractive; rejected for v1
  because the ecosystem breadth for unofficial SDKs is narrower than protobuf.
- **Shared memory from day one.** Deferred per §16.8: added only if profiling
  proves local IPC insufficient.
