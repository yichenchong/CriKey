# PR #8 audit — PR8R5

**Completed PR-scoped audit.** Four in-scope issues were fixed and verified: one runtime ownership defect and three documentation inconsistencies. Three inherited observations are recorded separately, including an additional strict documentation-generation failure outside the PR. No audit proves absence of every possible bug.

## Scope, starting point and accounting

This is the fifth fresh review of **pull request #8 only**, starting at `160f25abeb9148a26a9a33e1740099c15c9da907`, relative to `feature/page-image-canvas`. The initial working tree was clean. The PR changed 53 files: 49 implementation, test, build, schema and public-documentation paths, plus four historical audit reports. It was not a whole-repository audit. Surrounding files were read only where needed to follow the PR's producers, consumers and contracts.

Eight owners reviewed separate sections: engine/runtime; native adapter; engine-independent host helpers; launcher/application supervision; UI; core/protocol/permissions/Rust SDK; packaging/build integration; and public documentation. All eight section reports were used for this report. Owners made no mid-flight builds, tests, linter, formatter or runtime executions. Python was restricted to `/home/yichen/projects/crikey/.venv/bin/python` (3.14.4); the coordinator forces that virtual environment first on PATH for fixtures. All explicit Python commands used the repository virtual environment; this is not a change to product interpreter-selection policy. No LSP was configured.

**There are four confirmed entries: one runtime defect and three documentation corrections.**

1. **PR8R5-ENGINE-1:** the safe public engine runtime can dispatch a creator-thread engine call from a foreign GLib default-context owner; actual baseline-matching control/probe confirmed it.
2. **PR8R5-contracts-1:** core API documentation still generalized display-list behavior to web pages, including generic redraw comments.
3. **PR8R5-DOCS-1:** a separate rejected-webview paragraph still presented shared textures as necessary for offscreen rendering.
4. **PR8R5-DOCS-2:** the PR-rewritten display-list/web specification paragraph retained an inherited false framebuffer prohibition.

ENGINE-1’s native ownership reservation, error cleanup, public-run documentation and permanent native regression are implemented. The regression failed on isolated starting source and passed on fixed source; the public consumer probes, native failure-cleanup checks, workspace gates and actual-engine suites also passed. The other three entries are documentation-only; no runtime behavior or new permanent test is attributed to them. No repository schema, permission, dependency or renderer change was made. Review-status labels in local section artifacts are not extra counted defects.

Three inherited, outside-PR observations have their own labels: OBS-1, OBS-2 and OBS-4. OBS-1 is source-inferred and unconfirmed at runtime; OBS-2 is an untouched historical statement; OBS-4 is an observed strict rustdoc failure. None was silently fixed outside scope. ENGINE-1 is a confirmed safe-public-API defect with a foreign context owner, not an observed ordinary standalone-host trigger.

Historical `docs/notes/pr8-bughunt-PR8B.md`, `pr8-bughunt-PR8R2.md`, `pr8-bughunt-PR8R3.md` and `pr8-bughunt-PR8R4.md` were read-only. Existing wake delivery, activation cleanup, package artifact selection, native load handling, touch routing, storage and protocol fixes were reviewed but are not counted again.

## PR8R5-ENGINE-1 — Safe public runtime permits foreign-thread engine dispatch

**Kind:** Confirmed latent public-API runtime/creator-thread ownership defect. Fixed, with failing-before/passing-after regression and actual consumer proof.

### Context, cause and consumer impact

The safe public `crikey_web_host::engine::run` API initializes a thread-affine WPE surface. The native loop and fd/signal sources use GLib's process-global default context. A single-initialization guard prevents repeated engine initialization but does not reserve that context for the engine creator's lifetime. A different application thread using GLib's safe high-level context APIs can own and iterate that context and dispatch the runtime's queued engine commands.

