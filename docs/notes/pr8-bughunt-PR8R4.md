# PR #8 bughunt — PR8R4

**Completed audit.** All confirmed in-scope changes below are implemented and verified. The original outside-PR failure remains recorded; its cause was not established. This report separates fixes, documentation, added coverage, observed execution and unverified boundaries.

## Scope and reading guide

This is a fresh fourth audit of **PR #8 only**, starting at `c157f8e72b3eb9582ec2d184d9a3a38243cac34a`, relative to `feature/page-image-canvas`. Seven owners reviewed the engine/C boundary, engine-independent host helpers, launcher/application supervision, UI, core/protocol/permissions/Rust SDK, Linux packaging/build integration, and public documentation. Surrounding code was inspected only where needed to understand changed behavior and contracts. On the coordinator's explicit request, documentation corrections also update the connected architecture overview, ADR index and user shortcut guide left inconsistent by the feature; this is not a broader repository audit.

The PR8B, PR8R2 and PR8R3 reports are historical records. None was edited, relabeled as a new defect, or counted again. In particular, additional coverage for the existing R3 touch fix is identified as coverage, not another touch-routing bug.

There are **11 tracking entries**: **10 in-scope fix/documentation/coverage entries**, plus **PR8R4-X-1**, an unchanged outside-PR check-failure status with no confirmed root cause or fix. The 10 in-scope entries comprise:

- **2 runtime defects:** PR8R4-ENGINE-1 and PR8R4-supervisor-1.
- **1 packaging defect:** PR8R4-packaging-1.
- **1 misleading UI hint:** PR8R4-UI-1; not a newly broken key route.
- **3 documentation corrections:** PR8R4-UI-2, PR8R4-DOCS-1 and PR8R4-DOCS-2.
- **3 coverage improvements:** PR8R4-UI-3, PR8R4-contracts-1 and PR8R4-contracts-2; no newly discovered production defect is claimed for these.

These categories are not ten runtime fixes or ten independent crashes. A supporting test can exercise the boundary associated with another entry without becoming another counted issue. PR8R4-X-1 is the eleventh status entry, not a fix.

Seven audit owners worked without running builds, tests, linters, formatters or runtime experiments; the coordinator centralized execution after edits converged. All explicit worker Python commands followed the required `/home/yichen/projects/crikey/.venv/bin/python` rule, and imports remain at module scope. Initial coordinator packaging fixtures did not force venv PATH, so their indirect interpreter provenance is not claimed. The final packaging and Python checks explicitly used the virtual environment. Only the coordinator committed and pushed.

## PR8R4-ENGINE-1 — Coalesced wakeups could strand accepted commands, including CloseSurface

**Kind:** Runtime command-delivery defect.

**Changed file and symbols:** `crates/crikey-web-host/src/engine.rs`: `run`, `Runtime`, `Runtime::new`, `Runtime::process_commands`, `tests::assert_coalesced_commands_are_drained`, and `tests::wpe_process_lifetime_and_native_load_failure_boundaries`. No C shim or existing actual-host smoke file was changed for this entry.

### Root cause and impact

The nonblocking wake writer intentionally treats `WouldBlock` as successful notification: unread wake data already makes the loop ready, so wakeups may be coalesced. The old consumer nevertheless processed exactly as many queued commands as bytes returned by one wake-socket read. An accepted command whose wake write coalesced has no corresponding byte.

If the actual socket accepts fewer individual one-byte writes than the bounded channel accepts commands, the loop can consume all readable wake bytes after processing only a prefix of accepted commands. The remaining commands then wait without readiness until an unrelated notification arrives. Accepted resize, input, navigation or close commands can be affected. A final stranded `CloseSurface` produces no Closed notification or autonomous shutdown despite having entered the queue.

This is a control-flow contradiction, not evidence that the default socket capacity was observed to trigger it. The pressure regression deliberately requests a smaller real socket buffer and requires actual `WouldBlock`.

### Fix and reasoning

The command queue is now authoritative. One dispatch processes up to the existing 32-command budget rather than using wake-byte count as queue length. Runtime retains a clone of the wake-write endpoint and self-signals after exhausting that budget without termination, allowing remaining accepted work another bounded dispatch. If exactly 32 commands emptied the queue, the extra dispatch is finite and empty; an empty queue is not repeatedly rearmed.

The read/write endpoints are passed as one private constructor tuple. The clone is obtained before workers start, so a clone failure cannot leave a newly started reader behind. Existing bounded queue capacities, EOF/error paths, terminal-event priority and per-command allocation behavior remain. Shutdown affects the same Unix socket endpoint across clones, so retaining the descriptor does not disable worker-error HUP handling.

### Regression and before-fix setup

The existing single-initialization native test also runs a helper with a real WPE surface and a real nonblocking Unix socketpair. It requests `SO_SNDBUF=1024`, queues 63 valid Resize messages and a final CloseSurface before dispatch, and uses the actual wake writer. The fixture requires at least one real `WouldBlock`; mocked notification loss is not substituted.

Dispatch is gated by real GLib `g_poll` readiness with zero timeout. It does not manually invoke `process_commands` after readability disappears. It requires the exact Closed Gone reason and terminal runtime state. Queue closure bounds the old-code failure rather than allowing a hung test. It reuses the existing initialized engine and does not introduce another WPE initialization or competing GLib loop.

