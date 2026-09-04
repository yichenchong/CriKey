//! Host-driven plugin page sessions (spec 32.2-32.5, 32.10).
//!
//! A page is a display list a plugin draws and the host renders. Nothing about
//! it is pushed: the host asks for a frame, the plugin answers one, and the
//! host draws what came back. This module owns the asking.
//!
//! # Why a thread
//!
//! A frame request is a blocking round trip to a supervised child, bounded by
//! the worker's own call deadline. Doing it on the caller's thread would mean
//! the launcher stops repainting whenever a page is open, which is the exact
//! failure a plugin-drawn surface must not be able to cause. The session
//! therefore runs its own thread and the caller only ever posts commands and
//! takes finished frames.
//!
//! # Why coalescing is the session's job
//!
//! Pointer motion arrives far faster than a child can answer. One request per
//! event would queue a backlog the user has already moved past, so input that
//! arrives while a call is in flight is held and delivered whole on the next
//! request. A burst of motion costs one round trip, and the plugin still sees
//! every event in order.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crikey_core::{PageFrame, PageInput, PageInputKind, PagePalette, PluginId, MIN_PAGE_REDRAW_MS};

/// How many unanswered input events a page may accumulate before the host
/// concludes the plugin has stopped drawing.
///
/// Pointer motion coalesces, so reaching this takes that many *discrete*
/// events - keys, clicks, activations - between two frames. At any human
/// input rate the hard call deadline fires long first, which is what makes
/// this a backstop against unbounded growth rather than a limit a user can
/// walk into.
const MAX_PENDING_PAGE_EVENTS: usize = 256;

/// Rings the host's event loop after a frame is published. A page draws on
/// its own thread, so nothing else would notice the answer had arrived.
pub(crate) type PageWake = Arc<dyn Fn() + Send + Sync>;

/// One frame the host is willing to draw, and whether the session that
/// produced it has ended.
///
/// `closed` never arrives without a frame. The frame it carries is either the
/// plugin's own final frame or a repeat of the last one already accepted, so
/// a renderer dismissing the surface has something to draw for that last
/// instant instead of flashing an empty page on the way out. A session that
/// ended before any frame was accepted reports the default frame, whose
/// `generation` is zero and whose node list is empty.
#[derive(Debug, Clone, PartialEq)]
pub struct PageUpdate {
    pub frame: PageFrame,
    pub closed: bool,
}

/// One host-to-plugin frame request, as the session's backend needs it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PageDraw {
    pub page_id: String,
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub events: Vec<PageInput>,
    /// The launcher's own colours, so the page can match the surface it is
    /// drawn on instead of guessing at them.
    pub palette: PagePalette,
}

/// The plugin side of one page.
///
/// A trait rather than a concrete worker call because the session's rules —
/// coalescing, generation ordering, deadlines, the closing notification — are
/// decisions about timing, and a test that has to spawn a child process to
/// observe them cannot pin any of them.
pub(crate) trait PageBackend: Send {
    /// Asks for one frame. The returned frame is already validated; an `Err`
    /// names why the plugin could not serve this request.
    fn draw(&mut self, draw: &PageDraw) -> Result<PageFrame, String>;
}

/// How long one frame request may take before it counts against the plugin.
///
/// The soft bound is a diagnostic, exactly as it is for suggestion dispatch:
/// exceeding it means the page is sluggish, not broken. The hard bound is the
/// point at which the host stops believing an answer is coming and fails the
/// page, so a plugin that stalls closes its surface instead of leaving a
/// frozen one the user cannot get an explanation for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PageDeadlines {
    pub soft: Duration,
    pub hard: Duration,
}

impl Default for PageDeadlines {
    fn default() -> Self {
        Self {
            soft: Duration::from_millis(100),
            hard: Duration::from_millis(2_000),
        }
    }
}

/// What a caller can tell a running page.
#[derive(Debug)]
enum PageCommand {
    Input(Box<PageInput>),
    Resize { width: u32, height: u32 },
    Close,
}

