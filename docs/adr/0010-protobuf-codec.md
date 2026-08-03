# ADR-0010: Hand-written proto3 codec instead of generated bindings

Status: Accepted — amends the *encoding mechanism* clause of
[ADR-0004](0004-plugin-ipc.md). ADR-0004's wire decisions (proto3 on the wire,
schema in `sdk/protocol/crikey/v1/`, length-delimited framing, 8 MiB cap,
envelope shape, batching) all stand.
Spec: §16.3, §4.3

## Context

ADR-0004 said the host would generate its bindings from the `.proto` with
`prost`. Building that requires `protoc` at build time, which is not present in
the build environment and would become a prerequisite for every contributor and
CI runner. `prost` without `protoc` is possible (hand-written structs with
derive attributes), but `prost` decodes unknown fields by discarding them,
while both §16.3 and `sdk/protocol/README.md` promise that unknown fields
round-trip so a newer peer never loses data talking to an older one.

## Decision

- `crikey-native-protocol` implements the proto3 wire format directly:
  varint/LEN/fixed32/fixed64 keys, proto3 default-field elision, and one
  hand-written `Message` impl per message in the schema.
- Every message carries an `UnknownFields` buffer. Unrecognised fields — and an
  unrecognised `oneof` payload tag — are retained verbatim and re-emitted after
  the known fields, so round-tripping is byte-preserving.
- The `.proto` remains the normative contract; the Rust structs mirror it field
  number for field number, and the codec is wire-compatible with any conforming
  protobuf implementation.
- No codegen step, no build dependency, no `protoc`.

## Consequences

- Third-party SDKs still only need a protobuf implementation and a socket
  (§4.3): the bytes on the wire are ordinary proto3.
- Plugin authors using the Rust SDK need neither `protoc` nor `prost`, which is
  what ADR-0004 wanted committed bindings for in the first place.
- The codec is our code, so it is our bug surface. It is small, total (no
  panics on hostile input) and directly tested with adversarial byte strings.
- Adding a field means editing the `.proto` and its Rust mirror together. A
  drift between the two is a review failure, not a compile error — the schema
  test suite pins the tags to catch it.

## Alternatives

- **`prost` + `protoc`.** Rejected: build-time toolchain requirement, and no
  unknown-field retention.
- **`prost` with hand-written derives.** Rejected: same lost-unknown-field
  behaviour, plus a dependency for what is a few hundred lines of varint code.
- **Replace the native transport with JSON lines** (as the M4 Python worker
  uses). Rejected for the native protocol by ADR-0004 on size and parse cost
  for 500k-item catalog transfers; the modern and legacy Python workers keep
  their separate JSON protocols.
