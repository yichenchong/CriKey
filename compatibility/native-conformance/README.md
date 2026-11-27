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
|  | `gated-ignore-cancel` | Appends each callback's query to working-directory `conformance-gate-entered`, then ignores cancellation until `conformance-gate-release` contains exactly that query. Returns a fresh `(gated)` row; a 60-second watchdog only bounds failed tests. |
|  | `crash-on-suggest` | Aborts inside `suggest`. |
|  | `exit-on-suggest` | Exits with status 17 inside `suggest`, without a terminal response or SDK finalization. Isolates completed-crash ledger tests from OS abort-reporting latency. |
|  | `exit-with-held-streams` | Starts a real inherited-stdio descendant and exits with status 17. Requires `CRIKEY_CONFORMANCE_STREAM_HOLDER_PID` for its actual PID witness; the regression uses an isolated known-PID reaper. Host containment must settle the descendant and its pipes, and delayed EOF must not relabel the exited leader as a callback timeout. |
|  | `crash-on-start` | Aborts from `start`. |
|  | `fail-suggest` | Returns a plugin error from `suggest`. |
|  | `sequence` | Uses `CRIKEY_SEQUENCE_FILE` as a launch-count file: the first and third processes abort in `suggest`, while the second behaves like `echo`. |
|  | `icon` | Suggests one item per icon outcome and serves their references: `served.svg` immediately, `absent.svg` not at all, `oversized.svg` above any host limit, `late.svg` after 400 ms. |
|  | `icon-slow` | Suggests one item whose `after-window.svg` reference is served after 150 ms — past a 100 ms result collection window, inside a 250 ms icon deadline. |
|  | `icon-silent` | Suggests one item whose `never.svg` reference is withheld for ten seconds, so no host icon deadline can ever receive it. |
| `crikey-misbehaving-plugin` | `oversized` | Writes a frame length above `MAX_FRAME_BYTES`. |
|  | `flood` | Sends result batches without respecting host credits. |
|  | `stderr-flood` | Writes several MiB to standard error before sending a final result. |
|  | `partial-no-terminal` | Sends one partial result batch and never sends a terminal batch. |
|  | `log-flood` | After the handshake and a suggest request, continuously sends large log records without consuming result credit. |
|  | `control-witness` | Triggers truncation, records Cancel/FlowControl/Shutdown arrival order, and reports the order in terminal result items. |
|  | `bad-version:<v>` | Handshakes with protocol version `v`. |
|  | `bad-token` | Handshakes with a deliberately wrong session token. |
|  | `hang` | Completes the handshake and then never answers requests. |

Shutdown tests may set `CRIKEY_CONFORMANCE_STOP_FILE` to a private witness path.
The real SDK stop callback writes `started`, then `finished`; optional
`CRIKEY_CONFORMANCE_STOP_DELAY_MS` delays completion,
`CRIKEY_CONFORMANCE_STOP_RELEASE_FILE` holds the callback until a release file exists, and
`CRIKEY_CONFORMANCE_CRASH_ON_STOP` exits from that callback. These witnesses
distinguish graceful finalization, bounded forced cleanup, and loss of control
during checked shutdown.

The acceptance helper builds this workspace with:

```text
cargo build --manifest-path compatibility/native-conformance/Cargo.toml \
  --target-dir target/native-conformance
```

The `acceptance` mode explicitly negotiates `commands_v1` and declares
`conformance-one` targeting stable catalog item `catalog-1`. Other modes retain
unsupported behavior. `out_of_tree_commands_use_the_catalog_originating_worker`
checks that a replacement process cannot declare against an earlier catalog.
The SDK's `commands` example supplies stateful real-worker validation,
unsupported, cancellation and malformed/oversized declaration scenarios.
Raw `crikey-misbehaving-plugin` modes `commands-invalid`, `commands-oversized`,
`commands-count` and `commands-aggregate` bypass SDK producer validation to
exercise actual host decode/semantic refusals over the supervised channel.