/// The single-slot mailbox the session publishes finished frames into.
///
/// Replace-newest rather than a queue: a frame the caller never polled has
/// already been superseded by the one after it, and drawing it would put the
/// user a round trip behind. The terminal `closed` update is the last thing
/// written, so it can never be overwritten by a later frame.
#[derive(Debug, Default)]
struct PageMailbox {
    update: Option<PageUpdate>,
    /// The last frame drawn, kept beyond the update the caller consumes.
    /// [`PageSession::close`] reports the close synchronously and needs
    /// something to report it about; without this the surface would blink to
    /// empty on the way out whenever the caller had already polled.
    last_frame: PageFrame,
}

/// Everything a page needs before its first frame.
///
/// Grouped rather than passed loose because the six always travel together
/// and two are bare `u32`s: an argument list in which the viewport can be
/// silently transposed is a defect the compiler cannot catch.
pub(crate) struct PageOpen {
    pub plugin: PluginId,
    pub page_id: String,
    pub width: u32,
    pub height: u32,
    pub deadlines: PageDeadlines,
    pub palette: PagePalette,
}

/// One open page: the plugin that owns it, its identity, and the thread
/// driving it.
#[derive(Debug)]
pub(crate) struct PageSession {
    plugin: PluginId,
    page_id: String,
    commands: Sender<PageCommand>,
    mailbox: Arc<Mutex<PageMailbox>>,
    /// Why the page failed, once it has. Kept apart from the update slot
    /// because a failure is a diagnostic about a plugin, while an update is
    /// something to draw.
    failure: Arc<Mutex<Option<String>>>,
    /// Frame requests that exceeded the soft deadline.
    soft_timeouts: Arc<Mutex<u32>>,
    worker: Option<JoinHandle<()>>,
}

impl PageSession {
    /// Opens a page and returns immediately.
    ///
    /// The first request is issued by the session thread, carrying
    /// [`PageInputKind::Opened`], so opening a page costs the caller a thread
    /// spawn and nothing else.
    pub(crate) fn open(
        spec: PageOpen,
        mut backend: Box<dyn PageBackend>,
        wake: PageWake,
    ) -> Result<Self, String> {
        let PageOpen {
            plugin,
            page_id,
            width,
            height,
            deadlines,
            palette,
        } = spec;
        let (commands, receiver) = mpsc::channel();
        let mailbox = Arc::new(Mutex::new(PageMailbox::default()));
        let failure = Arc::new(Mutex::new(None));
        let soft_timeouts = Arc::new(Mutex::new(0));
        let thread_mailbox = Arc::clone(&mailbox);
        let thread_failure = Arc::clone(&failure);
        let thread_soft = Arc::clone(&soft_timeouts);
        let thread_page_id = page_id.clone();
        let worker = thread::Builder::new()
            .name(format!("crikey-page-{}", plugin.0))
            .spawn(move || {
                let mut driver = PageDriver {
                    page_id: thread_page_id,
                    width,
                    height,
                    deadlines,
                    palette,
                    generation: 0,
                    pending: vec![PageInput::new(PageInputKind::Opened)],
                    last_frame: PageFrame::default(),
                    redraw_at: None,
                    mailbox: thread_mailbox,
                    wake,
                    failure: thread_failure,
                    soft_timeouts: thread_soft,
                };
                driver.run(&receiver, backend.as_mut());
            })
            .map_err(|error| format!("page session thread did not start: {error}"))?;
        Ok(Self {
            plugin,
            page_id,
            commands,
            mailbox,
            failure,
            soft_timeouts,
            worker: Some(worker),
        })
    }

    pub(crate) fn plugin(&self) -> &PluginId {
        &self.plugin
    }

    pub(crate) fn page_id(&self) -> &str {
        &self.page_id
    }

