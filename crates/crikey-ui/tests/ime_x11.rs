//! Live end-to-end input-method evidence for the retained X11 launcher window.
//!
//! # What this proves, and how
//!
//! `crates/crikey-ui/examples/ime_probe.rs` established that winit's X11
//! backend emits `Ime::Preedit("", None)` and then `Ime::Commit("æ")` for a
//! real `Multi_key a e` compose sequence typed through XTEST. That probe drives
//! a bare `winit` window: it says nothing about `NativeApplication`,
//! `egui_state`, or whether the composed byte ever becomes a query. This test
//! closes exactly that gap by driving the shipped [`NativeLauncher`] — the same
//! object `crikey dev measure-activation` and `run_native_launcher` construct —
//! under a private `Xvfb`, typing into it with `xdotool` (XTEST, so the X server
//! really synthesises the key events), and asserting on the `UiCommand`s the
//! application dispatches to its host callback.
//!
//! ## Coverage, stated precisely
//!
//! * **Commit — live.** `Multi_key a e` is typed through XTEST. The key
//!   *presses* are swallowed by the input method (only releases reach the
//!   window, exactly as the probe recorded), the composed `æ` arrives as
//!   `Ime::Commit`, and the assertion is that it is *appended* to the query the
//!   application already held and dispatched as `UiCommand::SetQuery("aæ")`.
//!   Nothing here is synthesised in-process.
//! * **Empty preedit — live.** The `Ime::Preedit("", None)` that winit emits
//!   immediately before that commit is delivered by the same real sequence,
//!   while the query already holds `"a"`. The assertion is that the dispatched
//!   query sequence is exactly `["a", "aæ"]`: an empty preedit that cleared or
//!   corrupted the query would show up as an extra or different `SetQuery`.
//! * **Non-empty preedit — unit level only.** **No non-empty preedit was
//!   observed live, and none was reproducible here.** What was measured: this
//!   host runs no ibus, fcitx5, uim-xim or scim service, so the only input
//!   method is Xlib's built-in "local" one, and every run of the sequence
//!   above — through the bare probe and through the launcher alike — reported
//!   `Preedit(text="", range=None)` and nothing else. The non-empty case is
//!   therefore exercised against
//!   [`build_launcher_frame`], the frame builder `GraphicsState::draw` itself
//!   calls, with the same `egui::Event::Ime(ImeEvent::Preedit(..))` that
//!   `egui-winit` emits on the platforms where it forwards preedit at all.
//!
//! ## What the renderer had to change, and why the test would have failed
//!
//! `egui-winit` 0.29 discards *every* `WindowEvent::Ime` when compiled for
//! Linux (egui #5008). That silently loses composed characters, and this test
//! is what caught it: before the fix the live sequence dispatched
//! `["a", "a"]` — the `æ` never reached the query. `NativeApplication` now
//! forwards the commit itself, anchored with an `ImeEvent::Enabled` because
//! egui refuses a commit whose caret has moved off the composition anchor; the
//! reasoning is at the forwarding site in `native.rs`. The unit case
//! `a_commit_needs_its_anchor_to_reach_the_query` pins that anchoring, so the
//! forwarding cannot be reduced back to a bare commit without a failure that
//! names the reason.
//!
//! # Why `harness = false`
//!
//! `winit` refuses to build an event loop off the process main thread, and
//! permits exactly one per process. libtest runs `#[test]` functions on worker
//! threads, so the shipped launcher simply cannot be constructed inside one.
//! This target therefore owns its own `main`, which is the process main thread.
//! The consequence is that the whole file cannot carry a bare
//! `#![cfg(target_os = "linux")]` — a target with no `main` fails to link — so
//! the gate is on the module, and the other platforms get an empty `main`.
//!
//! # Isolation and honesty
//!
//! The `Xvfb` guard follows `crikey-platform-linux/tests/hotkeys_x11.rs`: a
//! private display number derived from the pid, torn down on `Drop` so a
//! panicking run leaks no server. A missing `Xvfb` or `xdotool` is a **named
//! panic**, never a skip. A skipped test is not evidence.

#[cfg(not(target_os = "linux"))]
fn main() {
    // The evidence this target carries is X11-specific. `crikey-ui` builds on
    // Windows and macOS, so the binary must still link there.
}

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(target_os = "linux")]
mod linux {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use crikey_core::Generation;
    use crikey_ui::{
        build_launcher_frame, create_launcher_context, egui, wgpu, NativeLauncher, NativeLauncherConfig,
        NativeLauncherEvent, NativeLauncherHandle, UiCommand, ViewModel,
    };

