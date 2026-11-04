# PR #8 bug hunt — round PR8R2

## Scope and method

This is a fresh second audit of **pull request #8 only**, starting at `1e5ee32769cedcf271d242635f455df662f1cb88`, against the PR base `feature/page-image-canvas`. It is not a whole-repository audit. The earlier report, `docs/notes/pr8-bughunt-PR8B.md`, remains a record of the previous round; its fixes are not counted again here.

Six implementation agents owned separate sections: browser engine/C interface; host helpers/build configuration; launcher/application/process supervision; UI; core/protocol/permissions/Rust SDK; and Linux packaging/documentation. Six additional reviews checked the resulting patches in those sections. Agents were instructed to use the repository virtual environment's Python, keep imports at module scope, and route naturally encountered findings outside their ownership to the coordinator rather than expanding their search. The coordinator ran the integration gates and actual-runtime checks. No language servers were configured in the project.

Each item below has a label unique to this round. Labels cover different kinds of work: behavioral defects, latent safety or validation defects, a small allocation improvement, additional end-to-end coverage, and a documentation correction. They must not be read as a count of independent production crashes. Related changes at different boundaries are explicitly linked.

**Completion:** This report contains 19 labeled entries. All confirmed in-scope fixes are included, including the review corrections and the download-fixture race found during final execution. Remaining inherited and unverified boundaries are stated separately.

## Browser engine and C interface

### PR8R2-E-1 — Persistent profiles lost persistent cookies on reopen

**Location:** `crates/crikey-web-host/csrc/ck_web_shim.c`, network-session creation; `tests/engine_smoke.rs`.

**Context:** A plugin needs separate permission to request persistent web storage. Its approved profile directory must keep the persistent browser state separate from other plugins. An ordinary persistent HTTP cookie, such as a login cookie with an expiry, should survive closing and reopening that profile. A session-only cookie should not.

**Problem:** Creating a persistent `WebKitNetworkSession` with data/cache directories does not automatically select persistent cookie storage. WebKit's cookie manager defaults to in-memory cookies. The code configured the former but not the latter. Local storage could survive while login cookies disappeared, which made the profile only partly persistent.

**Fix and reasoning:** Configure SQLite cookie storage at `cookies.sqlite` inside the approved profile before creating/loading its web view. The ephemeral branch never enables persistent cookie storage. No shared default browser cookie file is used, and session cookies retain their session-only meaning.

**Verification:** The new real-WPE scenario receives actual HTTP `Set-Cookie` headers for a persistent cookie and a session cookie, writes local storage separately, closes the host, then starts a fresh host using the same profile and origin. The reopened document checks persistent cookie/local storage survival and session-cookie absence through its rendered result. Distinct persistent and ephemeral profiles must see empty state. A separate scenario verifies ephemeral state disappears on reopen. Before the fix, the same-profile check painted the failure color instead of the required success color.

**Boundary:** The fixture allows a bounded settling period before close. This does not prove persistence after an arbitrary power loss or forced termination during a storage write.