`local://pr8r4-engine-regression.patch` contains the baseline-only regression. Its constructor call uses the old baseline signature, not the fixed endpoint tuple. Apply it to a disposable starting-commit tree, not the integrated fixed tree.

**Observed before/fixed execution:** the coordinator ran the baseline real-socket/native pressure regression and observed failure because the accepted CloseSurface was stranded. The fixed actual-WPE run passed **78 library tests, 3 CLI tests and 13 smoke tests**, including the pressure regression. Worker and independent reviewer performed source review only; these runtime results are coordinator observations. The pressure fixture uses a real socket with deliberately reduced capacity and does not establish that default socket sizing triggers the defect.

### Independent review and limits

The independent wake review found no blocker. It checked producer enqueue-before-notify races, budget rearming, shared-endpoint shutdown, native IN/HUP/ERR dispatch, terminal output fencing and readiness-gated regression behavior. The frozen-patch review confirmed the production mechanism was unchanged; a final fixture break after termination cannot hide a stranded Close because the exact terminal diagnostic is still required.

No default-buffer capacity measurement, sanitizer proof, or real ibus/fcitx exercise is claimed. Existing fatal-load behavior was reviewed critically but not counted again as a new issue.

## PR8R4-supervisor-1 — A superseded dismissal leaked the old page and engine into a new launcher session

**Kind:** Runtime lifecycle/resource-owner defect.

**Changed file and symbols:** `crates/crikey-cli/src/lib.rs`: `run_native_launcher`'s `NativeLauncherEvent::Activated` arm, new private `activate_launcher_session`, and `tests::reactivation_closes_a_page_even_when_dismissal_was_superseded`. `web_host.rs` and native UI event sequencing are intentionally unchanged.

### Root cause and reachable sequence

The Activated branch called `view_model.dismiss()` followed by `view_model.activate()`. This discarded the old visible page without calling `WebPageRuntime::close` or `SearchService::close_page`. Those cleanups normally happened through Dismissed or explicit close/failure effects, but native event ordering does not guarantee a Dismissed callback before every replacement Activated event.

The UI owner and independent activation reviewer established the concrete path:

1. `request_toggle` synchronously marks the shared state hidden and queues Toggle.
2. A new activation can claim a newer session before that Toggle is processed.
3. The old Toggle's session guard drops it without emitting Dismissed.
4. The new Activate clears the previous frame and emits Activated.

The new session is then visible without its old page, while SearchService still owns that page. For PR #8's web route, the old independent web host/process tree can remain alive, produce frames or wakes, and later report failure into the wrong launcher session. The older display-list ownership omission predates PR #8; the newly added independent engine lifetime makes this resource leak relevant to this PR.

### Fix and reasoning

Activated now calls the private session helper, which closes WebPageRuntime and SearchService's page **before** resetting the model. Both existing close operations are idempotent. This restores ordinary dismissal's ownership invariant without changing event scheduling, permissions, cgroups, query cleanup, residency, or public UI/protocol APIs.

The supervisor's desired-owner reset invalidates late mailbox notices. Replacement surface identifiers remain distinct. The existing public guide and specification already require the engine to live only with its page, so no new public lifetime policy or redundant documentation change was needed.

### Regression and transparent baseline extraction

The deterministic regression uses real SearchService ownership state and LauncherViewModel plus the existing recording plugin executor. It opens the old page, activates again without a Dismissed effect, then requires visible replacement-session state, no old visible page or owner, exactly one plugin close, idempotent repeated activation, and successful opening under the replacement owner.

The test does **not** instantiate a supervisor or run WPE. It cannot, by itself, prove termination of a real web process.

`local://pr8r4-supervisor-baseline.patch` contains the regression and a behavior-neutral extraction of the old closure-local two-line dismiss/activate branch. The baseline helper deliberately omits both cleanup calls and therefore preserves the old behavior. This extraction is necessary to invoke the original branch deterministically; it is not a hidden product fix in the baseline.

**Observed before/fixed unit execution:** the baseline regression failed because the old plugin page owner remained. The fixed regression passed both within the fresh full workspace run and in a separate focused run. Its real SearchService ownership assertion, exactly-once close, repeated activation and replacement admission all passed.

**Full launcher → native plugin → supervisor → real WPE → UI execution: passed.** The coordinator exercised the throwaway native SDK plugin and localhost page through the actual launcher. Actual HTML appeared; its counter advanced from **5 to 87** without unrelated launcher input; a field received `R4_browser_text`; clicking the document button produced `Clicked` and a green background. Tab alone visibly focused the button. Ctrl+, did not open host settings. The first web group contained **7 PIDs**; hide/reactivate removed that group. Reopening created a working second group for surface identifier **2**, also containing **7 PIDs**; clicking the launcher Settings affordance opened host settings and removed that group. After another reopen, Tab alone produced the document button's focus ring, Escape removed the web group, and the actual **Quit CriKey** button exited the launcher successfully with **exit 0**. The coordinator manually inspected the actual local-HTML counter, typed-text, green-button, Tab and settings screenshots under `/tmp/pr8r4-fullpath/*.png`.