    /// Ceiling on an `Xvfb` becoming connectable. Not a performance assertion:
    /// it turns a server that never comes up into a named failure.
    const SERVER_READY_LIMIT: Duration = Duration::from_secs(20);

    /// Ceiling on the launcher mapping its window, and on a typed character
    /// reaching the query. Generous, because neither is being measured: a
    /// stalled renderer becomes a named failure instead of a hang.
    const UI_LIMIT: Duration = Duration::from_secs(30);

    /// Gap between polls. Polling, not sleeping-as-synchronisation: every loop
    /// ends on an observable, never on elapsed time.
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    /// Hands out a distinct display number per server within this process.
    static NEXT_DISPLAY_OFFSET: AtomicU32 = AtomicU32::new(0);

    /// The locale the built-in input method needs.
    ///
    /// Xlib opens no input method at all under the `C` locale, so winit falls
    /// straight through to raw key events and no compose sequence is ever
    /// composed. This was measured: the identical probe run reports only
    /// `KeyboardInput` events under `C`, and `Preedit("")` + `Commit("æ")`
    /// under this one.
    const IME_LOCALE: &str = "en_US.UTF-8";

    pub fn run() {
        a_commit_needs_its_anchor_to_reach_the_query();
        println!("ok  unit  a commit only reaches the query when it is anchored at the caret");

        a_non_empty_preedit_is_replaced_by_its_commit();
        println!("ok  unit  a non-empty preedit is replaced by its commit, not accumulated");

        a_live_compose_sequence_appends_its_character_to_the_query();
        println!("ok  live  a real XTEST compose sequence appends its character to the query");
    }

    // -----------------------------------------------------------------------
    // Unit level: the preedit this host cannot produce
    // -----------------------------------------------------------------------

    /// A composition in flight is shown, then *replaced* by what it commits.
    ///
    /// This is the case the host cannot deliver live (see the module header):
    /// it is asserted against [`build_launcher_frame`], the function
    /// `GraphicsState::draw` itself calls to turn accumulated egui input into
    /// [`UiCommand`]s, fed the exact `egui::Event` `egui_winit` produces from a
    /// `WindowEvent::Ime(Ime::Preedit(..))`.
    ///
    /// What the launcher does with a non-empty preedit, as measured here and
    /// not assumed: the composition is inserted into the query field so the
    /// user can see what they are typing, and that intermediate text *is*
    /// dispatched as a query. The invariant that matters is the one after it —
    /// the commit **replaces** the composition rather than appending to it.
    /// Kills the bug where an abandoned or accepted composition is left behind,
    /// so a user typing one Japanese word searches for `にほn日本`.
    fn a_non_empty_preedit_is_replaced_by_its_commit() {
        let context = create_launcher_context();

        // The first frame is what gives the query field focus: `draw_query`
        // requests focus on the response it just built, which egui applies to
        // the *next* frame. An unfocused field would ignore the events below
        // and the test would pass for the wrong reason.
        let focusing = build_launcher_frame(&context, raw_input(Vec::new()), &query_model("ab"));
        assert!(
            focusing.commands.is_empty(),
            "a frame with no input must dispatch no command, got {:?}",
            focusing.commands
        );

        let preedit = build_launcher_frame(
            &context,
            raw_input(vec![egui::Event::Ime(egui::ImeEvent::Preedit(
                "にほn".to_owned(),
            ))]),
            &query_model("ab"),
        );
        assert_eq!(
            preedit.commands,
            vec![UiCommand::SetQuery("abにほn".to_owned())],
            "the composition is shown in the query field, after the text already committed"
        );

        // The host applies what it was handed, exactly as the live callback
        // below does, so the commit frame sees the same model the application
        // would have submitted.
        let commit = build_launcher_frame(
            &context,
            raw_input(vec![egui::Event::Ime(egui::ImeEvent::Commit("日本".to_owned()))]),
            &query_model("abにほn"),
        );
        assert_eq!(
            commit.commands,
            vec![UiCommand::SetQuery("ab日本".to_owned())],
            "the commit must replace the composition it finishes, not be appended to it"
        );
    }