    /// Queues one host-hit-tested event. Never blocks and never waits for a
    /// frame: the answer arrives through [`Self::poll`].
    pub(crate) fn send_input(&self, input: PageInput) -> Result<(), String> {
        self.commands
            .send(PageCommand::Input(Box::new(input)))
            .map_err(|_| "plugin page session has ended".to_owned())
    }

    /// Tells the page its new size. A resize advances the generation, because
    /// a display list laid out for the old viewport is not the one to draw.
    pub(crate) fn resize(&self, width: u32, height: u32) {
        let _ = self.commands.send(PageCommand::Resize { width, height });
    }

    /// Takes the newest finished frame, if one is waiting.
    pub(crate) fn poll(&self) -> Option<PageUpdate> {
        self.mailbox
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .update
            .take()
    }

    /// Why the page failed, once it has.
    pub(crate) fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Frame requests this page's plugin answered later than the soft bound.
    pub(crate) fn soft_timeouts(&self) -> u32 {
        *self
            .soft_timeouts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Ends the session and reports the close at once, without waiting for the
    /// plugin to acknowledge it.
    ///
    /// Deliberately not a join. The closing notification is a request on the
    /// same child every other call uses, so it is bounded only by that child's
    /// hard call deadline - and this runs on the thread servicing the user's
    /// Escape key. Joining here would freeze the launcher for as long as a
    /// plugin felt like taking to answer a message whose reply is discarded
    /// anyway.
    ///
    /// The returned [`PageClosing`] is how ordering is still guaranteed: the
    /// notification is in flight on a detached thread, and whoever opens the
    /// next page on this plugin must wait for it first, or a `Closed` for the
    /// old page could overtake the `Opened` for the new one.
    pub(crate) fn close(mut self) -> (Option<PageUpdate>, PageClosing) {
        let _ = self.commands.send(PageCommand::Close);
        let closing = PageClosing(self.worker.take());
        let mailbox = self.mailbox.lock().unwrap_or_else(|error| error.into_inner());
        let frame = mailbox.last_frame.clone();
        drop(mailbox);
        (Some(PageUpdate { frame, closed: true }), closing)
    }
}

/// A closing session's thread, kept so the next page on the same plugin can
/// wait for the old one's notification to land before it starts.
#[derive(Debug)]
pub(crate) struct PageClosing(Option<std::thread::JoinHandle<()>>);

impl PageClosing {
    /// Waits for the closing notification to finish. Bounded by the child's
    /// own call deadline, and called only when a new page is being opened -
    /// never on the path that services a keystroke.
    pub(crate) fn settle(mut self) {
        if let Some(worker) = self.0.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for PageSession {
    /// Same refusal to block as [`PageSession::close`]: a session dropped
    /// during shutdown must not hold teardown behind a plugin that has
    /// stopped answering. Nothing orders itself against this one, because
    /// dropping the session means nothing will open another.
    fn drop(&mut self) {
        let _ = self.commands.send(PageCommand::Close);
        self.worker.take();
    }
}

/// The session thread's own state. Split from [`PageSession`] so nothing the
/// caller holds is reachable from the thread except the shared mailbox.
struct PageDriver {
    page_id: String,
    width: u32,
    height: u32,
    deadlines: PageDeadlines,
    palette: PagePalette,
    generation: u64,
    pending: Vec<PageInput>,
    last_frame: PageFrame,
    redraw_at: Option<Instant>,
    mailbox: Arc<Mutex<PageMailbox>>,
    failure: Arc<Mutex<Option<String>>>,
    soft_timeouts: Arc<Mutex<u32>>,
    wake: PageWake,
}

/// How long to wait before asking a page for a frame it scheduled itself.
///
/// Clamped rather than obeyed. A page may ask for a one millisecond period,
/// and with a raster aboard every frame that is gigabytes a second of encode,
/// transport and decode. The floor is one frame at the 60 Hz baseline the
/// launcher's budgets are written against, not a claim about what any display
/// can present. Zero keeps its meaning of "do not schedule one".
///
/// Only self-scheduled redraws are clamped. A frame answering user input is
/// never delayed by this.
fn redraw_delay(redraw_after_ms: u32) -> Option<Duration> {
    (redraw_after_ms != 0).then(|| Duration::from_millis(u64::from(redraw_after_ms.max(MIN_PAGE_REDRAW_MS))))
}

impl PageDriver {
    fn run(&mut self, commands: &Receiver<PageCommand>, backend: &mut dyn PageBackend) {
        loop {
            // Everything already queued is folded in before the request is
            // built, so one call carries the whole burst.
            match self.drain(commands) {
                Drained::Open => {}
                Drained::Closed => {
                    self.notify_closed(backend);
                    self.publish(self.last_frame.clone(), true);
                    return;
                }
                Drained::Overrun => return self.fail(self.overrun_reason()),
            }
            if !self.due() {
                match self.wait(commands) {
                    // `recv` consumes what it returns, so the command has
                    // already left the channel: applying it here is what stops
                    // it being dropped. Losing an ordinary input would swallow
                    // the user's click, and losing a `Close` would leave this
                    // loop parked on a channel whose sender is held open by the
                    // very caller waiting to join this thread.
                    Waited::Command(command) => match self.apply(command) {
                        Drained::Open => continue,
                        Drained::Closed => {
                            self.notify_closed(backend);
                            self.publish(self.last_frame.clone(), true);
                            return;
                        }
                        Drained::Overrun => return self.fail(self.overrun_reason()),
                    },
                    Waited::Woke => continue,
                    Waited::Gone => {
                        self.notify_closed(backend);
                        self.publish(self.last_frame.clone(), true);
                        return;
                    }
                }
            }
            self.generation = self.generation.saturating_add(1);
            let draw = PageDraw {
                page_id: self.page_id.clone(),
                generation: self.generation,
                width: self.width,
                height: self.height,
                events: std::mem::take(&mut self.pending),
                palette: self.palette,
            };
            let started = Instant::now();
            let outcome = backend.draw(&draw);
            let elapsed = started.elapsed();
            if elapsed > self.deadlines.soft {
                *self
                    .soft_timeouts
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) += 1;
            }
            let frame = match outcome {
                Ok(frame) => frame,
                Err(reason) => return self.fail(reason),
            };
            if elapsed > self.deadlines.hard {
                return self.fail(format!(
                    "plugin took {} ms to draw page `{}`, past the {} ms limit",
                    elapsed.as_millis(),
                    self.page_id,
                    self.deadlines.hard.as_millis()
                ));
            }
            // A frame answering a request the user has already moved past is
            // discarded rather than drawn: repainting the screen with it would
            // undo the input that superseded it. The accepted frame stays up.
            if frame.generation != 0 && frame.generation < self.generation {
                continue;
            }
            let close = frame.close;
            self.redraw_at = if close {
                None
            } else {
                redraw_delay(frame.redraw_after_ms).map(|delay| Instant::now() + delay)
            };
            self.last_frame = frame.clone();
            self.publish(frame, close);
            // The plugin asking to close IS the notification; sending it a
            // further request would ask a page that has finished to draw again.
            if close {
                return;
            }
        }
    }

    /// Folds every queued command into the request being prepared.
    fn drain(&mut self, commands: &Receiver<PageCommand>) -> Drained {
        loop {
            match commands.try_recv() {
                Ok(command) => match self.apply(command) {
                    Drained::Open => {}
                    other => return other,
                },
                Err(mpsc::TryRecvError::Empty) => return Drained::Open,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // The caller dropped the session without closing it, which
                    // is still a close: the page has no owner left.
                    return if self.pending.is_empty() {
                        Drained::Closed
                    } else {
                        Drained::Open
                    };
                }
            }
        }
    }