**API evidence:** [CookieManager.set_persistent_storage](https://wpewebkit.org/reference/stable/wpe-webkit-2.0/method.CookieManager.set_persistent_storage.html), [NetworkSession.new](https://wpewebkit.org/reference/stable/wpe-webkit-2.0/ctor.NetworkSession.new.html).

### PR8R2-E-2 — Supplementary-character IME offsets reached WebKit in the wrong units

**Location:** `ck_web_shim.c`, input-method preedit state and `get_preedit`; `ck_web_shim.h`; engine smoke coverage.

**Context:** The private protocol expresses preedit cursor positions in Unicode scalar values. A supplementary character such as an emoji is one scalar but two UTF-16 units. WebKit's editing ranges use UTF-16 units.

**Problem:** The C adapter returned scalar counts as cursor and underline offsets. WebKit consumes those values directly as editing ranges; it does not supply a missing scalar-to-UTF-16 conversion. For `😀中`, a scalar cursor at 2 should be a UTF-16 cursor at 3. The old adapter supplied 2, potentially locating the caret inside a surrogate pair and shortening the underline.

**Fix and reasoning:** Preserve the wire contract. When preedit text changes, scan its valid UTF-8 once and cache the clamped UTF-16 cursor and full UTF-16 length for WebKit. Reset both fields when clearing state. This avoids another text allocation and puts the conversion at the API boundary that needs it. The header states the distinction.

**Verification:** `supplementary_preedit_uses_utf16_engine_offsets_and_commits_expected_text` focuses a real HTML input, sends supplementary-character preedit, and checks the document's actual value and `selectionStart` through rendered success/failure pixels. It then commits different text and requires the exact final value. Before the fix the preedit check painted failure. The test name was corrected during review: final value is evidence about the resulting text, not an explicit count of commit events.

**Boundary:** The test does not visually measure underline length, exercise a real ibus/fcitx session, or establish full IME interoperability.

**Source evidence:** [WebKit InputMethodFilter.cpp](https://github.com/WebKit/WebKit/blob/main/Source/WebKit/UIProcess/API/glib/InputMethodFilter.cpp), especially `preeditChanged` and its editing-range construction.

### PR8R2-E-3 — Native-byte profile paths crossed a UTF-8-only engine API

**Location:** `crates/crikey-web-host/src/engine.rs`, persistent startup; C header; author guide and specification.

**Context:** Unix paths need not be UTF-8. The CLI intentionally preserves native filename bytes. That parsing guarantee does not mean every downstream library can accept those bytes: WebKit's session directories and persistent-cookie filename APIs require UTF-8.

**Problem:** Persistent startup passed raw non-UTF-8 bytes into that API without a supported conversion or early refusal. The approved directory's identity could not be promised across this unsupported boundary. This report does not claim a specific fallback directory, corruption, or crash was observed.

**Fix and reasoning:** Reject a non-UTF-8 effective persistent profile before initializing WPE or creating that profile directory. Return `Gone(StartupFailed)` with the UTF-8 requirement. Do not convert the path lossily. An ephemeral request has no effective persistent path and remains usable even if an unused configured directory contains native non-UTF-8 bytes. Document the persistent-path restriction while retaining lossless CLI parsing.

**Verification:** The actual-host scenario first paints an ephemeral page with the raw-byte configuration, then requests persistence and requires structured startup failure, its UTF-8 diagnostic, unsuccessful exit, EOF, and no created unusable profile. Before the fix the persistent request reached the terminal-message deadline rather than returning that early refusal.

### PR8R2-E-4 — The safe public engine entry point did not enforce its process-global lifetime

**Location:** `engine.rs`, `EngineLoop` construction and public `run()` documentation.

**Context:** Normal launcher use starts a separate host process for each engine lifetime. The crate also exposes a safe Rust entry point. Its C backend keeps process-global loop/source/loader state.

**Problem:** Two safe callers could finish separate opening handshakes and both enter `ck_web_init`. Initialization begins by cleaning global state, so the second caller could remove or destroy the first caller's running loop. Sequential reuse could also move thread-affine state to a different thread. This is a latent public-API safety problem, not an observed ordinary-launcher crash.

**Fix and reasoning:** Atomically claim the process's single initialization attempt before entering C. The claim is permanent, including failed initialization. Concurrent or subsequent calls return `EngineAlreadyStarted` and a surface-specific structured startup failure without touching C. A retry requires a fresh process, matching normal launcher operation. Successful ownership still performs cleanup exactly once.

**Verification:** The engine-enabled `wpe_process_lifetime_and_native_load_failure_boundaries` regression holds the first engine lifetime while a second thread invokes the real public entry point over framed transport. It requires the explicit refusal and diagnostic. The same single-initialization fixture exercises native load-error boundaries described in E-5. The old global initialization/cleanup route was established from source; no sanitizer proof or unsafe concurrent-before execution is claimed.

### PR8R2-E-5 — An expected media-document handoff was treated as a fatal load

**Location:** `ck_web_shim.c::ck_web_on_load_failed`; native callback regression and unsupported-document recovery fixture.

**Context:** WebKit can report a preliminary media load failure because it will handle the media through a new load. Its `WEBKIT_MEDIA_ERROR_WILL_HANDLE_LOAD` code is expected control flow, unlike a genuine page-load failure.

**Problem:** The adapter already excluded navigation cancellation and policy interruption but not this media handoff. A supported direct-media navigation could therefore produce `Failed` and close the page before the replacement load.

**Fix and reasoning:** Exclude exactly this documented media error from fatal-load reporting. Keep genuine errors unchanged. This is not generic error suppression.

**Verification:** The original WAV completion fixture was invalid for this installed engine. After supplying the initially missing GStreamer plugins, native diagnostics established that the engine sniffed `audio/wave`, rejected it as an unsupported document, reported policy interruption 102, and produced Started/Finished with an empty URL plus a blank frame. Neither before nor after timeout reproduced error 204. That fixture was replaced, not ignored or given a longer deadline.

The deterministic native regression creates real WPE surfaces and real GLib `GError` objects, invokes the actual signal callback, and inspects Rust callback/output state. Expected media 204, cancellation 302 and policy interruption 102 must not emit failed load state. Genuine media 999, network 399 and policy 100 errors must emit failure. The existing callback is link-visible within the private C interface to exercise this boundary directly; production signal registration uses the same callback. A separate actual-host scenario serves an explicitly unsupported MIME type, requires nonfatal handling, then navigates to an ordinary HTTP document and requires its exact pixels. Both focused scenarios passed before the final full engine gate.

**Boundary:** These checks prove native error classification and continued navigation after unsupported content, not successful direct-media-document playback or natural error-204 reachability on this engine. Media support depends on the installed engine/GStreamer stack.

**API evidence:** [WebKit MediaError](https://wpewebkit.org/reference/stable/wpe-webkit-2.0/error.MediaError.html) describes this code as a preliminary failure followed by a new load; WebKit's own WebView failure handler excludes it.

### PR8R2-E-6 — Added actual-host coverage for malformed startup URLs

**Classification:** Additional cross-layer coverage, not another independent URL bug.

**Location:** `crates/crikey-web-host/tests/engine_smoke.rs`.

The underlying validation fixes are PR8R2-C-2 and PR8R2-H-1. Their safe model tests alone did not prove the executable returned a usable terminal response. The new scenario sends direct framed opening commands containing hostless authorities and over-limit URLs. It requires `ProtocolViolation`, a nonzero host exit, and EOF, rather than a blank policy-ignored page or a later browser failure. Before the fixes, the actual host reached its terminal-message deadline on malformed input. This test connects admission to executable shutdown without introducing another URL parser.

### PR8R2-E-7 — The download regression required a request that cancellation could legitimately prevent

**Classification:** Confirmed regression-fixture race, not a new failure of production download cancellation.

**Location:** `crates/crikey-web-host/tests/engine_smoke.rs`, download-test HTML.

**Problem:** The fixture used an anchor with the HTML `download` attribute, then required the server to observe `/download`. That attribute initiates an explicit download before its response arrives. The engine's synchronous download-started cancellation can win before the request reaches the server. A correct, earlier cancellation therefore caused a request-channel timeout. The full engine suite exposed this race after the other fixes.

**Fix and reasoning:** Remove the attribute and let the real server response's `Content-Disposition: attachment` initiate the download. The request must then have arrived before the cancellation boundary can execute. Keep the observed request, successful parent-page paint, and absent destination-file assertions; do not extend a deadline or suppress the failure.

**Verification:** The final real-engine suite exercises this response-driven attachment route. The earlier suite's download request timeout is recorded as the reason for this fixture correction, not as a production download escape.

## Protocol, core model, permissions, and SDK

### PR8R2-C-1 — Outer protobuf fragments still discarded web declarations

**Location:** `crates/crikey-native-protocol/src/message.rs`, Envelope field 32; wire regressions; canonical schema comments.

**Context:** Protobuf permits repeated fragments of the same singular message-valued oneof member. Those fragments merge. A valid sender can put generation and web URL in one `Envelope.page_frame` occurrence, then storage mode and title in another. The previous round fixed merging inside `PageFrame.web` but not the outer envelope through which real consumers receive it.

**Problem:** Each outer field-32 occurrence decoded a new `PageFrame` and replaced the previous one. The URL and generation could disappear. A later title-only fragment could erase the web declaration. Splitting drawn nodes and a web declaration between fragments also hid an invalid mixed frame instead of allowing validation to refuse it.

**Fix and reasoning:** Use the existing budget-aware same-member merge operation for field 32. Omitted fields survive, explicit defaults overwrite, unknown fields remain, and malformed fragments still fail. A different oneof member still replaces the previous member. This shared boundary necessarily also corrects old PageFrame scalar/node merging; unrelated older message-replacement sites were not migrated.

**Scope distinction:** Earlier PR8B-X1 listed outer PageFrame replacement as inherited and unchanged. This round followed the PR-added web declaration through that boundary and demonstrated actual new-feature data loss. The narrow fix is included for that end-to-end web behavior, not as a whole-codec rewrite.

**Verification:** Independent raw-wire regressions check URL/storage/generation/title/unknown-field merging through conversion to the core consumer, explicit defaults, empty fragments, mixed-frame rejection, different-oneof replacement, wrong wire types, and truncation. Before the fix, the generation became 0 instead of 7 and a split mixed frame validated successfully instead of returning `WebSurfaceWithNodes`.

### PR8R2-C-2 — A nonempty authority was mistaken for a nonempty document host

**Location:** `crates/crikey-core/src/page.rs` and root export; Rust SDK coverage/comments; native wire coverage/schema; host state integration.

**Context:** An HTTP URL authority may contain user information, a host, and a port. A nonempty authority is not sufficient: `https://@/`, `https://:443/`, `https://user@:443/`, and `https://[]/` have no host.

**Problem:** Core and the host's duplicate validator accepted these forms. SDK building and wire conversion could succeed before later engine policy ignored/refused the navigation. Initial page admission could incur process startup without a usable document address.

**Fix and reasoning:** Export the bounded, zero-copy `crikey_core::validate_web_surface_url` helper and use it for both core declarations and host commands. Identify the host after user information and before a nonbracketed port, or inside brackets. Require a nonempty host. Preserve existing case-insensitive HTTP(S), 8,192-byte, control-character, and authority-whitespace rules. Core's empty stay declaration remains a separate allowed operation; empty host commands remain invalid. SDK/schema comments state the host requirement.

**Verification:** Core, SDK, wire, and host-command tests reject the absent-host forms. Positive cases retain credentials with a port, bracketed IPv6, and internationalized names/paths. Before the fix, the wire consumer returned successful validation for `https://@/`. Real executable failure behavior is covered by PR8R2-E-6.

**Boundary:** This is not a complete RFC/WHATWG URL parser. Port ranges, nonempty malformed IPv6, IDNA and other complete browser grammar remain browser concerns. The fix does not invent an ASCII-only request-URI policy.

## Engine-independent host helpers and build integration

### PR8R2-H-1 — Direct host commands bypassed the existing URL byte limit

**Location:** `crates/crikey-web-host/src/state.rs`, crate manifest, workspace lockfile, and module documentation.

**Context:** Validated plugin declarations already had an 8,192-byte URL limit. The host has its own framed `OpenSurface` and `Navigate` commands and must enforce its boundary independently.

**Problem:** The host's duplicate validator omitted the byte limit. An 8,193-byte URL could become an engine action even though the core model refused it. Transport framing still had a larger allocation bound; this was validation divergence, not unlimited transport allocation.

**Fix and reasoning:** Reuse the core helper from PR8R2-C-2 through a direct `crikey-core` dependency and refresh the lockfile. Retain the host's control-character error precedence. Review caught a new avoidable allocation for unsupported schemes: the core's detailed scheme error owns a string that the host immediately discards. A small HTTP(S) guard preserves allocation-free host refusal without duplicating the full validator. Module documentation now accurately assigns protocol/input/frame/IME work to Rust and engine-specific navigation/download/storage policy to the C adapter; it no longer says C contains no policy.

**Verification:** Tests check helper-level acceptance at exactly 8,192 bytes and command-level rejection at 8,193 bytes through both opening and navigation, rejection without claiming a surface, successful recovery after an invalid opening, and control-character diagnostic precedence. The old-host regression accepted the oversized opening and failed its expected refusal. The same helper migration carries C-2's host-presence fix; that root cause is not counted twice.

### PR8R2-H-2 — IME clear/commit paths allocated temporary vectors unnecessarily

**Location:** `crates/crikey-web-host/src/ime.rs`.

**Classification:** Small allocation improvement and simplification; not a semantic composition bug or a measured latency claim.

**Problem:** Empty preedit created a capacity-three vector before returning no operations. A composing commit allocated its destination vector and another temporary vector for the fixed composition-end operations, then immediately moved the latter into the former.

**Fix and reasoning:** Return an allocation-free empty vector for deferred empty preedit. Represent the private fixed end sequence as a three-element array and extend the already-capacious commit destination directly. Flush/cancel convert that array into the result vector they actually need. Public APIs and operation order remain unchanged.

**Verification:** Existing IME sequence tests cover start/change, deferred clear, commit-before-finish, cancellation without commit, idempotence, scalar cursor handling and restart. Review checked that all downstream operations are still consumed in order. No allocator benchmark or timing improvement is claimed; the removed allocation requests are visible in the implementation.

## Launcher and process supervision

### PR8R2-L-1 — The first cgroup mount could hide a usable delegated view

**Location:** `crates/crikey-cli/src/web_host.rs`, cgroup discovery, inherited-memory policy and lease selection.

**Context:** Web hosting requires usable delegated cgroup-v2 controls. Linux can expose the same hierarchy through multiple mount views: an earlier read-only full view and a later writable delegated bind mount are valid arrangements.

**Problem:** Discovery returned the first safely resolved view before checking whether a lease could be created there. Startup then refused supervision without trying the later writable view. Merely selecting the later mount would introduce another risk: ancestors above its bind root can contain stricter memory limits visible only through the full view.

**Fix and reasoning:** Retain all safely resolved cgroup2 views. Compute the original launcher's minimum inherited memory limit across all of them, then attempt proper-ancestor lease creation in each view. Preserve path containment, symlink/traversal checks, no-internal-process restrictions, migration permission, pre-exec attachment and mandatory memory/kill controls. Keep the existing memory floor, cap and physical-memory fraction. Production still moves no existing process and enables no controller.

**Verification:** Deterministic tests model a read-only full view followed by a usable delegated view and require the selected host limit to retain a stricter ancestor visible only in the first. A below-floor hidden ancestor must refuse before any child creation. A private mount-namespace smoke creates the real read-only/writable arrangement and runs the three real detached-child cleanup scenarios; its actual results are recorded below.

### PR8R2-L-2 — Persistent-store symlinks could redirect owners and change unrelated permissions

**Location:** `web_host.rs`, persistent storage and private endpoint directories; shared private-directory helper in CLI `lib.rs`.

**Context:** Each plugin's full identity is encoded into its own approved persistent directory. A directory check must not turn an existing symlink into an accepted isolated store or change the symlink target's permissions.

**Problem:** A web-specific duplicate helper used recursive creation followed by chmod, then canonicalized afterward. It followed an existing owner-directory or storage-root symlink. This could merge stores or chmod an unrelated target. The common storage root was not itself restricted to 0700.

**Fix and reasoning:** Remove the weaker duplicate and reuse the existing checked helper for both root and owner leaf. It rejects a symlink leaf before chmod. Restrict root and owner to 0700, preserve full-identity encoding and absolute canonical host paths, generalize the shared helper's diagnostic to “private directory,” and keep its Unix permissions import at module scope.

Fresh endpoint directories are created atomically with private mode. Review caught that creation mode is still filtered by umask: masking owner execute could prevent socket binding. The final code restores 0700 afterward and cleans up on failure, retaining private initial creation without the availability regression. This is an integration correction to the same directory change, not a separate previously shipped issue.

**Verification:** Regressions reject a symlinked owner leaf while preserving an unrelated marker and 0750 permissions, and reject a symlinked root before creating an owner there. Existing privacy/full-owner/reopen coverage now also checks root mode. A subprocess regression exercises an owner-execute-masking umask without mutating the test process's global umask.

**Boundary:** This reuses the repository's checked-directory convention. It is not a claim to eliminate every concurrent same-UID ancestor-replacement race or sandbox arbitrary same-account executables.

### PR8R2-L-3 — A quick host exit could erase its buffered failure diagnostic

**Location:** `web_host.rs::manager_loop`.

**Context:** A host can flush a structured `Gone` or failed load state and exit before the manager checks its process. Successful process exit does not imply an empty socket receive buffer.

**Problem:** The manager checked `try_wait` first and immediately published a generic unexpected-exit message. This could discard an already-delivered actionable startup/load failure and show an unhelpful exit-status-0 diagnostic instead.

**Fix and reasoning:** Cache observed process exit, but continue reading complete buffered envelopes. Handle at most one per manager turn so ownership/shutdown checks remain active. Use generic exit status only when receive reaches timeout/closure without a structured terminal reason. Inspection errors still fail promptly. Terminal publication still clears pending frames/caret and retires that runtime.

**Verification:** A deterministic regression preloads `Started` followed by either `Gone(StartupFailed)` or failed load state while the root already reports successful exit. It requires the exact original reason, one fatal notice, one termination, and no revival by a later queued frame. A no-terminal case still requires generic fallback. The before-fix version discarded the structured reason.

**Boundary:** Socket timeouts remain per blocking operation. This does not add an absolute whole-frame deadline for a continuously progressing peer.

## UI input, rendering and native chrome

### PR8R2-UI-1 — Escape reordered preceding input and left later input aimed at a closed document

**Location:** `crates/crikey-ui/src/native.rs`, page/frame input collection; native renderer tests.

**Context:** Frame input is ordered. A single batch can contain text before Escape and additional events afterward. Extra egui layout passes must not replay those effects.

**Problem:** The old route removed Escape and emitted `ClosePage` before translating the rest. `[Text(before), Escape, Text(after), ArrowDown]` became `[ClosePage, before, after, ArrowDown]`. Supervisor identity checks prevented wrong-engine delivery but discarded legitimate preceding input after teardown.

**Fix and reasoning:** Process admitted-web Escape at its real position: preserve preceding UI commands, emit close, stop translating the batch, and never forward Escape release. Keep inherited display-list behavior and allow an unadmitted web declaration to answer host Escape. Preserve first-pass command ordering across multiple layout passes. This fixes UI command order, not guaranteed browser delivery before close: the CLI dispatches asynchronously, and immediate ownership retirement can still discard queued preceding input that the manager has not consumed.

**Verification:** Single-pass and forced-three-pass regressions require exactly `[before, ClosePage]`; another protects admission-time Escape. The old regression produced the four-command wrong order above. Native winit Escape was already handled synchronously and remains reserved.

### PR8R2-UI-2 — Native web motion left launcher hover state stale

**Location:** `native.rs`, native event routing, private frame-builder mode and touch translation.

**Context:** The browser receives rich native events immediately. egui also needs current pointer position/departure to draw launcher chrome correctly. Sending its generated events through the fallback browser translator would duplicate already-dispatched input.

**Problem:** The old workaround let egui-winit update its internal cursor bookkeeping, then truncated the resulting egui events. The egui context itself retained its old hover point when the cursor entered the document or left the window. A footer control could remain highlighted or show a stale tooltip. Keeping those events without changing dispatch would instead replay document motion.

**Fix and reasoning:** Explicitly distinguish native-owned web input from standalone frame input. Preserve pointer movement/departure for egui chrome, but do not replay mouse/key/wheel events already handled by the native route. Standalone frame building retains fallback translation.

Review caught that native touch had no immediate native branch and previously depended on egui translation. The final route preserves original `Event::Touch` events as a single active document contact, through the existing pointer/capture behavior, while excluding their synthesized mouse copies. Start/move/end/cancel are covered. This review correction prevents the hover fix from breaking touch support.

**Verification:** A private frame regression checks chrome hover position/absence and no fallback replay. Touch regressions protect the contact lifecycle. An actual native-window smoke, using an explicitly synthetic accepted web raster rather than WPE, drives the cursor with xdotool under Xvfb, captures footer pixels, and counts emitted document motion/departure. Its final observed results are recorded below. The starting-head private regression failed because native-owned motion was replayed by the fallback.

### PR8R2-UI-3 — The first wheel event could borrow a future pointer position

**Location:** `native.rs`, pointer bootstrap before frame processing and ordered fallback translation.

**Context:** egui's input state after processing a batch describes the batch's final pointer position. It cannot establish where an earlier wheel event happened.

**Problem:** Missing fallback pointer state was initialized from that already-folded final position. `[Wheel(1), PointerMoved(inside), Wheel(2)]` could send both wheels to the document even when the first was over chrome or had no known location. Earlier ordering coverage began with motion and missed this first-event boundary.

**Fix and reasoning:** Seed from the preceding frame before egui processes the new batch, then update in event order. Review caught another boundary: creating retained state before web admission could freeze a stale chrome/document position because pending input was not translated. The final code defers bootstrap until admission and discards pending predecessor state. Native mouse input does not allocate/use this fallback state.

**Verification:** A forced-three-pass regression covers unknown and known-chrome prior positions and requires only Wheel(2). Another covers pending motion followed by admission and stationary wheel in both directions, including departure. Before the fix both wheel deltas were forwarded at the later inside position.

## Packaging and documentation

### PR8R2-packaging-1 — Debian scanning could silently omit versioned engine dependencies

**Location:** `packaging/linux/build.sh`, `packaging/linux/tests/build.test.sh`, ADR-0020.

**Context:** `ldd` proves a library resolves on the builder. A Debian dependency record determines whether a different installation gets the runtime libraries it needs. Those are separate checks.

**Problem:** The scanner always used `dpkg-shlibdeps --ignore-missing-info`. For a locally installed versioned public engine library with no Debian symbols/shlibs metadata, the tool could succeed while omitting that dependency. Nonempty libc output then looked authoritative and bypassed the existing explicit WPE fallback. The package could install without declaring its engine runtime requirements.

**Fix and reasoning:** Web-enabled builds use strict metadata handling, so missing metadata errors select the existing conservative WPE runtime fallback. No-web scanning retains its old options. Preserve valid computed dependency versions when complete distribution metadata is available. Document the exact guarantee rather than calling any successful scan complete.

**Verification:** The packaging regression models successful-but-incomplete scanning and inspects the generated package's dependencies. Before: 83 passed / 1 failed, precisely the new check. After: 84 passed / 0 failed. An independent real-tool smoke compiled an ELF linked to a temporary versioned library with no package metadata, then built and inspected a real `.deb`. Old `Depends` contained libc and Python but no WPE fallback; fixed `Depends` contained the known WPE/Wayland/GLib fallback and Python.

**Boundary corrected during review:** Even strict `dpkg-shlibdeps` deliberately skips some private, unversioned and no-SONAME library cases. This fix does not infer packages or versions for arbitrary custom libraries. The fallback names the known WPE stack and remains conservative/unversioned. An arbitrary supplied ELF is still structurally checked, not proven to implement the host protocol.

### PR8R2-D-1 — Whole-tree cleanup wording needed an explicit launcher-death boundary

**Classification:** Documentation clarification of a source-backed limitation, not a new crash-recovery implementation.

**Location:** ADR-0020, plugin author guide, specification.

The launcher supervisor owns the `cgroup.kill` handle. While alive, it uses that control to clean the host tree, including detached descendants and an already-exited root. If the launcher itself dies abruptly, the parent-death signal reaches the host root, not every detached descendant, and early host startup may take the default signal action before GLib handlers exist. WPE destruction alone is not an explicit descendant-wait/kill guarantee.

The documentation now states that deterministic whole-tree cleanup requires the launcher supervisor to remain alive. The cgroup memory ceiling remains kernel-enforced after launcher death. No independent watchdog/reaper was added, and this report does not claim an experiment proved that a real WPE descendant survives. This clarification avoids overstating the demonstrated normal/failure cleanup guarantee.

## Areas reviewed without another confirmed fix

The section agents also reviewed the following boundaries. These are source-review conclusions, not additional runtime passes:

- Stable callback-state ownership, synchronous engine callbacks, borrowed strings/pixels, shutdown source removal, wake failures, startup deadlines and bounded command/output queues. Existing prior fixes were not duplicated.
- Frame stride/length arithmetic, generation exhaustion, buffer recycling, queue snapshot coalescing and terminal fencing. No new helper defect was confirmed.
- Exact-owner permission admission, separate network/storage grants, first stay refusal, owner/storage stability, repeated declaration behavior, display-list transitions, stale-ID filtering and mailbox locking.
- Terminal/frame publication: failure clears frame/caret under the mailbox lock, checks current ownership, and stops further receive for that runtime. A hypothetical UI frame-revival concern was routed and closed without a speculative patch.
- Native pointer capture/chords/departure, wheel ownership, hardware-key normalization, dead keys, measured caret placement, texture validation/eviction and multipass effects. Only the specific new issues above justified edits.
- Schema tags/types, unknown fields, byte budgets, storage defaults, core/display-list exclusivity, SDK building, transport timeout documentation and separate native/Python protocols.
- Packaging modes, supplied-binary combinations, staging replacement, locked metadata decoding, required dependency preflight, target-list parsing, Flatpak refusal of unsupported web mode, RPM recipe and truthful accessibility/platform limitations.

## Explicit unchanged and unverified risks

These are not reported as fixed bugs:

- Earlier inherited issues outside this PR remain outside scope: configured cross-target package artifact/architecture selection, unrelated older protobuf message-replacement sites, display-list resize memoization, and native swapchain limits. C-1 explains the narrowly justified outer PageFrame exception.
- Wayland SHM format/endianness beyond the exercised ARGB8888 little-endian path was not established. No speculative XRGB conversion or platform-support claim was added.
- Upstream legacy WPE button-mask inconsistencies and extreme signed-coordinate click-count arithmetic remain external/unverified boundaries. No arbitrary coordinate clamp or remapping that would break known button transitions was introduced.
- No real ibus/fcitx candidate session, reconversion/delete-surrounding support, native accessibility-tree bridge, mixed-DPI hardware transition, sanitizer/Miri, deliberate PID reuse, OOM pressure or adversarial same-UID filesystem race is claimed.
- Legacy WPE still limits discrete wheel magnitude and scroll-stop injection. Active lock-state flags unavailable from the input source are not invented.
- HTTP(S) document policy and canceled downloads are not a general network/subresource sandbox.
- Bounded cleanup does not prove prompt removal of processes stuck in uninterruptible kernel I/O. Abrupt launcher-death limits are described in D-1.

## Verification record

The coordinator's final integrated results are recorded here after execution. Before-fix failures described above came from a disposable checkout at the starting commit with regression-only patches, not from weakening assertions or running the final implementation under an old label.

| Check | Observed result |
| --- | --- |
| `cargo fmt --all --check` | Passed on final source |
| `cargo check --locked -j 2 --workspace --all-targets` | Passed |
| `cargo clippy --locked -j 2 --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --locked -j 2 --workspace --all-targets` | 2,547 passed; 4 ignored; 28 filtered; 155 suites |
| `cargo test --locked -j 2 --workspace --doc` | 1 passed; 27 suites |
| `cargo check --locked -j 2 --manifest-path compatibility/native-conformance/Cargo.toml --all-targets` | Passed |
| `bash -n packaging/linux/build.sh packaging/linux/tests/build.test.sh` | Passed |
| `bash packaging/linux/tests/build.test.sh` | 84 passed; 0 failed |
| `.venv/bin/python -m compileall -q sdk/python crates/crikey-legacy-compat/python compatibility/test-plugins` | Passed |
| WPE container: `RUSTFLAGS="-D warnings" cargo test --locked -j 1 -p crikey-web-host --features engine` | 76 library, 3 CLI and all 12 actual-host scenarios passed |
| Real read-only full cgroup mount plus writable delegated bind: prebuilt CLI test binary, `web_host::tests --ignored --nocapture` | All 3 detached-child/root-exit/late-child cleanup cases passed |
| `cargo check --locked -j 1 -p crikey-web-host --all-targets --all-features --target aarch64-apple-darwin` | Passed without installed WPE on the host |
| Same optional-host check for `x86_64-pc-windows-gnu` | Passed |
| Actual `NativeLauncher` under Xvfb, driven by xdotool, with a model-admitted synthetic web raster | Hover/leave pixels and exact controlled event counts passed |
| Real compiler + `dpkg-shlibdeps` + Debian package inspection, versioned library without package metadata | Old package omitted WPE fallback and failed; fixed package selected the known runtime fallback and passed |

The actual native UI smoke used the repository virtual environment's Python. It opened a real 720×420 native window and captured footer pixels. Hovering Settings changed 456 pixel channels from baseline; moving into the document or leaving from document/chrome restored the baseline crop exactly. The controlled sequence emitted exactly one inside document move and one departure. Startup/map motions were recorded outside that settled measurement window, not mistaken for duplicates. This isolates native UI routing with synthetic accepted pixels; the separate WPE gate proves browser rendering and engine behavior.

The throwaway native harness needed readiness and environment corrections before its final passing run: wait for page expansion, use authenticated Xvfb startup, retain an X connection to prevent last-client resets, remove both Wayland selector variables, stream build logs, bound child processes, reuse matching workspace build profiles, and separate startup from controlled-event measurement. No production assertion was weakened to make this harness pass. The visual Settings underline and its disappearance were also inspected directly.

### Observed failing-before evidence

- Raw-wire web tests: 13 passed / 3 failed, specifically hostless URL admission, lost outer-fragment generation, and hidden mixed display-list/web content.
- Host helper tests: 64 passed / 2 failed, accepting hostless and oversized direct opening commands.
- UI renderer selected tests: 13 passed / 2 failed, with the exact wrong Escape ordering and extra first wheel. The private native-owned-input regression separately failed on fallback replay.
- Both persistent-directory symlink regressions failed because the old implementation accepted the aliases. The exited-root diagnostic regression failed with generic status 0 instead of the structured reason.
- All three real cgroup scenarios failed on the old first read-only view despite the writable delegated bind. The same scenarios passed after the mount-view fix.
- The real browser showed the cookie-reopen and supplementary-preedit failures, and failed to provide the required early malformed-URL/non-UTF-8 refusals. The initial 7-pass/5-fail smoke run also contained the invalid WAV fixture; that fifth failure is not counted as media-204 reproduction.
- The package regression was 83 passed / 1 failed before, and the independent real scanner/package reproducer showed missing runtime dependencies.

### Execution qualifications

Initial concurrent full builds exhausted the practical resources of this 7.2 GiB host and reached command deadlines before tests ran. Disposable build storage was removed and Cargo compilation was bounded with `-j 2` or `-j 1`; completed gates, not timed-out attempts, are listed as passes. A stale container build artifact briefly hid the new core export after copying older/newer source timestamps; cleaning only generated crate artifacts resolved that integration-environment issue.

The workspace suite ran with the virtual environment first on `PATH`. The last engine-only correction changed a feature-gated native regression and smoke fixture; final workspace check/Clippy/format and the full engine-enabled suite cover that final source. The previously completed default-feature workspace tests remain applicable to their unchanged runtime paths.

WPE execution used WPE WebKit 2.52.6, WPEBackend-fdo 1.16.1 and libwpe 1.16.3. The headless container emitted software-GPU and accessibility-bus diagnostics while the tests still required actual document pixels and the stated behavior. GStreamer base/good plugins were installed during media-fixture diagnosis; that does not establish general codec support. Engine-feature Clippy was not run; workspace engine-disabled Clippy and engine-enabled compilation/tests with Rust warnings denied are distinct checks.

All three real cgroup tests normally remain ignored in environments without writable delegation; they were explicitly exercised in the private real-delegation scenario. The fourth ignored workspace test is not silently counted as executed. Cross-target checks establish build isolation, not a working browser engine on macOS or Windows. No check proves that every possible bug has been found.
