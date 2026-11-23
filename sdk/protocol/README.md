# CriKey plugin protocol

Versioned, transport-independent IPC schema for **native** plugin workers
(spec 16.3).

Modern Python plugin workers do NOT use this schema. They speak a
newline-delimited JSON protocol implemented in
`crates/crikey-python-host/src/protocol.rs`, which shares none of the message
definitions below. It does reuse this protocol's version number, so a version
bump is visible on both sides, but that is a shared constant and not a
guarantee: the two message shapes can still drift apart, and only their
respective tests catch that. If you are generating code from these files, you
are building a native plugin.

- `crikey/v1/*.proto` - the wire schema. Additive evolution only; unknown
  fields must round-trip.
- Transports: Windows named pipes, Unix domain sockets, stdio for development.
- Framing: length-delimited, `crikey-native-protocol::MAX_FRAME_BYTES` cap.

Every request carries a connection id, a request id, an optional query
generation and an optional deadline. Responses reference the request id.

## Explicit executable commands

Native `commands_v1` is opt-in and independently negotiated from previews.
Envelope tags 45/46 carry a distinct `CommandsRequest`/`CommandsResponse`.
The request uses the existing connection/request/generation identity, call
deadline, cancellation handle and supervised worker; it is not a second worker
or scheduler. Build the complete catalog on that worker first. Missing
capability returns unsupported, not successful empty declarations.

Rust plugins override `supports_commands` and `commands`, returning
`PluginCommands::Declarations`. For example,
`CommandBuilder::new("open-report", "Open report", "report").action("open").build()`
declares the exact stable catalog item/action, without provider identity,
availability or any host target. See `sdk/rust/examples/commands.rs`.
WASM and C ABI adapters retain the default unsupported callback; this addition
changes no guest ABI.

Responses are refused whole for malformed identities/policies, duplicate command
IDs, more than 10,000 declarations, identity fields over 256 UTF-8 bytes, labels
over 8 KiB, or aggregate declaration strings over 4 MiB. Aggregate arithmetic
is checked. Existing frame and decode-allocation ceilings remain unchanged;
unknown fields still round-trip without granting authority.

The native Rust callback is synchronous inside the worker. It must poll the
request context's cancellation token during expensive work. The SDK checks
cancellation and elapsed deadline after the callback returns; it does not
preempt callback code. Host transport timeout and worker supervision provide
separate safeguards.

Provider/registry integration preserves these authority boundaries:

1. Request declarations before releasing the catalog-originating worker. Capture
   the exact installed owner, catalog revision and producer connection; reject
   replies from replaced owners/catalogs/connections.
2. Validate every target against the current catalog of that exact owner,
   including the optional action ID and permitted argument policy. Neither
   labels, metadata nor an unqualified item ID establish a command.
3. Only then bind the installed owner and host-computed availability using the
   neutral Core declaration. Publish via the registry's owner replacement path;
   retain unavailable preference records without retargeting.
4. A healthy idle process release does not invalidate installed catalog
   commands. Package/catalog lifecycle controls availability. In-flight
   declaration replies require their original connection; deferred previews
   separately require their original producer to remain live.
5. Resolve again at invocation through canonical catalog command admission on
   the existing owner-qualified action router, including action/category/
   execution-policy and permission checks. Do not substitute a live suggestion
   with the same item ID. A stored preference never authorizes execution.

Modern Python independently opts in by overriding sync/async `Plugin.commands()`
and returning SDK `Command` declarations. Its separate JSON bridge uses
`commands`/`commands_result`; it does not use envelope tags 45/46. Legacy, WASM
and restricted C-ABI runtimes have no command callback added by this capability.

