# ADR-0006: Legacy scheduling uses obsolete-work replacement

Status: Accepted
Spec: §3.6, §7.1, §8.4, §8.8, §14.5, §25.3, §25.4

## Context

Debouncing is the obvious way to protect slow plugins from rapid typing. Applied
to a legacy Keypirinha plugin it is also a behaviour change: the documented
legacy contract is prompt dispatch, broadcast of the initial suggestion request
to all loaded plugins, serialized callbacks, and cooperative termination through
`should_terminate()`. A plugin written against that contract can legitimately
depend on receiving a callback promptly.

## Decision

`legacy-strict` is the default for unchanged legacy plugins and is defined by
what the host must *not* do:

- No time-based debounce. No host-imposed minimum query length. No prefix or
  keyword gating. No dynamic suggestion caching.
- Dispatch promptly when the plugin instance is idle.
- When a newer query arrives while a callback runs: flip `should_terminate()`
  for the running work, keep exactly one pending request (the newest), discard
  older undispatched ones, and dispatch the pending request only after the
  current callback returns.
- No two lifecycle callbacks ever run concurrently on one plugin instance.
- Reject stale results at the aggregator regardless of plugin cooperation.
- No modern hard-kill deadline. Soft-latency warnings, cooperative termination
  and a much longer hung-worker watchdog instead; forced restart is recovery only.

`legacy-optimized` exists for users who accept behaviour change, and is never
enabled by default for an unchanged plugin.

This is implemented as `ObsoleteWorkManager` in `crikey-input-scheduler`, and
`SchedulingProfile::allows_time_debounce()` returns `false` for `LegacyStrict`.

## Consequences

- Rapid typing against a slow legacy plugin produces at most one running and one
  pending callback — bounded without altering semantics.
- A legacy plugin that never checks `should_terminate()` still cannot corrupt the
  result list; it only wastes its own worker time.
- Two scheduling code paths must be maintained. That cost is accepted: unifying
  them is precisely the non-goal in §2.3.

## Alternatives

- **Debounce everything uniformly.** Simpler, and breaks the documented legacy
  contract — explicitly prohibited by §8.4.
- **Queue every legacy query.** Preserves delivery, creates the unbounded
  obsolete-work queue that §6.5 forbids.