    /// The exact event pair the Linux renderer forwards, and why it is a pair.
    ///
    /// `NativeApplication` hands egui `ImeEvent::Enabled` immediately before
    /// `ImeEvent::Commit`, because egui refuses a commit whose caret has moved
    /// away from where the composition was anchored, and X11 delivers no
    /// anchor of its own before a commit. This pins both halves: the pair
    /// inserts, and the bare commit — which is what the forwarding would
    /// degrade to if the `Enabled` were "tidied away" — silently does not.
    /// That silent failure is the live bug this whole target caught.
    fn a_commit_needs_its_anchor_to_reach_the_query() {
        // Typed text moves the caret to the end, which is what puts it off
        // egui's default composition anchor at index zero.
        let anchored = create_launcher_context();
        build_launcher_frame(&anchored, raw_input(Vec::new()), &query_model(""));
        let typed = build_launcher_frame(
            &anchored,
            raw_input(vec![egui::Event::Text("a".to_owned())]),
            &query_model(""),
        );
        assert_eq!(
            typed.commands,
            vec![UiCommand::SetQuery("a".to_owned())],
            "a plain typed character must reach the query"
        );
        let committed = build_launcher_frame(
            &anchored,
            raw_input(vec![
                egui::Event::Ime(egui::ImeEvent::Enabled),
                egui::Event::Ime(egui::ImeEvent::Commit("æ".to_owned())),
            ]),
            &query_model("a"),
        );
        assert_eq!(
            committed.commands,
            vec![UiCommand::SetQuery("aæ".to_owned())],
            "an anchored commit must append the composed character at the caret"
        );

        let unanchored = create_launcher_context();
        build_launcher_frame(&unanchored, raw_input(Vec::new()), &query_model(""));
        build_launcher_frame(
            &unanchored,
            raw_input(vec![egui::Event::Text("a".to_owned())]),
            &query_model(""),
        );
        let bare = build_launcher_frame(
            &unanchored,
            raw_input(vec![egui::Event::Ime(egui::ImeEvent::Commit("æ".to_owned()))]),
            &query_model("a"),
        );
        assert_eq!(
            bare.commands,
            vec![UiCommand::SetQuery("a".to_owned())],
            "an unanchored commit is dropped by egui and the query is unchanged -- this is why \
             the renderer sends the anchor, and removing it would lose every composed character \
             while still looking like the field reacted"
        );
    }

    fn query_model(query: &str) -> ViewModel {
        ViewModel {
            generation: Generation::ZERO,
            query: query.to_owned(),
            rows: Arc::default(),
            selected: 0,
            pending_plugins: false,
            actions_open: false,
        }
    }