Explicit declarations participate in search, favorites/home, exact successful
command recents, hidden state, ordered fallbacks and item-ranking reset through
one dispatcher. **Settings → Configure Commands** includes declared commands and
all saved preference references, including hidden and unavailable nonfavorites.
The exact installed-owner/command-ID pair remains identity even when labels or
targets coincide; unavailable references never acquire replacement targets.
Favorite and fallback removal change only their respective memberships. Ranking
reset affects the current verified owner/item, shared by commands targeting it,
without clearing preferences, command recents or query recall.

Controls submit revision-checked semantic edits to the user-global layer,
preserving unrelated fields there rather than copying effective higher-layer
values. Profile/session overrides may mask a saved edit. Conflicts require
refresh/retry; failed persistence does not publish the proposed edit.

Full CommandRef row identity is separate from original owner/item/catalog-source
preview provenance. Deferred preview and link permissions remain with that
source; command availability does not promise a live preview producer. Only
`OpenSettings` is declared as a host command. Command-specific alias/global
shortcut controls and structured multi-field arguments are not supplied by
this capability.

## Root typed arguments and presentation

Protocol version 1 adds independent optional `typed_arguments_v1` and
`result_presentation_v1` capabilities. Envelope tags 47/48 were checked against
the complete existing 10–46 assignment set before allocation. Catalog/result
field 5 carries bounded presentation sidecars; suggest field 6 carries an
optional selected scope ID. No standard routes or navigation outcomes are
introduced.

Sidecars declare sections/scopes, per-item memberships and one schema per exact
item/optional-action executable. Endpoint ownership is host-supplied. Absence
of the action field is genuine actionless execution; a present empty ID is
invalid. Metadata accumulates with the complete catalog or exact suggestion
generation and grants no authority until all references and duplicates pass
transactional validation. Presentation-only negotiation cannot authorize typed
execution; typed-only negotiation can carry schemas without visual metadata.

Schemas allow text, password and dropdown fields, with at most 64 fields,
256 options per dropdown, 256-byte IDs, 8 KiB UTF-8 strings/values and a 256 KiB
schema allocation footprint. Password defaults are forbidden. Optional strings
preserve absent versus present-empty; omitted limits normalize to min 0/max
8192. Decoders share the existing frame allocation budget, tighten it for each
schema before allocation and accumulate nested fragments/counts/unknown bytes.
Producer preflight accounts for sidecars and rows before serialization.
Empty optional text/password values (including text defaults) are valid even
when the minimum is positive; nonempty values still obey both byte bounds.
Required fields reject empty values.

Typed requests contain item ID, explicit optional-action presence, exactly
32 fingerprint bytes and typed field values, never a legacy argument.
The fingerprint is Core's canonical v1 SHA-256 structural binding, including
ordered fields/options and all properties but excluding unknown wire fields.
The host confirms that binding against the actual physical worker's complete
metadata before values enter its writer queue. A fresh static worker bootstraps
bounded catalog discovery; dynamic suggestion authority is never regenerated.
The SDK independently checks its current binding. Mismatches are fixed
refusals, not fallback to legacy execution.

Responses are Completed, unique current-field failures with fixed codes, or a
fixed failure code. They carry no callback error message or navigation outcome.
Legacy SDK dispatch refuses schema-only targets even when an older host cannot
consume their metadata. Existing catalog/suggestion callbacks remain compatible
through optional metadata callbacks.
Recoverable presentation callback errors and panics finish the current result
stream with its ordinary failure terminal, without closing the connection or
publishing schema authority. Typed callbacks use the request context's
cooperative cancellation token; cancellation does not require a host timeout.

Every typed submission makes its physical incarnation diagnostically sensitive
before writer admission. The transition clears retained diagnostics; later
plugin logs, stderr, callback errors, panic payloads and raw diagnostic excerpts
are suppressed for that process's remaining lifetime, including background
callbacks. Diagnostic-aware decoding validates/budgets discarded text without
copying it. Fixed lifecycle/error codes still reach the host. This is not a
sandbox against deliberate plugin I/O, nor a cryptographic memory-erasure
guarantee. Live values are move-only and are not query history or snapshots.
