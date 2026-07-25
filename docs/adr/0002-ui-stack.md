# ADR-0002: UI stack

Status: Accepted — revisit if warm activation misses 30 ms p95
Spec: §6, §25.1, §25.5

## Context

The launcher must show a window within 30 ms p95 of the hotkey, render query
text in the next frame, never block on plugin work, keep idle memory under
100 MiB, and eventually run on Windows, macOS and Linux. It is a single
keyboard-driven window with a list — not an application with rich chrome.

## Decision

`winit` for windowing and input, `wgpu` for the surface, and an immediate-mode
widget layer (`egui`) for the list and query field. All of it sits behind the
`LauncherWindow` / `ViewModel` contracts in `crikey-ui`, so the renderer is
replaceable without touching any other crate.

Warm activation is achieved by creating the window and its GPU surface at
startup and keeping it alive but hidden; the hotkey path shows an existing
surface rather than constructing one.

## Consequences

- Consistent rendering across platforms, one code path for the result list.
- Immediate mode suits a view model rebuilt per generation: there is no retained
  widget tree to diff or to leak stale-generation state.
- GPU driver variance becomes a support surface; a software fallback path will
  be needed for VMs and remote sessions.
- Native accessibility is not free. Keyboard-only operation is the primary
  contract; screen-reader support is a tracked follow-up, not an accident.

## Alternatives

- **Webview (Tauri/Electron).** Rejected: process and memory cost, startup
  latency, and IME/global-hotkey friction directly conflict with §25.1.
- **Native per-platform UI (Win32/AppKit/GTK).** Best platform fidelity, three
  implementations of the hot path, and the Windows-first schedule would stall on
  the other two.
- **`iced`.** Retained-mode alternative, viable; rejected for now because the
  per-generation view model maps more directly onto immediate mode.