This is actual full-path evidence, not a separately launched host or synthetic raster. It does not deterministically prove the superseded-Dismiss race: the rapid off/on automation ended hidden, so the exact stale-Toggle ordering is supported by the baseline regression and source/reviewer trace, not claimed as observed browser-process race reproduction. The concrete smoke source/setup remain in `local://pr8r4-supervisor-smoke.rs` and `local://pr8r4-supervisor.md`.

The first remote-X-display GPU capture was clipped; using local container Xvfb resolved the capture without a product change. The first plugin-build lxc execution hit an I/O timeout; the resumed single-job build passed. These are execution/setup corrections, not additional fixes.

### Independent review

The final activation reviewer found no blocker. The reachable stale-Toggle path and cleanup order match existing lifecycle rules. Close remains idempotent, owner reset retires late notices, and the regression's recording-executor/no-supervisor limit is explicit. This was read-only review, not runtime evidence.

## PR8R4-packaging-1 — A configured native Cargo target could cause stale binaries to be packaged

**Kind:** Packaging payload-selection defect.

**Changed files and symbols:**

- `packaging/linux/build.sh`: `resolve_binaries`, new `cargo_executable`, and matching build-tool usage comments.
- `packaging/linux/tests/build.test.sh`: recording Cargo fixture and staged-content/artifact-refusal regressions.

### Root cause and impact

Cargo metadata's `target_directory` is an output root, not necessarily the directory containing an executable. The packaging driver appended `/release/<binary>`. With `CARGO_BUILD_TARGET` or Cargo `build.target` set to the native target triple, Cargo writes under `<target_directory>/<triple>/release` instead. Old executables at `<target_directory>/release` can still exist and pass file/executable checks.

Consequently a successful release build could silently package stale launcher/runtime binaries, including a stale web host. This is not only a clean-directory missing-file failure. Configured native-target output selection was not previously verified and is not an old corrected finding.

### Fix and reasoning

Both Cargo release commands retain `--locked` and request `--message-format=json-render-diagnostics`. Build exit status must succeed before output is used. The selector accepts exactly one nonempty absolute executable from a `compiler-artifact` record matching the requested package's manifest path, binary target name, bin target kind, and non-test profile.

Guessed release paths and metadata probing are removed. Actual emitted paths support Cargo's configured native-target/output-directory layout. Existing native-file/executable checks, supplied-binary mode, optional web support, ELF/library checks and stage replacement remain. No shell evaluation of JSON/path values or new production temporary file is introduced. This is still native packaging labeled from the machine, not new cross-architecture packaging support.

### Regression and observed results

The recording Cargo fixture emits fresh executable paths beneath a configured native triple while leaving `/bin/false` at each formerly guessed root/release path. Tests compare the bytes of all five staged payloads with the fresh artifacts. Same-name wrong-package, non-bin and test-profile decoys surround valid artifacts. Separate cases require refusal before staging if the requested release artifact is missing or if Cargo exits unsuccessfully despite emitting convincing artifact records.

The coordinator applied `local://pr8r4-packaging-tests.patch` to the starting tree:

- **Baseline: 86 passed, 6 failed.** The five fresh-content comparisons and missing-artifact refusal exposed the original selection behavior.
- **Fixed: 92 passed, 0 failed.**
- **Shell syntax checks: passed.**

The coordinator also exercised the exact production `cargo_executable` function against **141 actual Cargo 1.86 JSON records**, including compiler-artifact, build-script-executed and build-finished messages. The smoke used a configured native target and a space-bearing output path; selection passed. This supplies real Cargo stream/schema/path evidence beyond the recording fixture. It does **not** establish a complete native packaging build of every payload or an engine-enabled distribution package.

The first smoke harness unnecessarily invoked the selected C-ABI host with `--help`. That host does not support this option: it interpreted it as a package path and returned exit 3. The selector had passed. The unrelated invocation was removed; this is a smoke-harness correction, not an additional product defect.

### Independent review and limits

Independent review found no blocker. Cargo 1.86's documented artifact fields match the selector, jq's conjunction short-circuits ordinary non-artifact messages, quoted argument/assignment handling preserves paths, and unsuccessful Cargo/selection exits stop stage creation. Cached current artifacts remain valid; rejecting `fresh=true` would be incorrect. Missing or ambiguous records fail rather than falling back to stale guessed files.

The recording fixture itself emits artifact records, not the complete variety of normal Cargo messages; the actual 141-record selector smoke covers that integration boundary. Strict JSON parsing can fail closed if a future procedural macro emits non-JSON stdout; no current occurrence was established and no speculative parser fallback was added.

Real RPM requirements, actual WPE-enabled Debian/RPM/tarball contents and installation, complete real native packaging, and a Flatpak build were **not exercised**. Existing Flatpak web refusal remains deliberate. The exercised package fixtures and real Cargo selector are not substituted for those unperformed distribution checks.

## PR8R4-UI-1 — Web-page chrome advertised shortcuts owned by the document

**Kind:** Misleading user-visible UI hint, not a new keyboard-routing defect.

**Changed files and symbols/sections:** `crates/crikey-ui/src/native.rs::draw_status`; `docs/user-guide.md`, Configuration and Keyboard and query behavior.