    fn raw_input(events: Vec<egui::Event>) -> egui::RawInput {
        let window = NativeLauncherConfig::default();
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(window.width as f32, window.height as f32),
            )),
            focused: true,
            events,
            ..Default::default()
        }
    }

    // -----------------------------------------------------------------------
    // Live: the real application, a real X server, a real compose sequence
    // -----------------------------------------------------------------------

    /// What the host callback saw, in the order the event loop dispatched it.
    #[derive(Debug, Default)]
    struct Host {
        /// The query the host has applied, exactly as `LauncherViewModel` would.
        query: String,
        /// Every query the application dispatched, in order.
        queries: Vec<String>,
        /// Every command, so a stray `Dismiss` or `Cancel` is visible.
        commands: Vec<UiCommand>,
    }

    /// A real compose sequence reaches the query through the shipped launcher.
    ///
    /// Kills the bug the scratch probe could not: winit delivering `Ime::Commit`
    /// while `NativeApplication` drops it, forwards it to a stale `egui_state`,
    /// or dispatches it as something other than the query the user is typing.
    /// The composed character is produced by the X server through XTEST, not by
    /// pushing a `WindowEvent` into the application, so the whole chain —
    /// XIM filtering, winit translation, `egui_winit`, the `TextEdit`, and
    /// `dispatch_command` — is under test.
    fn a_live_compose_sequence_appends_its_character_to_the_query() {
        require_tool("xdotool");
        let server = XvfbServer::start();

        // Set before the event loop exists: winit calls `setlocale(LC_CTYPE,
        // "")` while building its X connection, and opens the input method
        // once, from that locale.
        env::set_var("DISPLAY", server.display());
        env::remove_var("LC_ALL");
        env::set_var("LC_CTYPE", IME_LOCALE);
        env::remove_var("XMODIFIERS");
        // Lavapipe: the same software path `crikey dev measure-activation`
        // uses headlessly.
        env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");

        let title = format!("crikey-ime-evidence-{}", std::process::id());
        let config = NativeLauncherConfig {
            title: title.clone(),
            present_mode: wgpu::PresentMode::AutoNoVsync,
            ..NativeLauncherConfig::default()
        };
        let launcher = NativeLauncher::new(config).unwrap_or_else(|error| {
            panic!(
                "the launcher could not be created on {}: {error}",
                server.display()
            )
        });
        let handle = launcher.handle();
        let host = Arc::new(Mutex::new(Host::default()));

        let (report, receive_report) = mpsc::channel();
        let driver_handle = handle.clone();
        let driver_host = Arc::clone(&host);
        let driver_title = title.clone();
        let driver = thread::spawn(move || {
            let outcome = drive(&driver_handle, &driver_host, &driver_title);
            let _ = report.send(outcome);
            let _ = driver_handle.request_exit();
        });

        // The host callback is the production shape: apply the command to the
        // query state, then submit the next frame through a cloned handle.
        let callback_handle = handle.clone();
        let callback_host = Arc::clone(&host);
        let render_result = launcher.run(move |event| {
            let model = {
                let mut state = lock(&callback_host);
                if let NativeLauncherEvent::Command { command, .. } = &event {
                    state.commands.push(command.clone());
                    if let UiCommand::SetQuery(query) = command {
                        state.query.clone_from(query);
                        state.queries.push(query.clone());
                    }
                }
                query_model(&state.query)
            };
            let _ = callback_handle.submit_frame(&model);
        });

        let outcome = receive_report
            .recv_timeout(UI_LIMIT)
            .unwrap_or_else(|error| panic!("the driver thread reported nothing: {error}"));
        let _ = driver.join();
        render_result.unwrap_or_else(|error| panic!("the renderer failed: {error}"));

        let state = lock(&host);
        if let Err(message) = outcome {
            panic!("{message}\ncommands dispatched: {:?}", state.commands);
        }

        assert_eq!(
            state.queries,
            vec!["a".to_owned(), "aæ".to_owned()],
            "the live sequence must dispatch exactly two queries: `a` from the plain key, then \
             `aæ` once the composed character commits. An `Ime::Commit` that never reached the \
             query would stop at `a`; the `Ime::Preedit(\"\")` winit emits immediately before the \
             commit clearing or corrupting the query would show up as a third, different entry. \
             Commands dispatched: {:?}",
            state.commands
        );
    }

    /// Drives the live scenario off the event-loop thread and returns a verdict.
    ///
    /// `winit` owns the main thread, so the harness observes the launcher the
    /// way the application does: through the handle, and through the queries
    /// the host callback recorded.
    fn drive(handle: &NativeLauncherHandle, host: &Arc<Mutex<Host>>, title: &str) -> Result<(), String> {
        handle
            .request_activation()
            .map_err(|error| format!("the activation was refused: {error}"))?;

        let window = wait_for(UI_LIMIT, || visible_window(title)).ok_or_else(|| {
            format!(
                "no window titled `{title}` became visible within {UI_LIMIT:?}; the launcher never \
                 mapped its surface"
            )
        })?;

        // Xvfb runs no window manager, so nothing assigns input focus and the
        // `focus_window` the launcher issues while mapping cannot take. XTEST
        // delivers to whatever holds focus, so the harness sets it, then waits
        // for the server to confirm rather than assuming.
        xdotool(&["windowfocus", &window]);
        wait_for(UI_LIMIT, || {
            (xdotool(&["getwindowfocus"]).trim() == window).then_some(())
        })
        .ok_or_else(|| format!("window {window} never took input focus within {UI_LIMIT:?}"))?;

        // A plain key first: it gives the query something to lose, so the
        // empty preedit that precedes the commit has something to corrupt.
        xdotool(&["key", "a"]);
        wait_for(UI_LIMIT, || (lock(host).query == "a").then_some(())).ok_or_else(|| {
            format!(
                "a plain `a` typed through XTEST never reached the query within {UI_LIMIT:?}; \
                 queries seen: {:?}",
                lock(host).queries
            )
        })?;

        // The compose sequence. The input method swallows these presses and
        // answers with `Ime::Preedit("", None)` then `Ime::Commit("æ")`.
        // One invocation, because `xdotool` binds `Multi_key` to a spare
        // keycode and restores the keymap when it exits: three separate
        // processes would race that remapping against the presses.
        xdotool(&["key", "--delay", "80", "Multi_key", "a", "e"]);
        wait_for(UI_LIMIT, || (lock(host).query == "aæ").then_some(())).ok_or_else(|| {
            format!(
                "the composed `æ` never reached the query within {UI_LIMIT:?}; queries seen: {:?}. \
                 If the query stopped at `a`, the commit was dropped between winit and the query; \
                 if it is empty, the empty preedit cleared it.",
                lock(host).queries
            )
        })?;

        Ok(())
    }

    /// The id of the mapped window carrying `title`, if the server has one.
    fn visible_window(title: &str) -> Option<String> {
        let found = xdotool(&["search", "--onlyvisible", "--name", title]);
        found.split_whitespace().next_back().map(str::to_owned)
    }

    /// Runs `xdotool` against the display in the environment, or fails by name.
    fn xdotool(args: &[&str]) -> String {
        let output = Command::new("xdotool")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("running `xdotool {}` failed: {error}", args.join(" ")));
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Panics by name if `tool` is not on `PATH`.
    ///
    /// A missing tool is a failure, not a skip: this target exists to produce
    /// evidence, and a run that produced none must say so.
    fn require_tool(tool: &str) {
        let found = Command::new(tool)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if found.is_err() {
            panic!(
                "this test drives a real X server and requires `{tool}`; it is not runnable. A \
                 missing tool is a test failure, never a skip."
            );
        }
    }

    /// Polls `probe` against a deadline, yielding its first `Some`.
    fn wait_for<T>(limit: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(value) = probe() {
                return Some(value);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // -----------------------------------------------------------------------
    // The private server
    // -----------------------------------------------------------------------

    /// A private `Xvfb` instance, killed when the guard is dropped.
    ///
    /// Dropping is what keeps a panicking run clean: the unwind passes through
    /// here, so no orphaned server outlives it and no display number leaks.
    struct XvfbServer {
        display: String,
        socket: PathBuf,
        child: Child,
    }

    impl XvfbServer {
        /// Starts a server on an unused display number and waits until it
        /// accepts connections.
        ///
        /// Panics — loudly and by name — if `Xvfb` is absent or never comes up.
        fn start() -> Self {
            let base = 100 + (std::process::id() % 700);
            let offset = NEXT_DISPLAY_OFFSET.fetch_add(1, Ordering::Relaxed);

            let mut last_error = String::new();
            for attempt in 0..16 {
                let number = base + offset * 16 + attempt;
                let display = format!(":{number}");
                let socket = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
                if socket.exists() || PathBuf::from(format!("/tmp/.X{number}-lock")).exists() {
                    continue;
                }

                let child = match Command::new("Xvfb")
                    .arg(&display)
                    .args(["-screen", "0", "1280x800x24", "-nolisten", "tcp"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(child) => child,
                    Err(error) => panic!(
                        "this test requires a real X server; spawning `Xvfb {display}` failed: \
                         {error}. A missing Xvfb is a test failure, never a skip."
                    ),
                };

                let mut server = Self {
                    display,
                    socket,
                    child,
                };
                match server.wait_until_ready() {
                    Ok(()) => return server,
                    Err(error) => last_error = error,
                }
                // `server` drops here, killing the failed attempt.
            }

            panic!("no Xvfb came up after 16 display numbers from :{base}; last failure: {last_error}");
        }

        /// Polls until the display socket exists and the server is still alive.
        fn wait_until_ready(&mut self) -> Result<(), String> {
            let deadline = Instant::now() + SERVER_READY_LIMIT;
            loop {
                match self.child.try_wait() {
                    Ok(Some(status)) => {
                        return Err(format!("Xvfb {} exited early with {status}", self.display))
                    }
                    Ok(None) => {}
                    Err(error) => return Err(format!("Xvfb {} could not be polled: {error}", self.display)),
                }
                if self.socket.exists() {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "Xvfb {} did not accept connections within {SERVER_READY_LIMIT:?}",
                        self.display
                    ));
                }
                thread::sleep(POLL_INTERVAL);
            }
        }

        /// The `:N` string a client connects with.
        fn display(&self) -> &str {
            &self.display
        }
    }

    impl Drop for XvfbServer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            // Xvfb normally removes these itself; a killed one may not.
            let _ = fs::remove_file(&self.socket);
            if let Some(number) = self.display.strip_prefix(':') {
                let _ = fs::remove_file(format!("/tmp/.X{number}-lock"));
            }
        }
    }
}
