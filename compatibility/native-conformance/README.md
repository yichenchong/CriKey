# Native conformance fixture

This directory is a standalone Cargo workspace so the acceptance suite builds a
native plugin the way an out-of-tree SDK consumer does. The fixture binaries
use the endpoint and session token supplied in `CRIKEY_PLUGIN_ENDPOINT` and
`CRIKEY_SESSION_TOKEN`.

Mode selection is identical in both binaries: `CRIKEY_CONFORMANCE_MODE`, then
`argv[1]`, then trimmed `conformance-mode` in the working directory, then
`echo`.

| Binary | Mode | Observable behaviour |
| --- | --- | --- |
| `crikey-conformance-plugin` | `echo` | Three-item catalog; two partial suggestion batches and a final batch. |
|  | `same-id` | Behaves like `echo` but always reports the fixed handshake id `shared.identity`, ignoring `CRIKEY_PLUGIN_ID`. |
|  | `slow-witness:<ms>` | Emits a `slow-start` partial item immediately, then polls cancellation for `<ms>` and finishes cancelled or final. |
|  | `env-witness` | Catalog and suggestions contain every visible environment variable as `env:<NAME>`, with the value as the target. |
|  | `acceptance` | A query beginning with `slow` waits about two seconds and cooperatively cancels; every other query streams 35 items across multiple batches. |
|  | `stream:<n>` | Catalog and suggestions emit `n` items in batches of 16 and finish the stream. |
|  | `slow:<ms>` | Polls cooperative cancellation while waiting, then returns cancelled or a final result. |
|  | `ignore-cancel:<ms>` | Never checks cancellation and returns only after the delay. |
|  | `crash-on-suggest` | Aborts inside `suggest`. |
|  | `crash-on-start` | Aborts from `start`. |
|  | `fail-suggest` | Returns a plugin error from `suggest`. |
|  | `sequence` | Uses `CRIKEY_SEQUENCE_FILE` as a launch-count file: the first and third processes abort in `suggest`, while the second behaves like `echo`. |
| `crikey-misbehaving-plugin` | `oversized` | Writes a frame length above `MAX_FRAME_BYTES`. |
|  | `flood` | Sends result batches without respecting host credits. |
|  | `stderr-flood` | Writes several MiB to standard error before sending a final result. |
|  | `partial-no-terminal` | Sends one partial result batch and never sends a terminal batch. |
|  | `log-flood` | After the handshake and a suggest request, continuously sends large log records without consuming result credit. |
|  | `control-witness` | Triggers truncation, records Cancel/FlowControl/Shutdown arrival order, and reports the order in terminal result items. |
|  | `bad-version:<v>` | Handshakes with protocol version `v`. |
|  | `bad-token` | Handshakes with a deliberately wrong session token. |
|  | `hang` | Completes the handshake and then never answers requests. |

The acceptance helper builds this workspace with:

```text
cargo build --manifest-path compatibility/native-conformance/Cargo.toml \
  --target-dir target/native-conformance
```
