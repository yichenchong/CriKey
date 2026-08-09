# ADR-0015: Restricted C-ABI plugins in a dedicated host

Status: Accepted
Spec: §2.2, §2.3, §16.1, §16.6, §16.7; acceptance criteria 30 and 31

## Context

Spec §2.2 lists “restricted in-process native plugins”; §2.3 forbids ABI
compatibility with arbitrary Rust dynamic libraries; acceptance criterion 30
forbids the main process loading arbitrary third-party native libraries. These
hold together only when “in-process” means inside a dedicated plugin host, not
inside CriKey’s launcher/UI process. A C library can still crash, hang, leak,
spawn threads and use all authority of its host: supervision is containment,
not a pretend capability boundary.
## Decision

- `crikey-cabi-host` is a supervised native-protocol executable. CriKey starts one per installed `c-abi` package; only the host, never the launcher, dlopens the library. It is in-process for the host and out-of-process for CriKey.
- The host accepts only an installed package directory. The manifest supplies a relative entrypoint; absolute paths, `..`, symlinks, escapes and query paths are refused. The selected member is rechecked against `crikey-package.lock`.
- The header defines ABI version 1, an exported data version symbol, init/shutdown, suggest/execute and matching batch-free. Strings are bounded length-delimited UTF-8; ownership is explicit; no Rust values, panics or callbacks cross the boundary; calls are serialised and non-reentrant.
- The host reads the version symbol first, resolves every required symbol by name, then calls init. Counts, pointers and strings are validated before copying. Malformed batches are refused whole and poison the handle; successful batches always receive their matching plugin free call.
- Soft deadlines set cancellation. A C call still running at the hard deadline cannot be safely unwound, so the host aborts; the supervisor records a worker crash while sibling hosts remain available. `c-abi` reuses native archives, locks, installation, namespace and supervisor. The out-of-tree C fixture uses only the header and a strict Makefile and covers round trip, refusals, crash and hang modes.


## Consequences

Authors use a narrow, versioned ABI and an ordinary C toolchain. CriKey gets
existing protocol bounds, deadlines, restart policy and sibling isolation. The
host adds `libloading`; its unsafe C boundary is confined to this executable.
A C plugin has the host process’s filesystem, environment and other granted
authority. This protects the launcher/UI from faults, not the host from a
malicious plugin; claiming a sandbox would violate the honesty invariant.

## Alternatives rejected

- Load libraries in `crikey-app`: violates acceptance 30 and launcher safety.
- Load arbitrary Rust dynamic libraries: violates §2.3’s ABI prohibition.
- Pass Rust values or callbacks: ownership, reentrancy and unwind behaviour are
  unportable and unverifiable.
- Kill a thread at a deadline: C has no safe unwind/thread-kill contract; host
  abort is the only honest containment.