    /// Says which plugin stopped answering, since the page simply vanishing
    /// with a generic message would leave an operator guessing which of the
    /// loaded plugins to look at.
    fn overrun_reason(&self) -> String {
        format!(
            "page `{}` accumulated more than {MAX_PENDING_PAGE_EVENTS} unanswered input events, \
             so the plugin has stopped drawing it",
            self.page_id
        )
    }

    fn apply(&mut self, command: PageCommand) -> Drained {
        match command {
            PageCommand::Input(input) => {
                // Pointer motion coalesces: only the latest position is worth
                // delivering, because no intermediate one was ever drawn. A
                // moving mouse over a plugin that is mid-call would otherwise
                // be the one input source that grows without bound. Every
                // discrete event - press, release, key, text, activation - is
                // kept, since dropping one loses something the user did.
                if input.kind == PageInputKind::PointerMoved
                    && self
                        .pending
                        .last()
                        .is_some_and(|last| last.kind == PageInputKind::PointerMoved)
                {
                    let last = self.pending.len() - 1;
                    self.pending[last] = *input;
                    return Drained::Open;
                }
                if self.pending.len() >= MAX_PENDING_PAGE_EVENTS {
                    // Unreachable before the hard deadline fires at a normal
                    // input rate, so arriving here means the plugin has stopped
                    // answering entirely. Failing says so; growing the queue
                    // would spend memory pretending otherwise.
                    return Drained::Overrun;
                }
                self.pending.push(*input);
                Drained::Open
            }
            PageCommand::Resize { width, height } => {
                self.width = width;
                self.height = height;
                // A resize is a reason to redraw even with no input: the
                // display list was laid out for a viewport that is gone.
                self.redraw_at = Some(Instant::now());
                Drained::Open
            }
            PageCommand::Close => Drained::Closed,
        }
    }

