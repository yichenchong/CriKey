# PR #8 bug hunt — round PR8B

## Scope and reading guide

This report covers pull request #8, starting at commit `0fa6198`, relative to its base branch `feature/page-image-canvas`. It is **not a whole-repository audit**. Existing code was inspected where the new web-page path calls it or depends on it. Unrelated inherited problems were reported but were not silently included in this change.

The audit was split into six separately owned sections: engine and C interface; engine-independent host helpers; launcher and process supervision; UI rendering and input; core/protocol/permissions/SDK contracts; and packaging/documentation. Further reviewers checked the resulting UI, process-containment, platform, and shutdown changes. Agents reported findings outside their ownership to the coordinator for routing. Agents did not run build, test, lint, or formatting commands while editing. The coordinator ran the verification gates. Agents were explicitly required to use the repository's `.venv/bin/python`, not system Python, and to keep imports at module scope.

Every tracked item has an identifier unique to this round. Items include runtime bugs, latent boundary failures, performance waste, missing regression coverage, and documentation errors. They are **not all independent production crashes**. Some IDs describe separate fixes at different boundaries of the same feature; those relationships are stated below. Coverage-only and documentation-only changes are labeled accordingly. Hypothetical or impractical boundaries are not presented as observed user failures.

**Completion:** The report contains 80 uniquely labeled entries, including runtime fixes, latent-boundary fixes, coverage/documentation improvements, and four inherited issues intentionally left unchanged. Final coordinator verification includes the PR8B-L14 cleanup correction, not just the earlier audit commit.

## Important behavior change chosen by the user

**Web hosting now requires delegated cgroup-v2 supervision.** The user explicitly selected refusal over the previous warning-only fallback. Before executing a web host, CriKey must establish a private cgroup with an enforceable memory limit, process attachment, and process-tree killing. Without those facilities, opening a web page fails with a diagnostic; the ordinary launcher and display-list pages remain available.

A suitable parent must already have the memory controller enabled for children. CriKey searches eligible ancestors within the discovered cgroup mount, excluding the launcher's populated leaf and other populated non-root candidates. It does not move existing processes or enable unrelated controllers. The host is attached before its executable can run. Its memory ceiling remains the minimum of 768 MiB, one eighth of physical memory, and the launcher's inherited memory limits; a result below 256 MiB is refused.

This is an availability tradeoff, not just a wording change. Some Linux sessions that previously attempted unsupervised web hosting will now refuse it. The reason is that process-group cleanup alone cannot reliably identify a descendant that detaches and is reparented before its host crashes.

The cgroup's `cgroup.kill` control is the authoritative process-tree escalation. Raw descendant-PID scans are retired, not a second cleanup guarantee. Graceful process-group SIGTERM is allowed only while the original root remains unreaped; once its exit has been observed and reaped, teardown never signals its numeric PID or process group again. PR8B-L14 explains the PID-reuse safety correction.

## Engine, C interface, and real-engine tests

### PR8B-E1 — Engine callbacks could violate Rust's exclusive-reference rules

**Where:** `crates/crikey-web-host/src/engine.rs`, `Runtime`, surface callbacks, and termination callbacks; callback documentation in `csrc/ck_web_shim.h`.

**Problem and context:** Loading a URI or dispatching input can synchronously invoke an engine callback. The command path already held `&mut Runtime`, while the callback reconstructed another mutable reference to the same allocation. Two live exclusive references violate Rust's rules even if a visible crash has not occurred.

**Fix:** Engine callbacks now refer to a separate, stable `Rc<CallbackState>`. Callback output and termination state use controlled interior mutability; command handling no longer exposes its mutable runtime through that callback data. The header no longer promises that callbacks cannot be reentrant. Integration also corrected one remaining reference to the moved termination field.

**Verification:** Callback metadata and terminal-state tests exercise the separate state. The real engine tests exercise load, navigation, resize, wheel, and shutdown callbacks. No Miri or sanitizer run was performed, so this is not a claim of exhaustive unsafe-code verification.

### PR8B-E2 — Wake failures could race loop destruction or lose an early quit

**Where:** `engine.rs::signal_loop`, command processing, and `ck_web_shim.c` loop initialization/run/quit/cleanup.

**Problem and context:** Worker threads could call into a process-global GLib loop pointer while the engine thread was cleaning it up. A quit issued before `g_main_loop_run` also did not reliably prevent that later call from waiting forever.

**Fix:** Worker failures shut down the wake socket. Its persistent hangup is handled by the engine thread; workers do not access the C loop pointer. C loop access stays on the engine thread, an early quit is remembered, and Rust does not run an already-terminated surface. Interrupted wake reads are retried.

**Verification:** Wake-write unit tests cover interruption, full nonblocking sockets, zero writes, and definitive errors. Real WPE tests cover termination with an incomplete incoming frame and launcher disconnection.

### PR8B-E3 — Startup could be prolonged indefinitely by a slowly delivered command

**Where:** `engine.rs::DeadlineReader`, startup transport, and `run`.

**Problem and context:** A timeout on each individual read is only an inactivity timeout. A peer could send another byte before each timeout and keep the first `OpenSurface` incomplete indefinitely. Direct startup-error replies also had no write bound, and invalid storage initialization could return without a structured failure.

**Fix:** One absolute deadline now covers the entire first framed command, including prefix and body. Direct startup writes are bounded. Invalid startup storage configuration is reported as `StartupFailed`.

**Verification:** `startup_deadline_is_not_extended_by_a_drip_fed_partial_frame` and `expired_startup_deadline_does_not_consume_already_available_bytes` test the actual deadline reader rather than checking source text.

### PR8B-E4 — Page-controlled metadata could exceed the queue's memory and wire bounds

**Where:** `engine.rs::borrowed_c_string`, text truncation, and terminal-detail creation.

**Problem and context:** A bounded number of messages is not a bounded amount of memory when titles, URLs, and errors are arbitrarily large. Copying an enormous C string could allocate excessive memory, and encoding the resulting message could exceed the transport limit. A large close reason could also prevent delivery of the terminal message.

**Fix:** Callback strings are scanned/copied only up to 8,192 bytes. Terminal details are capped at a valid UTF-8 boundary before they enter the output queue.

**Verification:** Callback tests supply oversized metadata and multibyte text, verify the bound and valid text, and verify that subsequent callbacks cannot replace the terminal reason. A giant-title browser workload was not separately benchmarked.

### PR8B-E5 — Links, redirects, and frame navigations bypassed the HTTP(S) rule

**Where:** `ck_web_shim.c::on_decide_policy` and surface signal registration.

**Problem and context:** Protocol `OpenSurface` and `Navigate` were checked, but the document could initiate navigation itself. Validating only launcher commands did not enforce the same rule for links, redirects, or subframes.

**Fix:** The engine navigation-policy boundary permits HTTP(S) navigation with a host and rejects unsupported schemes and auxiliary-window requests. The unused originless `load_html` diagnostic entry point was removed.

**Verification:** A real WPE fixture attempts to load a `data:` iframe that would send an escape message to its parent if allowed. The test requires the blocked outcome, observed through actual rendered pixels. This is a document-navigation restriction, **not a general subresource or network sandbox**.

