# ADR-0003: Threading and async model

Status: Accepted
Spec: §5.1, §6.5, §8, §12.4

## Context

The UI thread must never block on plugin work. Plugin IPC and process
supervision are blocking operations in the current implementation. Matching and
ranking are small Rust operations on the local search path, while provider
catalog builds and native per-plugin calls may use dedicated worker threads.
Scheduling decisions are deterministic state machines that take explicit
timestamps.

## Decision

- **UI event loop**: the `winit` event loop. It handles input, immediate local
  search, and presentation. It never waits for plugin I/O or a child process.
- **Provider drivers**: `LegacyDriver`, `ModernDriver`, and `NativeDriver` each
  own a `QueryPipeline` on a dedicated supervisor thread. The pipeline owns
  generation tracking, debounce or obsolete-work decisions, bounded intake,
  aggregation, and cancellation bookkeeping.
- **Worker and dispatch threads**: blocking child-process calls stay off the UI
  thread. Modern/native catalog builds and native per-plugin calls use
  dedicated threads where the provider needs parallel work; shutdown joins or
  cancels them.
- **Scheduling state**: `Debouncer` and `ObsoleteWorkManager` receive an
  explicit `Millis` timestamp. Timers, condition variables, and channels live
  outside the state machines, so scheduling tests do not read the wall clock.
- **Channels**: every inter-stage channel is bounded and carries a documented
  overflow policy. Unbounded channels are prohibited.

## Consequences

- Scheduling behaviour is unit-testable without sleeping, which is what makes
  §27.1's debounce and obsolete-work requirements testable at all.
- Timestamp injection is a small ergonomic cost at call sites, paid once.
- Dedicated supervisor and dispatch threads keep blocking child work off the UI
  path, at the cost of explicit mailbox, cancellation, and shutdown-join
  bookkeeping.

## Alternatives

- **`async` everywhere including the UI.** Blurs the non-blocking guarantee and
  makes accidental awaits on the render path easy.
- **Thread-per-plugin, no async runtime.** Simpler mental model, but hundreds of
  installed plugins (§27.3) makes thread-per-connection wasteful.
- **Reading `Instant::now()` inside the scheduler.** Rejected: makes every
  scheduling test timing-dependent and flaky in CI.