    /// Whether there is anything to ask for right now.
    fn due(&self) -> bool {
        if self.generation == 0 || !self.pending.is_empty() {
            return true;
        }
        self.redraw_at.is_some_and(|at| Instant::now() >= at)
    }

    /// Sleeps until the next command, or until the plugin's own redraw request
    /// falls due. A static page costs nothing here.
    fn wait(&self, commands: &Receiver<PageCommand>) -> Waited {
        let timeout = self
            .redraw_at
            .map(|at| at.saturating_duration_since(Instant::now()));
        let received = match timeout {
            Some(timeout) => commands.recv_timeout(timeout),
            None => commands.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(command) => Waited::Command(command),
            Err(RecvTimeoutError::Timeout) => Waited::Woke,
            Err(RecvTimeoutError::Disconnected) => Waited::Gone,
        }
    }

    /// Tells the plugin its page is gone (spec 32.10). The frame it answers
    /// with is discarded: there is no longer a surface to draw it on, and a
    /// plugin cannot refuse to be closed.
    fn notify_closed(&mut self, backend: &mut dyn PageBackend) {
        self.generation = self.generation.saturating_add(1);
        let mut events = std::mem::take(&mut self.pending);
        events.push(PageInput::new(PageInputKind::Closed));
        let _ = backend.draw(&PageDraw {
            page_id: self.page_id.clone(),
            generation: self.generation,
            width: self.width,
            height: self.height,
            events,
            palette: self.palette,
        });
    }

    /// Ends the page as failed rather than leaving it frozen.
    ///
    /// No closing notification follows: the request that failed is the one
    /// that would have carried it, and asking a plugin that just missed its
    /// deadline for one more round trip would hold the caller's close behind
    /// the same stall.
    fn fail(&mut self, reason: String) {
        *self.failure.lock().unwrap_or_else(|error| error.into_inner()) = Some(reason);
        self.publish(self.last_frame.clone(), true);
    }

    fn publish(&self, frame: PageFrame, closed: bool) {
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|error| error.into_inner());
        mailbox.last_frame = frame.clone();
        mailbox.update = Some(PageUpdate { frame, closed });
        drop(mailbox);
        // The event loop is asleep between keystrokes. Without this the frame
        // would sit in the mailbox until something unrelated woke the
        // launcher, so a page would appear to freeze after every interaction
        // and a timer-driven one would never animate at all.
        (self.wake)();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drained {
    Open,
    Closed,
    /// More input piled up than a page that is still answering could have
    /// accumulated, so the plugin has stopped answering.
    Overrun,
}

/// Why the driver stopped sleeping. Carries the command when one arrived,
/// because `recv` takes it out of the channel: anything not handed back here
/// is lost, and a lost `Close` parks this thread on a channel whose sender the
/// closing caller still holds.
#[derive(Debug)]
enum Waited {
    Command(PageCommand),
    Woke,
    Gone,
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::Arc;

