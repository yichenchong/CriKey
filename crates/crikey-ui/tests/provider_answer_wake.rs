//! Live evidence that a provider answer reaches an idle event loop.
//!
//! # Why this target exists
//!
//! The launcher's file search is ranked against the application catalog, and
//! the ranker lives on the UI thread. Its worker therefore cannot compose a
//! frame the way the legacy, modern and native supervisors do: it parks its
//! items and asks the UI thread to merge them. That ask is
//! [`NativeLauncherHandle::request_provider_answer`], and everything depends on
//! one property that cannot be established by reading code — that the event it
//! sends really does wake a loop already blocked in its platform wait. If it
//! did not, a file answer would sit in the driver until the user's next
//! keystroke, and the rows would appear one character late for the rest of the
//! session.
//!
//! So the loop here is the shipped [`NativeLauncher`], running under a private
//! `Xvfb`, left to go quiet before anything is sent to it. What is asserted is
//! what the host callback saw and when.
//!
//! # What is proved here, and what is not
//!
//! * **Delivery while idle — live.** The launcher is activated, then left
//!   untouched until it has been silent long enough to be parked in
//!   `ControlFlow::Wait`. Only then is `request_provider_answer` called, from
//!   another thread, and the host callback receives
//!   [`NativeLauncherEvent::ProviderAnswer`]. Nothing in this process pumps the
//!   loop; if the proxy did not wake it, the wait below would time out.
//! * **The announcement is retired — live.** A second call after the first
//!   event has been delivered produces a second event. A promise that outlived
//!   its event would make the launcher deaf to every later answer.
//! * **Inert while hidden — live.** After the session is hidden, a call
//!   delivers nothing: an answer belonging to a dismissed session must not
//!   resurrect it, and must not be merged into whatever comes next.
//! * **Coalescing — unit level.** That a burst of answers buys one wake is
//!   asserted in `native.rs`'s `lifecycle_tests`, not here: how many of a burst
//!   land before the loop consumes the first is a genuine race, and a live
//!   "exactly one" assertion would be flaky rather than true.
//!
//! A missing `Xvfb` is a **named panic**, never a skip. A skipped test is not
//! evidence.

#[cfg(not(target_os = "linux"))]
fn main() {
    // The evidence this target carries needs a real display server. `crikey-ui`
    // builds on Windows and macOS, so the binary must still link there.
}

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(target_os = "linux")]
mod linux {
    use std::env;
    use std::fs;
    use std::io::{BufRead, BufReader};
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use crikey_core::Generation;
    use crikey_ui::{
        wgpu, NativeLauncher, NativeLauncherConfig, NativeLauncherEvent, NativeLauncherHandle, ViewModel,
    };

    /// Ceiling on an `Xvfb` becoming connectable.
    const SERVER_READY_LIMIT: Duration = Duration::from_secs(20);

    /// Ceiling on the launcher activating and on an announcement arriving.
    /// Generous, because neither is being measured: a stalled renderer becomes
    /// a named failure instead of a hang.
    const UI_LIMIT: Duration = Duration::from_secs(30);

    /// How long the loop must be silent before it counts as parked.
    ///
    /// The launcher schedules repaints while its window settles, and a wake
    /// that arrived during one of those would prove nothing: the loop would
    /// have run anyway. Waiting for a stretch with no callback and no scheduled
    /// repaint is what makes the delivery below attributable to the proxy.
    const QUIET: Duration = Duration::from_secs(2);

    /// How long to wait after a call that must deliver nothing.
    ///
    /// Absence needs a bound. Comfortably longer than any observed delivery,
    /// which lands within milliseconds.
    const SILENCE: Duration = Duration::from_secs(3);

    /// Gap between polls.
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    pub fn run() {
        an_answer_reaches_an_idle_loop_and_stops_at_a_hidden_one();
        println!("ok  live  a provider answer wakes an idle event loop, and a hidden one ignores it");
    }

    /// What the host callback saw, in the order the event loop dispatched it.
    #[derive(Debug, Default)]
    struct Host {
        activations: usize,
        answers: usize,
    }

