//! Wall-clock backstop for guest calls.
//!
//! Fuel (see [`crate::guest`]) is the mechanism that interrupts a spinning
//! module, and it is deterministic: a call is stopped after a fixed number of
//! executed instructions. What fuel cannot express is *time*. wasmi 1.1 has no
//! epoch interruption, so there is no way to signal a running interpreter from
//! another thread, and the fuel-to-milliseconds calibration in
//! [`crate::config`] is by nature approximate.
//!
//! This watchdog closes that gap the only way a single-threaded interpreter
//! allows: if a call is still running well past its hard deadline — meaning
//! fuel was calibrated too generously for this machine — the host process
//! aborts. The native supervisor then records a crashed worker and restarts
//! it, which is the behaviour it already has for a native plugin that wedges.
//! A hung query is thereby impossible: either fuel stops the guest and the
//! request fails cleanly, or the process dies and the supervisor notices.
//!
//! The window is deliberately several times the hard deadline
//! ([`crate::config::WATCHDOG_SLACK`]) so this never fires on a call fuel
//! would legitimately have allowed to finish. It is a backstop, not a second
//! deadline.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Longest the poller sleeps between checks.
const MAX_POLL: Duration = Duration::from_millis(250);
/// Shortest the poller sleeps between checks.
const MIN_POLL: Duration = Duration::from_millis(25);

/// Sentinel meaning "no call is in flight".
const IDLE: u64 = 0;

#[derive(Debug)]
struct Shared {
    origin: Instant,
    /// Milliseconds after `origin` by which the in-flight call must finish, or
    /// [`IDLE`].
    expiry_ms: AtomicU64,
    stop: AtomicBool,
}

/// What the watchdog does when a call overruns.
///
/// Production aborts the process. Tests substitute a recorder, because a test
/// that proves the watchdog fires must survive to assert it.
pub type Overrun = Arc<dyn Fn(Duration) + Send + Sync>;

/// Aborts the process, after saying why on standard error so the supervisor's
/// captured stderr explains the crash.
pub fn abort_on_overrun() -> Overrun {
    Arc::new(|elapsed: Duration| {
        eprintln!(
            "crikey-wasm-host: a guest call ran {} ms past its watchdog window; \
             aborting so the supervisor restarts this worker rather than hanging a query",
            elapsed.as_millis()
        );
        std::process::abort();
    })
}

/// Wall-clock supervisor for guest calls.
#[derive(Debug)]
pub struct Watchdog {
    shared: Arc<Shared>,
    window: Duration,
    poller: Option<thread::JoinHandle<()>>,
}

impl Watchdog {
    /// Starts a watchdog whose window is `window`, aborting on overrun.
    pub fn spawn(window: Duration) -> Self {
        Self::spawn_with(window, abort_on_overrun())
    }

    /// [`Self::spawn`] with an injected overrun action.
    pub fn spawn_with(window: Duration, on_overrun: Overrun) -> Self {
        let shared = Arc::new(Shared {
            origin: Instant::now(),
            expiry_ms: AtomicU64::new(IDLE),
            stop: AtomicBool::new(false),
        });
        let poll = window.min(MAX_POLL).max(MIN_POLL);
        let watched = Arc::clone(&shared);
        let poller = thread::Builder::new()
            .name("crikey-wasm-watchdog".to_owned())
            .spawn(move || loop {
                if watched.stop.load(Ordering::Acquire) {
                    return;
                }
                let expiry = watched.expiry_ms.load(Ordering::Acquire);
                if expiry != IDLE {
                    let elapsed = watched.origin.elapsed().as_millis() as u64;
                    if elapsed >= expiry {
                        // Clear first: the action may not terminate, and a
                        // recorder must not be invoked once per poll.
                        watched.expiry_ms.store(IDLE, Ordering::Release);
                        on_overrun(Duration::from_millis(elapsed.saturating_sub(expiry)));
                    }
                }
                thread::sleep(poll);
            })
            .ok();
        Self {
            shared,
            window,
            poller,
        }
    }

    /// The window an overrunning call is given.
    pub fn window(&self) -> Duration {
        self.window
    }

    /// Marks a call in flight until the returned guard is dropped.
    pub fn guard(&self) -> CallGuard<'_> {
        let expiry = self.shared.origin.elapsed().saturating_add(self.window);
        // `IDLE` is zero and an expiry of zero would mean "already overdue" on
        // the very first millisecond of the process, so it is nudged forward.
        let expiry_ms = (expiry.as_millis() as u64).max(IDLE + 1);
        self.shared.expiry_ms.store(expiry_ms, Ordering::Release);
        CallGuard { shared: &self.shared }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(poller) = self.poller.take() {
            let _ = poller.join();
        }
    }
}

/// Clears the in-flight marker when a call returns, however it returns.
#[derive(Debug)]
pub struct CallGuard<'a> {
    shared: &'a Shared,
}

impl Drop for CallGuard<'_> {
    fn drop(&mut self) {
        self.shared.expiry_ms.store(IDLE, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorder() -> (Overrun, Arc<AtomicU64>) {
        let count = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&count);
        let action: Overrun = Arc::new(move |_| {
            observed.fetch_add(1, Ordering::Release);
        });
        (action, count)
    }

    #[test]
    fn a_call_that_finishes_inside_the_window_is_not_reported() {
        let (action, count) = recorder();
        let watchdog = Watchdog::spawn_with(Duration::from_millis(400), action);
        {
            let _guard = watchdog.guard();
            thread::sleep(Duration::from_millis(30));
        }
        thread::sleep(Duration::from_millis(200));
        assert_eq!(count.load(Ordering::Acquire), 0);
    }

    #[test]
    fn an_overrunning_call_is_reported_exactly_once() {
        let (action, count) = recorder();
        let watchdog = Watchdog::spawn_with(Duration::from_millis(40), action);
        let guard = watchdog.guard();
        thread::sleep(Duration::from_millis(400));
        assert_eq!(
            count.load(Ordering::Acquire),
            1,
            "the overrun action fires once per overrunning call, not once per poll"
        );
        drop(guard);
    }

    #[test]
    fn an_idle_watchdog_never_reports() {
        let (action, count) = recorder();
        let _watchdog = Watchdog::spawn_with(Duration::from_millis(20), action);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(count.load(Ordering::Acquire), 0);
    }
}