The web route correctly leaves non-Escape keys with the document and disables launcher Ctrl+, interception. The shared footer still advertised `Settings  Ctrl+,` and `Up/Down navigate   Tab complete   Esc cancel`. Ctrl+, therefore went to the document rather than opening settings, and Tab navigated the document rather than completing a launcher result.

For web pages only, the footer now keeps the clickable Settings affordance without advertising Ctrl+, and displays `Tab navigate page   Esc close page`. Ordinary launcher/display-list hints and `show_hints` behavior remain unchanged. No shortcut or input policy was added.

The connected user guide no longer promises Ctrl+, or result Tab completion unconditionally. It scopes the launcher shortcut table to query/results, describes document-owned web keys and Escape page-close, and preserves the persistent Settings mouse route even with hints disabled. Configuration instructions and the legend now distinguish ordinary launcher hints from web-page hints. These complete the same misleading-shortcut contract correction, not an additional counted documentation issue.

Supporting consumer coverage in `crates/crikey-ui/tests/native_renderer.rs::a_web_document_keeps_the_footer_settings_route_without_leaking_chrome_clicks` clicks the rendered settings affordance with hints enabled and disabled, from a previous document hover and across three layout passes. It requires exactly one OpenSettings and no engine PointerButton for the chrome click. It does not assert incidental text. This behavior test is expected to pass before the wording correction; it is not a dishonest failing-before test of footer wording. Its coverage contribution is included in PR8R4-UI-3 rather than counted as another bug.

**Observed actual-window baseline:** the coordinator's synthetic engine-raster capture passed and visibly showed the old `Settings Ctrl+,` affordance. The native window/presentation was real, but the supplied raster was synthetic, not WPE content.

**Observed current integration:** Tab alone visibly focused the document button, Ctrl+, did not open host settings, and clicking Settings opened the host panel and retired the web cgroup. The actual WPE window visibly showed the corrected web footer. The separate final native-window capture also showed only `Settings`, without the misleading shortcut, and its pixel/event smoke passed. Its hover changed 252 pixel channels; entering the document and leaving either document or chrome restored the exact baseline, with one document move and one departure. The focused headless settings/chrome regression passed. This does not claim an inspected DOM Ctrl+, handler or hardware-touch input.

## PR8R4-UI-2 — Public PageSurface API docs excluded its new web rendering model

**Kind:** API documentation correction.

**Changed file and symbols:** `crates/crikey-ui/src/lib.rs::PageSurface` and its `frame` field documentation.

The docs still described every page as a display list and allowed pixels only in image nodes, despite the type now carrying a web declaration and separately received engine frames. Consumers were given the wrong rendering/ownership model.

The docs now describe mutually exclusive display-list and web declaration routes, the host-owned clipping rectangle, separately shared engine frames, and `frame` as either an accepted display list or a web declaration. No public signature, frame format, or production behavior changed. No wording test was added; source declarations and consumers are the evidence.

## PR8R4-DOCS-1 — Generic input and styling promises did not distinguish web pages

**Kind:** Public contract documentation correction.

**Changed files and exact sections:**

- `docs/plugin-author-guide.md`, Pages → The cycle: common request metadata versus display-list input batching; direct web pointer/key/IME delivery to the engine.
- `docs/adr/0020-plugin-pages.md`, Decision declaration-cycle paragraph and Consequences host-control bullet: private engine input, display-list versus engine hit-testing/styling, common clipping/Escape ownership, and the declaration's lack of screen-position fields.
- `docs/spec/crikey-spec-v1.md`, §32.3 input bullet and palette paragraph: web pointer/key/IME uses §32.11's private engine transport; display-list palette purpose is distinct from web documents controlling their own styling. The launcher palette does not automatically style them.
- `docs/architecture.md`, Processes prose/diagram and Threads table/page paragraphs: distinguish display-list nodes from optional supervised Linux engine pixels; add the launcher web manager and separate WPE host/content route; replace the all-pages/two-threads claim with UI, provider, web-manager and host reader/writer/engine-loop ownership.

The PR broadened pages to include web declarations but retained generic wording that every request carries every input event and that the host hit-tests/supplies styling for every page. The later web subsection was more specific and correct, but the shared cycle/consequences conflicted with it. Authors could expect web input in Plugin::page or automatic launcher-theme styling of HTTP(S) content, neither of which is supplied.

Evidence is the web branch of `native.rs::draw_page`, which paints the engine raster, routes private input and returns before node/focus/pointer/keyboard processing; `NativeApplication::web_command` and `web_frame_input`; CLI WebInput/WebIme handling and WebPageRuntime forwarding; private supervisor RawInput/ImeEvent payloads; HostState engine actions; and the C WebKitView/input construction. OpenSurface carries identity, dimensions, URL and storage mode, not launcher palette/nodes/screen position.

The architecture additions are grounded in `WebHostSupervisor::with_spawner`/`manager_loop`, the private socket payloads, and `engine.rs::{run,spawn_reader,spawn_writer}`. They preserve the existing modern/legacy JSON-lines versus native proto3 distinction and do not extend plugin-worker Landlock wording into a new blanket browser-sandbox promise.

No source behavior was changed. No new automatic theme bridge, input feature, screen-reader claim or stronger screen-position security guarantee was introduced. Documentation wording tests would not protect the consumer boundary, so none was added.

