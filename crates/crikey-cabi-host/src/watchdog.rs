//! Deadline enforcement for a call that cannot be interrupted.
//!
//! Once control is inside a C entry point there is no safe way to get it back:
//! the restricted ABI has no unwinding contract, no cancellation callback and
//! no thread the host is allowed to kill. So the host enforces deadlines in the
//! only two ways that are actually true.
//!
//! * At the **soft** deadline it sets the cancellation flag the plugin was
//!   given. A plugin that polls it, as the header requires, returns promptly.
//! * At the **hard** deadline it aborts the host process. A plugin that ignores
//!   the flag costs its host process, the supervisor observes a worker crash,
//!   and every other plugin — living in its own host process — keeps serving.
//!
//! Aborting is not a failure of imagination; it is the honest outcome. Any
//! other choice would mean reporting a request as finished while a foreign
//! thread is still writing to memory this process owns.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What the watchdog should do at a given instant for a given armed call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to do yet; sleep at most this long before re-deciding.
    Wait(Duration),
    /// Soft deadline reached: raise the cancellation flag.
    Cancel,
    /// Hard deadline reached: the call is unrecoverable.
    Abort,
}

/// One armed call. Public so the scheduling decision can be tested directly
/// rather than by racing a real timer.
#[derive(Debug, Clone, Copy)]
pub struct Armed {
    pub soft: Instant,
    pub hard: Instant,
    pub cancelled: bool,
}

impl Armed {
    /// The pure scheduling decision. `Abort` wins over `Cancel` when both
    /// deadlines have passed, because at that point cancelling would only
    /// delay an outcome that is already decided.
    pub fn verdict(&self, now: Instant) -> Verdict {
        if now >= self.hard {
            return Verdict::Abort;
        }
        if !self.cancelled {
            if now >= self.soft {
                return Verdict::Cancel;
            }
            return Verdict::Wait(self.soft - now);
        }
        Verdict::Wait(self.hard - now)
    }
}

#[derive(Debug)]
struct State {
    armed: Option<Armed>,
    /// What the armed call is, for the abort message.
    what: String,
    stopping: bool,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    signal: Condvar,
    /// The flag handed to plugin code. `1` means "the host wants you to stop".
    cancelled: AtomicI32,
}

/// A single background thread that owns every deadline in this process.
///
/// One thread, not one per call: calls are serialised, so a second timer would
/// only be a second thing to get wrong.
#[derive(Debug)]
pub struct Watchdog {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    pub fn spawn() -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                armed: None,
                what: String::new(),
                stopping: false,
            }),
            signal: Condvar::new(),
            cancelled: AtomicI32::new(0),
        });
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("crikey-cabi-watchdog".to_owned())
            .spawn(move || run(&worker))
            .expect("the watchdog thread is required to enforce plugin deadlines");
        Self {
            shared,
            thread: Some(thread),
        }
    }

    /// Pointer to the cancellation flag handed to plugin code.
    ///
    /// Stable for the life of the watchdog: the flag lives in the `Arc`, which
    /// outlives every call that can observe it.
    pub fn cancel_flag(&self) -> *const i32 {
        // `AtomicI32` is `repr(transparent)` over `i32`, so this is the same
        // address the plugin's `const volatile int32_t*` needs.
        let flag: &AtomicI32 = &self.shared.cancelled;
        (flag as *const AtomicI32).cast::<i32>()
    }

    /// Arms the deadlines for one call and clears the cancellation flag.
    pub fn arm(&self, what: &str, soft: Duration, hard: Duration) {
        let now = Instant::now();
        self.shared.cancelled.store(0, Ordering::SeqCst);
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.what.clear();
        state.what.push_str(what);
        state.armed = Some(Armed {
            soft: now + soft,
            hard: now + hard.max(soft),
            cancelled: false,
        });
        drop(state);
        self.shared.signal.notify_all();
    }

    /// Disarms after a call returned. Idempotent.
    pub fn disarm(&self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.armed = None;
        drop(state);
        self.shared.signal.notify_all();
    }

    /// Whether the soft deadline fired for the call that just finished.
    pub fn cancellation_raised(&self) -> bool {
        self.shared.cancelled.load(Ordering::SeqCst) != 0
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.stopping = true;
            state.armed = None;
        }
        self.shared.signal.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(shared: &Arc<Shared>) {
    let mut state = shared.state.lock().unwrap_or_else(|error| error.into_inner());
    loop {
        if state.stopping {
            return;
        }
        let Some(armed) = state.armed else {
            state = shared
                .signal
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
            continue;
        };
        match armed.verdict(Instant::now()) {
            Verdict::Wait(remaining) => {
                let (next, _) = shared
                    .signal
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(|error| error.into_inner());
                state = next;
            }
            Verdict::Cancel => {
                shared.cancelled.store(1, Ordering::SeqCst);
                if let Some(armed) = state.armed.as_mut() {
                    armed.cancelled = true;
                }
            }
            Verdict::Abort => {
                let what = state.what.clone();
                drop(state);
                abort_overrun(&what);
            }
        }
    }
}

/// Terminates the host because a plugin call cannot be recovered.
///
/// Written to standard error first: the supervisor captures and bounds this
/// stream, so the crash the operator sees is named rather than mysterious.
fn abort_overrun(what: &str) -> ! {
    eprintln!(
        "crikey-cabi-host: aborting: {what} ignored its cancellation flag past the hard deadline; \
         a restricted C-ABI call cannot be interrupted, so the host process is the unit of recovery"
    );
    std::process::abort()
}
