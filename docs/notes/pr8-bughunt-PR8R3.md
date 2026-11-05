# PR #8 bug hunt — round PR8R3

## Scope, starting point and method

This is a fresh third audit of pull request #8, starting at commit `8e302771e15894bc1f8fd461107c9c2915690245` on `feature/page-web-surface`. The comparison base is `feature/page-image-canvas`. At the starting commit, the pull request changed 48 files. This was not a whole-repository audit: surrounding code was inspected only to understand the changed paths. The previous reports, `pr8-bughunt-PR8B.md` and `pr8-bughunt-PR8R2.md`, remain unchanged historical records; their fixes are not counted again.

Six implementation agents separately reviewed the browser engine/C interface, Rust host helpers, launcher/application/process supervision, UI, core/native protocol/permissions/Rust SDK, and Linux packaging/public documentation. They were instructed to fix confirmed problems in their assigned files, use only the repository virtual environment's Python, keep imports at module scope, and route naturally encountered cross-area findings through the coordinator. They did not expand the search into unrelated subsystems. The coordinator also reviewed the small workspace-member and lockfile additions. No language servers were configured.

Six independent review assignments checked the resulting changes: documentation accuracy, packaging, failed-load delivery/shutdown, persistent-path safety, touch routing, and complete change-to-issue accounting. Review corrections are included below, not hidden or counted as unrelated discoveries. The coordinator ran baseline reproductions, fresh-source compiler/test gates and actual-process/window/filesystem checks. Review conclusions and actual execution results are distinguished throughout.

There are **eight unique change-tracking entries** in this report: three runtime defects, one low-severity build-prerequisite ordering improvement, and four documentation corrections. One documentation correction completes a missed paragraph from an earlier round. This is not a claim of eight new runtime bugs. No audit can prove the absence of every possible bug.

## PR8R3-HOST-1 — A genuine browser load failure could be lost, and the failed host kept running

**Kind:** Runtime correctness and diagnostic delivery. Joint fix across engine output, engine lifecycle and launcher supervision.

**Locations:** `crates/crikey-web-host/src/output.rs`, `crates/crikey-web-host/src/engine.rs`, `crates/crikey-web-host/tests/engine_smoke.rs`, and `crates/crikey-cli/src/web_host.rs`.

### Context and failure

The browser host sends `LoadState` messages to the launcher. `Started` and `Finished` are snapshots that can be replaced by more recent state. A genuine `Failed` is different: the launcher treats it as fatal, closes the page and presents the native failure diagnostic. Expected replacement-navigation cancellation, policy interruption and preliminary media handoff are already filtered out by the C adapter and must remain nonfatal.

The output queue treated every load state as replaceable. If a real failure was queued while the writer was busy, a later navigation's `Started` or `Finished` could replace it. A subsequent `Gone(Closed)` could also clear it and leave only a generic close. The browser engine continued accepting commands and callbacks after reporting failure, so another navigation could occur before the failure was written. Even a directly connected client that received `Failed` did not get autonomous host termination: the process waited for a later close command or client disconnect.

Earlier fixes protected the C callback's load classification and some same-navigation ordering, but did not protect the already-queued notification from a later navigation. This is a different boundary from those previous fixes.

### Implemented fix and reasoning

1. `OutputBuffer::push` recognizes genuine `LoadState(Failed)` as terminal, like `Gone`. The first terminal message takes priority over obsolete queued state, survives queue closure, and cannot be replaced by later state, caret, frame or close messages. Displaced/rejected frame buffers are still recycled. The writer receives the terminal message before end-of-stream. A frame already being written can still precede the error; this does not promise preemption of an in-progress socket write.
2. The engine's load callback records a fatal-load result and quits immediately after queuing the actual diagnostic. Later callbacks and commands cannot restart the failed surface. After cleanup and worker shutdown, `run()` returns `RunError::EngineLoad`; the executable therefore exits unsuccessfully. The native reason remains in the protocol message, without copying its string into another error field. Expected cancellation/policy/media control flow remains nonterminal.
3. Independent review caught a consumer-side consequence of autonomous exit. The launcher processes queued input, resize or navigation commands before reading host output. A queued write could now fail because the host had already flushed `Failed` and exited. The old send-error path immediately discarded the socket and published a generic transport error, losing the buffered native reason. The launcher now checks that same reader for a bounded number of inbound diagnostic candidates before closing. Only a current-surface `Failed` or `Gone` can replace the original write error; stale owners, ordinary snapshots and unknown output cannot. If there is no eligible terminal diagnostic, the original send error remains the fallback. No failed write is retried, and teardown cannot write again through the still-live connection.