    use super::{
        redraw_delay, Duration, PageBackend, PageDeadlines, PageDraw, PageOpen, PageSession, PageUpdate,
        MIN_PAGE_REDRAW_MS,
    };
    use crikey_core::{PageFrame, PageInput, PageInputKind, PagePalette, PluginId};

    /// A plugin that answers exactly what a test tells it to, and records what
    /// it was asked.
    struct ScriptedBackend {
        seen: Sender<PageDraw>,
        replies: Receiver<Result<PageFrame, String>>,
    }

    impl PageBackend for ScriptedBackend {
        fn draw(&mut self, draw: &PageDraw) -> Result<PageFrame, String> {
            let _ = self.seen.send(draw.clone());
            self.replies.recv().unwrap_or_else(|_| Ok(PageFrame::default()))
        }
    }

    struct Harness {
        session: Option<PageSession>,
        seen: Receiver<PageDraw>,
        replies: Sender<Result<PageFrame, String>>,
    }

    impl Harness {
        fn open() -> Self {
            let (seen_tx, seen_rx) = mpsc::channel();
            let (reply_tx, reply_rx) = mpsc::channel();
            let session = PageSession::open(
                PageOpen {
                    plugin: PluginId("test.page".to_owned()),
                    page_id: "page".to_owned(),
                    width: 800,
                    height: 600,
                    deadlines: PageDeadlines::default(),
                    palette: PagePalette::default(),
                },
                Box::new(ScriptedBackend {
                    seen: seen_tx,
                    replies: reply_rx,
                }),
                // Nothing to wake: a test polls the mailbox directly.
                Arc::new(|| {}),
            )
            .expect("page session starts");
            Self {
                session: Some(session),
                seen: seen_rx,
                replies: reply_tx,
            }
        }

        fn session(&self) -> &PageSession {
            self.session.as_ref().expect("session is open")
        }

        /// Waits for the next request the session issued.
        fn next_draw(&self) -> PageDraw {
            self.seen
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("session issues a frame request")
        }

        fn reply(&self, frame: Result<PageFrame, String>) {
            self.replies.send(frame).expect("session is listening");
        }