### PR8B-E6 — Replacing a pending navigation could close the entire page

**Where:** `ck_web_shim.c::on_load_failed` and `on_load_changed`.

**Problem and context:** WebKit reports cancellation when a new navigation supersedes an unfinished one. Treating that cancellation as a genuine failed load caused the launcher to close the page. A later finish callback could also overwrite a real failure with apparent success.

**Fix:** Expected cancellation and policy-interruption errors are ignored. Genuine failures remain failures through the subsequent finish event and are reset only when a new load starts.

**Verification:** The real engine test deliberately holds `/slow` open, navigates elsewhere, rejects unexpected failed-load messages, and requires the replacement document's exact color.

### PR8B-E7 — A web page could start an unapproved file download

**Where:** `ck_web_shim.c::on_download_started` and network-session setup.

**Problem and context:** Ephemeral cookies/cache do not disable downloads. Without a handler, WebKit can choose its default download destination even though CriKey has not approved a file destination for the page.

**Fix:** Session downloads are canceled synchronously. No implicit download destination is granted.

**Verification:** A real attachment response is requested under an isolated home directory. The test observes the request and page result and verifies that `Downloads/unapproved.txt` was not created. See [WebKit's default-destination documentation](https://wpewebkit.org/reference/stable/wpe-webkit-2.0/signal.Download.decide-destination.html).

### PR8B-E8 — The engine adapter treated line-wheel input as precise pixel input

**Where:** `engine.rs::Runtime::apply_input` and `ck_web_surface_axis` in the C interface.

**Problem and context:** A wheel notch and a touchpad pixel delta have different meanings. Even after transport, the adapter always used the smooth-pixel path, making a notch act like a tiny pixel movement. Discrete two-axis dispatch could also invent an orthogonal notch for a zero axis.

**Fix:** The adapter consumes the precision flag preserved by PR8B-H3. Precise input uses smooth two-dimensional events; nonzero discrete axes are dispatched separately.

**Verification:** A real document inspects wheel events and changes its rendered color. The test distinguishes line and precise input, verifies zero orthogonal motion, and checks distinct magnitudes. Legacy libwpe still reduces discrete magnitude to direction and has no injectable scroll-stop phase; those API limits are not presented as solved.

### PR8B-E9 — Focus loss and navigation did not cancel an active composition

**Where:** `engine.rs::Runtime::apply` and engine shutdown.

**Problem and context:** `ImeBridge::flush` only handled a deferred empty preedit. A nonempty composition could survive focus loss or document replacement, leaving the next document without a fresh composition start.

**Fix:** The adapter uses the explicit cancellation operation added in PR8B-H4 before navigation, focus-out, and close, and resets state during teardown. Remaining IME operations stop after synchronous termination.

**Verification:** Helper tests cover cancellation without commitment, restart, and idempotence. Real ibus/fcitx candidate-window interoperability was not exercised.

### PR8B-E10 — Native frame geometry had unchecked arithmetic and slice preconditions

**Where:** `ck_web_shim.c::on_export_shm` and `engine.rs::CallbackState::receive_frame`.

**Problem and context:** Signed C arithmetic for `width * 4` could overflow for corrupt geometry. Rust also needed to reject short stride and an excessive borrowed-slice length before creating that slice, not merely rely on later conversion checks.

**Fix:** C uses widened row-size arithmetic and requires positive stride. Rust checks stride and the `isize::MAX` slice-length bound before constructing a borrowed slice.

**Verification:** Frame-conversion boundary tests and real frame geometry/pixel checks run. No synthetic corrupt Wayland SHM object was injected into the engine.

### PR8B-E11 — Host CLI options deferred invalid values until engine startup

**Where:** `crates/crikey-web-host/src/main.rs::Options::parse`.

**Problem and context:** A generic endpoint parser accepted relative or non-Unix endpoints unsuitable for this host. Invalid storage paths could make the host connect and wait for its first command before failing.

**Fix:** Parsing now rejects non-absolute/non-Unix endpoints and invalid storage paths immediately. Existing rejection of ambiguous options and lossless handling of valid non-UTF-8 storage paths are retained and directly regression-tested.

**Verification:** CLI tests cover the supported private endpoint, malformed options, exclusive help handling, relative paths, non-UTF-8 paths, and embedded NUL.

### PR8B-E12 — The original engine smoke test could pass without painting the requested page

**Where:** `crates/crikey-web-host/tests/engine_smoke.rs`.

**Problem and context:** Any first frame counted as success, including an initial blank frame. The HTTP fixture also had unbounded accept/read/join paths, and a fixed endpoint name restricted concurrent scenarios.

**Fix:** The suite now requires actual document colors, resize dimensions, increasing generations, replacement navigation, security outcomes, input outcomes, terminal delivery, and bounded shutdown. HTTP/socket/process fixtures have deadlines and unique endpoints. During integration, three new fixtures were corrected to give their bodies viewport height: changing a short body's background beneath an opaque root could not change the sampled center pixel. Assertions were retained, not relaxed. Deadline diagnostics now report expected pixels and the last frame/load state.

**Verification:** All six real-engine scenarios pass. They cover document pixels/resize/superseded navigation; non-HTTP frame navigation; rejected downloads; line versus precise wheel input; incomplete-frame termination; and launcher-disconnect shutdown.

## Engine-independent host helpers and build configuration

### PR8B-H1 — Middle and right mouse buttons were reversed

**Where:** `crates/crikey-web-host/src/input.rs::InputTranslator` and button translation.

**Problem and context:** The protocol normalizes left/middle/right to 1/2/3. Legacy WPE uses a different order for middle and right, so forwarding the values unchanged reversed those actions.

**Fix:** Translate normalized values to the legacy WPE ABI without changing the wire contract. Companion comments are corrected in PR8B-C4.

**Verification:** `middle_and_right_buttons_use_the_legacy_wpe_numbering` tests both mappings. ABI sources: [libwpe input header](https://raw.githubusercontent.com/WebPlatformForEmbedded/libwpe/1.14.2/include/wpe/input.h) and [WebKit's legacy event factory](https://raw.githubusercontent.com/WebKit/WebKit/main/Source/WebKit/Shared/libwpe/WebEventFactory.cpp).

### PR8B-H2 — Motion and wheel events forgot held mouse buttons

**Where:** `input.rs::InputTranslator`.

**Problem and context:** A press followed by motion carried only keyboard modifiers. The engine could not recognize a drag or reliably report held buttons. Independent button releases also needed to preserve other held buttons.

**Fix:** Track held-button masks through button transitions; include them in motion, button, and wheel events. Focus loss clears them. Invalid input is rejected before state changes.

**Verification:** Tests cover chorded buttons, independent releases, focus loss, and invalid transitions leaving state intact.

### PR8B-H3 — Translation discarded wheel precision

**Where:** `input.rs::EngineInput::Axis` and input translation.

**Problem and context:** `RawInput.precise` disappeared before the engine adapter could distinguish line-wheel input from pixel scrolling.

**Fix:** Preserve the flag, fractional deltas, and held-button state. PR8B-E8 consumes this information at the C boundary.

**Verification:** Helper tests check fractional deltas, units, and button masks; real WPE tests distinguish the two engine paths. This is a separate information-loss point from the adapter's incorrect dispatch choice.

### PR8B-H4 — The IME bridge had no explicit cancellation operation

**Where:** `crates/crikey-web-host/src/ime.rs::ImeBridge::cancel`.

**Problem and context:** A live nonempty preedit was not ended by `flush`, so callers had no correct way to abandon it without committing.

**Fix:** Add idempotent cancellation that clears preedit and ends composition without inserting text. The engine uses it at the boundaries listed in PR8B-E9.

**Verification:** Tests cover canceling nonempty preedit and canceling a deferred clear exactly once.

### PR8B-H5 — Omitted storage mode contradicted the safe protocol default

**Where:** `state.rs::HostState::open`.

**Problem and context:** The schema defines omitted/unspecified storage as ephemeral, but the host rejected it as an error.

**Fix:** Normalize it to ephemeral and never pass the configured persistent directory in that case. Remove the obsolete error variant.

**Verification:** Tests verify that omission remains ephemeral even when a persistent directory is configured, and that explicit persistence uses only the configured absolute directory.

### PR8B-H6 — Stride-size arithmetic could overflow on 32-bit targets

**Where:** `frame.rs::FrameConverter::swap_into`.

**Problem and context:** `(height - 1) * stride + row_bytes` used unchecked `usize` arithmetic. Small dimensions with an enormous stride could panic in debug builds or wrap in release builds on a 32-bit target.

**Fix:** Use checked multiplication and addition, reporting `MappingSizeOverflow` before allocation or slicing.

**Verification:** The maximal-stride test contains architecture-specific expected outcomes. The available runtime exercised the 64-bit branch; no 32-bit execution is claimed.

### PR8B-H7 — Frame generation could wrap to zero

**Where:** `frame.rs::FrameConverter::convert`.

**Problem and context:** Incrementing `u64::MAX` panics in debug or wraps in release, violating monotonic identity. This is a latent exhaustion boundary, not a practically observed session length.

**Fix:** Refuse exhausted generation state without changing the counter or consuming reusable pixel storage.

**Verification:** The test explicitly seeds the exhaustion boundary and checks refusal, the unchanged counter, and retained storage.

### PR8B-H8 — Recycled frames were needlessly zero-filled

**Where:** `frame.rs::FrameConverter::recycle` and conversion.

**Problem and context:** Recycling cleared vector length, so the next conversion zero-initialized the whole destination and then overwrote every byte.

**Fix:** Retain initialized recycled pixels and resize directly, initializing only newly exposed storage.

**Verification:** Tests verify allocation reuse and correct complete overwrite. No timing improvement is claimed because no timing benchmark was run; the removed full-buffer initialization follows from the operations themselves.

### PR8B-H9 — Obsolete load snapshots could crowd out useful pending output

**Where:** `output.rs::OutputBuffer::push` and replacement matching.

**Problem and context:** A redirect burst accumulated obsolete load snapshots and could evict the only pending frame or caret update from a bounded queue.

**Fix:** Coalesce same-surface snapshots of the same kind and append the newest at its proper chronological position. Genuine load failure remains protected by PR8B-E6.

**Verification:** Tests flood load snapshots while retaining a frame/caret, and check chronology and isolation between surface IDs.

### PR8B-H10 — A terminal output stream did not end after its terminal message

**Where:** `output.rs::OutputBuffer::pop`.

**Problem and context:** After `Gone` was drained, the next consumer call still waited for a separate `close`, despite the terminal fence already excluding future output. Normal runtime often supplied that close; this was an inconsistent boundary rather than a reproduced production hang.

**Fix:** Return end-of-stream after terminal output drains and wake terminal waiters.

**Verification:** The terminal-fence regression now asserts terminal end-of-stream, exactly-once delivery, and rejection/recycling of later output.

### PR8B-H11 — Missing-WPE errors made a false packaging claim

**Where:** `crikey-web-host/build.rs`.

**Problem and context:** The diagnostic said CriKey tarballs built WPE from source, while the actual packaging links/stages against system WPE libraries.

**Fix:** Describe system linkage and the required development packages accurately.

**Verification:** Source-backed diagnostic correction; no source-text test was added.

### PR8B-H12 — A manifest comment overstated browser isolation

**Where:** `crikey-web-host/Cargo.toml`.

**Problem and context:** The comment claimed this was the only workspace crate linking any system library. Platform crates also link native system libraries; the intended isolation concerns the browser engine.

**Fix:** Scope the claim to WPE probing and browser-engine linkage.

**Verification:** Documentation-only correction; no production behavior changed.

### PR8B-H13 — Native-library link order broke engine library tests

**Where:** `crikey-web-host/build.rs`.

**Problem and context:** The real engine gate failed with an unresolved `g_main_loop_quit`. System libraries were emitted before the static shim, so the linker's `--as-needed` handling could discard GLib before the shim introduced its references.

**Fix:** First probe dependencies without emitting linkage metadata to collect include paths and errors. Compile the static shim, then emit complete pkg-config linkage metadata after it.

**Verification:** The previously failing engine library test link now succeeds, and the complete engine test command passes with Rust warnings denied. No source-text assertion was used as a substitute for linking.

### PR8B-H14 — Host-state boundaries lacked direct consumer-visible regressions

**Where:** Host state, input, and frame helper tests.

**Context:** This is **coverage-only** for additional already-correct boundaries, not a claim of newly discovered production failures.

**Added coverage:** Embedded-NUL storage paths are rejected before FFI; rejected opens do not claim a surface and allow a later valid open; exact frame-byte-limit dimensions are accepted while the next row is refused; Unicode IME cursor ranges count characters rather than UTF-8 bytes; open and navigate share absolute-URL restrictions; signed pointer-coordinate limits are checked before conversion. The UI suite also explicitly verifies that returning from a web page to a display list releases the engine texture without releasing the display-list texture.

**Verification:** Helper, protocol, and renderer suites exercise these boundaries. The texture transition is companion ownership coverage for PR8B-U2/U15, not an unrelated feature.

## Launcher ownership, startup, and process supervision

### PR8B-L1 — Opening a surface could erase its only first frame

**Where:** `crates/crikey-cli/src/web_host.rs::WebHostSupervisor::open`, mailbox publication and polling.

**Problem and context:** Opening published the desired identity and queued startup before clearing the mailbox. A fast host could publish its only first frame during that interval and then have it erased. Checking ownership before acquiring the mailbox lock also allowed an old publication to cross a replacement.

**Fix:** Clear the mailbox before identity publication/enqueueing, and check ownership while holding the publication lock. Pixel conversion remains outside that lock.

**Verification:** A deterministic test publishes frame and caret synchronously before enqueue returns, then attempts old-owner publications. It checks the initial frame survives and obsolete updates do not replace it. Generation tests use a processed-message marker rather than a timing sleep.

### PR8B-L2 — Dismissal during startup could still open the dismissed document

**Where:** `web_host.rs::handle_control`.

**Problem and context:** Startup can block while spawning and accepting a connection. Ownership was checked before that work but not after it, so a dismissed/replaced page could still receive `OpenSurface` and startup notices. Old controls could also affect a newer host.

**Fix:** Recheck ownership after startup and before owner-scoped controls. Obsolete runtimes are closed and terminated without opening their document. An obsolete open request cannot tear down a newer active runtime.

**Verification:** A barrier-controlled spawner proves dismissal during startup sends no open command and terminates once. Cancellation does not interrupt acceptance itself; acceptance remains bounded by its five-second timeout.

### PR8B-L3 — A host that stopped reading could block launcher shutdown

**Where:** Production transport setup and supervisor writes in `web_host.rs`.

**Problem and context:** A full socket could block the manager in `send`. Dropping the supervisor joins that manager, so a non-reading host could hang shutdown.

**Fix:** Configure a 250 ms production Unix write timeout using PR8B-C3's transport facility. Failure to establish the timeout cleans up startup instead of pretending it is bounded.

**Verification:** A real Unix peer stops reading; the test verifies fatal send failure and bounded supervisor shutdown. A deadline on individual writes does not claim a total wall-clock budget for a peer that continuously makes progress.

### PR8B-L4 — Teardown reused a connection after a partial frame write failed

**Where:** `web_host.rs` open/navigation/resize/input failure branches.

**Problem and context:** A timed-out write can leave half a frame on the stream. Sending `CloseSurface` afterward would append a new frame to an already-corrupted stream.

**Fix:** Close the failed transport before entering ordinary teardown. Process cleanup still runs exactly once.

**Verification:** An injected send timeout models a potentially partial frame write and verifies that teardown performs no later live-connection write and terminates once.

### PR8B-L5 — Non-executable files shadowed valid host executables

**Where:** Host discovery and `executable_file`.

**Problem and context:** Discovery selected regular files without checking whether the account could execute them. A non-executable sibling or early PATH entry prevented a usable later candidate from being tried.

**Fix:** Require executable regular files and reject invalid explicit overrides.

**Verification:** Discovery tests put a non-executable sibling ahead of a runnable candidate and verify correct selection. Normal filesystem replacement races between checking and executing are not claimed to be eliminated.

### PR8B-L6 — Attaching the host after spawn missed early descendants

**Where:** Production spawning, `prepare_host_process`, and `attach_current_process`.

**Problem and context:** Moving the parent into a cgroup after `spawn` returns does not move children it already created. Browser descendants could escape the intended limit before attachment.

**Fix:** Open the attachment control before spawning and write the child's PID in the pre-exec hook, before executable instructions can fork browser processes. Attachment failure prevents execution.

**Verification:** A controlled attachment-file test proves ordering before the first executable instruction. Required-control and attachment-refusal tests prove failure does not execute the target. Real delegated containment is exercised separately.

### PR8B-L7 — Exhausted surface IDs could become reusable

**Where:** `WebHostSupervisor::open` ID allocation.

**Problem and context:** After the counter reached its exhausted zero sentinel, another failed allocation advanced it back to one. That could reuse old identities rather than staying exhausted.

**Fix:** Treat zero as terminal and refuse without advancing.

**Verification:** Repeated allocations after seeded exhaustion remain refused and do not recycle an ID.

### PR8B-L8 — Parent-death protection assumed adoption always means PID 1

**Where:** `prepare_host_process`.

**Problem and context:** If the launcher died before protection was installed, a subreaper could adopt the child. Checking only `getppid() == 1` missed that case.

**Fix:** Capture the original launcher PID and refuse execution whenever the parent no longer matches it.

**Verification:** The production mismatch guard is exercised with a controlled parent mismatch and must exit with 127 before running the target. A live launcher-death/subreaper race was not independently forced.

### PR8B-L9 — Warning-only fallback could not guarantee cleanup of detached descendants

**Where:** Mandatory cgroup preparation, production startup, process ownership, and teardown in `web_host.rs`; obsolete warning handling in `crikey-cli/src/lib.rs`.

**Problem and context:** Process groups plus a later ancestry scan cannot reliably find a process that detached and was reparented before its root exited. The previous unavailable-cgroup path still executed the host with only a warning, despite this cleanup limitation.

**Fix and decision:** The user chose mandatory cgroup supervision. Missing/unusable memory, attachment, or kill controls now cause a named startup refusal before host execution. The owned cgroup lease is mandatory and its kill operation is used even after root exit. Warning-only startup plumbing and its consumer were removed. The ordinary launcher remains available. PR8B-L14 removes the obsolete raw-PID fallback that initially remained alongside this cgroup cleanup.

**Verification:** Deterministic tests cover unavailable delegation, absent/failed controls, invalid and below-floor inherited limits, refusal cleanup, no execution after failed attachment, and root-exited cleanup. An opt-in test also ran successfully against actual delegated cgroups and killed a detached child. PR8B-L13 makes the hierarchy selection workable; PR8B-P15 documents the availability tradeoff.

### PR8B-L10 — Old UI events could close a newer page when the queue was full

**Where:** `navigate`, `resize`, `send_input`, and `send_ime`.

**Problem and context:** Old-owner events entered the bounded queue before ownership filtering. If the queue was full, their errors could reach the launcher's fatal-page path and close the new document.

**Fix:** Discard obsolete events before validation/enqueueing. Keep real backpressure errors for the current owner.

**Verification:** A deterministically saturated queue accepts old-owner events as harmless discards while current-owner input still reports genuine backpressure.

### PR8B-L11 — Permission, initial navigation, and storage boundaries lacked direct coverage

**Where:** App permission-router tests, CLI declaration tests, and supervisor tests.

**Context:** This is **coverage-only**, not a claim that these already-correct production paths were broken.

**Added coverage:** Exact owners sharing an executor retain distinct network/storage grants; unknown owners are refused; denied declarations never create a supervisor; a stay request without an existing document is refused before startup; repeated URL/stay declarations do not reload; a changed URL navigates; persistent directories are account-private and separated by the full owner identity.

**Why it matters:** These tests protect authorization and lifecycle behavior at the actual caller boundaries, rather than merely repeating permission-helper outputs.

### PR8B-L12 — Relative override selection and execution could name different files

**Where:** `executable_file` and host discovery.

**Problem and context:** Discovery could accept a bare relative file in the current directory, but `Command::new` interprets a bare program name as a PATH lookup. Selection and execution could therefore disagree.

**Fix:** Canonicalize accepted executable candidates before passing them to execution.

**Verification:** The discovery regression verifies that the relative override resolves to the selected absolute executable. It also covers the non-executable-shadow issue in PR8B-L5.

### PR8B-L13 — Mandatory supervision originally chose an unusable cgroup topology

**Where:** `CgroupHierarchy`, ancestor selection, memory-policy detection, and cgroup preparation in `web_host.rs`.

**Problem and context:** Making cgroups mandatory exposed a real setup problem: creating the host directly below the launcher's populated non-root cgroup cannot provide domain controllers to children under Linux's no-internal-process rule. Writable delegation alone did not make that layout usable.

**Fix:** Discover the actual cgroup2 mount and search eligible proper ancestors. Require an already-enabled memory subtree, an empty non-root parent, writable migration permissions, and a private child with functioning memory/attachment/kill controls. Exclude the launcher leaf, enforce mount boundaries, and reject traversal or symlink escape. Do not move existing processes or enable unrelated controllers. Preserve the original launcher's inherited memory ceiling even when choosing a sibling location. Failed candidate setup removes its directory.

**Verification:** Fake hierarchies cover valid topology, populated/ineligible parents, controller and permission refusal, candidate cleanup/retry, inherited caps, mount boundaries, and no execution on refusal. The ignored real-delegation regression was explicitly run in the WPE container and passed. See [Linux's no-internal-process constraint](https://docs.kernel.org/admin-guide/cgroup-v2.html#no-internal-process-constraint).

### PR8B-L14 — Remembered process IDs could kill an unrelated process after reuse

**Where:** `web_host.rs::terminate_child_tree` and its obsolete procfs descendant helpers; companion cleanup wording in ADR-0020, the author guide, and the specification.

**Problem and context:** Teardown remembered descendant PIDs, then used `Child::try_wait` to observe and reap the root. It continued signaling numeric PIDs and the original process group, and reused remembered PIDs as ancestry-scan roots. Once a process is reaped, Linux may assign its PID to an unrelated process. A later scan or signal could therefore reach outside the supervised host tree. This is a confirmed unsafe cleanup path, not a claim that an unrelated process was observed being killed.

**Fix:** Use the already-required dedicated cgroup's `cgroup.kill` as the authoritative process-tree escalation. Remove the raw descendant-PID collection, repeated procfs ancestry scans, and numeric descendant-kill fallback rather than adding a pidfd abstraction. Allow graceful process-group SIGTERM only while the original root is still unreaped; after `try_wait` observes and reaps its exit, never signal its numeric PID or group again. Replace the final unbounded wait with bounded reaping polls and an error diagnostic if the kernel cannot complete exit promptly. Cgroup cleanup still runs after root exit and covers detached or reparented descendants. The companion documentation explicitly states this contract.

**Coverage added or retained:** `already_reaped_root_bypasses_numeric_signaling_and_still_kills_cgroup` starts with an explicitly reaped root, installs a group-signal callback that panics if called, and checks that the cgroup kill control is still written. It tests the no-later-signal boundary without touching an unrelated process or forcing real PID reuse. `graceful_root_close_runs_before_cgroup_escalation` checks the SIGTERM-handler marker, successful reaping, and mandatory cgroup kill after graceful exit. The obsolete procfs/fake-control descendant tests are replaced by opt-in real-delegation tests `exited_root_still_cleans_up_its_detached_cgroup_child` and `cgroup_cleanup_contains_a_late_detached_descendant`. The retained `real_delegated_cgroup_contains_and_kills_a_detached_child` also gains an explicit root-reaping assertion. All three real-delegation tests passed when explicitly executed in the delegated container.

**Integration fixture correction:** The first late-detached real-cgroup run exposed a test-shell mistake: the SIGTERM trap called `close_host` without forwarding its positional argument, leaving the function's `$1` empty. The coordinator corrected the trap to `trap 'close_host "$1"' TERM` without weakening assertions. This is part of PR8B-L14's regression integration, not another original-code finding. The subsequent three-test real-cgroup run passed.

**Verification status:** Fresh coordinator formatting, workspace check/clippy, 32 focused supervision tests, full workspace tests, documentation tests, and all three opt-in real-delegation cases passed after the correction.

## UI rendering, input, composition, and viewport ownership

### PR8B-U1 — The web page's IME caret did not enable IME

**Where:** `native.rs::draw_page` and native graphics output handling.

**Problem and context:** Opening a page surrendered query focus. Without egui IME output, egui-winit disabled IME; separately setting the native caret rectangle did not re-enable it and bypassed egui scaling.

**Fix:** Publish the engine caret, or a page-origin fallback, through egui's `IMEOutput` so the normal integration handles both enablement and scaling.

**Verification:** A renderer regression checks IME output at the page caret and verifies the old query is not edited. Real candidate-window placement and ibus/fcitx behavior remain unmeasured.

### PR8B-U2 — Uncommitted composition survived document replacement or Escape

**Where:** Native IME state and accepted-frame ownership transitions.

**Problem and context:** Composition belonged to the retained window, not the web surface. Closing/replacing a document during preedit could leave `composing` set and suppress later launcher keys such as Enter.

**Fix:** Scope composition to the surface identity. Ownership changes cancel/reset platform IME and composition state and retire pointer ownership and cached web geometry.

**Verification:** The replacement/close regression checks that uncommitted composition is retired. This does not substitute for real platform IME event-order testing.

### PR8B-U3 — Document drags lost their motion and release at page boundaries

**Where:** Shared `WebPointerState`, native web routing, and egui fallback.

**Problem and context:** Requiring every event to be inside the page lost the release when a drag crossed into launcher chrome, leaving the browser button held.

**Fix:** Track surface-owned button capture through the final release, including chorded buttons and boundary-crossing motion. Replacement retires capture. PR8B-U17 additionally fixes departure from the entire native window.

**Verification:** Tests cover a drag leaving the document, independent chorded releases, replacement, and retained ownership through release.

### PR8B-U4 — Withheld web motion left the chrome's cursor position stale

**Where:** Native window-event dispatch and egui-winit bookkeeping.

**Problem and context:** Web-owned motion was correctly withheld from egui controls, but that also prevented egui-winit from updating its internal cursor position. A later chrome click could use an old location.

**Fix:** Update native cursor bookkeeping while removing document-owned generated egui events. Genuine chrome motion remains available without replaying web input.

**Verification:** Shared routing tests cover ownership boundaries. No claim is made that every native window-system click transition was exercised interactively.

### PR8B-U5 — Scroll gestures lost their final event when the pointer left the page

**Where:** `WebPointerState::scroll_position` and native wheel routing.

**Problem and context:** A gesture that began over the document lost its `Ended`/`Cancelled` event after pointer departure.

**Fix:** Retain gesture ownership through its terminal event and retire it on replacement. Window departure uses the outside endpoint described in PR8B-U17.

**Verification:** Tests cover a document-owned gesture ending after departure. The current legacy WPE API still cannot inject a distinct stop-phase event downstream.

### PR8B-U6 — Retained pointer positions used an obsolete scale

**Where:** Native retained cursor position and logical-coordinate conversion.

**Problem and context:** Moving the window to another scale without a new mouse move left later clicks/scrolls using coordinates converted with the previous scale.

**Fix:** Retain physical coordinates and convert at event time using the current window scale and egui zoom. Pixel-wheel deltas use the same current conversion.

**Verification:** Deterministic tests reinterpret one retained native position at different scales. A real mixed-DPI monitor transition was not run.

### PR8B-U7 — Linux physical-key numbers used the wrong convention for WPE

**Where:** `native.rs::web_hardware_keycode` and `web_key_input`.

**Problem and context:** Winit exposes Linux evdev scancodes; WPE expects XKB hardware keycodes. Passing KeyA's 30 instead of 38 gave the browser the wrong physical-key identity.

**Fix:** Restore the checked Linux offset of eight at the UI-to-WPE boundary. Unavailable/overflowed identity remains zero. This adjustment is not applied as a universal Windows/macOS convention.

**Verification:** Known-key tests require XKB values such as KeyA=38. PR8B-C5 documents the convention; [WebKit's mapping](https://raw.githubusercontent.com/WebKit/WebKit/main/Source/WebCore/platform/libwpe/PlatformKeyboardEventLibWPE.cpp) provides the ABI reference.

### PR8B-U8 — Logical-key reconstruction discarded available distinctions

**Where:** `native.rs::x_keysym` and key translation.

**Problem and context:** Right modifiers became left modifiers, keypad keys became ordinary keys, and available raw XKB symbols became zero. Several representable editing/desktop/function keys were missing.

**Fix:** Use key location and available native XKB identity; preserve right/keypad distinctions, Meta versus Super, AltGraph, additional editing keys, and F25–F35. Unsupported backend identities still remain unavailable rather than invented.

**Verification:** Tests cover locations, raw symbols, lock-key keysyms, and representative named keys. Active Caps Lock/Num Lock state is a separate unavailable input-source feature, documented in PR8B-P9.

### PR8B-U9 — A dead key was fabricated as a printable spacing accent

**Where:** `native.rs::x_keysym`.

**Problem and context:** A dead acute accent was translated as printable `´` before the eventual composed-text commit, risking an extra character.

**Fix:** Do not invent printable text from a dead-key marker; use actual composition/commit text.

**Verification:** A regression distinguishes the dead-key marker from committed text. Real desktop dead-key/IME ordering was not independently exercised.

### PR8B-U10 — The egui fallback lost input and ignored event order

**Where:** `native.rs::web_frame_input` and fallback keysym translation.

**Problem and context:** Named navigation keys and real text were ignored. Wheel routing used the frame's final cursor position rather than the position when each event occurred. Page-sized wheel units were treated as line counts.

**Fix:** Forward available named keys and actual text/IME without guessing missing hardware identity. Process pointer/wheel events in order and convert page units using page dimensions.

**Verification:** Tests cover event-local wheel coordinates, page units, named keys, text, Escape ownership, and host-shortcut isolation.

### PR8B-U11 — The old query could consume document composition during transition

**Where:** `native.rs::draw_query`.

**Problem and context:** The query editor processed input before surrendering focus to the new page, so composition arriving during the transition could edit the previous query.

**Fix:** Make the query noninteractive while a page owns input.

**Verification:** The page-composition regression verifies correct IME output and unchanged query text.

### PR8B-U12 — Native and fallback routes disagreed about host-owned shortcuts

**Where:** `native.rs::draw_launcher` and `draw_page`.

**Problem and context:** Modified Escape could fail to close an egui-fed page, while Ctrl+, could open host settings over a document that should own that input.

**Fix:** Reserve page Escape regardless of modifiers and do not hijack the web page's Ctrl+, input. Prevent synthetic native keyboard events from being replayed through the fallback. A behavior-neutral boolean simplification was applied during the lint gate.

**Verification:** Renderer tests cover modified Escape, named/text input, and absence of unintended settings commands.

### PR8B-U13 — Valid wire dimensions could still exceed the GPU texture limit

**Where:** `native.rs::web_texture`.

**Problem and context:** A legal 2049×1 web raster can satisfy the wire byte limit while exceeding a renderer's advertised 2048 texture dimension. Uploading it could reach an egui assertion or invalid GPU operation.

**Fix:** Check both dimensions against the actual renderer limit before pixel conversion/upload. Release the obsolete texture instead of silently stretching or resampling the rejected input.

**Verification:** A renderer test constrains the texture limit and verifies rejection before upload and release of the old handle. The inherited swapchain-size issue listed later is separate and unchanged.

### PR8B-U14 — Finite caret components could produce infinite rectangle endpoints

**Where:** `LauncherViewModel::update_web_caret_area` and web geometry construction.

**Problem and context:** Individually finite `x` and `width` near `f32::MAX` can overflow when added, producing an invalid native IME rectangle.

**Fix:** Validate resulting endpoints as well as components, retain the previous valid caret, and defensively reject invalid renderer geometry.

**Verification:** The regression supplies overflowing endpoints and verifies that the valid prior anchor survives.

### PR8B-U15 — A replacement engine could miss its measured viewport

**Where:** `native.rs::report_page_size`.

**Problem and context:** Resize memoization used page ID and dimensions but not web-engine identity. Replacing the host while keeping the same ID/rectangle suppressed its first exact size report, leaving the new host with an estimate.

**Fix:** Include the admitted web surface ID in web measurement ownership. Repeated frames for the same surface remain resize-free. Preexisting display-list memoization is unchanged.

**Verification:** The replacement regression requires each new engine to receive the measured viewport exactly once.

### PR8B-U16 — Global command deduplication removed real repeated input

**Where:** `native.rs` frame construction and pass-command handling.

**Problem and context:** Calling `commands.dedup()` could not distinguish a repeated layout pass from two actual identical events. Two text events containing `a`, or repeated same-key input in one frame, could become one.

**Fix:** Collect commands per egui pass. Preserve ordered first-pass effects, including duplicates within that pass. Later layout passes update the latest viewport measurement in the proper position without replaying input or dropping first-pass-only query/settings/display-list effects.

**Verification:** Tests cover two identical commits, repeated keys, forced three-pass layout with unchanged/changing geometry, query edits, setting commits, preferences, display-list focus/input, and final viewport reporting.

### PR8B-U17 — Leaving the native window could leave hover or release targeting inside

**Where:** Native `CursorLeft`, fallback `PointerGone`, and `WebPointerState`.

**Problem and context:** Clearing local coordinates without notifying the engine left hover active. Suppressing departure while captured also left the last inside coordinate in use: pressing inside, leaving the window, and releasing without another motion could release on the original control.

**Fix:** Emit an outside motion at departure and remember the departed state. Keep button/scroll ownership, but use an outside endpoint for the captured release or terminal scroll event until genuine motion resumes. Repeated departures are idempotent; re-entry restores normal coordinates.

**Verification:** Native-state and same-frame/cross-frame fallback tests exercise inside → press → departure → release without an intervening outside movement, including stale release coordinates, capture retirement, and re-entry. The exact focused regression passed after the final source settled; no assertion was reduced from two transitions to one.

## Core model, wire contract, permissions, and SDK

### PR8B-C1 — Host-side URL validation was weaker than SDK validation

**Where:** `crikey-core/src/page.rs`, `WebSurfaceBuilder`, and schema comments.

**Problem and context:** A plugin bypassing the SDK builder could submit `https:relative`, an empty authority, whitespace in the authority, or embedded control characters/NUL. Core validation checked only the scheme, so different callers received different contracts.

**Fix:** Centralize the existing absolute-HTTP(S), control-character, authority, and byte-limit checks in `PageWebSurface::validate`; frame validation and the SDK delegate to it. Unsupported schemes remain distinguishable from malformed URLs. Empty URL retains its later-frame stay meaning. No new URL dependency or unrelated URL grammar was introduced.

**Verification:** Core, decoded-wire, and SDK tests cover malformed input, NUL, exact UTF-8 byte boundaries, oversized URLs, and valid stay declarations. Full URL interpretation beyond this stated boundary remains the engine's job.

### PR8B-C2 — Repeated protobuf web submessages replaced fields instead of merging

**Where:** New web decoding in `crikey-native-protocol/src/message.rs`.

**Problem and context:** Protobuf singular message occurrences merge when repeated. A URL fragment followed by a storage fragment of `PageFrame.web` instead replaced the whole declaration and lost the URL. Repeating the same new web oneof member similarly lost earlier geometry/identity and unknown fields.

**Fix:** Add an internal decode-into-existing operation for `PageFrame.web` and the new web Envelope members 33–42. Explicit defaults overwrite earlier fields; omitted fields survive; unknown fields accumulate. A different oneof member still replaces the old member. Existing unrelated decoders are intentionally unchanged.

**Verification:** Independent raw-wire fixtures cover every new web oneof member, split declarations, explicit-zero overwrite, omitted geometry, unknown retention, variant replacement, malformed fragments, and safe handling of unknown storage values.

### PR8B-C3 — Transport had no way to enforce a stalled-peer write deadline

**Where:** `Transport::set_write_timeout`, Unix transport, and transport tests.

**Problem and context:** The supervisor could configure receive timeouts but had no transport operation for bounded writes. PR8B-L3 needed a real deadline, not an assumed one.

**Fix:** Add an OS-enforced Unix write-timeout operation. Other transports explicitly reject a requested deadline rather than pretending to enforce it. Clearing an unsupported deadline is harmless. Document that timeout can leave a partial frame and requires closing the connection.

**Verification:** A real Unix socket with a non-reading peer releases the blocked sender. Another test verifies that unsupported deadline requests are not reported as successful enforcement.

### PR8B-C4 — Pointer-button comments confused protocol values with engine ABI values

**Where:** Protocol constants and `plugin.proto::RawInput.button`.

**Problem and context:** The comments called left/middle/right = 1/2/3 the engine's numbering. That was false for legacy WPE and encouraged the direct forwarding bug in PR8B-H1.

**Fix:** Document normalized launcher numbering and the required engine-adapter translation. Wire values do not change.

**Verification:** Companion host tests prove the actual middle/right translation and held-button behavior. No source-text test was added for the comment itself.

### PR8B-C5 — Hardware-key documentation incorrectly implied a universal raw scancode

**Where:** `plugin.proto::RawInput.hardware_keycode`.

**Problem and context:** A raw backend scancode is not necessarily the engine's hardware-key convention. The supported Linux WPE path requires the normalization described in PR8B-U7.

**Fix:** Document Linux evdev+8/XKB hardware identity for that path, and zero when identity is unavailable. Do not redefine the field as one universal Windows/macOS scancode format or change its wire tag.

**Verification:** Companion UI known-key tests enforce the Linux normalization.

## Packaging, examples, and capability documentation

### PR8B-P1 — Cargo target directories were extracted from JSON as raw text

**Where:** `packaging/linux/build.sh::resolve_binaries` and packaging tests.

**Problem and context:** The new `sed` extraction did not decode JSON escapes. A valid target directory containing quotes or backslashes could be truncated or left escaped, so built binaries were not found for staging.

**Fix:** Decode `target_directory` with `jq` and fail clearly on invalid metadata. Require/document `jq` only in Cargo-building modes; supplied-binary staging and Flatpak-only paths do not gain that dependency.

**Verification:** The recording Cargo fixture returns escaped JSON for a real quoted/backslash-containing directory, and staging must use those actual binaries.

### PR8B-P2 — Metadata lookup could rewrite a supposedly locked release build

**Where:** Cargo metadata invocation in `build.sh`.

**Problem and context:** The subsequent build used `--locked`, but the preceding new metadata command did not. It could resolve/change dependencies before that supposedly fixed release build.

**Fix:** Add `--locked` to metadata too.

**Verification:** The recording fixture requires the metadata invocation to include it.

### PR8B-P3 — Missing WPE build dependencies were discovered after expensive compilation

**Where:** Packaging preflight and binary resolution.

**Problem and context:** Engine-enabled source packaging compiled ordinary release binaries before discovering that pkg-config or WPE development dependencies were missing. This was poor failure ordering rather than a silent successful package defect.

**Fix:** Check pkg-config and all five required engine packages before invoking Cargo.

**Verification:** A missing-WPE fixture requires the named dependency error and proves Cargo was not invoked.

### PR8B-P4 — Multiline target lists silently discarded later targets

**Where:** `build.sh` target parsing.

**Problem and context:** `read -a` consumed only the first line. `stage` followed by a newline and an invalid target could succeed instead of rejecting the whole request.

**Fix:** Reject multiline lists before parsing.

**Verification:** The exact multiline request is rejected using supplied executables, without accidentally selecting a real source build.

### PR8B-P5 — Empty supplied-host arguments silently selected source-building behavior

**Where:** Both `--web-host-binary` option forms.

**Problem and context:** An explicitly empty path enabled web mode but left no supplied executable, allowing the script to fall into Cargo-building mode instead of reporting the bad argument.

**Fix:** Reject both an empty separate value and an empty equals-form value immediately.

**Verification:** Both forms must fail with their argument diagnostic before the recording Cargo is invoked.

### PR8B-P6 — Successful Debian dependency scanning had no direct regression

**Where:** `packaging/linux/tests/build.test.sh`.

**Context:** This is **coverage-only**. Script executable fixtures previously exercised fallback dependency declarations, not the successful scanner path added by the PR.

**Added coverage:** A recording successful `dpkg-shlibdeps` fixture verifies that all five installed executables, including the web host, are inspected and that a scanner-derived versioned WPE dependency reaches the generated Debian control record.

### PR8B-P7 — The author example could issue stay before any accepted document

**Where:** Dashboard example in `docs/plugin-author-guide.md`.

**Problem and context:** The example set `web_opened` when producing a first frame. If resize made that generation obsolete, the next accepted frame could request stay even though the launcher had never accepted an initial URL.

**Fix:** Repeat the fixed entry URL statelessly. Explain that returning a declaration is not the same as host acceptance and that repeating the last requested URL does not reload the current document.

**Verification:** Supervisor behavior is covered by PR8B-L11's repeated-declaration regression. No documentation-text test was added.

### PR8B-P8 — Page documentation still assumed every page was a display list

**Where:** Author guide, ADR-0020, and specification.

**Problem and context:** The introduction denied an embedded-web route while other sections described one. Generic manual layout/editing/timer statements also conflicted with the web-frame rules.

**Fix:** Describe both page forms. Scope drawn-node/layout/editing/timed-redraw rules to display lists, identify native/Rust declaration availability, and separate plugin declarations from private engine-frame/input/IME traffic.

**Verification:** Source-backed documentation correction, not a new runtime feature.

### PR8B-P9 — Modifier-preservation claims exceeded the input source's capabilities

**Where:** Specification and author guide.

**Problem and context:** Winit/egui expose Shift/Control/Alt/Super but not active Caps Lock/Num Lock state. Unqualified preservation wording implied those flags were available.

**Fix:** State the actual supported modifier flags and the unavailable lock-state flags. Distinguish active state from forwarded logical lock keys and keyboard-resolved text. Do not invent lock state by guessing from key presses.

**Verification:** Logical keys and text have behavioral tests. No test was added merely to pin an incidental omitted flag. Real IME interoperability remains separately qualified.

### PR8B-P10 — Supplied-host compatibility validation was overstated

**Where:** ADR-0020 packaging conclusion.

**Problem and context:** Documentation implied semantic CLI compatibility checks, but packaging can establish only native executable structure and resolvable dynamic libraries.

**Fix:** State those structural checks precisely and explicitly disclaim proof that an arbitrary supplied ELF implements CriKey's protocol or is an engine-enabled host.

**Verification:** Existing packaging behavior is exercised; this correction does not claim a machine-readable protocol compatibility probe exists.

### PR8B-P11 — The documented failure prefix did not match the launcher

**Where:** Author guide.

**Problem and context:** It promised `Web page closed: ...`, while the implementation reports `Page closed: ...`.

**Fix:** Match the implemented diagnostic.

**Verification:** Source-backed wording correction only.

### PR8B-P12 — Accessibility and navigation restrictions needed clear boundaries

**Where:** Author guide, ADR-0020, specification.

**Problem and context:** Describing DOM semantics could be mistaken for a bridged native accessibility tree. HTTP(S)-only wording could be mistaken for a general network/subresource sandbox.

**Fix:** Explicitly state that there is no engine-to-launcher accessibility-tree bridge or complete screen-reader support. Describe the document-navigation rule and canceled downloads without promising broader confinement.

**Verification:** Navigation/download behavior has real-engine tests under PR8B-E5/E7. Accessibility support is a documented limitation, not a newly implemented feature.

### PR8B-P13 — Packaging help lost existing useful information

**Where:** `build.sh` usage/help text.

**Problem and context:** The PR removed prefix documentation, shortened optional desktop-validator information, and retained a false claim that every target consumes the staged tree.

**Fix:** Restore the missing option/tool descriptions, distinguish source-built Flatpak from staged formats, and document the JSON-parser dependency.

**Verification:** Executable help and packaging contract checks run; no source-text-only tests were added.

### PR8B-P15 — Documentation still described optional cgroup enforcement

**Where:** Author guide, ADR-0020, specification, and Linux packaging help.

**Problem and context:** The user selected mandatory supervision during this audit. Continuing to promise warning-only fallback would misdescribe availability and cleanup guarantees. Describing only a writable populated launcher cgroup would also omit Linux's actual hierarchy requirement.

**Fix:** Document required memory/attachment/kill controls, a suitable memory-enabled delegated parent with a separate host leaf, no movement of existing processes/controller changes, inherited caps, startup refusal, and continued ordinary-launcher availability. Align the supervision-unavailable and below-floor diagnostics with the implementation.

**Verification:** Source review and the real delegated-cgroup regression support the described topology. This is the documentation companion to PR8B-L9/L13, not an additional independent runtime defect.

## Inherited issues encountered but intentionally not changed

The user limited this round to PR #8. These findings were returned to the coordinator and kept out of the patch rather than silently expanding the task. They are not reported as fixed.

### PR8B-P14 — Existing cross-target package artifact selection

The packaging script derives package architecture from `uname -m` and reads the native `target_directory/release` layout while Cargo can inherit a configured cross target. The same selection behavior exists in the PR base. Such a build can fail lookup or select stale native artifacts. The new JSON-path parsing is fixed in PR8B-P1; inherited cross-target selection is unchanged.

### PR8B-X1 — Existing protobuf message-replacement behavior outside the new web fields

Older nested fields, including `LifecycleAck.error`, `PageNode.image`, and Envelope's existing `PageFrame` member, also use replacement rather than full singular-message merge behavior. PR8B-C2 fixes only fields introduced by this PR. Older codec behavior was not expanded into this audit.

### PR8B-X2 — Existing display-list resize memoization across page reuse

Display-list size memoization still uses page ID and measured size, which can suppress an initial measurement when reusing an ID across reopen/replacement. That logic predates the PR. The newly introduced web-engine replacement case is fixed in PR8B-U15 without changing unrelated display-list behavior.

### PR8B-X3 — Existing native swapchain limits at large physical window sizes

The renderer requests ordinary downlevel device limits while native surface configuration uses physical window dimensions. A sufficiently large/high-DPI window can exceed those limits. The device/swapchain configuration is unchanged by this PR; the PR-added web-texture upload boundary is separately fixed in PR8B-U13. No claim is made that this audit fixed high-DPI swapchain configuration.

## Final local verification record

The final formatting, workspace check/clippy, workspace tests, documentation tests, and delegated cleanup tests were rerun after PR8B-L14. Earlier successful engine, packaging, Python, external-consumer, and cross-target checks remain applicable: their source did not change in that final cleanup correction.

| Gate | Result |
| --- | --- |
| `cargo fmt --all --check` | Passed |
| `cargo check --workspace --all-targets` | Passed |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed |
| `cargo test --workspace --all-targets` | 2,526 passed; 4 ignored; 28 filtered |
| `cargo test --workspace --doc` | 1 passed |
| `bash -n packaging/linux/build.sh packaging/linux/tests/build.test.sh` | Passed |
| `bash packaging/linux/tests/build.test.sh` | 83 passed |
| Repository virtual-environment Python `-m compileall -q sdk/python crates/crikey-legacy-compat/python compatibility/test-plugins` | Passed |
| WPE container: `RUSTFLAGS="-D warnings" cargo test -p crikey-web-host --features engine` | 72 library tests, 3 CLI tests, and 6 real-engine tests passed |
| Delegated container: `cargo test -p crikey-cli --lib web_host::tests -- --ignored --nocapture` | All 3 selected real-cgroup tests passed |
| `cargo check -p crikey-web-host --all-targets --all-features --target aarch64-apple-darwin` | Passed without installed WPE dependencies |
| `cargo check -p crikey-web-host --all-targets --all-features --target x86_64-pc-windows-gnu` | Passed without installed WPE dependencies |
| `cargo check --locked --manifest-path compatibility/native-conformance/Cargo.toml --all-targets` | External SDK consumer passed |
| `cargo test -p crikey-cli --lib web_host::tests -- --nocapture` | 32 passed; 3 delegation-dependent tests ignored in the ordinary environment |

The normally ignored cgroup tests were run separately because ordinary CI runners need not provide writable delegation. They exercise a detached child, an already-exited root with a surviving detached child, and a descendant created during graceful termination. Hosted cross-platform CI results are recorded on the pull request after publication, not inferred from these local gates.

## Verification limits

- Real WPE tests run with WPE WebKit 2.52.6, WPEBackend-fdo 1.16.1, and libwpe 1.16.3. The headless container emits expected software-GPU/accessibility-bus diagnostics; passing tests require actual exported document pixels, not absence of those messages.
- Real delegated cgroup containment and killing were exercised for all three scenarios above. This does not claim exhaustive coverage of every kernel scheduling, process-crash, or uninterruptible-I/O timing.
- No Miri, address/thread sanitizer, native screen-reader bridge, mixed-DPI hardware transition, or real ibus/fcitx interoperability run is claimed.
- The maximal-stride regression includes a 32-bit branch, but the runtime verification here is 64-bit.
- Linux is the supported WPE engine platform. Cross-platform CI verifies that ordinary macOS/Windows builds remain usable without WPE; it does not establish a web engine on those systems.
- A bug hunt and green tests reduce risk; they do not prove the absence of every possible bug.