The coordinator executed the prepared consumer against source matched to the starting baseline hash, in the actual WPE container with isolated build output. This is no longer inferred from [GLib context-acquisition semantics](https://docs.gtk.org/glib/method.MainContext.acquire.html): the observed resize boundary ran on a foreign thread. The probe’s high-level `glib = "0.20"` dependency was confined to an external consumer project; no repository manifest or lockfile gained that dependency.

### Exact observed before-fix evidence

- **Same-thread control: exit 0.** `RESIZE_BOUNDARY` recorded creator `ThreadId(1)` and actual `ThreadId(1)`; the host reported `Gone(Closed)` and `run` returned `Ok`.
- **Foreign-context consumer: exit 73.** The default-context owner was `ThreadId(5)`; the resize boundary recorded creator `ThreadId(1)` and actual `ThreadId(5)`, with `WRONG_THREAD_DISPATCH`.

The consumer uses safe `engine::run` and safe `glib::MainContext::{default,acquire,iteration}`. It sends actual framed Open, Resize and Close commands over a launcher socket while a localhost HTTP request keeps the document pending. Linker `--wrap=ck_web_surface_resize` instrumentation logs the actual Rust-to-native boundary and stops before delegating a foreign-thread C resize. The wrapper's unsafe instrumentation is not an unsafe GLib misuse by the consumer.

These results demonstrate a creator-thread contract violation reachable through the safe public API. They do **not** claim a native crash, observed memory corruption, a proved data race, or the same trigger in the ordinary standalone host binary, which has only its own GLib loop thread. The control establishes that the probe reaches the genuine resize boundary normally rather than treating a timeout or generic abort as proof.

### Fix, every changed native/Rust hunk and coverage

**`crates/crikey-web-host/csrc/ck_web_shim.c`:**

- Add retained default-context state `s_context`.
- `ck_web_init` obtains `g_main_context_default()` and attempts `g_main_context_acquire()` **before cleanup or any WPE initialization**. A foreign owner returns `CK_WEB_INIT_CONTEXT_BUSY (-3)` without changing existing sources/loop state or initializing WPE.
- Keep successful ownership across surface creation, command dispatch, loop execution and surface teardown; construct the loop explicitly on that context. GLib's recursive acquisition allows `g_main_loop_run` to take/release its nested ownership without releasing the lifetime reservation.
- Balance a same-thread repeated native initialization's new acquisition against cleanup of the prior reservation; there is no unowned interval introduced by cleanup.
- `ck_web_cleanup` removes fd/signal sources, unreferences the loop and clears quit state before releasing the retained context. Existing Rust shutdown destroys the surface first.
- Loader and SHM initialization failures call cleanup, releasing ownership rather than leaving the context reserved after a failed init.

**`crates/crikey-web-host/csrc/ck_web_shim.h`:** add the distinct private `CK_WEB_INIT_CONTEXT_BUSY (-3)` status and document creator/initialization-thread reservation, busy-before-mutation and cleanup ownership. The header introduction now limits protocol-enum mirroring to WebStorageMode/WebLoadCode/WebGoneCode and identifies CK_WEB_INIT_* as private non-wire initialization results. That introductory clarification belongs to this finding, not a separate documentation count.

**`crates/crikey-web-host/src/engine.rs`:**

- `run` documents creator-thread/default-context ownership for the call and refusal when a foreign thread owns the context. This accompanies native enforcement; it is not a documentation-only workaround.
- Preserve `RunError::EngineInit(i32)` for `-3`; no exported error variant, protocol result or Rust dependency is added.
- Extend `engine::tests::wpe_process_lifetime_and_native_load_failure_boundaries` with an isolated subprocess of its own test binary. The child starts a foreign context-owning thread, calls `EngineLoop::new`, requires `RunError::EngineInit(-3)`, and releases/joins the owner. The parent then performs its sole successful WPE initialization and continues the existing native load/callback/coalescing checks. This preserves the one-initialization constraint.

The tests-only starting-tree extraction is `local://pr8r5-engine-baseline.patch`. Initial hand-assembled artifacts had hunk/terminator and missing trailing-context errors; the coordinator mechanically regenerated the extraction from raw `git diff` test hunks, and `git apply` succeeded. Those artifact errors are not product findings or relaxed test expectations.

**Permanent regression failing-before — observed:** source `/root/crikey-pr8r5-before` contained starting commit `160f25abeb9148a26a9a33e1740099c15c9da907` plus only the tests patch; output was isolated at `/tmp/pr8r5-before-engine-target`. With a genuine foreign context owner, old initialization did not return private `EngineInit(-3)`. That assertion failed; child exit was 101 rather than the required success code 42, so the parent test failed and Cargo exited 101. Code 42 is emitted only after the child performs its ownership check; zero matching tests cannot satisfy it. The failure completed in 0.09 seconds, not by watchdog timeout. This independently supports the external exit-73 wrong-thread proof. The fixed permanent fixture passed in the actual-WPE suite: 78 library tests, 3 CLI tests and all 13 actual-host scenarios passed. `tests/engine_smoke.rs` remains unchanged.

No private-context migration, blocking wait/retry, source-routing rewrite, schema change or public error/protocol expansion was introduced. The fix immediately refuses a foreign owner rather than initializing thread-affine engine state on an externally dispatched context.

**Independent review:** R5ReviewContext approved the frozen C change and final Rust fixture without findings, including the child success code 42, ten-second watchdog and five-second channel bounds. R5ReviewDocs approved all three documentation corrections without findings. These are source reviews, not runtime passes.

**Failure cleanup probe — PASSED:** `local://pr8r5-native-init-failure.c` links the actual current shim while wrapping loader/SHM init to force false. Coordinator observed loader status `-1` and SHM status `-2`, ownership assertions inside the initializer wrappers, and successful foreign context acquisition after failure cleanup. This executes real C cleanup under forced initialization results, **not actual browser loads**, and does not substitute for actual-WPE control/smoke.

**Fixed public-consumer proof — PASSED both modes:** actual-WPE control reached resize creator `ThreadId(1)` / actual `ThreadId(1)`, reported `Gone(Closed)` and `run Ok`; safe foreign context reacquisition after cleanup also passed. Busy-owner mode returned `EngineInit(-3)`, emitted exactly one surface-7 `Gone(StartupFailed)`, invoked zero resize boundaries, and released/joined its foreign owner. The socket reported reset after the structured diagnostic because commands were already queued; the diagnostic was observed and was not lost. No native foreign-thread resize was delegated.

Probe artifacts: `local://pr8r5-engine-context-probe.rs` and `local://pr8r5-engine-context-probe.Cargo.toml`. This actual before-fix result is separate from the documentation-only core validation smoke.

## PR8R5-contracts-1 — Core page API documentation described every page as a display list

**Kind:** Public API-documentation correction. No runtime change.

**Changed repository file:** `crates/crikey-core/src/page.rs`.

### Context, cause and impact

The core module introduction still called a page a flat list of `PageNode`s, said pixels crossed only in `PageImage`s, and described plugin-side layout and node-only semantics as applying to all pages. Its `PageFrame` summary still said “One frame of a plugin-drawn page.” The same module already exposes `PageFrame.web` and `PageWebSurface`, and its validator rejects a frame mixing the two rendering forms.

The addition of web declarations had not been carried through to that module-level API documentation. A plugin author reading the public type documentation could wrongly apply display-list layout responsibilities, image-node byte limits and node-event routing to a web document. This is misleading guidance, not evidence that the implementation rendered or routed input incorrectly.

### Every changed documentation hunk

- **Module introduction / What crosses the boundary:** identify a page as either a retained display list or a supervised web surface. State that the plugin sends a declarative `PageFrame`, not executable code for the launcher. Scope node rendering, palette and DPI ownership to display lists.
- **Plugin-pixel and raster-limit prose:** distinguish plugin-produced image-node pixels from engine frames; keep the existing per-node/per-frame raster limits and the fact that bounded tiles may cover a complete page.
- **Document-model/layout section:** scope manual positioning, wrapping/reflow/scrolling and edit-state responsibility to display-list nodes rather than web documents.
- **Accessibility section:** scope `role`, `label`, `focus_order` and `unlabelled_interactive` to display-list nodes. No platform accessibility bridge or screen-reader support is added or promised.
- **New web-path explanation:** state that `PageWebSurface` declares HTTP(S) navigation and storage mode; the supervised engine owns document layout, focus and repaints; the plugin receives neither the document nor browser pixels. Engine pixels and document input use a private host-to-host path, not `PageImage` or node-event traffic. The forms remain exclusive.
- **`MIN_PAGE_REDRAW_MS` comment completion:** describe the constant as the floor for display-list self-scheduling, scope the timing/raster-bandwidth explanation and input exception accordingly, and explicitly state that web declarations must use `redraw_after_ms = 0` because the engine owns repainting. This is documentation of existing validation, not a new clamp or web scheduling rule.
- **`PageFrame` type summary:** replace the all-pages/plugin-drawn wording with “a display list or a web-surface declaration.”
- **`PageFrame::redraw_after_ms` field comment completion:** zero means a display list remains static until input; web declarations also require zero but may repaint independently through their engine. Explicitly connect the nonzero-web refusal to `PageFrame::validate`.

The two redraw-comment completions belong to this same entry, not additional issues or new behavior. No unrelated PageNode comments were changed.

### Source contracts supporting the correction

`PageFrame.web`, `PageWebSurface` and the web branch of `PageFrame::validate` already distinguish the two forms and reject mixed nodes, nonzero focus nodes and nonzero plugin redraw timers. `WebSurfaceBuilder` creates a declaration with no nodes and zero timer. `WebPageRuntime::declare` admits the exact owner's web request before startup; the separate host renders and sends engine frames. `draw_page` takes the web branch before display-list node/focus processing. These are existing declarations and consumers, not an implementation supplied by this documentation change.

This core API correction is distinct from PR8R4's earlier `crikey-ui::PageSurface` API-doc correction. The old UI hunk is not counted again, and no source hunk is shared with DOCS-1 or DOCS-2 below.

**Coverage and limits:** this documentation entry makes no runtime failing-before claim, adds no baseline patch and adds no wording test. Existing core tests were unchanged. Integrated formatting, check, Clippy and ordinary documentation tests passed. The additional strict HTML-documentation build failed in an unchanged module, recorded separately as OBS-4; it is not reported as a pass.

## PR8R5-DOCS-1 — Rejected-webview paragraph still required shared textures

**Kind:** Architectural-documentation consistency correction. No transport or performance change.

**Changed repository hunk:** `docs/adr/0020-plugin-pages.md`, Alternatives rejected → In-process or child-window webview per surface, lines 228–235.

### Context, cause and impact

PR #8 renamed this rejected alternative and accepted a supervised offscreen Linux route. The body of this separate paragraph still said the choices were a rectangular child window or an offscreen render sharing textures with `wgpu`, and described that choice as unresolved. That categorically misstated the accepted CPU-pixel transport and launcher ownership model.

This is **not** a recount of PR8R4-DOCS-2. R4 corrected the different Why not pixels paragraph that asserted megabyte frames required a shared-memory data plane. The remaining shared-texture requirement was in the separately PR-renamed rejected-webview paragraph; only that residual paragraph is changed here.

### Exact correction and producer-to-consumer evidence

The replacement keeps the rejected child-window integration and out-of-process content requirement. It says an offscreen renderer needs an explicit frame path into the launcher's `wgpu` surface, not necessarily shared textures; the accepted Linux socket-and-upload route is the concrete counterexample. It keeps the remaining platform-specific integration work separate from lazy startup and idle memory.

The actual path is:

1. `csrc/ck_web_shim.c::on_export_shm` (161–184) borrows the engine's SHM mapping only during the callback, then ends access and releases it.
2. `src/frame.rs::FrameConverter::{convert,swap_into}` (67–150) copies/reorders bounded BGRA pixels into an owned `WebFrame.rgba8` vector.
3. `src/engine.rs::CallbackState::receive_frame` queues `Payload::WebFrame`; its writer sends the envelope through `Transport`.
4. `crikey-cli/src/web_host.rs::handle_incoming` (857–876) checks surface/generation, validates the byte-carrying frame and publishes it to the launcher.
5. `crikey-ui/src/native.rs::web_texture` (3627–3665) constructs a premultiplied `ColorImage` from those bytes and calls `context.load_texture`; `draw_page` clips/composes the resulting UI-owned texture.

No GPU texture handle crosses the host/launcher connection. Engine-local SHM export does not establish cross-process shared textures or a shared-memory launcher transport.

**Coverage and limits:** no new test for prose; no new latency, bandwidth, GPU or ancestry measurement. The wording corrects the implemented architecture, not its performance.

## PR8R5-DOCS-2 — PR-rewritten pixel paragraph retained a false framebuffer ban

**Kind:** Specification-description correction. No new raster capability.

**Changed repository hunk:** `docs/spec/crikey-spec-v1.md`, §32.1 Purpose and scope, original lines 2349–2354, now 2349–2355.

### Context, provenance and impact

The statement forbidding a display-list page as a surface-sized framebuffer was inherited, not introduced by PR #8. However, PR #8 edited this exact paragraph to contrast display-list pixels with the new web route and retained the false assertion. The coordinator explicitly kept correction of that rewritten PR-facing contract in scope; this is not permission for a broad rewrite of inherited rationale.

The prohibition conflicted with the current core and ADR, which explicitly allow image-node tiles to cover a surface within existing limits. It could mislead authors into treating a valid bounded tiled image page as forbidden, or obscure the difference between plugin image nodes and private engine frames.

### Exact correction and existing behavior

The paragraph now says display-list pixels cross as bounded rasters on image nodes; each raster occupies its node's rectangle, and multiple image nodes may tile a surface-sized framebuffer within the per-node/per-frame limits. There is no separate display-list framebuffer payload. The web form remains separate: the plugin declares it, receives neither its pixels nor DOM, and engine pixels travel only between supervised host and launcher.

`PageImage`, `NodeShape::Image`, `MAX_PAGE_IMAGE_BYTES` (1 MiB), `MAX_PAGE_IMAGE_TOTAL_BYTES` (4 MiB), and `MAX_PAGE_IMAGE_EDGE` (1,024 pixels) already define the bounds. `PageFrame::validate` iterates image nodes, checks each raster's dimensions/length and sums frame bytes; it has no ban based on their rectangles together covering the page. ADR-0020's Decision and pixel-alternative paragraphs already explicitly describe two bounded tiles reconstructing a 720×400 framebuffer. No cap, field, validator, redraw rule or renderer changed.

**New coordinator-observed executable evidence:** a standalone consumer of the public API accepted two bounded image tiles covering 720×400 pixels, totaling 1,152,000 bytes. It also refused a mixed web-and-node frame and a web frame with redraw delay 1. This smoke **PASSED**. It exercises public core validation, not WPE, GPU upload, a native plugin transport, or whole-launcher rendering. It is not a failing-before regression: the existing implementation already accepted the tiles and rejected the invalid combinations before the prose correction.

**Coverage and limits:** no wording test or new permanent test; no baseline runtime defect claimed. PR8R5-contracts-1 separately owns core source API documentation and has no shared hunk with this specification paragraph.

## Complete changed-file and review ledger

All paths below were covered by the assigned owners. “Unchanged” means no R5 edit, not that the path was absent from PR #8. Three documentation files and ENGINE-1's C/header/Rust file have changes, all accounted for in their entries and inventory. All other paths were intentionally left unchanged after review. Centralized execution and the separate outside-PR documentation failure are recorded below.

### Engine/runtime — ENGINE-1 API documentation and native regression

- `crates/crikey-web-host/src/engine.rs`: complete implementation and feature-gated tests; absolute Unix endpoint admission, first-command deadline/partial reads, native-byte persistent-path handling, one-attempt initialization, startup diagnostics, thread-spawn failures, bounded reader/writer shutdown, callback userdata ownership, stable callback allocation, frame-slice prerequisites, output terminal fence, load/Gone failure handling, synchronous callback possibilities, IME cancellation and idempotent cleanup. Reviewed R4's enqueue-before-wake/coalescing fix, real readiness, the 32-command dispatch budget, self-rearm and terminal priority without relabeling it.
- `crates/crikey-web-host/src/main.rs`: complete CLI parser/tests, Linux gating, help exclusivity, missing/duplicate/unsupported options, endpoint kind/absoluteness, native-byte/NUL path handling.

Initial source review did not establish a runtime defect; the coordinator’s actual probe then confirmed ENGINE-1. The permanent native subprocess regression failed for the intended missing busy-context refusal on isolated starting source and passed on fixed source. Both fixed consumer modes and the complete actual-WPE suite passed. src/main.rs remains unchanged.

### Native adapter — ENGINE-1 context-ownership fix frozen

- `crates/crikey-web-host/csrc/ck_web_shim.c` and `ck_web_shim.h`: complete C implementation/header; backend/exportable ownership transfer, failure unwind, view/session/IM references, signal disconnection, disabled callbacks before destruction, native-watch source lifetime, quit-before-run, borrowed mapping lifetime, widened row arithmetic, frame completion, navigation/popups/download policy, persistent data/cache/cookie configuration, session-cookie exclusion, ephemeral sessions, key/button/modifier/focus/scroll handling and Unicode preedit offsets.
- `crates/crikey-web-host/tests/engine_smoke.rs`: complete real-host smoke inventory and assertions, including actual documents, local HTTP, pixels/resize/navigation, storage isolation/cookies, attachment refusal, malformed startup, IME/input and process/socket shutdown.

Exact-version upstream ownership/input sources were consulted rather than guessing new tracking logic: [WebKitWebViewBackend.cpp](https://raw.githubusercontent.com/WebKit/WebKit/webkitgtk-2.52.6/Source/WebKit/UIProcess/API/wpe/WebKitWebViewBackend.cpp), [WPEBackend-fdo exportable implementation](https://raw.githubusercontent.com/Igalia/WPEBackend-fdo/1.16.1/src/view-backend-exportable-fdo.cpp), [WebEventFactory.cpp](https://raw.githubusercontent.com/WebKit/WebKit/webkitgtk-2.52.6/Source/WebKit/Shared/libwpe/WebEventFactory.cpp), [InputMethodFilter.cpp](https://raw.githubusercontent.com/WebKit/WebKit/webkitgtk-2.52.6/Source/WebKit/UIProcess/API/glib/InputMethodFilter.cpp) and [NetworkSession.new](https://wpewebkit.org/reference/stable/wpe-webkit-2.0/ctor.NetworkSession.new.html).

ENGINE-1's frozen C/header change reserves the default context before mutation, refuses a foreign owner, retains ownership through native lifetime and releases on cleanup/failure. Every native/header hunk is accounted for above. `tests/engine_smoke.rs` remains unchanged. Known upstream cursor-only preedit and legacy wheel limitations remain separate and are not recast as new shim bugs.

### Engine-independent host helpers — unchanged

All six modules and their existing tests were read completely:

- `crates/crikey-web-host/src/frame.rs`: dimension/byte caps before allocation; padded/truncated stride and final-row length; checked mapping arithmetic; premultiplied channel permutation; generation exhaustion; rejected-frame nonmutation; initialized shrinking/reused buffers; actual engine/writer/UI consumers.
- `src/ime.rs`: first/update/empty preedit, commit-before-teardown, deferred clears, flush/cancel, absent/clamped cursors, UTF-8 byte-to-Unicode-scalar conversion, supplementary/interior-byte positions, actual navigation/focus/close consumers.
- `src/input.rs`: finite-value admission, recognized modifiers, timestamp conversion, repeat and signed coordinate bounds, button numbering/held masks, focus reset, rejected-transition atomicity, precise/fractional wheels and native consumer encodings.
- `src/lib.rs`: helper exports, feature gates, engine-free availability and crate documentation.
- `src/output.rs`: bounded one-frame coalescing, caret/load replacement order, frame-not-control displacement, terminal-first-wins fences, Failed/Gone priority, recycling, close/drain/condition-variable waiting and actual producer/writer use. Terminal priority cannot preempt bytes already being written; that existing boundary is not a newly established delivery failure.
- `src/state.rs`: first OpenSurface, nonzero/exact IDs, single surface/direction/closed transitions, rejected-open nonmutation, checked viewport budget, persistent directory requirement, ephemeral default, shared URL admission/byte limits, NUL checks and IME cursor validation before engine calls.

No confirmed new helper issue; no allocation abstraction, duplicate validation convention or speculative IME feature was added.

### Launcher/application — unchanged

- `crates/crikey-cli/src/lib.rs`: every PR hunk and needed surrounding page/runtime flow, native notices, active owner admission, resize/input/IME forwarding, loading/failure closure, activation/dismissal/quit, private directories and related regressions.
- `crates/crikey-cli/src/web_host.rs`: complete 3,930-line implementation/test file at starting HEAD; bounded commands/output, identity/generation/mailbox fences, lazy startup and replacement cancellation, serialized teardown, first OpenSurface, terminal diagnostics, real socket deadlines, executable discovery, per-owner storage, memory limits, cgroup mount/delegation/pre-exec containment and descendant cleanup.
- `crates/crikey-cli/src/plugin_commands.rs`: new nondegrading doctor IME warning and verdict flow.
- `crates/crikey-cli/src/settings.rs`: web effects retain residency.
- `crates/crikey-cli/tests/m7_plugin_commands.rs`: doctor integration fixture and healthy-verdict expectations.
- `crates/crikey-app/src/lib.rs`: SearchService web admission and needed current-owner/open/poll/close lifecycle.
- `crates/crikey-app/src/plugin_action.rs`: exact manifest permission registration/admission, shared-executor ownership regression and page routing.

The producer/consumer path admits the exact current plugin's network/storage grants before lazy supervisor construction. First stay, storage-mode mutation and owner mutation are refused. Current native failure notices retire runtime, plugin owner and model before reporting `Page closed: …`. Existing R4 activation cleanup remains in place and is not a new R5 fix.

Cgroup review preserved min(768 MiB, physical RAM/8, inherited finite limits), the 256-MiB floor, safe multi-mount views, proper-ancestor-only admission, enabled memory children/common-ancestor migration permission, private writable memory/kill controls and pre-exec attachment. It does not move the launcher or enable unrelated controllers. Whole-tree escalation uses cgroup.kill, not remembered descendant PIDs; numeric process-group SIGTERM is limited to the root's unreaped lifetime. Storage keeps full owner identity, bounded components and checked 0700 real directories. No second permission/storage convention or retry policy was introduced.

### UI — unchanged

- `crates/crikey-ui/src/lib.rs`: frame validity, transport-independent input/IME types and effects, page/surface identity, strict engine generations, rendering-model replacement, pixel clearing with retained generation floor, checked caret endpoints and dismiss/settings lifecycle.
- `crates/crikey-ui/src/native.rs`: complete changed rendering/input mechanisms and necessary graphics context; multipass command retention/final geometry, clipped page/chrome layout, loading until engine generation, premultiplied texture upload, identity/generation memoization and release, native/fallback ownership, retained pointer coordinates/chord capture, phased wheels, primary touch/contact/device isolation, deferred-event retirement, logical/native keys, Linux hardware +8 mapping, repeat/synthetic keys, Escape and IME state/caret handling.
- `crates/crikey-ui/tests/launcher_view_model.rs`: current/stale/duplicate/wrong-surface/incomplete frame and caret admission, replacement/rendering-model transitions, cleared-generation floor and host effects.
- `crates/crikey-ui/tests/native_renderer.rs`: texture bytes/reuse/eviction/close, GPU maximum size, viewport delivery, footer clipping, multipass behavior, ordered Escape, keyboard and Settings chrome ownership, hints enabled/disabled, existing synthetic-touch lifecycle coverage.

No confirmed new PR UI defect. R3 touch fixes and R4 hide/reactivation/chrome/hint coverage are historical, not new findings. The inherited zoom observation is separately marked OBS-1.

### Core, native protocol, permissions and Rust SDK — only core API comments changed

- `crates/crikey-core/src/lib.rs`: web/storage/shared URL exports; unchanged.
- `crates/crikey-core/src/page.rs`: HTTP(S) URL length/control/authority admission, userinfo/port/bracket-host handling, empty stay, safe storage default, web/node/focus/timer exclusivity, raster bounds and existing tests. Only the documentation hunks listed under contracts-1 changed.
- `crates/crikey-native-protocol/src/convert.rs`: bidirectional web fields and safe unknown/unspecified-to-Ephemeral conversion; unchanged.
- `src/message.rs`: canonical discriminants/tags 33–42 and PageFrame field 7, default/explicit-default behavior, nested/same-oneof fragment merge, different-oneof replacement, unknown retention, cumulative allocation/depth budgets and WebFrame edge/length/2-MiB validation; unchanged.
- `src/transport.rs`: Unix write timeout, unsupported transport refusal, timeout clearing/cloning and close-after-partial-write contract; unchanged.
- `tests/transport.rs`: real stalled Unix peer and unsupported-timeout consumer coverage/cleanup; unchanged.
- `tests/wire.rs`: independent raw keys/fragments, oneof/default/unknown behavior, maximum/oversized/truncated/degenerate frames, IME absent/zero distinction, URL/mixed-page refusal and cumulative allocation budget; unchanged. R4's existing budget regression is not new.
- `crates/crikey-plugin-model/src/permissions.rs`: kebab-case/default false storage grant, independent navigation/persistence admission, stay distinction and unchanged legacy baseline; unchanged. R4's persistent-stay regression is not recounted.
- `sdk/protocol/crikey/v1/plugin.proto`: additive tags/types, exclusivity/older-host behavior, storage lifetime, private host traffic, pointer/hardware-key/Unicode-scalar/premultiplied-pixel contracts; unchanged.
- `sdk/rust/src/builder.rs`: independent web builder, shared validation, defaults, repeated persistent choice, stay semantics and PageBuilder web=None; unchanged.
- `sdk/rust/src/lib.rs`: reexports; unchanged.
- `sdk/rust/tests/page.rs`: framed web navigation/persistent stay, defaults, URL/byte-boundary validation; unchanged.

No new runtime/protocol defect or validation expansion. Shared URL admission remains a bounded policy check, not a newly invented complete browser URL parser. Unrelated pre-existing protobuf sites were not migrated.

### Packaging/build/dependencies — unchanged

- Root `Cargo.toml`, `Cargo.lock`: workspace host member/dependency consistency, Rust 1.86/resolver-2 context and no launcher browser-link edge.
- `crates/crikey-web-host/Cargo.toml`: empty defaults, explicit engine feature, required binary feature and engine-independent Rust dependencies.
- `crates/crikey-web-host/build.rs`: Linux/feature gating, shim rerun tracking, five pkg-config probes, diagnostics/includes, C11 build and static-shim-before-system-link ordering.
- `crates/crikey-cli/Cargo.toml`: Linux-only libc change and existing platform edges.
- `packaging/linux/build.sh`: source/supplied/no-web flags, target parsing/preflight, Cargo JSON artifact selection, quoting/failure propagation, executable/ELF/forced-C-locale ldd checks, stage replacement, tarball timestamps, Debian scanner/fallback, RPM and no-web Flatpak flow.
- `packaging/linux/tests/build.test.sh`: supplied/script fixtures, no-web replacement, archive/package consumption, refusal cases, recording Cargo/WPE/scanner fixtures, configured/custom/escaped/relative outputs, scanner fallback, missing-ldd and Flatpak source/archive fixtures.
- `packaging/linux/rpm/crikey.spec`, `packaging/linux/flatpak/org.crikey.CriKey.yaml`: runtime inclusion/dependency and explicit no-web recipes.

The existing R4 selector uses actual successful Cargo artifacts, not guessed target paths; missing/ambiguous artifacts refuse and fresh=false is not wrongly rejected. Default builds do not compile/link WPE. Supplied ELF checks do not prove semantic protocol compatibility. No-web staging removes a previously staged web host. Web Debian scanner failures retain the documented conservative fallback and its private/unversioned/no-SONAME limits; RPM relies on ELF requirements; Flatpak intentionally remains no-web. Package labels describe this machine, not a new cross-packaging capability. WPE's selected 2.0 pkg-config ABI already starts at 2.40, so Since-2.40 APIs did not justify a speculative new floor.

Adjacent read-only context was limited to `packaging/linux/deb/control.in` and relevant CI/release steps. They were not expanded into edits or claims of actual engine-enabled distribution verification.

### Public documentation — two hunks changed, other four paths unchanged

- `docs/architecture.md`: process/thread diagram, plugin-code boundary, optional Linux WPE/private pixels, native proto3 versus Python JSON-lines, worker Landlock scope, retained display-list staleness/direct web input; unchanged.
- `docs/user-guide.md`: all PR shortcut/footer changes and required install/binary/runtime context; native web ownership, Tab/Ctrl+, persistent Settings mouse route and Escape; unchanged. R4 hint correction is not recounted.
- `docs/plugin-author-guide.md`: all changed page/cycle/web/input/permissions paragraphs and Rust/TOML examples; SDK API, grants/owner, first stay/repeated URLs/storage mode, platform/discovery, explicit packaging/Flatpak, UTF-8 persistent paths, loading/failure, cgroup/memory/cleanup, keyboard/IME/accessibility limitations; unchanged.
- `docs/adr/README.md`: changed ADR-0020 row matches accepted optional Linux route; unchanged.
- `docs/adr/0020-plugin-pages.md`: PR-facing decision/rationale/rejection/revisit/measurement attribution/cgroup/lifetime/loading/input/packaging; only DOCS-1 hunk changed. Historical numbers were not rerun or rewritten.
- `docs/spec/crikey-spec-v1.md`: all §32 PR changes, both forms, requests/palette/input/generations, bounds/refusal/semantics/open/close, navigation/popups/downloads/storage/private transport/lazy loading/failures/IME/cgroup/packaging; only DOCS-2 hunk changed.

### Historical reports — read-only

`docs/notes/pr8-bughunt-PR8B.md`, `pr8-bughunt-PR8R2.md`, `pr8-bughunt-PR8R3.md` and `pr8-bughunt-PR8R4.md` were used only to identify existing fixes, prior evidence and explicit limits. None was rewritten. The original R4 late-answer failure is not root-caused or made a new R5 finding without new evidence.

## Inherited observations outside PR scope — left unchanged

### PR8R5-OBS-1 — Inherited egui zoom/layout question

**Status:** source inference, outside the PR's unchanged graphics code; not fixed and not counted as a PR runtime defect.

**[INFERENCE]** `crikey-ui/src/native.rs::GraphicsState::draw` around 2261 overrides `RawInput.screen_rect` using `window.scale_factor()` alone. Installed egui-winit 0.29.1 normally divides physical dimensions by `egui_zoom_factor * native_pixels_per_point` (its lib.rs:228–242; helper at 43–47). The native web input conversion already uses that product (native.rs:1190–1196), and egui's context can invoke keyboard zoom. At steady nonunit egui zoom, the override appears to over/undersize the logical layout viewport **[INFERENCE from source]**.

The PR leaves that screen-rectangle override unchanged; its adjacent graphics hunks change the frame-builder call and add an IME comment. No runtime zoom failure was observed this round, no unrelated scaling investigation/fix was made, and this is not the already-merged adapter-resolution/high-DPI fix from PR #7.

### PR8R5-OBS-2 — Inherited ADR size-step rationale

**Status:** exact inherited wording, coordinator scope exclusion; not fixed and not counted.

ADR-0020:68–72 says a 720×400 RGBA frame is 1.10 MiB and that a window “one size step larger stops fitting at all,” despite the stated 8-MiB native message cap. That exact rationale statement predates PR #8 and was not edited by the PR. The coordinator directed it remain unchanged under this PR-only scope. Unlike DOCS-2's explicitly rewritten paragraph, this is not silently included as an extra correction.


### PR8R5-OBS-4 — Existing link blocks strict core API documentation generation

**Status:** Observed failure in an unchanged, outside-PR file; not fixed or counted as an in-scope correction.

The additional command `RUSTDOCFLAGS="-D warnings" cargo doc --locked -j 2 -p crikey-core --no-deps` failed at `crates/crikey-core/src/generation.rs:72`. Its documentation links to `CoreError::CapacityExceeded` without bringing `CoreError` into that module’s scope. The implementation at line 80 uses the fully qualified `crate::CoreError::CapacityExceeded`. The file has no PR #8 diff.

The coordinator routed this to the contracts owner, who confirmed the qualification and provenance without changing the file. No warning was suppressed and no unrelated source was edited. Therefore this report does **not** claim a successful warnings-denied core HTML-documentation build. The ordinary workspace documentation tests passed separately; those are different checks. Source review found the changed page module’s links valid, but that does not erase this strict-build failure.

## Actual complete browser path on the fixed implementation

The coordinator built the current launcher and a throwaway native Rust SDK plugin, using its own localhost HTTP page and isolated configuration/runtime directories. The launcher ran in a temporary delegated leaf; the actual WPE host used its ordinary private socket and dedicated memory-controlled process group. Rendering used container-local Xvfb and the native launcher, not Chromium automation or a supplied synthetic browser raster.

The first capture showed the intended loading state. Later captures showed the actual `PR8R5 full launcher web smoke` HTML document. Its live counter advanced from 84 to 501, the input displayed exactly `R5_runtime_text`, and clicking the document button changed its label to `Clicked` and its background from blue to green. A settled isolated Tab check visibly focused the button. Ctrl+, left the document visible rather than opening launcher settings. Early interaction captures were not treated as settled focus results.

Clicking the launcher’s Settings affordance opened the real settings panel and removed the browser cgroup. Reopening produced a new group ending in `-2`, containing seven process IDs, and a fresh blue document with an empty input and counter 3. Escape removed that group. Clicking Quit CriKey ended the actual launcher with exit status 0. The first group also contained seven process IDs. These are observed containment and cleanup results, not a claim that every possible crash or activation race was reproduced.

Screenshots were manually inspected at `/tmp/pr8r5-fullpath/loaded.png`, `clicked.png`, `tab-settled.png`, `document-shortcut.png`, `settings.png` and `reopened.png`. This is separate from the standalone public-API foreign-context reproduction and from the synthetic-raster native pixel smoke. It verifies the normal launcher-to-plugin-to-supervisor-to-WPE-to-native-window route after ENGINE-1’s fix.

## Fresh verification ledger — coordinator evidence only

Owners' source/test inspection is not a test pass. Earlier R4/R3/R2 results are historical and do not fill current gates.

| Check or event | Current observed status | What it establishes / does not establish |
| --- | --- | --- |
| Public core API executable smoke | **PASSED** | Accepted two bounded tiles totaling 720×400 / 1,152,000 bytes; rejected mixed web+nodes and web redraw=1. Not WPE/GPU/launcher integration; not a failing-before product regression. |
| Existing Linux packaging suite and shell syntax | **PASSED** | Final fresh rerun observed **92 passed, 0 failed**, plus syntax validation. Earlier command exited 0 but its output/artifact capture failed during disk exhaustion; the final rerun supplies the missing summary. This is fixture/syntax evidence, not an actual WPE-enabled distribution install. |
| Initial native pixel harness link | **FAILED: ENOSPC**, later native-window smoke passed | Preserve the explicit disk-full link failure as an environment failure, not a code finding. Successful later window evidence is listed separately. |
| Host/container disk recovery | **Observed environment recovery** | Root initially had about 25 MiB free. Cleanup commands reported removing about 3.0 GiB of generated host workspace artifacts and 736 MiB of container workspace artifacts; a later df reported about 13 GiB free. The remaining difference is not attributed to an unmeasured mechanism. No source, assertion, deadline or product behavior was changed for storage recovery. |
| Host integrated fmt/check/Clippy | **PASSED** | Coordinator observed all three integrated-source gates succeed. |
| Host serial workspace all-target tests | **PASSED** | `cargo test --locked -j 2 --workspace --all-targets -- --test-threads=1`: **2,559 passed, 155 suites, 5 ignored, 28 filtered**, with virtual environment first on PATH. |
| Rust documentation tests | **PASSED** | `cargo test --locked -j 2 --workspace --doc`: **1 passed across 27 suites**. |
| Additional warnings-denied core HTML documentation build | **FAILED outside PR** | Unresolved `CoreError::CapacityExceeded` link in unchanged generation.rs:72; see OBS-4. No warning suppression or unrelated fix. |
| Fixed WPE-enabled native library and actual-host all-target gates | **PASSED** | WPE 2.52.6 container / Cargo 1.95, `RUSTFLAGS="-D warnings"`, `--features engine --all-targets -- --test-threads=1`: **78 library + 3 CLI + 13 actual-host scenarios passed**, including ENGINE-1. |
| External GLib consumer before-fix control/probe | **CONFIRMED ENGINE-1** | Baseline-hash-matched isolated source: control exit 0 / creator1=actual1 / GoneClosed / runOk; foreign context exit 73 / owner5 / creator1≠actual5 / WRONG_THREAD_DISPATCH. |
| Permanent ENGINE-1 regression on isolated baseline | **FAILED as intended** | Starting160 + tests-only patch, isolated target: foreign-owned context not refused as EngineInit(-3); child101 versus required42, parent failure, Cargo101, bounded 0.09s. Not an artifact/compile failure or timeout. |
| Fixed external GLib consumer, both modes | **PASSED** | Actual-WPE control creator1=actual1 / GoneClosed / runOk and foreign reacquire after cleanup. Busy mode EngineInit(-3), exactly one surface7 StartupFailed Gone, zero resize calls, owner released/joined. Socket reset followed the observed diagnostic with commands already queued; no lost-diagnostic claim. |
| Native initialization-failure cleanup probe | **PASSED** | Actual shim with loader/SHM calls forced false: -1/-2, ownership assertions inside initializers and foreign acquire after cleanup. Not actual browser initialization/load evidence. |
| Fresh native window/pixel harness | **PASSED** | Coordinator observed 252 changed hover pixel channels, exact restoration, and one move plus departure. Actual native window and synthetic engine raster; not WPE content. |
| Ignored synthetic-touch lifecycle check | **PASSED** | Explicit Xvfb invocation of native_touch_dispatch_precedes_ime_and_cannot_retarget_after_redraw with --ignored --exact; real window, injected winit touch, not hardware or a complete IME service. |
| Actual delegated-cgroup tests | **PASSED: all 3** | Fresh current CLI test executable in a private mount namespace: detached child, exited root with detached child, late detached descendant. Actual writable kernel controls, not a mock. |
| Full launcher → native plugin → supervisor → WPE → UI smoke | **PASSED** | Actual HTML, live counter, typed text, click, settled Tab focus, Settings, fresh reopen, browser-cgroup removal and launcher exit 0; detailed above. |
| External native consumer / optional-host platform checks | **PASSED** | Native conformance all-target check and optional host all-target/all-feature checks for aarch64-apple-darwin and x86_64-pc-windows-gnu. This is build isolation, not WPE support on those platforms. |
| Virtual-environment Python compilation | **PASSED** | `python -m compileall -q sdk/python crates/crikey-legacy-compat/python compatibility/test-plugins`, using the repository interpreter. |
| Real engine-enabled distribution contents/dependencies/install, RPM and Flatpak builds | **NOT EXERCISED** | Recording fixtures are not distribution installation; Flatpak’s deliberate no-web contract is unchanged. |

No baseline checkout was built against the live checkout’s target cache. The initial source/archive and the before-test source tree were separate, and the native before-regression target was isolated at /tmp/pr8r5-before-engine-target. Both the baseline consumer and permanent test failed for their intended ownership violation; neither failure was a compilation error, artifact mix-up or timeout. Fixed consumer, native and full-engine gates passed separately. All documentation entries describe existing behavior and make no failing-before runtime claim.

## Change inventory and limits

The complete implementation, test and public-documentation inventory contains six files. The seventh changed file is this requested report, docs/notes/pr8-bughunt-PR8R5.md. All entries below map to the confirmed issues; inherited OBS-1/2/4 files are intentionally unchanged.

| Repository path | Entry | Change kind |
| --- | --- | --- |
| `crates/crikey-core/src/page.rs` | PR8R5-contracts-1 | Module/type documentation and display-list/web redraw comments, including coordinator follow-through |
| `docs/adr/0020-plugin-pages.md` | PR8R5-DOCS-1 | One residual rejected-webview shared-texture paragraph |
| `docs/spec/crikey-spec-v1.md` | PR8R5-DOCS-2 | One PR-rewritten pixel paragraph describing existing bounded tiling |
| `crates/crikey-web-host/src/engine.rs` | PR8R5-ENGINE-1 | Safe public-run documentation and native context-busy subprocess regression in the existing one-initialization fixture |
| `crates/crikey-web-host/csrc/ck_web_shim.c` | PR8R5-ENGINE-1 | Default-context state/reservation before mutation, explicit loop context, busy refusal, failure cleanup and release after teardown |
| `crates/crikey-web-host/csrc/ck_web_shim.h` | PR8R5-ENGINE-1 | Private -3 initialization result, ownership/cleanup contract and non-wire result clarification |

The three documentation entries add or change no permanent tests or behavior. ENGINE-1 changes native ownership and extends an existing permanent native test; its C, header, Rust documentation, test imports, opaque GLib FFI declarations, child helper, bounded synchronization and parent sentinel check are all accounted for above. No schema tags, permissions, dependencies or package recipes changed. No speculative fallback was introduced.

Source review does not establish exhaustive race freedom, sanitizer or big-endian/32-bit results, actual ibus/fcitx interoperability, surrounding-text correction, hardware touch/mixed-DPI behavior, screen-reader integration, power-loss storage durability, arbitrary supplied-host compatibility, or whole-tree cleanup after abrupt launcher death. Do not turn synthetic winit events into hardware evidence, validation into GPU proof, engine smoke into whole-launcher composition, package fixtures into real distribution installs, or ENGINE-1's demonstrated foreign-owner API trigger into an observed standalone-host failure.

## Section artifacts used

- `local://pr8r5-engine.md`
- `local://pr8r5-native.md`
- `local://pr8r5-host.md`
- `local://pr8r5-launcher.md`
- `local://pr8r5-ui.md`
- `local://pr8r5-contracts.md`
- `local://pr8r5-packaging.md`
- `local://pr8r5-docs.md`

The section artifacts distinguish inspected source, existing coverage and observed execution. Earlier tentative GLib assessments are superseded by ENGINE-1’s concrete before/after proof. Session-local artifact paths are evidence pointers, not dependencies a reader must possess: issue context, source locations, exact observed behavior, changed contracts and test names are explained above.

Temporary baseline sources and archives, isolated probe projects/targets, the public-core smoke, full-path plugin/configuration/cgroup fixtures and test display were removed after execution. Permanent regressions remain in the repository. Native evidence is retained at /tmp/pr8r5-native-smoke-final and actual browser captures at /tmp/pr8r5-fullpath. No throwaway program was added to the repository.
