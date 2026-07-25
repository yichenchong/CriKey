# CriKey plugin protocol

Versioned, transport-independent IPC schema shared by native and modern Python
plugin workers (spec 16.3).

- `crikey/v1/*.proto` - the wire schema. Additive evolution only; unknown
  fields must round-trip.
- Transports: Windows named pipes, Unix domain sockets, stdio for development.
- Framing: length-delimited, `crikey-native-protocol::MAX_FRAME_BYTES` cap.

Every request carries a connection id, a request id, an optional query
generation and an optional deadline. Responses reference the request id.