### Regressions and observed evidence

The two new output-queue regressions were installed without the implementation fix in an isolated copy of the starting commit. **Both failed.** One returned a replacement navigation's `Finished` instead of the original `Failed`; the other accepted a late frame rather than rejecting/recycling it. The fixed host-helper suite passed all 69 tests.

The actual-host regression serves an HTTP connection that closes without returning a response. An HTTP 500 page is deliberately not used: it can be a successfully loaded error document rather than a native load failure. The test observes the real request and real `Failed` message with the exact surface and requested URL, requires a nonempty library-supplied diagnostic, keeps the launcher connection open, then requires unsuccessful host exit and EOF without sending `CloseSurface`. At the starting commit, the host remained alive past the five-second bound and had to be killed. With fresh fixed artifacts, this regression passed in the full actual-WPE gate.

The existing native callback classification regression now also checks fatal/nonfatal state. It uses actual WPE surfaces and GLib error objects, calls the registered C failure handler, and retains exact controlled URL/detail assertions. Media 204, network cancellation 302 and policy interruption 102 remain nonterminal; genuine media 999, network 399 and policy 100 are fatal. This directly tests classification, not natural reachability of every native error code.

The launcher regression covers closed-peer queued navigation, resize, ordinary input and IME, with a `Started` prefix, stale-owner terminal messages, and current-owner failure/close. It requires exactly one specific fatal notice rather than both a generic and a specific notice, retired frame/caret state, one process termination and no additional live write. Missing-terminal and scan-limit cases retain the generic fallback.

`failed_control_resumes_partial_terminal_frames_until_the_diagnostic_deadline` additionally uses real Unix framing to split both a header and a body. The remaining bytes are released only after the actual reader reports a receive timeout, not after an assumed sleep. The same cached reader must resume and publish only the specific failure. A frame that remains incomplete exercises bounded generic fallback.

### Review corrections and limits

Review identified a regression-fixture deadlock: a test attempted to synchronously fill a 64-entry pair transport with more than 64 messages before starting its consumer. This is a test construction error, not a production output-queue defect. The over-cap fixture now uses Unix buffering instead. The first diagnostic-scan implementation also stopped at its first receive timeout, despite `FrameReader` preserving partial-message state for retry. It now resumes the same reader within its short scan deadline; only complete envelopes consume the message cap. These corrections are included in HOST-1 rather than counted as independent discoveries.

Focused compilation/execution also corrected two new fixture mistakes before accepting their results: the partial-frame fixture needed the `Message` trait imported at module scope to encode its envelope, and a payload-less default envelope encoded to a prohibited zero-length frame. The ignored-output case now includes a nonzero connection ID, making it a valid nonempty envelope without an actionable payload. These are supporting test corrections, not extra claimed production bugs.

The diagnostic scan is bounded and is not a general transport recovery feature. A terminal message beyond its finite scan/deadline, an incomplete terminal frame that never finishes, or a stale surface cannot replace the original control failure. Existing per-operation socket timeouts are not a claim of a whole-frame absolute deadline against a peer that continuously makes partial progress. No new write retries, network policy or error suppression were added.

## PR8R3-S-1 — Valid long plugin identities could not create persistent web storage

**Kind:** Runtime availability defect.

**Location:** `crates/crikey-cli/src/web_host.rs::persistent_storage_dir` and its filesystem regressions.