## PR8R4-DOCS-2 — ADR still said megabyte frames demanded shared memory

**Kind:** Architectural rationale consistency correction.

**Changed files and sections:** `docs/adr/0020-plugin-pages.md`, Why not pixels, concluding paragraph; `docs/adr/README.md`, ADR-0020 index row.

The new accepted web route sends bounded engine pixels over private native-protocol sockets. The preceding rationale still categorically said display lists alone kept ADR-0017 standing and that megabytes per frame would demand a shared region. That contradicted the accepted implementation itself.

The paragraph retains display lists' per-frame geometry advantage but states the actual arrangement: no shared-memory transport is implemented, and the distinct web route accepts up-to-2-MiB private-socket frames. The categorical shared-memory requirement was removed. Historical measurements, geometry limits and recorded performance observations were not rewritten.

The ADR index still described all embedded webviews as rejected. Its row now records bounded display lists or optional supervised Linux web surfaces and limits the rejected alternatives to in-process/child-window webviews. That is the same accepted-architecture consistency correction, not another finding.

Evidence is `MAX_WEB_FRAME_BYTES`, owned `WebFrame.rgba8` conversion, private Unix endpoint setup, and UI texture composition. This is not a performance regression finding and makes no new latency, throughput or frame-rate claim. No test was added for prose.

## PR8R4-UI-3 — Add actual-window hide/reactivation coverage for the existing touch boundary

**Kind:** Coverage improvement, not a new production touch defect.

**Changed files and symbols:**

- `crates/crikey-ui/src/native.rs::web_input_tests::native_touch_dispatch_precedes_ime_and_cannot_retarget_after_redraw`.
- `crates/crikey-ui/tests/native_renderer.rs::a_web_document_keeps_the_footer_settings_route_without_leaking_chrome_clicks` — chrome ownership coverage also supports UI-1.

The existing real-window touch test covered replacement and pending admission, but not a retained native window hiding and reopening before an old contact ended. The same ignored X11/GPU test now starts an admitted contact, hides through the actual application user-event path, reopens under a new session/surface, redraws and delivers the old contact end. It requires no replay of native or buffered-egui input, then requires exactly one fresh contact press/release on the new surface. It preserves one winit loop per test process.

The chrome regression checks that a document hover followed by a settings click remains exactly one launcher action rather than leaking a browser button press, including hints disabled and repeated layout passes.

No production touch routing was changed. This coverage protects the already corrected R3 behavior; it is not another instance of the historical defect. The window/GPU is real, but touch events are synthetic in-process winit events. It is not physical touchscreen, WPE, real IME-service or mixed-DPI hardware evidence.

**Observed added UI coverage execution:** the extended ignored X11 native hide/reactivation test passed, including a fresh-source focused rerun. The headless settings/chrome regression passed in both the full workspace and a focused run. No newly failing-before production touch behavior is claimed; these are coverage additions for already-correct behavior.

## PR8R4-contracts-1 — Pin cumulative allocation accounting through fragmented PageFrame envelopes

**Kind:** Missing security-boundary regression, not a new decoder production defect.

**Changed file and symbol:** `crates/crikey-native-protocol/tests/wire.rs::fragmented_page_frames_share_one_decode_allocation_budget`.

Existing tests exercised PageFrame fragment merging and generic allocation bounds separately. They did not pin a single cumulative decode budget through the newly added repeated PageFrame envelope merge path. Restarting the budget per fragment could allow a small wire input to materialize a large repeated-node allocation.

The added raw fixture contains 100,000 tag-32 PageFrame fragments, each adding an empty node. Its 500,000-byte input fits below MAX_FRAME_BYTES, while public Envelope::decode must refuse with DecodeBudgetExceeded. This tests real consumer refusal rather than decoder source text or internal call wiring.

**Observed:** this new focused test passed. Existing production accounting is consistent with the assertion; no decoder fix or failing-before claim is made. No mutation experiment resetting budgets was run. This does not claim arbitrary hostile-message fuzz coverage.

## PR8R4-contracts-2 — Pin independent storage admission on persistent stay frames

**Kind:** Missing permission-boundary regression, not a new permission production defect.

**Changed file and symbol:** `crates/crikey-plugin-model/src/permissions.rs::tests::staying_on_a_persistent_surface_still_requires_the_storage_grant`.

Existing admission tests covered navigating surfaces and ephemeral stays but omitted the no-new-navigation/persistent-storage combination. A future regression coupling storage admission to navigation could skip storage authorization on a stay; treating a storage grant as network authorization would also be wrong.

The new test calls the public admission function and proves that persistent stay without grants is denied specifically for storage, storage-only permission allows persistent stay without navigation, and the same permission does not allow a fresh persistent navigation. Production behavior, SDK defaults and permission semantics are unchanged.

**Observed:** this new focused test passed. This boolean admission regression does not by itself prove stateful first-open validation, manifest registration, browser storage persistence or full launcher admission. No failing-before production defect is claimed.

## PR8R4-X-1 — Unchanged native-provider pipeline check failed during centralized workspace verification

**Kind:** Outside-PR verification failure/status; no confirmed new root cause and no fix.

