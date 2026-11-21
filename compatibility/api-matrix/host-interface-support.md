# Host-interface support matrix

This matrix covers CriKey's G01–G13 host-interface features, not Legacy API
symbols. The [Legacy matrix](README.md) and `matrix.toml` retain their closed
API-symbol schema; this document does not extend that schema.

**Acceptance boundary:** P0–P7 are accepted at
[PR22](https://github.com/yichenchong/CriKey/pull/22), checkpoint
`c269c7bcaa7f8a5485934f1b91dad13c1d0d8939`. P8/G12 is **in progress and not
runtime-accepted**. Accepted below means applicable isolated Linux
native-window scenarios and component/real-worker checks, not every desktop,
runtime or interaction. Platform evidence and exclusions follow the matrices.

## Host behavior and plugin requirements

Ordinary host affordances apply to admitted results without adding a plugin
callback. Callback-backed content and durable command operations have separate
requirements; a host-rendered control does not grant them.

| ID | Host-interface behavior | Plugin boundary and meaningful limits | Delivery |
| --- | --- | --- | --- |
| G01 | Right-hand preview; narrow-window detail/Back; separate scrolling and focus | Host-known description/provider/category needs no new callback. Deferred typed content requires `preview_v1` from its exact Native/Modern producer. No callback for Legacy/WASM/C ABI. Inert bounded markdown/pixels; links require deliberate activation and owner permission. | P1 accepted |
| G02 | Searchable structured Actions panel, sections/submenus, stable selection | Wraps existing admitted actions; executes exact owner/item/action, not a filtered index. Host actions are separate from plugin actions. A panel does not add new runtime callbacks or authorization. | P2 accepted |
| G03 | Bounded nested navigation and Back with query, selection, scope and logical scroll restoration | Host stack is shared across root, Settings, custom pages and standard routes (16 entries/8 MiB). Custom pages close before reopening with fresh identities; arbitrary document state is not restored. Standard plugin routes need `structured_view_v1`. | P0/P3, P6 routes accepted |
| G04 | Configurable empty-query home, ordered favorites and successful command recents | Durable `CommandRef` declarations only: Native/Modern `commands_v1` or the declared host command. Ordinary/cached results are not automatically commands. Missing owners remain manageable, not retargeted by label. Compact home remains default. | P4 accepted |
| G05 | Per-command aliases, global shortcut editor/recorder and direct invocation | Requires a declared command. Existing textual `[aliases]` remain separate. Typed targets open their independently supported schema. Saved desired bindings and actually held OS bindings are distinct; registration can fail. macOS global hotkeys are unavailable. | P5 accepted; platform limits below |
| G06 | Inline Text/Password/Dropdown arguments, validation and structured submission | Requires `typed_arguments_v1` and an admitted exact executable schema; not inferred from commands or metadata. Legacy free-form arguments are unchanged. No positional/shell reconstruction. Passwords do not enter restoration/history; no secure-erasure claim. | P6 accepted |
| G07 | Grouped results, scope dropdown and section navigation | Plugin sections/scopes require `result_presentation_v1`. Host grouping preserves authoritative provider order; headers are not executable. Scope changes retire old generations; unsupported providers gain no scope callback. | P6 accepted |
| G08 | Shared native list/grid/detail/form routes, spatial grid navigation and Actions | Native/Modern `structured_view_v1` supplies standard descriptors and route operations. Typed form submission and other optional operations still require their own capabilities. Not a display-list wrapper, web engine or new Legacy/WASM/C-ABI API. | P6 accepted |
| G09 | Configurable logical resume and bounded submitted-query recall | Immediate reset is default; delayed snapshots are memory-only, expire monotonically and issue fresh work. Recall defaults to session-only, with Off/Clear and opt-in persistence. Legacy/free-form admissions are conservatively excluded from recall; argument values are excluded. Not ranking history or arbitrary page/process persistence. | P3 accepted |
| G10 | In-context command configuration, favorite/order, alias/hotkey, visibility and exact ranking reset | Durable-command controls require explicit declarations; ordinary result actions remain available without them. Configure command navigates to exact host preferences, not the guest callback. Hiding is discovery policy, not authorization. Reset affects the verified exact item's ranking, not all history. | P2/P4/P5; P7 deep-link accepted |
| G11 | Searchable categorized settings and graphical plugin/package management | Shared schema/validation/layer provenance, not a second settings registry. Native/Modern live configuration has same-process Linux evidence; do not infer guest delivery for other ABIs. Package jobs reuse installer/signature/lock policy, report cancellation/restart limits and outlive closing Settings. Install/remove permanently stops third-party runtimes for that launcher session. | P7 accepted; package evidence limits below |
| G12 | Theme, bounded color overrides, interface size/density and navigation preferences | P8 implementation/integration is active. User layout size is not OS DPI. System-theme fallback, focus/input ownership, shortcut conflicts, custom-page palette and genuine web-document color-scheme support require distinct proof. Existing rounded-corner capability checks still apply. | **P8 in progress; no P8 runtime acceptance yet** |
| G13 | Ordered configurable fallback actions | Declared commands accepting literal free-form query arguments only; typed-schema/argument-forbidden targets are ineligible. All applicable ordinary providers must finish successfully with no matches. Pending/failed/cancelled searches block fallbacks. Explicit selection passes the captured query as one argument, not shell syntax or automatic execution. | P4 accepted |

## Independent runtime capabilities

Each column below is independently negotiated. None implies another: not
`preview_v1` → views, `commands_v1` → typed fields, or structured routes → typed
submission. A declaration/descriptor must also pass current owner, target,
schema, generation and permission checks. Metadata, labels, cached item IDs and
transport reuse cannot substitute for runtime support.

**Optional** means implemented only when that worker negotiates the named
capability, not enabled for every plugin using that runtime. **No** means no
exposed runtime callback/contract for this feature, not that ordinary results
cannot use host chrome.

| Runtime/source | `preview_v1` | `commands_v1` | `typed_arguments_v1` | `result_presentation_v1` | `structured_view_v1` |
| --- | --- | --- | --- | --- | --- |
| Native / Rust SDK | Optional | Optional | Optional | Optional | Optional |
| Modern Python / Python SDK | Optional | Optional | Optional | Optional | Optional |
| Older Native/Modern worker | Only if negotiated | Only if negotiated | Only if negotiated | Only if negotiated | Only if negotiated |
| Legacy Python | No | No | No | No | No |
| WASM guest ABI | No | No | No | No | No |
| Restricted C ABI | No | No | No | No | No |
| Remote catalog source | No worker callback | No declaration intake | No | No | No |
| Builtin/host providers | Host-known content, not negotiation | Only `OpenSettings` declared | No general plugin contract | No general plugin contract | No general plugin contract |

- Native uses protobuf/Rust SDK; Modern Python uses a separate bounded JSON/SDK
  contract. Both retain ordinary operations when optional capabilities are
  absent. Unsupported new operations refuse explicitly rather than fabricate
  callbacks or silently flatten typed input.
- Legacy keeps its existing callbacks, scheduling, ordering and argument
  semantics. Host previews/actions/navigation do not mean every Legacy callback
  supports host-new capabilities.
- WASM and restricted C-ABI adapters speak the native envelope but expose no
  guest/ABI entry points for these five capabilities. This transport relationship
  does not inherit Native's runtime API. In particular, the WASM guest ABI has
  no configuration entry point; host publication is not guest consumption.
- Remote sources publish bounded ordinary catalog slices through the shared
  catalog path, not executable worker command/view declarations. Builtin search,
  file/application/calculator results likewise do not become durable commands
  merely because they are cached or displayed. The sole declared host command
  is `crikey.host` / `open-settings` (`HostCommand::OpenSettings`).

## Platform, evidence and security boundaries

- [P7 native-platform CI](https://github.com/yichenchong/CriKey/actions/runs/44919120216)
  passed Linux/macOS/Windows Rust suites, lint, documentation and conformance
  checks at the accepted checkpoint. [P6 CI](https://github.com/yichenchong/CriKey/actions/runs/44899764023)
  also covers native platforms and Python SDK. Compilation/CI and off-target
  checks are not interactive macOS/Windows GUI acceptance.
- Actual interface evidence is from private Linux native windows on Xvfb,
  including Native/Modern previews, exact action dispatch, navigation/resume,
  favorites/fallbacks, physical X11 command shortcuts, typed validation/privacy,
  six-cell grid navigation/detail/forms, and settings/package flows. This is not
  real-desktop compositor, high-DPI, broad live-IME, screen-reader or non-Linux
  GUI proof. P8 appearance/remapping has no runtime acceptance yet.
- Global hotkey implementations use X11, the Wayland GlobalShortcuts portal or
  Windows registration. Desktop policy/chord availability can refuse them.
  Actual X11 activation is narrower evidence than Wayland/Windows runtime
  acceptance; the macOS backend reports global hotkeys **unavailable**.
- Custom display lists are plugin-positioned host-drawn primitives, not a DOM
  or embedded browser. Real WPE WebKit HTTP/input and engine teardown were
  exercised separately on Linux. Web surfaces require an engine-enabled
  `crikey-web-host`; standard no-web packaging and the current Flatpak recipe
  do not stage it. Windows/macOS have no web-surface host in v1. Display-list or
  host-chrome rendering is not proof of document theming, engine support or live
  web-at-package-quiescence; the latter remains unproved.
- P7 actual package evidence includes install/remove, same-ID runtime inventory,
  no respawn after global stop, a real busy Modern catalog producer, cancellation,
  failed-closed stop refusal, Quit and competing-lock exclusion. It does not
  establish GUI proof of every internal epoch/admission race, simultaneous
  resource/preview work, initially partial descendant ownership, commit-phase
  NotCancellable or rollback/reload-failure reconciliation. Backend/process
  consumers are separate evidence, not renamed GUI observations.
- **AccessKit remains disabled** in the UI dependency configuration. Keyboard
  controls and visible focus are not an accessibility-delivery or screen-reader
  support claim.
- Typed-argument passwords are masked live values, excluded from nonsecret
  snapshots, history, configuration and host diagnostics. Secret configuration
  fields are separate: explicit schema-owned saves persist them to their
  designated configuration layer. Typed submission makes the physical worker
  diagnostically sensitive until exit. Buffer filling is best effort; Rust/Python
  runtime copies, crash dumps and secure erasure are not guaranteed. This is not
  a plugin sandbox; deliberately performed file/network I/O is not prevented.

## Source and contract locations

Paths below are repository sources; the guides describe author/user contracts,
not additional acceptance evidence.

| Area | Authoritative locations |
| --- | --- |
| Host UI/state and rendering | [`crikey-ui/src`](../../crates/crikey-ui/src/): `lib.rs`, `native.rs`, `action_panel.rs`, `root_presentation.rs`, `standard_view.rs`, `typed_input.rs`, `appearance.rs`, `navigation.rs` |
| Typed content/identity contracts | [`crikey-core/src`](../../crates/crikey-core/src/): `preview.rs`, `command.rs`, `structured_arguments.rs`, `result_presentation.rs`, `structured_view.rs`, `content_accounting.rs` |
| Runtime wire/SDK contracts | [`plugin.proto`](../../sdk/protocol/crikey/v1/plugin.proto), [`native protocol`](../../crates/crikey-native-protocol/src/), [`Python host`](../../crates/crikey-python-host/), [`Rust SDK`](../../sdk/rust/), [`Python SDK`](../../sdk/python/) |
| Host admission, persistence and jobs | [`crikey-app/src`](../../crates/crikey-app/src/): `plugin_preview.rs`, `plugin_action.rs`, `standard_view.rs`, `command_registry.rs`, `command_preferences.rs`, `command_hotkeys.rs`, `query_history_store.rs`, `launcher_settings.rs`, `settings_search.rs`, `package_jobs.rs`, `runtime_control.rs`; [`configuration`](../../crates/crikey-config/src/) |
| Adapter/source limits | [`WASM adapter`](../../crates/crikey-wasm-host/src/plugin.rs), [`C-ABI adapter`](../../crates/crikey-cabi-host/src/plugin.rs), [`remote catalogs`](../../crates/crikey-app/src/remote_catalog.rs), [`macOS capability reporting`](../../crates/crikey-platform-macos/src/lib.rs) |
| Engine and accessibility configuration | [`web host`](../../crates/crikey-web-host/), [`UI manifest`](../../crates/crikey-ui/Cargo.toml), [`workspace dependency flags`](../../Cargo.toml) |
| Consumer documentation | [User guide](../../docs/user-guide.md), [plugin author guide](../../docs/plugin-author-guide.md) |