A persistent web surface gets a private directory derived from its exact plugin owner, including the native/modern namespace. Previously, every UTF-8 byte of that identity was hex-encoded into one filename. A 127-byte owner produces a 254-byte filename, but a 128-byte owner produces 256 bytes and exceeds the ordinary Linux 255-byte per-component limit. For example, `native.` followed by 121 ASCII `a` characters is a 128-byte owner, although the manifest/package ID itself is an otherwise valid 121-byte install component. Non-ASCII characters use multiple UTF-8 bytes, so character count alone does not avoid the boundary.

Such a plugin could use ephemeral storage but failed before spawning its persistent browser host. Truncating the name would be unsafe because distinct owners with the same prefix could share cookies and local storage.

The fix preserves every previously usable short-owner path unchanged. Longer identities use `_long/<hex components of at most 254 ASCII bytes>/profile`. `_long` cannot collide with the existing hex-only short names. The final `profile` component distinguishes complete identities from longer identities with the same encoded prefix. Every identity byte remains represented: no truncation, hash collision assumption, lossy Unicode conversion or new dependency. Every added directory uses the existing real-directory check and 0700 permission helper, including the storage root, `_long`, each encoded component and final profile.

Both new storage regressions failed against the starting commit with `File name too long (os error 36)`. They cover the 127/128/129-byte boundary, same-prefix owners, different suffixes, Unicode owners, distinct retained marker files, stable reopen paths, bounded components and private modes. A second test replaces each root/intermediate/leaf with a symlink and requires refusal without modifying an unrelated directory's entries, marker or 0750 permissions. This extends the previous round's checked-directory invariant; it is not a newly counted symlink vulnerability.

A separate executable smoke compiled the exact production storage-directory and private-directory function snapshots into a disposable Rust program. It recorded source hashes and ran on the real Linux filesystem outside the test runner. At the starting commit it created the 127-byte owner's directory, then exited with ENAMETOOLONG for the 128-byte owner. With the fix it created five isolated stores, reopened their markers, checked 0700 permissions and component sizes, and refused all five tested path-component aliases. It exited successfully. This smoke does not claim a complete launcher/WPE session with a long-ID plugin.

Linux's total pathname-length limit still applies. Checked private directories are not a general defense against a hostile process with the same user ID concurrently replacing ancestors. The existing WebKit restriction requiring the effective persistent profile path to be UTF-8 remains unchanged. No existing short-owner persistent data was moved.

## PR8R3-ui-1 — Deferred touch could arrive after text input or be sent to a replacement page

**Kind:** Runtime input ordering and surface identity defect.

**Location:** `crates/crikey-ui/src/native.rs`, especially `NativeApplication::window_event`, `web_touch_commands`, `WebPointerState::native_touch_input`, `web_frame_input` and native input regressions.

Native mouse, key and IME events are dispatched immediately. Touch had instead been preserved inside egui's input and translated on the next redraw. That difference was observable: `Touch(Start)` followed by an IME commit before redraw sent the commit first, so the tap intended to focus a field could occur too late. Translation also read the current surface at redraw, not the surface that owned the original contact. Replacing or admitting a document before redraw could deliver an old or pending touch to the new engine.

The fix dispatches original native touch immediately through the same application callback as other native input. It uses the contact/device identity, current physical-to-egui scale and current surface at that time. The deferred touch translator was removed; native redraw mode ignores buffered browser-input copies, including touch and synthesized mouse events. Those events still reach egui-winit for launcher chrome. The output uses a bounded two-command array rather than a new per-event allocation.

Single-contact mouse emulation is preserved. Secondary contacts cannot take over; captured movement can continue outside the document; End/Cancel release the correct contact; and a late end from a replaced surface cannot release a new surface's button. Touch cannot steal an existing mouse drag or phased wheel gesture. While a touch owns the emulated pointer, physical/synthesized mouse traffic cannot release or move that contact. After touch ends, a stationary physical mouse can resume using its separately retained physical position. Window departure still clears that position.