The first all-target workspace attempt failed unchanged `crates/crikey-app/tests/native_provider_pipeline.rs::a_native_answer_after_the_collection_window_still_reaches_the_user`. The observed publications were `[(Generation(1), true, 0), (Generation(1), false, 0)]`: pending with zero rows, then settled with zero rows from the `slow:400` fixture. The suite elapsed **65.49 seconds**. The failing test source is unchanged by this PR. The root cause is unconfirmed; unchanged source does not alone prove every integrated change irrelevant.

Narrow source triage, recorded in `local://pr8r4-supervisor.md`, found:

- The fixture constructs NativeProvider, QueryPipeline, NativeDriver and a view-model publication closure. It does not run the CLI, construct NativeLauncher, deliver Activated, open a page, install PluginActionRouter, or call either new web admission method. Thus it does not execute the new activation helper or the new app permission methods.
- With no performance overrides, the manifest supplies the existing **500 ms suggest-hard timeout**. The deliberate **400 ms** fixture delay leaves **100 ms** of scheduling/transport margin. Scheduling contention is a possible explanation **[INFERENCE]**, not an observed or confirmed cause.
- The existing **100 ms collection window**, **60-second late-publication wait**, **10-second startup handshake**, generic **5-second call** budget and **2-second shutdown** budget are distinct. The 500 ms manifest override is the effective suggest allowance here; the approximately 65.49-second suite duration is not a 60-second child suggest deadline.
- Settling without a row does not reveal the worker's error. No captured worker health/error, child stderr, per-call timing or resource measurements establish why the answer was absent. The triage rules out execution of those new helper/admission paths, not every conceivable indirect interaction.

A subsequent serial-test-thread attempt stopped at the audit-artifact contamination described below and was not a passing run. After removing generated artifacts for all 27 workspace packages, the complete fresh workspace passed: **2,559 tests, 155 suites, 5 ignored and 28 filtered**. The original native late-answer failure remains disclosed despite that later success. No unrelated source, deadline or assertion was changed. Neither the cache finding nor possible contention establishes the original failure cause.

## Complete changed-file inventory

This inventory covers the audit’s **15 changed implementation/test/public-documentation files**, not every path introduced by PR #8. The sixteenth changed file is this requested report, `docs/notes/pr8-bughunt-PR8R4.md`. Shared paths are listed once with their entries; the unchanged test tracked as PR8R4-X-1 is intentionally absent.

| File | Entries | Nature of change |
| --- | --- | --- |
| `crates/crikey-web-host/src/engine.rs` | ENGINE-1 | Authoritative bounded command draining, self-wake endpoint, native regression |
| `crates/crikey-cli/src/lib.rs` | supervisor-1 | Activation-session cleanup helper/caller and real owner/model regression |
| `packaging/linux/build.sh` | packaging-1 | Select successful Cargo artifact paths, matching usage comments |
| `packaging/linux/tests/build.test.sh` | packaging-1 | Cargo recording fixtures, fresh staged-byte comparisons and refusal cases |
| `crates/crikey-ui/src/native.rs` | UI-1, UI-3 | Web-specific footer hints; extend native hide/reactivation touch regression |
| `crates/crikey-ui/src/lib.rs` | UI-2 | PageSurface/frame API docs |
| `crates/crikey-ui/tests/native_renderer.rs` | UI-1 supporting coverage, UI-3 | Rendered settings affordance and chrome/input ownership regression |
| `crates/crikey-native-protocol/tests/wire.rs` | contracts-1 | Cumulative fragmented PageFrame allocation-budget regression |
| `crates/crikey-plugin-model/src/permissions.rs` | contracts-2 | Persistent stay/storage-only authorization regression |
| `docs/adr/0020-plugin-pages.md` | DOCS-1, DOCS-2 | Input/rendering ownership and private-pixel-transport rationale |
| `docs/plugin-author-guide.md` | DOCS-1 | Display-list versus web request/input cycle |
| `docs/spec/crikey-spec-v1.md` | DOCS-1 | §32.3 input transport and styling contract |
| `docs/architecture.md` | DOCS-1 | Both page forms; optional host/process diagram; launcher web-manager and host thread ownership |
| `docs/adr/README.md` | DOCS-2 | ADR-0020 index reflects accepted supervised Linux web route |
| `docs/user-guide.md` | UI-1 | Scope Ctrl+, Tab/query shortcuts and hints; preserve web-page Settings click and Escape |

No schema tags, wire format, SDK signatures, Cargo dependencies, C shim behavior, cgroup policy or platform-support expansion was introduced by this round. Historical reports remain unchanged. The permanent PR8R4 report itself is a coordinator-owned documentation integration, not an additional bug.

## Reviewed areas with no new confirmed defect

### Engine and safe-host helpers

The host reviewer examined all seven engine-independent modules: frame, IME, input, library exports, CLI parsing, output and state. No production fix was justified there. Review covered one-surface/direction/identity rules, URL and viewport bounds before engine actions, native-byte argument parsing, BGRA-premultiplied to RGBA-premultiplied conversion, padded mapping arithmetic, generation exhaustion, buffer recycling and shrink behavior, terminal output fencing, bounded/coalescing queues, pointer state validation, modifier/timestamp mapping, Unicode scalar and UTF-16 IME boundaries, composition ordering/cancellation and engine feature gating.