        /// Waits for the session to publish an update.
        fn next_update(&self) -> PageUpdate {
            for _ in 0..5_000 {
                if let Some(update) = self.session().poll() {
                    return update;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            panic!("session published no update");
        }

        /// Closes and then settles, so a test observes the notification the
        /// production path deliberately does not wait for.
        fn close(&mut self) -> Option<PageUpdate> {
            let (update, closing) = self.session.take().expect("session is open").close();
            closing.settle();
            update
        }
    }

    fn frame(generation: u64) -> PageFrame {
        PageFrame {
            generation,
            title: "page".to_owned(),
            ..PageFrame::default()
        }
    }

    /// The floor exists because a raster rides in every frame: a page asking
    /// for a one millisecond period would demand gigabytes a second for a
    /// surface presented at 60 Hz. Zero still means "do not schedule".
    #[test]
    fn a_self_scheduled_redraw_is_clamped_to_the_presentable_rate() {
        assert_eq!(
            redraw_delay(0),
            None,
            "zero stays off rather than becoming the floor"
        );
        assert_eq!(
            redraw_delay(1),
            Some(Duration::from_millis(u64::from(MIN_PAGE_REDRAW_MS))),
            "a period the host cannot present is raised to one it can"
        );
        assert_eq!(
            redraw_delay(250),
            Some(Duration::from_millis(250)),
            "a period a plugin can afford is left exactly as asked"
        );
    }

    #[test]
    fn the_first_request_tells_the_plugin_its_page_was_opened() {
        let mut harness = Harness::open();
        let draw = harness.next_draw();
        assert_eq!(draw.generation, 1);
        assert_eq!(draw.page_id, "page");
        assert_eq!(draw.width, 800);
        assert_eq!(
            draw.events.iter().map(|event| event.kind).collect::<Vec<_>>(),
            vec![PageInputKind::Opened]
        );
        harness.reply(Ok(frame(1)));
        assert_eq!(harness.next_update().frame.generation, 1);
        harness.reply(Ok(PageFrame::default()));
        let _ = harness.close();
    }

    #[test]
    fn a_frame_answering_an_older_generation_is_never_drawn() {
        let mut harness = Harness::open();
        let opened = harness.next_draw();
        assert_eq!(opened.generation, 1);
        harness.reply(Ok(frame(1)));
        assert_eq!(harness.next_update().frame.generation, 1);

        harness
            .session()
            .send_input(PageInput::new(PageInputKind::PointerMoved))
            .expect("input is queued");
        let second = harness.next_draw();
        assert_eq!(second.generation, 2);
        // The plugin answers the request before last.
        harness.reply(Ok(frame(1)));

        // The stale answer produces no update, and the session asks again
        // rather than stopping.
        harness
            .session()
            .send_input(PageInput::new(PageInputKind::PointerMoved))
            .expect("input is queued");
        let third = harness.next_draw();
        assert_eq!(third.generation, 3);
        assert_eq!(harness.session().poll(), None);
        harness.reply(Ok(frame(3)));
        assert_eq!(harness.next_update().frame.generation, 3);
        harness.reply(Ok(PageFrame::default()));
        let _ = harness.close();
    }

    #[test]
    fn input_arriving_during_a_call_becomes_one_follow_up_request() {
        let mut harness = Harness::open();
        assert_eq!(harness.next_draw().generation, 1);
        // The session is now blocked inside `draw`; everything queued here
        // arrives while that call is in flight.
        for step in 0_u8..8 {
            let mut moved = PageInput::new(PageInputKind::PointerMoved);
            moved.x = f32::from(step);
            harness.session().send_input(moved).expect("input is queued");
        }
        harness
            .session()
            .send_input(PageInput::new(PageInputKind::PointerPressed))
            .expect("input is queued");
        harness.reply(Ok(frame(1)));

        let follow_up = harness.next_draw();
        // The frame answering the first request was published before this one
        // was asked for; consuming it here is what makes the generation
        // asserted below the follow-up's own rather than whichever of the two
        // happened to reach the mailbox first.
        assert_eq!(harness.next_update().frame.generation, 1);
        assert_eq!(follow_up.generation, 2, "the whole burst costs one more request");
        // Motion the user has already moved past is not worth delivering, so
        // the eight positions collapse to the last one; the press is discrete
        // and survives, because dropping it would lose something they did.
        assert_eq!(
            follow_up.events.len(),
            2,
            "pointer motion coalesces to the latest position"
        );
        assert_eq!(follow_up.events[0].kind, PageInputKind::PointerMoved);
        assert_eq!(
            follow_up.events[0].x, 7.0,
            "the surviving position is the newest one"
        );
        assert_eq!(follow_up.events[1].kind, PageInputKind::PointerPressed);
        harness.reply(Ok(frame(2)));
        assert_eq!(harness.next_update().frame.generation, 2);
        harness.reply(Ok(PageFrame::default()));
        let _ = harness.close();
    }

    #[test]
    fn closing_never_waits_for_a_plugin_that_has_stopped_answering() {
        // This runs on the thread that services Escape. A close that joined
        // the session thread would freeze the launcher for as long as the
        // child took to answer a notification whose reply is thrown away --
        // up to its hard call deadline. The plugin here never answers at all,
        // which is the worst case and must still be instant.
        let mut harness = Harness::open();
        assert_eq!(harness.next_draw().generation, 1);
        harness.reply(Ok(frame(1)));
        assert_eq!(harness.next_update().frame.generation, 1);
        // Wake the driver into a second call and leave it there unanswered.
        harness
            .session()
            .send_input(PageInput::new(PageInputKind::PointerPressed))
            .expect("input is queued");
        let _ = harness.next_draw();

        let started = std::time::Instant::now();
        let (update, closing) = harness.session.take().expect("session is open").close();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(250),
            "closing blocked for {elapsed:?} behind a plugin that never answered"
        );
        // The surface does not blink to empty on the way out: the close is
        // reported against the last frame that was actually drawn.
        let update = update.expect("closing reports the close at once");
        assert!(update.closed);
        assert_eq!(update.frame.generation, 1);
        // Releasing the reply channel lets the detached thread finish.
        drop(harness.replies);
        closing.settle();
    }