A delayed review after the first publication (`59e068c`) identified a remaining contact-admission error in that patch: a touch starting in launcher chrome claimed the single web contact before the document bounds were checked. It could change the browser pointer state without pressing in the page, move into the document later, and block a distinct valid inside contact. The corrected helper requires a finite position inside the document before changing contact identity, hover or pointer position. Moves and ends from an unaccepted contact return without mutation. Already accepted contacts still retain outside drag/release capture.

The new `native_chrome_touch_cannot_claim_or_release_the_document_contact` regression covers outside, NaN and infinite starts; unchanged hover/position; an outside contact moving inside; a distinct valid inside start; and the original outside contact ending without releasing the inside contact. The real-window dispatcher fixture now includes the same outside-to-inside sequence before its original touch/IME ordering assertions. Both checks failed on unchanged `59e068c` production code before the correction: the helper emitted a pointer move for the rejected start, and the native dispatcher allowed the chrome-started contact into the document. This correction remains part of PR8R3-ui-1, not an extra counted issue.

The new buffered-copy regression forces three egui passes and checks current, replacement and unadmitted surfaces. It failed against the starting commit because deferred touch emitted browser input. A real-window/GPU-surface dispatcher regression also failed at the starting commit: immediately after synthetic Touch then IME, the callback contained only the IME commit rather than pointer move, pointer press and commit. The freshly rebuilt fixed tests passed, including the explicitly invoked ignored X11 dispatcher case. The permanent lifecycle coverage also checks 2x scaling, secondary contacts, outside capture, End/Cancel, replacement, late end and mouse/wheel ownership. The older deferred-touch test was replaced by coverage at the actual immediate boundary, not deleted without replacement.

The dispatcher regression injects synthetic winit Touch values through the real application event handler; it does not claim a hardware touchscreen or real IME server. Its window, GPU surface, layout, raster upload and callback dispatch are real. Separately, an external harness launched the actual `NativeLauncher` under Xvfb and used xdotool plus captured pixels: Settings hover changed 456 pixel channels, moving into the admitted raster restored the footer baseline exactly, leaving from document/chrome also restored it, and the controlled sequence emitted exactly one browser move and one departure. Captures were inspected directly. The raster was deliberately synthetic, not a WPE document. This check protects the real OS mouse/chrome path from collateral touch-routing changes.

The supported behavior remains one contact emulated as a mouse, not true multitouch gestures. UI callback order does not guarantee a host consumes all queued input before simultaneous page teardown. Hardware touch, ibus/fcitx candidate UI and mixed-DPI monitor transitions were not exercised.

## PR8R3-packaging-1 — Missing ldd was reported only after release compilation

**Kind:** Low-severity build prerequisite ordering improvement.

**Locations:** `packaging/linux/build.sh::preflight`, `resolve_binaries`, and `packaging/linux/tests/build.test.sh`.

Web-enabled native packaging requires `ldd` to inspect the staged host's runtime libraries. The script promises to check required tools before building, but checked this particular tool only in binary resolution, after Cargo metadata and both ordinary and engine-enabled release builds. A minimal release environment without ldd therefore did substantial compilation before failing with information available from the beginning.

The `ldd` prerequisite check now runs in native-web preflight. The redundant late check was removed; the actual forced-C-locale ELF inspection and unresolved-library refusal remain unchanged. No-web mode and unsupported Flatpak-web refusal retain their prior behavior. This is distinct from the earlier WPE/pkg-config dependency preflight fix.

The new isolated-PATH fixture supplies every prerequisite except ldd, a recording Cargo tool and a real ELF stand-in. It requires the named missing-ldd diagnostic, unsuccessful result and no Cargo invocation. Against the starting script the suite reported 84 passes and one failure, specifically this new no-Cargo check. With the fix, syntax checks and all 85 packaging checks passed. This is a real invocation of the packaging script with controlled build tools; it is not a real engine release build or proof of package operation on every distribution. No package corruption or escaped unresolved dependency is claimed.

## PR8R3-C-1 — IME comments described the wrong engine units and empty-preedit ownership

**Kind:** Documentation consistency; no new runtime behavior change.