The engine reviewer checked native signal/resource lifetimes, callback reentrancy ownership, export mapping lifetime, initialization/teardown order, document-navigation admission, popup/download refusal, persistent/non-session cookie isolation, UTF-8 profile limits, input/scroll conversion and existing actual-host smoke scenarios. Prior fatal-load fixes were reviewed without broadening expected-error suppression or weakening termination.

These are source-review results, not new runtime executions. In particular, exact-version WebKit source establishes initial native focus and legacy multiclick derivation; neither apparent absence justified a speculative duplicate focus/click tracker. Legacy discrete-wheel magnitude/phase and real input-method limitations remain limits rather than newly claimed fixes.

### Launcher, application and supervision

Review covered exact-owner grant lookup, lazy startup, page declaration/update acceptance, display-list/web transitions, bounded supervisor commands/mailboxes, replacement cancellation, monotonic generations, malformed-frame refusal, terminal diagnostic recovery, endpoint lifecycle, executable discovery, persistent owner-path isolation, physical/inherited memory policy, safely resolved cgroup mount views, eligible ancestor/delegation selection, pre-exec attachment and bounded process-tree teardown.

No new cgroup policy change was made. The required memory/cgroup gate and the absence of an independent detached-descendant reaper after abrupt launcher death remain documented. First-frame fatal notices close the whole page through the CLI; a supposed permanent-spinner fix was not added. Startup ordering did not establish a missing first-Open diagnostic fix, so no speculative recovery was added.

### UI, protocol and SDK

Review covered surface/generation/byte validation before upload, texture bounds/cache eviction, clipping and coordinate conversions, logical viewport/input consistency, input chronology, native versus egui ownership, contact/device identity, late-contact rejection, composition reset, Escape reservation, canonical tags/defaults, fragment merge semantics, persistent stay mode, shared URL admission, premultiplied web pixels, Unix timeout limitations and SDK serving/conversion behavior.

A logical viewport preserved on a monitor-scale-only change is not itself a coordinate bug: input and raster painting use the same logical units. Sharper high-DPI engine raster policy would be additional scope, not an established correction. No redundant multiclick tracker, hardware-code guess, broader URL parser or protocol-wide refactor was added.

### Packaging and public documentation

Review covered workspace/lockfile consistency, empty default features, executable engine gating, Linux build-script target/feature detection and pkg-config/link ordering, native/supplied packaging modes, no-web replacement, ELF/library checks, Debian dependency scanning/fallback, RPM automatic requirements and intentionally no-web Flatpak behavior. Existing CI/release defaults do not establish engine-feature compilation or an engine-enabled native release artifact.

The original three public PR documents were checked for install/discovery, SDK examples, permissions, storage/navigation, loading/failure, cgroup/memory, input/IME, accessibility and measurement claims. Explicit follow-through updates the connected process/thread architecture, ADR index and user shortcut descriptions without expanding into unrelated documentation. Existing corrected storage-mode, non-UTF-8 persistent-profile, no-web Flatpak, download and launcher-death limitations were retained, not recounted as fresh findings.

## Final verification ledger

| Check | Status and evidence |
| --- | --- |
| New contract allocation-budget regression | **Passed**, coordinator-observed focused execution |
| New persistent-stay permission regression | **Passed**, coordinator-observed focused execution |
| Packaging regression on starting tree plus tests-only patch | **86 passed / 6 failed**, expected stale-artifact/refusal failures |
| Fixed packaging fixtures, final run with virtual environment first on PATH | **92 passed / 0 failed** |
| Packaging shell syntax | **Passed** |
| Exact production Cargo artifact selector on actual Cargo 1.86 output | **Passed**, 141 JSON records, configured native target, path containing spaces; not a complete packaging build |
| Baseline actual native-window footer capture with synthetic raster | **Passed**, old Settings Ctrl+, visible; not WPE content |
| Current native-window footer capture / pixel and event smoke | **Passed**; corrected Settings label, 252 hover-channel changes, exact restoration, one move and one departure; synthetic raster, not browser content |
| Engine real-socket/native regression, baseline | **Failed as expected**, accepted CloseSurface stranded |
| Fixed actual-WPE host suite | **Passed: 78 library + 3 CLI + 13 smoke tests**, including pressure regression |
| Supervisor owner/model regression | **Baseline failed as expected; fixed passed** in fresh full suite and focused execution |
| Extended ignored X11 native hide/reactivation coverage | **Passed**, real window with synthetic winit touch |
| Focused headless chrome regression | **Passed**, independently focused and included in full workspace |
| Full launcher → native SDK plugin → supervisor → actual WPE → UI smoke | **Passed**, manually inspected HTML/counter/input/click/Tab/settings pixels; 7-PID groups retired on hide/Settings, Escape retired reopened group, Quit CriKey exit 0; exact stale-Toggle browser race not claimed |
| Initial workspace format/check/clippy | **Passed** before clean rebuild; not final fresh-workspace acceptance |
| Initial workspace all-target tests | **Failed**, unchanged native-provider test; see PR8R4-X-1 |
| Subsequent serial-thread workspace attempt | **Failed in audit setup**, Python SDK path resolved to baseline checkout from cached build metadata; not a product defect |
| Fresh post-clean `cargo fmt --all --check`; `cargo check --locked -j 2 --workspace --all-targets`; `cargo clippy --locked -j 2 --workspace --all-targets -- -D warnings` | **All passed** |
| Fresh `cargo test --locked -j 2 --workspace --all-targets -- --test-threads=1` with virtual environment first on PATH | **2,559 passed; 155 suites; 5 ignored; 28 filtered** |
| Fresh `cargo test --locked -j 2 --workspace --doc` | **1 passed across 27 suites** |
| Native consumer and macOS/Windows optional-host checks | **Passed**, coordinator-observed; not a cross-platform WPE runtime claim |
| Actual delegated-cgroup checks | **3 passed** using the current container's prebuilt CLI test binary; not a newly rebuilt host-workspace gate |
| Actual engine-enabled distribution package contents/requirements/install | **Not exercised**; no claim of passing |
| Virtual-environment Python `-m compileall -q sdk/python crates/crikey-legacy-compat/python compatibility/test-plugins` | **Passed** |