    #[test]
    fn a_command_arriving_while_the_driver_sleeps_is_not_swallowed() {
        // `recv` takes the command out of the channel, so a driver that woke
        // and discarded it would lose the user's click outright. The page is
        // idle here - the first frame is drawn and answered - which is exactly
        // the state where the driver is parked in `wait`.
        let mut harness = Harness::open();
        assert_eq!(harness.next_draw().generation, 1);
        harness.reply(Ok(frame(1)));
        assert_eq!(harness.next_update().frame.generation, 1);

        harness
            .session()
            .send_input(PageInput::new(PageInputKind::PointerPressed))
            .expect("input is queued");
        let woken = harness.next_draw();
        assert_eq!(
            woken.events.iter().map(|event| event.kind).collect::<Vec<_>>(),
            vec![PageInputKind::PointerPressed],
            "the command that woke the driver must reach the plugin"
        );
        harness.reply(Ok(frame(2)));
        assert_eq!(harness.next_update().frame.generation, 2);
        harness.reply(Ok(PageFrame::default()));
        let _ = harness.close();
    }

    #[test]
    fn closing_the_page_tells_the_plugin_once_and_keeps_the_last_frame() {
        let mut harness = Harness::open();
        assert_eq!(harness.next_draw().generation, 1);
        harness.reply(Ok(frame(1)));
        assert_eq!(harness.next_update().frame.generation, 1);

        // The closing notification is a request like any other, so it needs a
        // reply for the session thread to finish.
        harness.reply(Ok(PageFrame::default()));
        let closing = harness.close().expect("closing publishes a final update");
        assert!(closing.closed);
        assert_eq!(closing.frame.generation, 1, "the last drawable frame is kept");
        let notification = harness.next_draw();
        assert_eq!(
            notification.events.last().map(|event| event.kind),
            Some(PageInputKind::Closed)
        );
    }

    #[test]
    fn a_plugin_that_closes_its_own_page_is_not_asked_again() {
        let mut harness = Harness::open();
        assert_eq!(harness.next_draw().generation, 1);
        harness.reply(Ok(PageFrame {
            close: true,
            ..frame(1)
        }));
        let update = harness.next_update();
        assert!(update.closed);
        assert_eq!(update.frame.generation, 1);
        let session = harness.session.take().expect("session is open");
        drop(session);
        assert!(
            harness.seen.try_recv().is_err(),
            "a plugin that closed its own page is never asked to draw again"
        );
    }

    #[test]
    fn a_plugin_that_cannot_serve_a_frame_fails_the_page() {
        let mut harness = Harness::open();
        assert_eq!(harness.next_draw().generation, 1);
        harness.reply(Err("plugin page frame is not drawable".to_owned()));
        let update = harness.next_update();
        assert!(update.closed);
        assert_eq!(
            harness.session().failure().as_deref(),
            Some("plugin page frame is not drawable")
        );
        let session = harness.session.take().expect("session is open");
        drop(session);
        assert!(
            harness.seen.try_recv().is_err(),
            "a failed page does not ask the same plugin for one more round trip"
        );
    }
}