**Locations:** `crates/crikey-native-protocol/src/message.rs` comments for `ImeEvent`; `sdk/protocol/crikey/v1/plugin.proto` IME/cursor comments; and three companion comments in `crates/crikey-web-host/src/ime.rs`.

The wire uses Unicode scalar offsets, the launcher starts with UTF-8 byte offsets, and WebKit consumes UTF-16 offsets. An emoji can be four UTF-8 bytes, one scalar and two UTF-16 units. The previous round already fixed the actual C conversion, but protocol comments still claimed WebKit counted characters and assigned empty-preedit suppression to the launcher rather than the host's deferred-clear bridge.

The comments now describe all three boundaries: byte-to-scalar conversion in the launcher, scalar positions on the wire, and scalar-to-UTF-16 conversion in the native host. They also describe host handling of an empty clear before commit and preserve absent-versus-zero cursor semantics. This prevents a new producer from using engine units on the wire or double-converting offsets. No tags, fields, validation, events or adapter behavior changed.

The existing supplementary-preedit actual-WPE scenario passed in this round's full browser gate. That is compatibility evidence for the documented behavior, not a newly failing-before IME regression. No source-text or wording-pinning test was added. This entry documents the already-fixed PR8R2-E-2 behavior rather than counting it again.

## PR8R3-C-2 — Persistent stay declarations lacked a clear storage-mode warning

**Kind:** API documentation footgun; no change to defaults or storage policy.

**Locations:** `sdk/rust/src/builder.rs` (`WebSurfaceBuilder::stay`, `storage`, `build`); `crates/crikey-core/src/page.rs::PageWebSurface::storage`; canonical `plugin.proto`; author guide; and specification §32.11.

Both `navigate` and `stay` start with safe Ephemeral storage. A live surface cannot change its storage mode. An author can open correctly with `.storage(WebStorage::Persistent)`, then naturally call plain `WebSurfaceBuilder::stay(next).build()`. That declaration is Ephemeral; the launcher rejects the mode change and closes the page with a diagnostic. The public comments did not sufficiently explain that persistent callers must repeat Persistent on stays as well as navigation.

The SDK/core/schema/guide/spec now consistently state the fixed mode for the entire surface lifetime, the Ephemeral builder default, the required `.storage(WebStorage::Persistent)` on every persistent declaration, and the need to close/reopen to change mode. The build comment also no longer claims an empty URL can only come from `stay`: `navigate(generation, "")` represents the same stay operation. No API behavior was narrowed or made silently persistent.

The existing SDK page scenario explicitly repeats Persistent through a native-framed navigation and stay. Launcher tests enforce refusal of a storage change. Those existing behavioral checks are appropriate; a new source-comment assertion would not protect users. The warning does not imply that a builder can validate a previous surface or manifest permissions; that state belongs to the launcher.

## PR8R3-C-3 — The storage enum was incorrectly described as host-only

**Kind:** Protocol documentation correction.

**Location:** `crates/crikey-native-protocol/src/message.rs`, introductory web-enum comment before `WebStorageMode`.

The comment said the listed web enums never cross the plugin connection. Storage mode is a deliberate exception: it appears in plugin-facing `PageWebSurface.storage` as well as private `OpenSurface.storage_mode`. Treating it as host-only misleads SDK/codec authors about the native plugin declaration.

The revised comment names that exception and keeps input/load/gone enums scoped to private host traffic. It retains the crate's existing unknown-value-to-unspecified convention. No enum value, decoder, transport or permission changed. This does not mean plugins receive raw input, browser frames or private host commands. Existing native declaration round trips and SDK page tests exercise the shared storage field; no text-pinning test was added.

## PR8R3-packaging-2 — A remaining guide paragraph still recommended timers for web pages

**Kind:** Documentation correction; residual completion of PR8B-P8, not another independently discovered runtime timer bug.

**Location:** `docs/plugin-author-guide.md`, pages section, `The cycle` redraw paragraph.