The initial format check found only wrapping differences in three new assertions; the coordinator ran cargo fmt, and the workspace format gate passed. This is routine formatting integration, not another bug.

The initial Cargo-selector smoke's unsupported C-ABI `--help` invocation was removed as described under packaging-1; it must not be reported as either a selector failure or a successful full C-ABI functional smoke.

### Audit artifact provenance failure and fresh rebuild

The subsequent serial workspace run failed `python-host::the_worker_host_resolves_the_real_shipped_sdk`: its actual SDK root was `/tmp/crikey-pr8r4-baseline/sdk/python`, while the live checkout expected `/home/yichen/projects/crikey/sdk/python`. That exact path mismatch proves baseline/shared-target cached `CARGO_MANIFEST_DIR` contamination in this audit setup. It is not a product bug, not an additional PR8R4-X-2 finding, and not a successful fixed-tree verification.

Cargo 1.86 rejected the initial `cargo clean --workspace` command without making changes. The corrected command took the 27 actual workspace package names from `cargo metadata --no-deps --format-version 1` and supplied each as `cargo clean -p <name>`. It removed 3,583 generated files (3.1 GiB), retaining dependencies. Fresh format, check, Clippy, full serial workspace tests and documentation tests then all passed. No source assertion, product behavior or timeout was changed to address this setup failure.

The cache finding does not establish the earlier late-answer failure’s cause. Three actual delegated-cgroup checks also passed separately using the freshly built current container CLI test executable; that execution is distinguished from the host workspace. Of the five ordinary-run ignored tests, the X11 case and three cgroup cases were explicitly exercised; the unrelated fifth ignored case was not run.

Initial packaging fixtures did not explicitly force virtual-environment PATH. The final packaging run did, and all 92 checks passed; syntax and explicit virtual-environment Python compilation also passed. This keeps interpreter provenance honest instead of retroactively attributing the earlier fixture subprocesses to an unverified interpreter.

## Independent review record

- **Engine wake mechanism:** final frozen production patch approved; no new blocker; source-only review. `local://pr8r4-wake-review.md` records the analysis and final confirmation.
- **Packaging artifact selection:** no blocker; Cargo 1.86 artifact schema, jq predicate/short-circuiting and shell failure propagation reviewed. `local://pr8r4-packaging-review.md`. Its fixture-message coverage observation is distinguished from the coordinator's later actual-stream smoke.
- **Activation cleanup:** final reviewer found no issue; reachable stale-Toggle path, cleanup order, idempotence and late-owner fencing checked. The reviewer could not persist a local file with its tools; the final result was delivered directly to the coordinator. No runtime execution was claimed.

## Evidence boundaries and retained record

This audit does not prove absence of all bugs. Source confirmation, deterministic fixture tests, real Unix sockets, native windows, a separately running WPE host, full launcher integration, real hardware input and distribution installation are different evidence levels and must remain separate.

Do not present synthetic winit touch as hardware touch, a synthetic raster as WPE rendering, a recording executor as process-tree teardown, a successful Cargo selector as every package format built, or source-reviewed existing smoke coverage as newly executed. No claim is made of complete ibus/fcitx interoperability, surrounding-text correction, screen-reader integration, power-loss cookie durability, arbitrary default socket capacity, other-platform web support, new cross-architecture packaging or post-launcher-SIGKILL whole-tree reaping.

The independent final accounting review matched every hunk in all 15 changed source/test/public-documentation files to the 10 in-scope entries, with no omissions or duplicate counts. A separate final documentation review found no contradiction in the corrected page/process/thread/input/styling/transport contracts. Both were source-only reviews, not additional runtime executions.

The native before/after captures are retained in `/tmp/pr8r4-ui-before` and `/tmp/pr8r4-ui-after`; actual full-path browser captures are in `/tmp/pr8r4-fullpath`. Those are execution-environment evidence paths, not repository dependencies a reader must possess. The permanent regressions, public symbols, setup descriptions and observed results above make the findings self-contained. Temporary baseline trees, transfer archives, full-path plugin/XDG/cgroup fixtures, external build output and display services were removed after proof. No throwaway smoke program was added to the repository.
