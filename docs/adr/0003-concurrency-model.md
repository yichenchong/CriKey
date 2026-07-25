# ADR-0003: Threading and async model

Status: Accepted
Spec: §5.1, §6.5, §8, §12.4

## Context

The UI thread must never block. Plugin I/O, process supervision, filesystem
watching and package management are I/O bound. Matching and ranking are CPU
bound. Scheduling decisions are neither — they are pure state machines that must
be trivially testable.

## Decision

- **UI thread**: the `winit` event loop. It renders a prepared `ViewModel` and
  sends commands. It never awaits, never locks a mutex held by plugin work.
- **Scheduler**: pure decision functions (`Debouncer`, `ObsoleteWorkManager`)
  that take an explicit `Millis` timestamp and return a `Dispatch`. Timers and
  channels live outside them. No wall-clock reads inside the logic, so tests are
  deterministic.
- **Async runtime**: one multi-threaded `tokio` runtime owns IPC, process
  supervision, watchers and package management.
- **CPU pool**: a bounded worker pool for matching, ranking and catalog builds.
  Sized to available parallelism, never unbounded.
- **Channels**: every inter-stage channel is bounded and carries a documented
  overflow policy. Unbounded channels are prohibited.

## Consequences

- Scheduling behaviour is unit-testable without sleeping, which is what makes
  §27.1's debounce and obsolete-work requirements testable at all.
- Timestamp injection is a small ergonomic cost at call sites, paid once.
- Two runtimes (async + CPU pool) require care that CPU work never runs on an
  async worker; enforced by keeping the pool's API synchronous.

## Alternatives

- **`async` everywhere including the UI.** Blurs the non-blocking guarantee and
  makes accidental awaits on the render path easy.
- **Thread-per-plugin, no async runtime.** Simpler mental model, but hundreds of
  installed plugins (§27.3) makes thread-per-connection wasteful.
- **Reading `Instant::now()` inside the scheduler.** Rejected: makes every
  scheduling test timing-dependent and flaky in CI.