The guide introduces both display-list and web pages, but the generic cycle text still told authors to set `redraw_after_ms` when a page changes without input. The implementation rejects every web declaration with a nonzero timer using `PageError::WebSurfaceWithRedraw`. Browser paints, not plugin timers, drive the embedded web surface. An author following that remaining paragraph could build a declaration the host refuses.

The paragraph now limits timer advice to display lists, explicitly requires zero for web declarations and explains engine-driven repaints. The specification already described the restriction correctly. The previous audit claimed the timer guidance had been scoped, but missed this live practical paragraph; this entry records that residual correction openly. Existing core scheduling/focus-refusal coverage remains the behavioral evidence. No timer feature, default or validation policy changed.

## Complete repository change inventory

Every implementation/documentation edit in this round maps to the entries above:

- `crates/crikey-cli/src/web_host.rs`: S-1 bounded storage path and two filesystem regressions; HOST-1 consumer diagnostic scan, three active-control failure branches, closed-peer matrix, real Unix partial-header/body timeout regressions and their fixtures.
- `crates/crikey-core/src/page.rs`: C-2 storage-lifetime comment.
- `crates/crikey-native-protocol/src/message.rs`: C-1 IME and C-3 enum-ownership comments.
- `crates/crikey-ui/src/native.rs`: ui-1 immediate touch translation/dispatch, pointer/contact ownership and finite inside-start checks, native redraw-copy suppression, comments/imports and four replacement/new regressions, including the extended native dispatcher case.
- `crates/crikey-web-host/src/engine.rs`: HOST-1 fatal-load outcome, quit/return behavior and native callback assertions.
- `crates/crikey-web-host/src/output.rs`: HOST-1 terminal failure preservation, documentation and two regressions.
- `crates/crikey-web-host/src/ime.rs`: C-1 three offset-unit comments.
- `crates/crikey-web-host/tests/engine_smoke.rs`: HOST-1 genuine-failure HTTP fixture and actual-process regression.
- `docs/plugin-author-guide.md`: C-2 persistent-stay warning and packaging-2 timer paragraph.
- `docs/spec/crikey-spec-v1.md`: C-2 storage-lifetime rule.
- `packaging/linux/build.sh` and `packaging/linux/tests/build.test.sh`: packaging-1 preflight and regression.
- `sdk/protocol/crikey/v1/plugin.proto`: C-1 IME units/ownership and C-2 storage comments.
- `sdk/rust/src/builder.rs`: C-2 stay/default/storage/empty-navigation comments.
- This report: the requested permanent issue/evidence record. Prior audit reports are intentionally unchanged.

No dependencies, lockfile values, protocol tags, permission rules, Python transport or unrelated subsystems were changed. Formatting is confined to the changed Rust sections. Throwaway baseline sources, generated executables and smoke evidence are outside the repository.

## Reviewed without a newly confirmed fix

- Engine/C ownership: stable callback state, borrowed strings/pixels, signal disconnection, backend/view ownership, SHM access/release, cleanup, bounded command/output paths, startup deadlines and process-global initialization. C source/header required no new edit.
- Host helpers: dimension/stride/length overflow, 2 MiB raster bounds, monotonic generations, buffer recycling, input validation, legacy button numbering, wheel units, focus clearing, IME operation ordering, cursor clamping, native-byte CLI parsing, single-surface state, URL admission and optional-engine build isolation.
- Launcher/app: exact-owner permissions, no shared-executor permission inference, first-stay refusal, owner/storage stability, stale generations, mailbox retirement, repeated URL suppression, lazy host startup, required cgroup attachment, inherited memory ceilings, mandatory cleanup, no numeric signaling after root reaping, private endpoints and diagnostics.
- UI beyond ui-1: valid raster admission, identity/generation texture memoization and eviction, device texture-size refusal, measured geometry/caret, resize identity, native/fallback mouse/key/IME routing, Escape reservation, hover departure and multipass command effects.
- Contracts: URL bounds/host admission, display-list/web exclusion, raw protobuf fragment merge, unknown/default behavior, allocation budgets, native transport timeouts, permissions, Rust SDK framing and safe defaults. Native and Python transports remain distinct.
- Packaging/docs: explicit web opt-in, source/supplied host modes, staging replacement, ELF checks, WPE dependency preflight, Debian dependency policy, RPM inclusion, Flatpak's explicit lack of web support, manifest examples and truthful platform/accessibility restrictions.

