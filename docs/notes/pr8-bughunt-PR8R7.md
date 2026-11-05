# PR #8 bughunt — PR8R7

## Result

This round fixes **two behavior defects and two documentation defects** in PR #8. The documentation corrections are not counted as runtime bugs. The new tests protect the behavior fixes; they are not counted again as separate discoveries.

The unique labels are:

- **PR8R7-LAUNCHER-1:** a relative temporary-directory path produced a relative private socket address that the real web worker refused.
- **PR8R7-UI-1:** the public fallback frame builder could send web-page keys to the document and also use them to activate launcher Settings.
- **PR8R7-CONTRACTS-1:** the canonical protocol schema still described web focus and redraw fields as though every page were a display list.
- **PR8R7-NATIVE-1:** a native shutdown-test comment claimed a blocked output send without evidence that the writer had reached that state.

Both behavior regressions failed on the starting revision and passed with the fixes. The fixed workspace passed 2,561 tests; the actual WPE suite, delegated-cgroup cases, native-window regression, public UI consumer and complete launcher/browser path also passed. Detailed evidence and limits follow.

## Scope, structure and coordination

Starting commit: `dbb678ebe946fcb134b93066b26654970bfea4d0` on `feature/page-web-surface`. The starting working tree was clean. Scope was [PR #8](https://github.com/yichenchong/CriKey/pull/8), compared with `feature/page-image-canvas`, not unrestricted cleanup of the repository.

The initial PR had 55 changed paths: **49 implementation, test, build, schema, SDK and public-documentation paths**, plus six historical audit reports. Those previous reports were used to avoid repeating old findings. They were not edited, and their test results were not treated as fresh evidence.

The implementation is a Rust workspace. A native/Rust plugin declares a web page. The launcher checks the registered plugin's permissions, starts a separate optional Linux WPE worker, routes document input over a private Unix connection, and composites accepted browser pixels into its native window. This is distinct from the older display-list page form, where a plugin supplies nodes and requests host-driven updates.

Eight section owners reviewed the complete assigned PR changes and their necessary consumer context, not just the last commit. Two independent reviewers then inspected the proposed fixes. The coordinator owned cross-section decisions, reproductions, builds, runtime checks, this report and publication.

Every dispatched agent received these requirements:

- Use `/home/yichen/projects/crikey/.venv/bin/python`, never system-wide Python.
- Keep imports at module scope, even for a function-level fix.
- Fix confirmed mistakes only within assigned ownership. Route naturally discovered findings outside that section back to the coordinator; do not start unrelated searches or edits.
- Do not run builds, tests, linters or formatters while section owners are editing. Central checks run after handoff.
- Do not manufacture findings or tests. New tests must check meaningful consumer behavior, boundaries, state transitions or errors, not source wording, field forwarding, mock echoes or incidental defaults.
- Separate source reasoning, historical results and newly observed execution.

No language server was configured. The coordinator preserved the starting source and used a separate baseline Cargo target. Baseline and fixed builds never shared an output directory, avoiding cached source-root values from another checkout. The host used Rust/Cargo 1.86; actual WPE checks used the `wpe` container with Cargo 1.95, WPE 2.52.6 and WPEBackend-fdo 1.16.1.

The final changes are limited to five existing source/test/schema files and this new report. There are no dependency, lockfile, schema-field/tag, permission, frame-limit, platform-support or shared plugin transport changes.

## PR8R7-LAUNCHER-1 — relative TMPDIR prevented the worker from connecting

### What was wrong

`PrivateEndpoint::create` in `crates/crikey-cli/src/web_host.rs` joined its private directory name onto `std::env::temp_dir()` without making the result absolute. Rust can return the configured `TMPDIR` spelling without forcing an absolute path. For example, an existing writable directory named by `TMPDIR=tmp` can be used to create and bind a socket relative to the launcher's current directory.

The private web worker has a stricter and intentional contract: both `Options::parse` and `engine::run` require an absolute Unix socket path. The launcher therefore successfully created a relative resource, passed its relative address to the worker, and waited while the worker refused before connecting. Production startup then reported the existing five-second accept timeout.

The path also depended on the current working directory for later connection and cleanup. A separate socket consumer reproduced that dependence by changing cwd after binding.

This is distinct from R6's non-UTF-8 corruption. R6 correctly preserves native bytes. R7's relative UTF-8 path still failed even with byte preservation working; the missing property was an absolute address. The existing absolute native-byte case remained a successful control before the R7 fix.

### Narrow correction

`PrivateEndpoint::create` now makes **only relative** temporary roots absolute before entering its existing allocation loop:

1. Obtain the native temporary root as an owned `PathBuf`.
2. If it is already absolute, retain that owned path directly, avoiding another allocation and copy.
3. Otherwise call `std::path::absolute` once. If cwd cannot be resolved, report the configured root in the error rather than silently choosing another temporary directory.
4. Use the resulting absolute root for all existing collision attempts.

`std::path::absolute` is available since Rust 1.79, below the workspace's 1.86 minimum. It preserves native filename bytes and does not resolve symlink targets. The worker's absolute-path requirement is not weakened.

The surrounding behavior remains unchanged: private 0700 directory creation and permission repair, the bounded 32-attempt collision loop, listener ownership, native `OsString` argument handling, automatic cleanup and existing cgroup/process policy. There is no fallback directory, new environment override, retry mechanism, public flag or shared endpoint serializer.

### Why not canonicalize the temporary root?

Physical path resolution is not needed to make this private address absolute. It can also break a previously working path. A short absolute symlink can point to a deeply nested directory whose physical spelling exceeds the Unix socket pathname capacity. Binding through the short alias works; expanding the target first can make it impossible to bind.

The final correction therefore preserves an existing absolute alias. The new tests cover a short native-byte alias whose physical target includes a 144-byte component, already longer than Linux's Unix socket pathname capacity. Successful socket binding and connection through that alias protect against introducing unconditional canonicalization.

This is not inconsistent with R6's packaging path fix: packaging must compare Cargo's physical manifest identity with its repository root, while a socket needs a usable absolute spelling that both endpoints can resolve. They are different contracts. The alias case is a guard against a rejected fix design, not a second defect attributed to the starting code.

### Ordinary real-socket regression

New test: `web_host::tests::relative_native_tmpdir_produces_a_cwd_independent_endpoint`.

The test isolates process-global TMPDIR and cwd changes in an exact-test subprocess. It uses real filesystem directories, a real private endpoint, a real Unix listener and the native framed transport. It covers a relative native-byte directory containing `0xff` and the short absolute native-byte symlink described above.

Each child creates and binds the endpoint, changes cwd to `/`, reconnects through the stored address, transfers and decodes an actual framed WebFrame, and checks resource cleanup. The connection and accept/receive operations are finite. The test does not substitute an argv-printing helper for a worker or merely compare a copied argument.

Observed starting-revision failure:

```text
TMPDIR "tmp-\xFF" subprocess failed
...
the endpoint remains connectable outside the launcher's working directory:
Io("No such file or directory (os error 2)")
```

The selected test exited 101. This was an actual kernel connection failure after changing cwd, not an early assertion that a field should be absolute. The fixed test passed and exercised both configured roots.

### Real production-spawner regression

The existing ignored test `real_host_connects_through_a_non_utf8_temporary_directory` now runs four isolated configurations through actual `spawn_production` and the installed worker:

- Existing absolute non-UTF-8 temporary root, retaining the R6 control.
- Relative UTF-8 temporary root.
- Relative non-UTF-8 temporary root.
- Short absolute non-UTF-8 symlink to the overlong physical target.

Each child has an explicit cwd and configured TMPDIR, requires the worker's actual connection, closes the transport, terminates through production ownership and checks endpoint cleanup. The test remains ignored in ordinary runs because a real worker and writable, memory-enabled cgroup delegation are prerequisites.

On the starting revision, the existing absolute native-byte case passed. The next `relative-utf8` case failed with:

```text
web host did not connect to its private endpoint within 5000 ms:
I/O operation timed out
```

The delegated run had the three existing detached-descendant cleanup cases pass and the expanded production case fail; exit status was 101. Later variants in that failed production case are not claimed as separately executed baseline failures.

On fixed source all four top-level delegated tests passed, including all four temporary-root variants within the production-spawner test. This proves the changed endpoint construction, actual worker parser/connection and cleanup. It does not claim document rendering under every possible global TMPDIR environment; normal document rendering is verified separately through the complete path below.

### Documentation and patch provenance

The public guides do not promise relative worker endpoint syntax; none needs to be loosened. This requested report records the fix. The private worker still receives the existing `--endpoint unix:/absolute/path` argument.

The agent captured the final raw tests-only diff with the original production lines restored, then applied the correction. The coordinator applied that test patch only to the preserved starting source and used its isolated Cargo target. An earlier canonicalization proposal was rejected before central verification; it is not described as a published or observed original-code failure.

## PR8R7-UI-1 — web keys could also activate host Settings in the fallback builder

### Which path was affected

There are two input routes to distinguish:

- The native winit dispatcher forwards document keys immediately and withholds those keys from egui's ordinary host input processing.
- The public `build_launcher_frame` API uses a fallback route where input is supplied through egui. That route reads the key events to produce document commands, while egui also processes focus and widget activation.

The confirmed defect is in the second route. It is a public-builder correctness problem, not a claim that the main native route was already duplicating every key. The shared footer is used by both routes, so native pointer and keyboard behavior was also checked after the change.

### Root cause and visible effect

The footer's Settings label used `Sense::click()`, which is keyboard-focusable as well as clickable. A document Tab could move egui's host focus to that label. A following Enter or Space was then treated as a synthetic primary click on Settings, in addition to the document's input handling.

That host click produces `UiCommand::OpenSettings`. The normal host consumer opens its Settings panel and closes/replaces the page. Document keyboard navigation could therefore trigger a launcher action the user had not requested. A pending declaration without an accepted surface ID had the same host-focus problem even though document input could not yet be sent.

Focus retained from an earlier display-list page was another relevant transition. Merely preventing new Tab focus would not be sufficient if Settings already held focus when a web page appeared.

Source review verified this against the installed egui 0.29.1 implementation:

- Tab traversal is recorded by egui's focus processing.
- `Context::create_widget` registers focus interest and surrenders disallowed focus before obtaining the widget response.
- Enter/Space synthetic clicks are computed during `get_response`, using the current focus. They are not irrevocably decided at frame start.
- Accessibility default-click handling depends on click sensing, independently of keyboard focusability.

For the launcher currently drawn with a web page, Settings is the only interactive host widget. The query is noninteractive while a page exists, the page renderer does not draw document controls as egui widgets, and the result/action/settings panels are not drawn in that branch. This made the footer the appropriate narrow correction rather than introducing an input-stream cloning or partitioning layer.

### Fix

In `crates/crikey-ui/src/native.rs::draw_status`:

1. Detect a declared web page from the page's web declaration, not from the presence of an accepted surface ID. Pending declarations are included.
2. Clear any retained egui host focus before constructing the footer response, so an old focus cannot turn the document's Enter/Space into a host click.
3. Keep the Settings label click-sensing but make it non-keyboard-focusable while that web declaration is active.
4. Leave ordinary launcher/display-list Settings focus and keyboard behavior unchanged.

The adjacent comment states that Ctrl+comma, Tab, Enter and Space belong to the document. Pointer/touch click sensing remains enabled, and the accessibility default-click route is not replaced with a pointer-only response check. Accessibility compatibility here is a source conclusion; a real assistive-technology session was not run.

No public type, protocol field, keycode mapping, IME policy or document input order changes. R6's separate IME bookkeeping fix remains intact and is not counted again.

### Public-builder regression

New test: `web_keyboard_navigation_cannot_activate_the_host_settings_control` in `crates/crikey-ui/tests/native_renderer.rs`.

It exercises the actual public builder across:

- Accepted document versus pending declaration with no surface ID.
- Fresh document Tab navigation versus genuine Settings focus retained from a prior display-list page.
- Enter versus Space, including Space's text event.
- Three layout passes during activation.

The prior-focus setup uses an empty display-list page whose nonfocusable probe leaves Settings as the eligible host control. It checks that focus was actually obtained before changing page forms. Those assertions establish the required starting condition; they are not substitutes for the next consumer-command assertions.

For an accepted document the test requires the appropriate document key or text delivery. Before admission it requires no document input to a nonexistent surface. In all cases it forbids host OpenSettings, SetQuery and Quit from those document-owned keys.

The expanded test-only patch was applied to the starting source. The actual baseline run failed at the forbidden-host-command assertion:

```text
web-owned keys cannot activate host chrome,
even before admission or after host focus
```

Exit status was 101. It failed at the behavior assertion, not at prior-focus setup. The fixed public-builder regression passed across its matrix. The initial raw regression was captured before production editing; the expanded test-file-only patch also contained no production correction.

### Native-copy and pointer nonregression checks

The existing native-copy unit test was renamed/expanded as `native_document_copies_update_chrome_without_replaying_or_activating_it`. It keeps pointer-motion/departure coverage and adds copied Tab, Enter and Space events in native-input mode. Those copies must neither replay document commands nor activate host controls. The selected fixed test passed.

The existing public pointer Settings test continues to check the route with keyboard hints enabled and disabled. Full workspace execution passed it. The explicit ignored real-window touch/IME dispatcher regression also passed with Xvfb, a real GPU surface and XTEST Enter.

### Separate public API smoke

A throwaway executable, outside the test harness, constructed a validated web declaration and accepted raster using public UI/core types. It called `build_launcher_frame` for an initial frame, Tab and Enter, then inspected the emitted commands. It did not call private helpers or use an echo object.

Observed fixed output:

```text
TAB_COMMANDS=[WebInput(Key {
    surface_id: 41, timestamp_ms: 33, pressed: true,
    keysym: 65289, hardware_keycode: 0, repeat: false, modifiers: 0
})]
ENTER_COMMANDS=[WebInput(Key {
    surface_id: 41, timestamp_ms: 50, pressed: true,
    keysym: 65293, hardware_keycode: 0, repeat: false, modifiers: 0
})]
PUBLIC_BUILDER_DOCUMENT_ENTER_WITHOUT_HOST_ACTIVATION=ok
```

The executable required document Enter and rejected host activation, exiting 0. Zero hardware code is the fallback's existing honest metadata; it is not an invented physical scancode. This is an actual headless public-builder command check, not browser rendering. The real native-window/browser observations below separately verify the shared footer and native surface.

## PR8R7-CONTRACTS-1 — canonical schema comments contradicted web-frame validation

**Classification: documentation-only contract correction.** No codec, validation or scheduling behavior changed.

The canonical `sdk/protocol/crikey/v1/plugin.proto` extends `PageFrame` with web declarations, but the old `focus_node` and `redraw_after_ms` comments still described display-list behavior without qualification. A protobuf/native SDK author could infer that a web frame may select a node or request a nonzero redraw interval which the host would clamp and answer.

Actual core validation refuses nonzero web focus and redraw fields. Web documents repaint through their engine even when the plugin's redraw value is zero. `WebSurfaceBuilder` constructs the zero values required by that contract.

The schema comments now:

- Describe `focus_node` as display-list node focus, requiring zero for a web frame because its document owns focus.
- Scope static-until-input/plugin self-scheduling and the 16-ms minimum to display lists.
- Explicitly require zero web redraw and explain independent engine repainting and refusal of a nonzero web value.

R5 corrected the analogous Rust API documentation. This entry is only the remaining canonical schema location, not a second count of that earlier core edit. The PR's new rendering form made these otherwise inherited generic comments incomplete.

All fields, numbers, enum values, defaults, generated/handwritten bindings and permission rules remain unchanged. No prose-matching test was added. Existing core tests, including web focus/redraw refusal, and SDK/web protocol consumers passed as part of the full workspace run. The correction itself is the documentation update; no speculative API or separate guide rewrite was needed.

## PR8R7-NATIVE-1 — shutdown-test prose overstated blocked-writer coverage

**Classification: documentation/evidence correction, not a newly found shutdown implementation bug.**

In `crates/crikey-web-host/tests/engine_smoke.rs`, `disconnected_launcher_unblocks_reader_and_writer_shutdown` opens a page, waits for its HTTP request, pauses reading for 300 ms, disconnects the socket and requires the child to exit within five seconds.

Its old comment said one 320×200 frame exceeded a typical socket buffer and that the sequence therefore exercised a blocked output writer, not an idle worker. Neither the HTTP request nor the delay proves that a frame reached the writer before disconnection. A cold or busy renderer may not yet have produced one, and a typical buffer size is not an observed blocked-send state.

The runtime owner noticed this outside its file ownership and routed it to the native section. After that section froze, the coordinator changed only the comment: frames may be produced while the peer stops reading; the test establishes bounded shutdown but does not establish that the writer reached a blocked socket send.

The test name, behavior, delay, transport and bounded child wait are unchanged. The actual WPE scenario passed in the fresh 15-scenario run. It is useful disconnect/shutdown coverage, but this report does not upgrade it to assured blocked-send coverage or claim to have fixed an unobserved deadlock. No weaker assertion or fake producer was introduced to hide a failure.

## Complete changed-file accounting

| Existing file changed | Every change and owning issue |
| --- | --- |
| `crates/crikey-cli/src/web_host.rs` | LAUNCHER-1: explanatory temporary-root comment; absolute-root reuse without copying; relative-root resolution and named error before the existing retry loop; new real socket/cwd/byte/alias regression; expanded existing real production-spawner test with four isolated root variants and cleanup checks. |
| `crates/crikey-ui/src/native.rs` | UI-1: clear retained host focus before web footer response; nonfocusable but click-sensing Settings for declared web pages, including pending admission; updated ownership comment; existing native-copy test renamed/expanded with Tab/Enter/Space no-replay/no-host-activation checks. |
| `crates/crikey-ui/tests/native_renderer.rs` | UI-1: new public-builder regression matrix for admission, retained/fresh focus, Enter/Space/text and three-pass layout; existing pointer Settings checks retained. |
| `sdk/protocol/crikey/v1/plugin.proto` | CONTRACTS-1: focus and redraw comments corrected; no declarations changed. |
| `crates/crikey-web-host/tests/engine_smoke.rs` | NATIVE-1: three-line explanation replaces unsupported blocked-writer guarantee; no test behavior changed. |

This report, `docs/notes/pr8-bughunt-PR8R7.md`, is the sixth changed path. No historical report, dependency, lockfile, shell build recipe or unrelated source was changed. No temporary executable, plugin, capture script, test helper API, compatibility alias or fallback implementation is committed.

The coordinator refined the path fix to retain already-absolute owned paths and rejected physical symlink expansion before publication. These are implementation choices under LAUNCHER-1, not additional original-code findings. Both independent reviewers found no further confirmed defect in the final scoped changes; their source review was not presented as runtime verification.

## Complete section-review inventory

The following accounts for all 49 assigned PR paths. Unchanged surrounding files were read only as needed to verify a declaration, actual consumer or historical exclusion.

### Engine runtime — 2 paths

- `crates/crikey-web-host/src/engine.rs`
- `crates/crikey-web-host/src/main.rs`

Reviewed both full PR additions: startup/partial-frame deadlines; absolute/native endpoint parsing; storage-mode admission before UTF-8/FFI use; one-attempt WPE initialization; default-context reservation and private thread-default refusal; bounded command work and wake coalescing; callback allocation/borrows and borrowed pixels; metadata bounds; terminal diagnostics; reader/writer setup, failures and shutdown; source cleanup after native return; and all tests. No new runtime implementation defect was confirmed here. The shutdown-test evidence concern was routed rather than edited out of ownership.

### Native adapter and browser scenarios — 3 paths

- `crates/crikey-web-host/csrc/ck_web_shim.c`
- `crates/crikey-web-host/csrc/ck_web_shim.h`
- `crates/crikey-web-host/tests/engine_smoke.rs`

Reviewed every native callback/function, backend/GObject ownership, mapped SHM access/release, surface teardown, GLib fd/signal source lifetime, keyboard/pointer/wheel/focus ABI, scalar-to-UTF-16 IME handling, profile/cookie lifetime, navigation/download policy and all 15 actual-host tests. No new native implementation defect was confirmed. The only file change is NATIVE-1's evidence comment, integrated by the coordinator after the original handoff.

### Host state and conversion helpers — 6 paths

- `crates/crikey-web-host/src/frame.rs`
- `crates/crikey-web-host/src/ime.rs`
- `crates/crikey-web-host/src/input.rs`
- `crates/crikey-web-host/src/lib.rs`
- `crates/crikey-web-host/src/output.rs`
- `crates/crikey-web-host/src/state.rs`

All six modules were reviewed in full: checked dimensions/stride/length arithmetic; byte caps; generation exhaustion and allocation reuse; input units and invalid-transition atomicity; held buttons and focus reset; composition/deferred-clear order; message direction, surface identity and terminal state; storage/URL admission; bounded output ordering, replacement and first-failure preservation; and feature gates. No new confirmed defect or justified extra test was found. All six remain unchanged.

### Launcher and application — 7 paths

- `crates/crikey-cli/src/web_host.rs`
- `crates/crikey-cli/src/lib.rs`
- `crates/crikey-cli/src/plugin_commands.rs`
- `crates/crikey-cli/src/settings.rs`
- `crates/crikey-cli/tests/m7_plugin_commands.rs`
- `crates/crikey-app/src/lib.rs`
- `crates/crikey-app/src/plugin_action.rs`

Reviewed the complete new supervisor and all assigned PR changes: exact registered-owner permissions before lazy worker creation; page ownership/activation/replacement/failure closure; bounded commands/notices and latest frames; stale identities and generations; discovery/native arguments; temporary and persistent paths; memory policy and inherited limits; mount/delegation selection; pre-exec attachment; parent-death qualification; identity-safe process cleanup; doctor verdicts and settings residency. LAUNCHER-1 was the only new behavior finding. Application, doctor and settings source remain unchanged.

### Native UI and rendering — 4 paths

- `crates/crikey-ui/src/lib.rs`
- `crates/crikey-ui/src/native.rs`
- `crates/crikey-ui/tests/launcher_view_model.rs`
- `crates/crikey-ui/tests/native_renderer.rs`

Reviewed frame identity/generation/bytes; texture upload/memoization/eviction; untinted drawing and clipping; measured viewport and multipass effects; native versus fallback input; keyboard/modifier routing; pointer/touch capture, wheel phase and hidden/reactivated ownership; query/chrome isolation; caret/IME positioning; composition resets and public view-model transitions. UI-1 was the only new finding. Other rendering and state paths were retained, not rewritten to establish a second convention.

### Contracts, permissions and SDK — 12 paths

- `crates/crikey-core/src/lib.rs`
- `crates/crikey-core/src/page.rs`
- `crates/crikey-native-protocol/src/convert.rs`
- `crates/crikey-native-protocol/src/message.rs`
- `crates/crikey-native-protocol/src/transport.rs`
- `crates/crikey-native-protocol/tests/transport.rs`
- `crates/crikey-native-protocol/tests/wire.rs`
- `crates/crikey-plugin-model/src/permissions.rs`
- `sdk/protocol/crikey/v1/plugin.proto`
- `sdk/rust/src/builder.rs`
- `sdk/rust/src/lib.rs`
- `sdk/rust/tests/page.rs`

Reviewed tags, defaults, unknown fields, scalar resets, same/different oneof merging, fragmented nested messages, shared allocation/depth budgets, raster/IME fields, conversion and error handling, URL/storage boundaries, independent network/persistence grants, SDK navigation/stay serving, and actual stalled-peer write-timeout tests. CONTRACTS-1 is the sole correction; no new codec, permission or SDK runtime defect was confirmed. Shared textual native endpoint handling remains separate from the private worker OS argument.

### Build and Linux packaging — 9 paths

- `Cargo.toml`
- `Cargo.lock`
- `crates/crikey-cli/Cargo.toml`
- `crates/crikey-web-host/Cargo.toml`
- `crates/crikey-web-host/build.rs`
- `packaging/linux/build.sh`
- `packaging/linux/tests/build.test.sh`
- `packaging/linux/rpm/crikey.spec`
- `packaging/linux/flatpak/org.crikey.CriKey.yaml`

Reviewed default-off engine/target gates, direct development-library/API requirements, C11/include/link ordering, pkg-config tracking, source versus supplied binaries, successful locked Cargo artifact selection, target/output identities and aliases, ELF/library checks, stage replacement and archive consumption, Debian versioned dependency/fallback policy, RPM ELF dependency generation and explicitly no-web Flatpak source boundaries. No new confirmed issue was found. Prior physical repository/output fixes remain intact. Scanner fixture versions were not mistaken for measured engine minimum versions; no speculative compatibility floor was added.

### Public documentation — 6 paths

- `docs/architecture.md`
- `docs/user-guide.md`
- `docs/plugin-author-guide.md`
- `docs/adr/README.md`
- `docs/adr/0020-plugin-pages.md`
- `docs/spec/crikey-spec-v1.md`

Reviewed every PR-modified claim and necessary adjacent text against declarations/consumers: two page forms, actual SDK support, permission/storage distinction, lifecycle/generations, process/thread/queue ownership, startup/first-frame/failure behavior, keyboard/IME/accessibility limits, navigation/download scope, cgroup memory/cleanup qualification, package modes and measured-versus-historical statements. No new error in these six documents was confirmed; they remain unchanged. This does not conflict with CONTRACTS-1, which belongs to the separate canonical schema file.

## Fresh central verification

All results here were exercised by the coordinator after section handoff. Owner inspection and prior-round outputs are not pass evidence.

| Check | Observed result |
| --- | --- |
| Rust formatting and `cargo fmt --all --check` | Passed, exit 0. |
| Locked all-target workspace check, Rust warnings denied | Passed, exit 0. |
| Locked all-target workspace Clippy, warnings denied | Passed, exit 0. |
| Serial locked workspace all-target tests | **2,561 passed**, 155 suites, 6 ignored, 28 filtered; no failed suite. |
| Ordinary workspace documentation tests | **1 passed**, 27 suites; exit 0. |
| Ordinary socket/cwd/native-alias regression | Failed on baseline with actual ENOENT; passed fixed. |
| Public fallback keyboard regression | Failed on baseline at host-command assertion; passed fixed. |
| Separate public UI consumer executable | Passed; document Tab/Enter only, no host activation. |
| Native-copy Tab/Enter/Space regression | Passed. |
| Existing explicit ignored native-window dispatcher regression | Passed with actual X11/GPU window and XTEST Enter. |
| Actual delegated-cgroup fixture | **All 4 passed**, including the expanded production test's 4 temporary-root variants. |
| Actual WPE library tests | **78 passed**. |
| Actual worker CLI tests | **5 passed**. |
| Actual browser scenarios | **All 15 passed**, including disconnect shutdown under its corrected evidence description. |
| Linux packaging consumer suite | **97 passed, 0 failed**. |
| Both Linux shell syntax checks | Passed individually. |
| External native conformance all-target check | Passed, exit 0. |
| WASM conformance release target | Passed, exit 0; existing target was up to date. |
| C-ABI conformance make target | Up to date, exit 0; no fresh C fixture compilation is claimed. |
| Virtual-environment Python compilation | Passed for SDK, Legacy Compatibility Layer and tracked test plugins. |
| Complete current native launcher/browser path | Real HTML, text, document Tab/Enter, host pointer Settings, restored query, fresh reopen, browser removal and normal launcher exit 0. |

Counts overlap: helper tests appear in both workspace and engine-feature runs. They must not be added as though all rows were different tests. Environment-dependent ignored cases were run explicitly; their absence from the ordinary suite was not hidden.

Representative commands, from the appropriate fixed/baseline source root and with the stated toolchain/environment:

```sh
cargo fmt --all --check
RUSTFLAGS='-D warnings' cargo check --locked --workspace --all-targets -j2
RUSTFLAGS='-D warnings' cargo clippy --locked --workspace --all-targets -j2 -- -D warnings
RUSTFLAGS='-D warnings' cargo test --locked --workspace --all-targets -j2 -- --test-threads=1
RUSTFLAGS='-D warnings' cargo test --locked --workspace --doc -j2
cargo test -p crikey-cli --lib web_host::tests::relative_native_tmpdir_produces_a_cwd_independent_endpoint -- --exact --nocapture
cargo test -p crikey-ui --test native_renderer web_keyboard_navigation_cannot_activate_the_host_settings_control -- --exact --nocapture
cargo test --locked -j1 -p crikey-web-host --features engine -- --test-threads=1 --nocapture
packaging/linux/tests/build.test.sh
```

Actual delegated tests used a private mount namespace with a read-only full cgroup view and a later writable delegated bind. Only the test subprocess was moved into its launcher leaf. All Cargo builds happened before entering that constrained fixture. The installed worker used in the production-spawner proof was the actual engine-enabled binary, not a script imitating its arguments.

### Complete live launcher/browser path

A throwaway native Rust SDK plugin served its own localhost HTML and used the actual SDK handshake/page declaration. It declared network permission and default ephemeral storage. The current launcher and actual WPE worker ran under a temporary delegated cgroup with isolated HOME and XDG configuration, cache, data, state and runtime directories. Rendering used container-local Xvfb/software rendering, not Chromium automation or a supplied static browser image.

Observed sequence:

1. Typing `r7web` displayed the real `r7web full web smoke` result.
2. Enter opened the actual `PR8R7 full launcher web smoke` HTML page. A finite helper waited for real blue document pixel RGB `(22, 50, 79)`, rather than assuming a delay established paint.
3. Clicking its input and typing displayed exactly `R7_runtime_text`.
4. A settled Tab capture showed the document button's focus outline. An immediate earlier capture still showed input focus; it was not misreported as a completed focus transition.
5. Enter activated that focused document button, changing its label to `Clicked` and background to green. A finite helper observed actual green RGB `(8, 107, 53)` before the result was claimed. Launcher Settings did not open.
6. Ctrl+comma left the document visible. The live counter was observed at 71, 245 and 387 in successive inspected captures, demonstrating continuing browser-produced updates.
7. Clicking the host footer Settings control opened the real Settings panel and removed the browser group. Thus making the label nonfocusable did not remove its pointer route.
8. Closing Settings restored the query. Ctrl+A and typing visibly changed it to `r7web_after`, verifying normal query editing after the web-page focus clear. It was then reset to `r7web` for reopening.
9. Reopening created a new browser group. The measured/inspected image showed a blue page, empty input, `Click me` and live counter 1, rather than the previous typed/green/clicked state.
10. Escape removed the new browser group. Ctrl+comma from the query opened Settings; clicking Quit CriKey ended the actual launcher normally.

The first group was `crikey-web-150866-1`; it contained three observed process IDs and `memory.max` was `805306368` bytes, the existing 768-MiB ceiling. Reopening created `crikey-web-150866-2`. After Settings and after Escape, the fixture had no matching browser groups. The final wrapper recorded:

```text
PR8R7_LAUNCHER_EXIT=0
```

The fixture's parent cgroup was absent afterward. Native evidence remains outside the repository at `/tmp/pr8r7-fullpath`, including `query.png`, `tab-settled.png`, `document-shortcut.png`, `settings.png`, `query-edited.png` and `reopened.png`.

These observations complement, rather than replace, the headless public-builder reproduction. The actual native route already withheld document keys; the live run verifies that the shared footer change preserves document behavior and host pointer access in that route.

## Verification setup corrections and cleanup

These are test-harness/environment details, not additional product findings or hidden failed gates:

- The temporary complete-path launcher wrapper was deliberately run with a nonexistent window pattern and a two-second readiness deadline. It returned the expected timeout status 124, killed its owned cgroup, waited for the launcher child and removed the parent group. The coordinator verified the parent was absent. This exercised the harness's failure cleanup instead of letting `set -e` bypass child reaping.
- The initial harness had not isolated HOME/XDG_STATE_HOME. Its deliberate kill left an abnormal-shutdown record, so a setup-only next launch correctly reported that previous shutdown. That setup launch was closed normally. The fixture was corrected to isolate HOME and state as well as config/cache/data/runtime before the reported browser observations. No application code or warning was suppressed.
- LXC refused overwriting the existing temporary script; metadata showed it was owned by UID/GID 1000 with mode 775. The coordinator removed only that owned staged file, pushed the revised copy, parsed it and ran it. The permission error did not exercise or change product code.
- The before/fixed Cargo targets were kept separate. The starting source used only regression test patches, not either behavior fix. Large completed baseline build artifacts were removed after their proof to preserve disk headroom.

After verification the coordinator removed owned baseline sources/targets/archives, external public-UI and plugin projects/targets, temporary runtime/configuration/state/home directories, mount/cgroup fixtures, staging scripts/archives and the temporary display. The temporary public consumer's own generated package artifacts were cleaned without deleting shared workspace dependencies. Captures and session evidence remain outside the repository. No scaffolding is committed.

## Explicit scope and evidence limits

- **No claim of zero remaining bugs.** This is a full sectioned PR review with concrete failures and checks, not a proof of every platform, input method or timing schedule.
- **Historical fixes are not new findings.** Native argument byte preservation, private GLib context refusal, default-context ownership, one-attempt initialization, IME callback ordering, packaging root identity, earlier queue/generation/storage fixes and earlier coverage remain in place but are not re-counted.
- **Inherited exclusions remain unchanged.** The egui zoom/layout question, ADR size-step sentence, strict core HTML rustdoc link in `generation.rs:72`, and extreme upstream signed-coordinate concern were not reopened without a new PR basis. The known strict core HTML-doc failure was not rerun merely to confirm it. Passing ordinary doctests is a different check.
- **Inherited title bounds are not silently expanded.** `PageFrame.title` and its existing consumer predate these PR changes. No unrelated title validator was added merely because the same source file contains new web code.
- **Blocked send is not guaranteed by the native disconnect scenario.** NATIVE-1 corrects that claim. The fresh run proves its actual bounded disconnect/shutdown assertion, not a measured blocked-write state or a newly fixed deadlock.
- **Accessibility and physical input remain qualified.** Source review confirms the Settings click/action routes are preserved. No real screen reader was exercised. The existing native-window fixture injects touch/IME events and captures XTEST Enter; it is not physical touch or a full ibus/fcitx candidate-window session. Actual WPE supplementary preedit tests do not establish complete OS IME interoperability.
- **Linux engine verification is not universal platform support.** Actual rendering used the stated WPE stack and X11/software display. No new Wayland hardware, mixed-DPI, Windows/macOS WPE, complete browser accessibility tree, sanitizer or arbitrary-media compatibility result is claimed.
- **Resource guarantees retain their boundaries.** Actual delegated startup and detached-descendant cleanup were checked while the supervisor was alive. Root-only parent-death signaling still does not promise deterministic cleanup of every detached descendant after abrupt launcher death. No general network sandbox was added.
- **Packaging checks are not distribution certification.** The controlled suite consumes staged payloads/archives/dependency metadata and protects artifact/path contracts. It is not a new real Flatpak SDK build, engine-enabled RPM installation or compatibility certificate for arbitrary supplied hosts. Flatpak remains no-web.
- **No unrelated fixes were hidden.** Sections without new confirmed defects remain unchanged; no artificial test, warning suppression, compatibility shim, retry layer or speculative validation was added to increase the issue count.