    fn an_answer_reaches_an_idle_loop_and_stops_at_a_hidden_one() {
        let server = XvfbServer::start();
        env::set_var("DISPLAY", server.display());
        // Lavapipe: the same software path `crikey dev measure-activation`
        // uses headlessly.
        env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");

        let title = format!("crikey-answer-wake-{}", std::process::id());
        let config = NativeLauncherConfig {
            title,
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
        let driver = thread::spawn(move || {
            let outcome = drive(&driver_handle, &driver_host);
            let _ = report.send(outcome);
            let _ = driver_handle.request_exit();
        });

        // The production shape: apply the event, then submit the frame the
        // merge would have produced.
        let callback_handle = handle.clone();
        let callback_host = Arc::clone(&host);
        let render_result = launcher.run(move |event| {
            {
                let mut state = lock(&callback_host);
                match event {
                    NativeLauncherEvent::Activated => state.activations += 1,
                    NativeLauncherEvent::ProviderAnswer => state.answers += 1,
                    NativeLauncherEvent::Command { .. } => {}
                }
            }
            let _ = callback_handle.submit_frame(&empty_model());
        });

        let outcome = receive_report
            .recv_timeout(UI_LIMIT + SILENCE + QUIET + UI_LIMIT)
            .unwrap_or_else(|error| panic!("the driver thread reported nothing: {error}"));
        let _ = driver.join();
        render_result.unwrap_or_else(|error| panic!("the renderer failed: {error}"));

        if let Err(message) = outcome {
            panic!("{message}");
        }
    }

    /// Drives the scenario off the event-loop thread and returns a verdict.
    fn drive(handle: &NativeLauncherHandle, host: &Arc<Mutex<Host>>) -> Result<(), String> {
        handle
            .request_activation()
            .map_err(|error| format!("the activation was refused: {error}"))?;
        wait_for(UI_LIMIT, || (lock(host).activations == 1).then_some(()))
            .ok_or_else(|| format!("the launcher never activated within {UI_LIMIT:?}"))?;

        // Let the loop settle into its platform wait. Anything delivered from
        // here on is delivered because it was announced, not because the window
        // happened to be redrawing.
        thread::sleep(QUIET);
        let before = lock(host).answers;
        if before != 0 {
            return Err(format!(
                "the loop reported {before} provider answers before any were announced"
            ));
        }

        handle
            .request_provider_answer()
            .map_err(|error| format!("the announcement was refused: {error}"))?;
        wait_for(UI_LIMIT, || (lock(host).answers == 1).then_some(())).ok_or_else(|| {
            format!(
                "an announced provider answer never reached the host within {UI_LIMIT:?}; the \
                 event loop was parked in its platform wait and the proxy did not wake it, so a \
                 file answer would only reach the screen on the user's next keystroke"
            )
        })?;

        // The promise the first event carried must have been retired with it.
        thread::sleep(QUIET);
        handle
            .request_provider_answer()
            .map_err(|error| format!("the second announcement was refused: {error}"))?;
        wait_for(UI_LIMIT, || (lock(host).answers == 2).then_some(())).ok_or_else(|| {
            format!(
                "a second announcement never arrived within {UI_LIMIT:?}; the first event's \
                 promise outlived it and the launcher is now deaf to every later answer"
            )
        })?;

        // A hidden launcher has no session to merge into. The announcement must
        // neither wake it nor be delivered to the host.
        handle
            .request_hide()
            .map_err(|error| format!("hiding was refused: {error}"))?;
        handle
            .request_provider_answer()
            .map_err(|error| format!("the announcement to a hidden launcher errored: {error}"))?;
        thread::sleep(SILENCE);
        let answers = lock(host).answers;
        if answers != 2 {
            return Err(format!(
                "a hidden session was told about a provider answer: expected 2 answers, saw \
                 {answers}. An answer belonging to a dismissed activation must be dropped, not \
                 merged into whatever comes next"
            ));
        }

        Ok(())
    }

    fn empty_model() -> ViewModel {
        ViewModel {
            generation: Generation::ZERO,
            query: String::new(),
            rows: Arc::default(),
            selected: 0,
            pending_plugins: false,
            actions_open: false,
            settings_open: false,
            settings: Arc::default(),
            settings_focus: None,
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

    /// A private `Xvfb` instance, killed when the guard is dropped.
    ///
    /// The number is chosen by `Xvfb` itself through `-displayfd`, never picked
    /// here: picking one and then spawning is a check-then-act race that two
    /// concurrently running test binaries really do lose.
    struct XvfbServer {
        display: String,
        socket: PathBuf,
        child: Child,
    }

    impl XvfbServer {
        fn start() -> Self {
            let mut child = match Command::new("Xvfb")
                .args(["-displayfd", "1"])
                .args(["-screen", "0", "1280x800x24", "-nolisten", "tcp"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(error) => panic!(
                    "this test requires a real X server; spawning `Xvfb` failed: {error}. \
                     A missing Xvfb is a test failure, never a skip."
                ),
            };

            let number = Self::reported_display(&mut child);
            Self {
                display: format!(":{number}"),
                socket: PathBuf::from(format!("/tmp/.X11-unix/X{number}")),
                child,
            }
        }

        /// The display number the server reports, bounded by
        /// [`SERVER_READY_LIMIT`]. Read on another thread because the read
        /// blocks: a server that starts and never reports would otherwise wedge
        /// the test rather than fail it.
        fn reported_display(child: &mut Child) -> u32 {
            let descriptor = child
                .stdout
                .take()
                .expect("`-displayfd 1` was asked for, so stdout is a pipe");
            let (sender, receiver) = mpsc::channel();
            thread::spawn(move || {
                let mut line = String::new();
                let outcome = BufReader::new(descriptor).read_line(&mut line).map(|_| line);
                let _ = sender.send(outcome);
            });

            match receiver.recv_timeout(SERVER_READY_LIMIT) {
                Ok(Ok(line)) => line.trim().parse().unwrap_or_else(|error| {
                    panic!("Xvfb reported {line:?} as its display, which is not a number: {error}")
                }),
                Ok(Err(error)) => panic!("Xvfb's display descriptor could not be read: {error}"),
                Err(_) => {
                    let _ = child.kill();
                    panic!("Xvfb did not report a display within {SERVER_READY_LIMIT:?}")
                }
            }
        }

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