These are audit coverage statements, not claims that each possible timing or environment was exercised.

## Unchanged or unverified boundaries

No speculative fix or new guarantee was added for the following:

- Full RFC/WHATWG URL parsing; inherited unrelated protobuf merge sites; configured cross-target package artifact selection; pre-existing swapchain/adapter limits or display-list resize behavior.
- Wayland SHM format/byte order beyond the measured little-endian ARGB/BGRA path. The upstream backend can forward raw SHM, but no supported-engine non-ARGB producer failure was established in this audit.
- Upstream legacy libwpe middle/right held-motion inconsistencies; a remapping without a current consumer-visible reproduction could break correct button masks.
- Browser title/history notifications not consumed by the launcher. No new live-title promise was inferred.
- Hardware touch, real ibus/fcitx candidates/reconversion/delete-surrounding, native web accessibility-tree bridging, mixed-DPI hardware transitions, sanitizers/Miri, deliberate PID reuse, memory-pressure/OOM testing, or hostile same-user filesystem races.
- Arbitrary subresource/network sandboxing. HTTP(S) document policy and canceled downloads are not a general network sandbox.
- Guaranteed whole-process-tree cleanup after abrupt launcher death. Existing documentation already distinguishes launcher-owned cleanup, root-only parent-death behavior and independently enforced kernel memory limits.
- Universal distribution/runtime compatibility or real RPM/Flatpak installation. Cross-target checks establish build isolation, not a browser engine on macOS/Windows.

## Reproduction and execution provenance

Before-fix evidence came from an isolated source copy of the starting commit with regression tests only, not a relabeling of fixed code. Its observed failures were: two lost-output tests; two long-owner storage tests; the buffered-touch test; the actual X11 touch-before-IME dispatcher test; the actual browser autonomous-exit test; the real-filesystem 128-byte-owner smoke; and the missing-ldd packaging check. Comments-only findings have no invented failing-before runtime claims.

The delayed outside-contact regressions used the first published round-three implementation, `59e068c`, rather than the original round-three starting commit. They were added and run while production touch code was still unchanged, then the implementation was corrected. This distinction keeps the additional failure evidence separate from the earlier isolated-baseline reproductions.

Sharing Cargo target directories between the isolated baseline and current tree exposed a build-cache pitfall: archive-preserved source timestamps were older than the just-built baseline executable. An initial nominally fixed WPE/UI attempt therefore ran old generated binaries. Matching source hashes, executable timestamps and absence of the new engine error string established this; the old UI test name likewise appeared in the cached run. Only affected generated package artifacts were cleaned, and current sources were rebuilt. No source assertion was weakened, no deadline increased and no production workaround added. The fresh actual browser gate and both fresh UI checks passed. The stale-artifact attempts are not counted as verification of current code.


## Final integrated verification

All commands below were actually exercised by the coordinator. Host Cargo uses Rust 1.86; Python commands use the repository virtual environment (Python 3.14.4), with that environment first on PATH for the workspace suite. Heavy builds were bounded to two jobs on the host and one in the WPE container.

