# CriKey plugin protocol

Versioned, transport-independent IPC schema for **native** plugin workers
(spec 16.3).

Modern Python plugin workers do NOT use this schema. They speak a
newline-delimited JSON protocol implemented in
`crates/crikey-python-host/src/protocol.rs`, which carries the same protocol
version number so the two cannot silently diverge, but shares none of the
message definitions below. If you are generating code from these files, you are
building a native plugin.

- `crikey/v1/*.proto` - the wire schema. Additive evolution only; unknown
  fields must round-trip.
- Transports: Windows named pipes, Unix domain sockets, stdio for development.
- Framing: length-delimited, `crikey-native-protocol::MAX_FRAME_BYTES` cap.

Every request carries a connection id, a request id, an optional query
generation and an optional deadline. Responses reference the request id.