| Check | Observed result |
| --- | --- |
| `cargo fmt --all --check` | Passed on final Rust source |
| `cargo check --locked -j 2 --workspace --all-targets` | Passed |
| `cargo clippy --locked -j 2 --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --locked -j 2 --workspace --all-targets` | 2,555 passed; 5 ignored; 28 filtered; 155 suites |
| `cargo test --locked -j 2 --workspace --doc` | 1 passed; 27 suites |
| CLI `--lib web_host::tests` focused gate | 42 passed; 3 delegated-cgroup tests ignored in the ordinary host environment |
| Engine-disabled web-host suite | 69 passed |
| WPE container: `RUSTFLAGS="-D warnings" cargo test --locked -j 1 -p crikey-web-host --features engine` | 78 library, 3 CLI and all 13 actual-host scenarios passed on synchronized final source |
| Prebuilt CLI test executable in a private mount namespace with real read-only full cgroup view plus writable delegated bind; `web_host::tests --ignored --nocapture` | All 3 detached-child, already-exited-root and late-descendant cleanup cases passed |
| Native UI input tests | 7 passed; the X11-specific case ignored in the ordinary run |
| Explicit Xvfb run of `native_touch_dispatch_precedes_ime_and_cannot_retarget_after_redraw` with `--ignored --exact` | Passed with a real native window/GPU surface and synthetic Touch/IME dispatch |
| Actual `NativeLauncher`, xdotool, native-window pixel/event smoke | Passed; accepted synthetic raster, not browser content |
| Exact production storage-helper executable on a real filesystem | Passed for five owners and five symlinked path components; starting source failed at owner byte length 128 |
| `bash -n packaging/linux/build.sh packaging/linux/tests/build.test.sh` | Passed |
| `bash packaging/linux/tests/build.test.sh` | 85 passed; 0 failed |
| External native consumer: `cargo check --locked -j 2 --manifest-path compatibility/native-conformance/Cargo.toml --all-targets` | Passed |
| Optional host `--all-targets --all-features`, target `aarch64-apple-darwin` | Passed without host-installed WPE |
| Same optional-host check, target `x86_64-pc-windows-gnu` | Passed without host-installed WPE |
| Virtual-environment `python -m compileall -q sdk/python crates/crikey-legacy-compat/python compatibility/test-plugins` | Passed |

The full workspace result includes the final diagnostic-timeout implementation and corrected fixtures. Its five ignored cases are not silently counted as passes. The native X11 case and all three delegated-cgroup cases were explicitly exercised separately; one unrelated ignored test was not run. The cgroup setup invokes a prebuilt test executable, not Cargo inside the constrained mount namespace. Only the test subprocess moves into the fixture's launcher leaf; the outer cleanup shell stays outside. Production code does not move the launcher or enable existing controllers, and descendant escalation uses the held private cgroup. Browser-enabled testing is a separate gate because the ordinary workspace configuration deliberately does not link WPE.

After the delayed outside-contact correction, formatting, workspace check, Clippy, all workspace targets and documentation tests were rerun successfully. The expanded native input suite passed seven tests, and the explicitly invoked real-window dispatcher test passed with its new chrome-contact sequence. The additional passing workspace test accounts for the increase from the first publication's 2,554 to 2,555. The WPE engine, host helpers, kernel cleanup and packaging source paths were unchanged by this final UI-only correction; their recorded runtime gates are retained rather than presented as new executions.

The actual NativeLauncher/xvfb/xdotool pixel-and-event smoke was also rerun after the contact correction and passed with the same 456 changed hover channels, exact baseline restoration and one document move/one departure. It still uses an accepted synthetic raster, not browser content.

The actual browser environment uses WPE WebKit 2.52.6 in the existing Linux container. The host has no WPE development package, so browser-enabled checks were not falsely attributed to the ordinary host gate. Headless EGL/software-GPU and missing accessibility-bus warnings appeared, but the browser scenarios still required actual native messages, process outcomes and rendered document pixels. The feature-enabled suite was compiled with Rust warnings denied; this is distinct from feature-enabled Clippy, which was not run. No new performance benchmark or sanitizer result is claimed.

The native visual evidence is retained in `/tmp/pr8r3-native-verified` and the final contact-correction run in `/tmp/pr8r3-native-contact-final`; production filesystem smoke source hashes/results are in `/tmp/pr8r3-storage-before/evidence.json` and `/tmp/pr8r3-storage-after/evidence.json`. These paths describe this execution environment, not files a reader must already possess. The permanent regressions and source locations above describe the checked behavior. Isolated baseline source trees and transfer archives were removed after reproduction; no throwaway smoke program was added to the repository.
