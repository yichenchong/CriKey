use std::{
    collections::HashMap,
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

use egui::{
    load::SizedTexture, text::LayoutJob, vec2, Align, ColorImage, FontFamily, FontId, Frame, Layout, Margin,
    RawInput, RichText, Rounding, Stroke, TextEdit, TextFormat, TextStyle, TextureHandle, TextureOptions,
};
use egui_wgpu::{wgpu, Renderer, ScreenDescriptor};
use thiserror::Error;
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
    event::{ElementState, Ime, KeyEvent, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{Window, WindowId, WindowLevel},
};

use crate::{theme, LauncherWindow, ResultRow, SettingRow, UiCommand, ViewModel};

/// Maximum number of activation-to-present observations retained in memory.
///
/// Retention is a fixed-size ring: observing another activation never grows a
/// collection and older samples are replaced in arrival order.
pub const ACTIVATION_SAMPLE_CAPACITY: usize = 128;

const SURFACE_RETRY_DELAY: Duration = Duration::from_millis(16);
const MAX_CONSECUTIVE_SURFACE_RETRIES: u32 = 60;

/// Configuration fixed before the native event loop starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeLauncherConfig {
    pub title: String,
    pub width: u32,
    pub height: u32,
    /// Whether the launcher window is composited with what is behind it.
    ///
    /// Drives both `with_transparent` on the window and the surface's
    /// compositing mode, so the two cannot disagree. The shipped theme is
    /// currently opaque, but that is a theming decision rather than a renderer
    /// invariant: a translucent launcher backdrop is enabled here, not by
    /// editing the surface configuration.
    pub transparent: bool,
    /// How the surface paces presentation.
    ///
    /// [`wgpu::PresentMode::AutoVsync`] is the shipped default and the right
    /// one for the product: tearing is worse than a frame of latency. It is
    /// configurable only so a measurement harness can separate CriKey's own
    /// cost from the vblank wait, which is otherwise inside the same span —
    /// `get_current_texture` blocks on swapchain backpressure, and that happens
    /// inside the activation-to-present measurement.
    pub present_mode: wgpu::PresentMode,
}

impl Default for NativeLauncherConfig {
    fn default() -> Self {
        Self {
            title: "CriKey".to_owned(),
            width: theme::DEFAULT_WINDOW_WIDTH,
            height: theme::DEFAULT_WINDOW_HEIGHT,
            transparent: false,
            present_mode: wgpu::PresentMode::AutoVsync,
        }
    }
}

/// A failure to create, drive, or present the native renderer.
///
/// Window-system and graphics-driver failures cross the API as values. The
/// event loop exits cleanly after a terminal rendering failure rather than
/// panicking from an application callback.
#[derive(Debug, Error)]
pub enum RendererError {
    #[error("native event loop error: {0}")]
    EventLoop(#[from] winit::error::EventLoopError),
    #[error("native window creation failed: {0}")]
    Window(#[from] winit::error::OsError),
    #[error("GPU surface creation failed: {0}")]
    SurfaceCreation(#[from] wgpu::CreateSurfaceError),
    #[error("no graphics adapter can present to the launcher surface")]
    NoCompatibleAdapter,
    #[error("GPU device creation failed: {0}")]
    Device(#[from] wgpu::RequestDeviceError),
    #[error("the graphics adapter exposes no usable surface format")]
    NoSurfaceFormat,
    #[error("the graphics adapter exposes no usable surface alpha mode")]
    NoSurfaceAlphaMode,
    #[error("the launcher width and height must both be non-zero (received {width}x{height})")]
    InvalidWindowSize { width: u32, height: u32 },
    #[error("the native event loop is no longer accepting launcher requests")]
    EventLoopClosed,
    #[error("the GPU reported an uncaptured error: {0}")]
    Driver(String),
    #[error("the GPU surface ran out of memory")]
    SurfaceOutOfMemory,
    #[error("the native launcher window was destroyed while its event loop was running")]
    WindowDestroyed,
}

/// Allocation-bounded warm-activation latency measurements.
///
/// Requests received before the retained GPU surface is ready are cold-start
/// work and are excluded. `observe` records the first successful surface
/// presentation after each later activation. `snapshot` calculates nearest-rank
/// p95 over only the retained ring, while `total_samples` reports the
/// process-lifetime warm-sample count.
#[derive(Debug, Clone)]
pub struct ActivationLatencyTracker {
    samples: [Duration; ACTIVATION_SAMPLE_CAPACITY],
    len: usize,
    next: usize,
    total_samples: u64,
    latest: Option<Duration>,
}

impl Default for ActivationLatencyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivationLatencyTracker {
    pub const fn new() -> Self {
        Self {
            samples: [Duration::ZERO; ACTIVATION_SAMPLE_CAPACITY],
            len: 0,
            next: 0,
            total_samples: 0,
            latest: None,
        }
    }

    pub fn observe(&mut self, elapsed: Duration) {
        self.samples[self.next] = elapsed;
        self.next = (self.next + 1) % ACTIVATION_SAMPLE_CAPACITY;
        self.len = self.len.saturating_add(1).min(ACTIVATION_SAMPLE_CAPACITY);
        self.total_samples = self.total_samples.saturating_add(1);
        self.latest = Some(elapsed);
    }

    pub fn snapshot(&self) -> ActivationLatencySnapshot {
        let mut ordered = [Duration::ZERO; ACTIVATION_SAMPLE_CAPACITY];
        ordered[..self.len].copy_from_slice(&self.samples[..self.len]);
        ordered[..self.len].sort_unstable();
        let p95 = if self.len == 0 {
            None
        } else {
            let nearest_rank = (self.len * 95).div_ceil(100);
            Some(ordered[nearest_rank.saturating_sub(1)])
        };

        ActivationLatencySnapshot {
            total_samples: self.total_samples,
            retained_samples: self.len,
            latest: self.latest,
            p95,
        }
    }
}

/// A point-in-time, allocation-free latency report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivationLatencySnapshot {
    pub total_samples: u64,
    pub retained_samples: usize,
    pub latest: Option<Duration>,
    pub p95: Option<Duration>,
}

/// Output of one renderer-independent egui frame construction.
///
/// This is deliberately callable without a window or GPU. Native drawing uses
/// the exact same function, and headless callers can inspect `output.shapes` to
/// verify what text and widgets the next presented frame contains.
pub struct NativeUiFrame {
    pub output: egui::FullOutput,
    pub commands: Vec<UiCommand>,
}

impl fmt::Debug for NativeUiFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeUiFrame")
            .field("shape_count", &self.output.shapes.len())
            .field("commands", &self.commands)
            .finish_non_exhaustive()
    }
}

/// Event delivered by [`NativeLauncher::run`] on the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeLauncherEvent {
    /// A platform activation request has reached the native event loop.
    ///
    /// The host should activate its existing `LauncherViewModel` and submit
    /// the coalesced frame through `NativeLauncherHandle`.
    Activated,
    /// Keyboard or pointer input translated to the existing command contract.
    ///
    /// `session` scopes effects such as hiding after a successful action. A
    /// late command from an older activation must never hide a newer one.
    Command { session: u64, command: UiCommand },
    /// A provider driver has an answer waiting for the UI thread to merge.
    ///
    /// Carries nothing: the answer itself lives in the driver the host will
    /// poll, and this event says only that polling is now worth doing. It is
    /// inert with respect to visibility — it neither shows nor hides the
    /// window — and it is delivered only for the session that is currently
    /// active, so an answer belonging to a dismissed or superseded activation
    /// is dropped rather than merged into a newer one.
    ProviderAnswer,
}

/// Creates an egui context configured with the launcher's shared visual tokens.
pub fn create_launcher_context() -> egui::Context {
    let context = egui::Context::default();
    theme::install(&context);
    context
}

/// Builds one immutable launcher frame by calling [`egui::Context::run`].
///
/// Text editing and pointer actions are returned as existing [`UiCommand`]
/// values. This function does not retain a query, selection, or action state;
/// the supplied [`ViewModel`] remains the sole frame state.
pub fn build_launcher_frame(context: &egui::Context, input: RawInput, model: &ViewModel) -> NativeUiFrame {
    build_launcher_frame_with_transparency(context, input, model, false)
}

fn build_launcher_frame_with_transparency(
    context: &egui::Context,
    input: RawInput,
    model: &ViewModel,
    transparent: bool,
) -> NativeUiFrame {
    let mut commands = Vec::new();
    let output = context.run(input, |context| {
        // `Context::run` may repeat a pass after a discard request. Preserve
        // input work from every pass, then collapse identical repeats.
        draw_launcher(context, model, &mut commands, transparent);
    });
    commands.dedup();
    NativeUiFrame { output, commands }
}

#[derive(Debug)]
enum NativeEvent {
    Activate { session: u64, requested_at: Instant },
    Toggle { session: u64 },
    Hide { session: u64 },
    FrameReady { session: u64 },
    ProviderAnswer { session: u64 },
    RepaintAfter(Duration),
    DriverError(String),
    Exit,
}

struct EventProxy {
    inner: EventLoopProxy<NativeEvent>,
}

impl EventProxy {
    fn new(inner: EventLoopProxy<NativeEvent>) -> Self {
        Self { inner }
    }

    fn send(&self, event: NativeEvent) -> Result<(), RendererError> {
        self.inner
            .send_event(event)
            .map_err(|_| RendererError::EventLoopClosed)
    }
}

#[derive(Debug)]
struct PendingFrame {
    session: u64,
    model: ViewModel,
}

#[derive(Debug, Default)]
struct FrameMailbox {
    latest: Option<PendingFrame>,
    wake_session: Option<u64>,
    /// The session an unconsumed [`NativeEvent::ProviderAnswer`] was announced
    /// for, coalescing answer wakes exactly as `wake_session` coalesces frame
    /// wakes: several drivers can answer before the loop next runs, and the
    /// host merges all of them in one turn, so a second event would only make
    /// the loop turn again for nothing. It is a separate flag rather than a
    /// share of `wake_session` because a frame wake and an answer wake mean
    /// different things — "draw this" against "merge, then decide what to
    /// draw" — and one must never swallow the other.
    answer_session: Option<u64>,
    /// The host-owned half of the view model, and the session it was published
    /// for.
    ///
    /// It lives under the mailbox lock rather than beside it because a provider
    /// frame reads it and inserts in one step: with a second lock, a provider
    /// could read an open panel, lose the race to a host frame that closed it,
    /// and then overwrite the closed frame with the stale open one. The session
    /// tag is what stops a panel left over from a dismissed activation
    /// reappearing over the next one, which has not published a host frame yet.
    overlay: Overlay,
    overlay_session: Option<u64>,
}

const VISIBLE_BIT: u64 = 1;
const SESSION_SHIFT: u32 = 1;

const fn lifecycle_state(session: u64, visible: bool) -> u64 {
    (session << SESSION_SHIFT) | visible as u64
}

const fn lifecycle_session(state: u64) -> u64 {
    state >> SESSION_SHIFT
}

const fn lifecycle_visible(state: u64) -> bool {
    state & VISIBLE_BIT != 0
}

#[derive(Debug)]
struct SharedState {
    lifecycle: AtomicU64,
    frames: Mutex<FrameMailbox>,
    latency: Mutex<ActivationLatencyTracker>,
}

/// The part of a frame that belongs to the host rather than to a query.
///
/// Provider drivers publish straight to the renderer from their own threads,
/// and the view model they build describes results only: it knows nothing
/// about a settings panel the user opened a moment ago. Taken at face value,
/// the first suggestion to arrive would blank that panel under the user's
/// hands.
#[derive(Debug, Default, Clone)]
struct Overlay {
    settings_open: bool,
    settings: Arc<[SettingRow]>,
    settings_focus: Option<String>,
}

/// Who composed a frame, and therefore who owns its overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameSource {
    /// The UI thread's retained view model: the whole frame, overlay included.
    Host,
    /// A provider driver's results-only frame.
    Provider,
}

/// Remembers the host-owned half of a frame the host itself composed.
fn record_overlay(mailbox: &mut FrameMailbox, session: u64, model: &ViewModel) {
    mailbox.overlay = Overlay {
        settings_open: model.settings_open,
        settings: Arc::clone(&model.settings),
        settings_focus: model.settings_focus.clone(),
    };
    mailbox.overlay_session = Some(session);
}

/// Puts this session's retained host overlay back onto a provider's frame.
///
/// An overlay published for an earlier session is not this session's business:
/// a new activation starts with no panel until the host says otherwise.
fn with_overlay(mailbox: &FrameMailbox, session: u64, model: &ViewModel) -> ViewModel {
    if mailbox.overlay_session != Some(session) {
        return model.clone();
    }
    ViewModel {
        settings_open: mailbox.overlay.settings_open,
        settings: Arc::clone(&mailbox.overlay.settings),
        settings_focus: mailbox.overlay.settings_focus.clone(),
        ..model.clone()
    }
}

impl SharedState {
    fn snapshot(&self) -> u64 {
        self.lifecycle.load(Ordering::Acquire)
    }

    fn session(&self) -> u64 {
        lifecycle_session(self.snapshot())
    }

    fn is_visible(&self) -> bool {
        lifecycle_visible(self.snapshot())
    }

    fn is_visible_session(&self, session: u64) -> bool {
        self.snapshot() == lifecycle_state(session, true)
    }

    fn claim_activation(&self) -> Option<u64> {
        let mut current = self.snapshot();
        loop {
            if lifecycle_visible(current) {
                return None;
            }
            let session = lifecycle_session(current)
                .saturating_add(1)
                .min(u64::MAX >> SESSION_SHIFT);
            let activated = lifecycle_state(session, true);
            match self.lifecycle.compare_exchange_weak(
                current,
                activated,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(session),
                Err(observed) => current = observed,
            }
        }
    }

    fn claim_hide(&self, session: u64) -> bool {
        self.lifecycle
            .compare_exchange(
                lifecycle_state(session, true),
                lifecycle_state(session, false),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn cancel_activation(&self, session: u64) {
        let _ = self.lifecycle.compare_exchange(
            lifecycle_state(session, true),
            lifecycle_state(session, false),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn restore_visible(&self, session: u64) {
        let _ = self.lifecycle.compare_exchange(
            lifecycle_state(session, false),
            lifecycle_state(session, true),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn clear_frames(&self, session: u64) {
        let mut mailbox = lock_recover(&self.frames);
        if mailbox
            .latest
            .as_ref()
            .is_some_and(|frame| frame.session == session)
        {
            mailbox.latest = None;
        }
        if mailbox.wake_session == Some(session) {
            mailbox.wake_session = None;
        }
    }

    /// Retires the pending wake for `session` without touching the frame it
    /// announced.
    ///
    /// A wake is a promise that exactly one `FrameReady` is on its way, and
    /// `submit_frame` stays silent while that promise is outstanding. Whoever
    /// consumes the `FrameReady` must therefore retire the promise even when
    /// it has no frame to draw, otherwise the mailbox is left permanently
    /// "already woken" and every later submission for that session is stored
    /// but never announced.
    fn acknowledge_wake(&self, session: u64) {
        let mut mailbox = lock_recover(&self.frames);
        if mailbox.wake_session == Some(session) {
            mailbox.wake_session = None;
        }
    }

    /// Claims the right to announce that a provider answer is waiting for
    /// `session`, returning false when an announcement is already outstanding.
    ///
    /// The same promise discipline `wake_session` uses, for the same reason: a
    /// driver may answer several times before the loop next runs, and every
    /// answer after the first would buy a wake the host has already been given.
    /// Nothing is lost by refusing them — the host polls the drivers when it
    /// runs, so the one announcement covers whatever has accumulated by then.
    fn claim_answer_wake(&self, session: u64) -> bool {
        let mut mailbox = lock_recover(&self.frames);
        if mailbox.answer_session == Some(session) {
            return false;
        }
        mailbox.answer_session = Some(session);
        true
    }

    /// Retires the outstanding answer announcement for `session`.
    ///
    /// Called before the host is given the event rather than after: merging is
    /// what makes the next answer worth announcing, and a promise still held
    /// while the host runs would swallow the wake for an answer that landed
    /// during the merge.
    fn acknowledge_answer_wake(&self, session: u64) {
        let mut mailbox = lock_recover(&self.frames);
        if mailbox.answer_session == Some(session) {
            mailbox.answer_session = None;
        }
    }

    fn clear_all_frames(&self) {
        let mut mailbox = lock_recover(&self.frames);
        mailbox.latest = None;
        mailbox.wake_session = None;
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            lifecycle: AtomicU64::new(lifecycle_state(0, false)),
            frames: Mutex::new(FrameMailbox::default()),
            latency: Mutex::new(ActivationLatencyTracker::new()),
        }
    }
}

/// Cross-thread control plane for the native event loop.
///
/// The handle is `Send + Sync` and cheap to clone. In particular, a platform
/// hotkey callback may call [`request_activation`](Self::request_activation):
/// it records the request time and does no work beyond
/// [`EventLoopProxy::send_event`]. The window, egui context, and GPU remain
/// confined to the event-loop thread.
#[derive(Clone)]
pub struct NativeLauncherHandle {
    proxy: Arc<EventProxy>,
    shared: Arc<SharedState>,
}

impl fmt::Debug for NativeLauncherHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeLauncherHandle")
            .field("visible", &self.is_visible())
            .field("session", &self.shared.session())
            .finish_non_exhaustive()
    }
}

impl NativeLauncherHandle {
    /// Wakes the native event loop to show and focus the retained window.
    ///
    /// Repeated requests while already active are idempotent and do not start a
    /// second latency sample.
    pub fn request_activation(&self) -> Result<(), RendererError> {
        let Some(session) = self.shared.claim_activation() else {
            return Ok(());
        };
        let event = NativeEvent::Activate {
            session,
            requested_at: Instant::now(),
        };
        if let Err(error) = self.proxy.send(event) {
            self.shared.cancel_activation(session);
            return Err(error);
        }
        Ok(())
    }

    /// Toggles the launcher from a global-hotkey callback without polling.
    ///
    /// Closing is delivered to the host as [`UiCommand::Dismiss`] on the event
    /// loop thread before the native window hides, so the view model clears the
    /// session exactly as it does for Escape or an OS close request.
    pub fn request_toggle(&self) -> Result<(), RendererError> {
        loop {
            let state = self.shared.snapshot();
            if !lifecycle_visible(state) {
                return self.request_activation();
            }
            let session = lifecycle_session(state);
            if !self.shared.claim_hide(session) {
                continue;
            }
            if let Err(error) = self.proxy.send(NativeEvent::Toggle { session }) {
                self.shared.restore_visible(session);
                return Err(error);
            }
            return Ok(());
        }
    }

    /// Hides the retained window and drops any frame queued for this session.
    pub fn request_hide(&self) -> Result<(), RendererError> {
        loop {
            let state = self.shared.snapshot();
            if !lifecycle_visible(state) {
                return Ok(());
            }
            let session = lifecycle_session(state);
            if self.shared.claim_hide(session) {
                return self.finish_hide_request(session);
            }
        }
    }

    /// Hides only the activation that emitted a command.
    ///
    /// A successful action may finish after another hotkey press has opened a
    /// newer session. In that case this is deliberately a no-op.
    pub fn request_hide_session(&self, session: u64) -> Result<(), RendererError> {
        if !self.shared.claim_hide(session) {
            return Ok(());
        }
        self.finish_hide_request(session)
    }

    fn finish_hide_request(&self, session: u64) -> Result<(), RendererError> {
        self.shared.clear_frames(session);
        self.proxy.send(NativeEvent::Hide { session })
    }

    /// Tells the UI thread that a provider driver has an answer to merge.
    ///
    /// Some provider answers cannot be turned into a frame on the thread that
    /// produced them: the launcher's file search is ranked against the catalog,
    /// and the ranker lives on the UI thread. Those drivers park their answer
    /// and call this, and the host merges it on the next turn of the loop.
    ///
    /// Deliberately inert with respect to the window. It says "there is work to
    /// merge" and nothing about visibility: a hidden launcher has no session to
    /// merge into, so the announcement is dropped rather than allowed to wake
    /// one back up. Coalesced per session, so a burst of answers costs one
    /// wake; see [`SharedState::claim_answer_wake`].
    pub fn request_provider_answer(&self) -> Result<(), RendererError> {
        let state = self.shared.snapshot();
        if !lifecycle_visible(state) {
            return Ok(());
        }
        let session = lifecycle_session(state);
        if !self.shared.claim_answer_wake(session) {
            return Ok(());
        }
        if let Err(error) = self.proxy.send(NativeEvent::ProviderAnswer { session }) {
            self.shared.acknowledge_answer_wake(session);
            return Err(error);
        }
        Ok(())
    }

    /// Replaces the frame waiting for the UI thread with the newest immutable
    /// view model and wakes the loop at most once for that session.
    ///
    /// This is the host's path: `model` is the whole frame, overlay included,
    /// so what it says about the settings surface becomes what a later provider
    /// frame inherits.
    ///
    /// Replacing a pending frame preserves the view model's coalescing
    /// semantics; rows remain shared through their `Arc` when `model` is cloned.
    pub fn submit_frame(&self, model: &ViewModel) -> Result<(), RendererError> {
        self.enqueue(model, FrameSource::Host)
    }

    /// Publishes a results-only frame from a provider thread.
    ///
    /// A provider driver builds its view model from a query and its own rows;
    /// it has no idea whether the user has the settings surface open, and the
    /// `false` it necessarily carries would close that surface mid-edit. This
    /// session's retained host overlay is put back on as the frame is queued,
    /// so a suggestion arriving during a settings edit updates the results
    /// behind the panel instead of dismissing it.
    pub fn submit_results(&self, model: &ViewModel) -> Result<(), RendererError> {
        self.enqueue(model, FrameSource::Provider)
    }

    /// Stores `model` as this session's pending frame, reconciling the overlay
    /// under the same lock that inserts it.
    ///
    /// Reading the overlay and inserting the frame have to be one step: split
    /// across two locks, a provider could read an open settings panel, lose the
    /// race to a host frame that closed it, and then overwrite that frame with
    /// the panel the user just dismissed.
    fn enqueue(&self, model: &ViewModel, source: FrameSource) -> Result<(), RendererError> {
        let state = self.shared.snapshot();
        if !lifecycle_visible(state) {
            return Ok(());
        }
        let session = lifecycle_session(state);
        let should_wake = {
            let mut mailbox = lock_recover(&self.shared.frames);
            if self.shared.snapshot() != state {
                return Ok(());
            }
            let model = match source {
                FrameSource::Host => {
                    record_overlay(&mut mailbox, session, model);
                    model.clone()
                }
                FrameSource::Provider => with_overlay(&mailbox, session, model),
            };
            mailbox.latest = Some(PendingFrame { session, model });
            if mailbox.wake_session == Some(session) {
                false
            } else {
                mailbox.wake_session = Some(session);
                true
            }
        };

        if should_wake && self.proxy.send(NativeEvent::FrameReady { session }).is_err() {
            let mut mailbox = lock_recover(&self.shared.frames);
            if mailbox.wake_session == Some(session) {
                mailbox.wake_session = None;
            }
            return Err(RendererError::EventLoopClosed);
        }
        Ok(())
    }

    /// Requests a normal event-loop shutdown.
    pub fn request_exit(&self) -> Result<(), RendererError> {
        self.shared.lifecycle.fetch_and(!VISIBLE_BIT, Ordering::AcqRel);
        self.shared.clear_all_frames();
        self.proxy.send(NativeEvent::Exit)
    }

    pub fn is_visible(&self) -> bool {
        self.shared.is_visible()
    }

    /// Reports process-lifetime count plus p95 over the fixed retained ring.
    pub fn activation_latency(&self) -> ActivationLatencySnapshot {
        lock_recover(&self.shared.latency).snapshot()
    }
}

impl LauncherWindow for NativeLauncherHandle {
    fn show(&mut self) {
        let _ = self.request_activation();
    }

    fn hide(&mut self) {
        let _ = self.request_hide();
    }

    fn is_visible(&self) -> bool {
        NativeLauncherHandle::is_visible(self)
    }

    fn present(&mut self, model: &ViewModel) {
        let _ = self.submit_frame(model);
    }
}

/// Owns the non-returning native event loop.
///
/// Construct this on the process main thread and only once. `winit` requires
/// creation and execution on that same thread on every supported desktop, and
/// macOS additionally requires it to be the application main thread. Obtain a
/// cloneable [`NativeLauncherHandle`] before calling [`run`](Self::run) to wire
/// hotkeys, search workers, and shutdown requests without polling.
pub struct NativeLauncher {
    event_loop: EventLoop<NativeEvent>,
    config: NativeLauncherConfig,
    handle: NativeLauncherHandle,
}

impl fmt::Debug for NativeLauncher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeLauncher")
            .field("config", &self.config)
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl NativeLauncher {
    /// Creates the event loop and its thread-safe proxy. The native window and
    /// GPU are created once in `resumed`, before queued activation events are
    /// dispatched, because `winit` only permits window creation there.
    pub fn new(config: NativeLauncherConfig) -> Result<Self, RendererError> {
        if config.width == 0 || config.height == 0 {
            return Err(RendererError::InvalidWindowSize {
                width: config.width,
                height: config.height,
            });
        }
        let event_loop = EventLoop::<NativeEvent>::with_user_event().build()?;
        let shared = Arc::new(SharedState::default());
        let handle = NativeLauncherHandle {
            proxy: Arc::new(EventProxy::new(event_loop.create_proxy())),
            shared,
        };
        Ok(Self {
            event_loop,
            config,
            handle,
        })
    }

    pub fn handle(&self) -> NativeLauncherHandle {
        self.handle.clone()
    }

    /// Runs without polling and delivers activation plus native input events.
    ///
    /// The callback executes on the event-loop thread. On `Activated`, it
    /// should activate the existing `LauncherViewModel`. On `Command`, it
    /// should apply the enclosed `UiCommand` and handle any returned
    /// `UiEffect`. After either event it should submit at most
    /// `LauncherViewModel::frame()` through a cloned handle. It must not await
    /// plugin work.
    pub fn run<F>(self, on_event: F) -> Result<(), RendererError>
    where
        F: FnMut(NativeLauncherEvent) + 'static,
    {
        let Self {
            event_loop,
            config,
            handle,
        } = self;
        let mut application = NativeApplication::new(config, handle, on_event);
        let event_result = event_loop.run_app(&mut application);
        if let Some(error) = application.terminal_error {
            Err(error)
        } else {
            event_result.map_err(RendererError::EventLoop)
        }
    }
}

struct NativeApplication<F> {
    config: NativeLauncherConfig,
    proxy: Arc<EventProxy>,
    shared: Arc<SharedState>,
    graphics: Option<GraphicsState>,
    graphics_ready_at: Option<Instant>,
    frame: Option<ViewModel>,
    active_session: Option<u64>,
    pending_activation: Option<Instant>,
    modifiers: ModifiersState,
    /// True while an input method is building a character out of several key
    /// presses. Those presses belong to the composition, not to the launcher.
    composing: bool,
    /// True once the compositor has told us this window holds the keyboard.
    ///
    /// Dismissal on focus loss is gated on this having been observed first: a
    /// headless or unfocusable backend that never sends `Focused(true)` (Xvfb
    /// with no window manager, which the launcher tests run under) would
    /// otherwise close the launcher the instant a stray `Focused(false)`
    /// arrived at startup.
    focused: bool,
    next_repaint: Option<Instant>,
    consecutive_surface_retries: u32,
    on_event: F,
    terminal_error: Option<RendererError>,
    exiting: bool,
}

impl<F> NativeApplication<F>
where
    F: FnMut(NativeLauncherEvent),
{
    fn new(config: NativeLauncherConfig, handle: NativeLauncherHandle, on_event: F) -> Self {
        Self {
            config,
            proxy: handle.proxy,
            shared: handle.shared,
            graphics: None,
            graphics_ready_at: None,
            frame: None,
            active_session: None,
            pending_activation: None,
            modifiers: ModifiersState::default(),
            composing: false,
            focused: false,
            next_repaint: None,
            consecutive_surface_retries: 0,
            on_event,
            terminal_error: None,
            exiting: false,
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: RendererError) {
        if self.terminal_error.is_none() {
            self.terminal_error = Some(error);
        }
        self.shared.lifecycle.fetch_and(!VISIBLE_BIT, Ordering::AcqRel);
        self.shared.clear_all_frames();
        self.exiting = true;
        event_loop.exit();
    }

    fn take_frame(&self, session: u64, acknowledge_wake: bool) -> Option<ViewModel> {
        let mut mailbox = lock_recover(&self.shared.frames);
        if acknowledge_wake && mailbox.wake_session == Some(session) {
            mailbox.wake_session = None;
        }
        match mailbox.latest.as_ref() {
            Some(pending) if pending.session == session => mailbox.latest.take().map(|pending| pending.model),
            _ => None,
        }
    }

    fn accept_latest_frame(&mut self, session: u64, acknowledge_wake: bool) -> bool {
        let Some(frame) = self.take_frame(session, acknowledge_wake) else {
            return false;
        };
        self.frame = Some(frame);
        true
    }

    fn request_redraw(&self) {
        if let Some(graphics) = &self.graphics {
            graphics.window.request_redraw();
        }
    }

    fn show(&mut self) {
        if let Some(graphics) = &self.graphics {
            graphics.show();
        }
    }

    fn hide(&mut self) {
        if let Some(graphics) = &self.graphics {
            graphics.hide();
        }
        self.frame = None;
        self.pending_activation = None;
        // Hiding disables the input method, so a half-built character does not
        // survive into the next activation.
        self.composing = false;
        // Unmapping the window makes the compositor hand the keyboard back to
        // whatever had it before, so a `Focused(false)` follows every hide.
        // Forgetting the focus here is what stops that event from being read as
        // a fresh click-away and dismissing the *next* activation.
        self.focused = false;
        self.next_repaint = None;
        self.consecutive_surface_retries = 0;
    }

    fn dispatch_command(&mut self, event_loop: &ActiveEventLoop, command: UiCommand) {
        let Some(session) = self.active_session else {
            return;
        };
        (self.on_event)(NativeLauncherEvent::Command { session, command });
        if self.accept_latest_frame(session, false) {
            self.request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::Wait);
    }

    fn schedule_repaint(&mut self, delay: Duration) {
        if self.active_session.is_none() || delay == Duration::MAX {
            return;
        }
        if delay.is_zero() {
            self.request_redraw();
            return;
        }
        let Some(deadline) = Instant::now().checked_add(delay) else {
            return;
        };
        if self.next_repaint.is_none_or(|scheduled| deadline < scheduled) {
            self.next_repaint = Some(deadline);
        }
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        if self.active_session.is_none() {
            return;
        }
        let Some(model) = self.frame.as_ref() else {
            return;
        };
        let Some(graphics) = self.graphics.as_mut() else {
            return;
        };

        self.next_repaint = None;
        let draw = match graphics.draw(model) {
            Ok(draw) => draw,
            Err(error) => {
                self.fail(event_loop, error);
                return;
            }
        };

        if let Some(presented_at) = draw.presented_at {
            if let Some(requested_at) = self.pending_activation.take() {
                let elapsed = presented_at.saturating_duration_since(requested_at);
                lock_recover(&self.shared.latency).observe(elapsed);
            }
        }
        if draw.retry {
            self.consecutive_surface_retries = self.consecutive_surface_retries.saturating_add(1);
            if self.consecutive_surface_retries >= MAX_CONSECUTIVE_SURFACE_RETRIES {
                self.fail(
                    event_loop,
                    RendererError::Driver("GPU surface recovery did not converge".to_owned()),
                );
                return;
            }
            self.schedule_repaint(SURFACE_RETRY_DELAY);
        } else {
            self.consecutive_surface_retries = 0;
            self.schedule_repaint(draw.repaint_after);
        }
        for command in draw.commands {
            self.dispatch_command(event_loop, command);
        }
    }
}

impl<F> ApplicationHandler<NativeEvent> for NativeApplication<F>
where
    F: FnMut(NativeLauncherEvent),
{
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::Wait);
        if self.graphics.is_some() {
            if self.active_session.is_some() {
                self.show();
            }
            return;
        }

        match GraphicsState::new(event_loop, &self.config, self.proxy.clone()) {
            Ok(graphics) => {
                self.graphics = Some(graphics);
                self.graphics_ready_at = Some(Instant::now());
                if self.active_session.is_some() {
                    self.show();
                }
            }
            Err(error) => self.fail(event_loop, error),
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: NativeEvent) {
        match event {
            NativeEvent::Activate {
                session,
                requested_at,
            } => {
                if !self.shared.is_visible_session(session) {
                    return;
                }
                if self.active_session != Some(session) {
                    self.active_session = Some(session);
                    self.frame = None;
                    self.pending_activation = self
                        .graphics_ready_at
                        .filter(|ready_at| requested_at >= *ready_at)
                        .map(|_| requested_at);
                    self.next_repaint = None;
                    (self.on_event)(NativeLauncherEvent::Activated);
                }
                self.accept_latest_frame(session, false);
                self.show();
                self.request_redraw();
            }
            NativeEvent::Toggle { session } => {
                if self.active_session != Some(session) || self.shared.session() != session {
                    return;
                }
                (self.on_event)(NativeLauncherEvent::Command {
                    session,
                    command: UiCommand::Dismiss,
                });
                self.shared.clear_frames(session);
                self.active_session = None;
                self.hide();
            }
            NativeEvent::Hide { session } => {
                if self.active_session == Some(session) {
                    self.active_session = None;
                    self.hide();
                }
            }
            NativeEvent::FrameReady { session } => {
                if self.active_session == Some(session) {
                    if self.accept_latest_frame(session, true) {
                        self.request_redraw();
                    }
                } else {
                    // The frame belongs to a session this thread has not
                    // activated yet (a submission can overtake its own
                    // `Activate`) or to one it has already left. Either way the
                    // wake this event carried is spent and must be retired, or
                    // the next submission for that session never wakes the
                    // loop and the list stops updating.
                    self.shared.acknowledge_wake(session);
                }
            }
            NativeEvent::ProviderAnswer { session } => {
                // Retired first, whatever happens next: the merge the host is
                // about to perform is exactly when the next answer can land,
                // and a promise still outstanding then would lose its wake.
                self.shared.acknowledge_answer_wake(session);
                // Nothing about the window changes here. An answer for a
                // session this thread has already left is simply dropped: the
                // rows it would merge into are gone, and merging it into
                // whatever came after would be the older query's answer under
                // the newer query's generation.
                if self.active_session == Some(session) {
                    (self.on_event)(NativeLauncherEvent::ProviderAnswer);
                }
            }
            NativeEvent::RepaintAfter(delay) => self.schedule_repaint(delay),
            NativeEvent::DriverError(message) => {
                self.fail(event_loop, RendererError::Driver(message));
            }
            NativeEvent::Exit => {
                self.exiting = true;
                self.active_session = None;
                // Cleared here rather than only in `request_exit`, so that the
                // event means the same thing whoever sends it: the Windows
                // session-end subclass sends it from a window procedure that
                // holds no handle.
                self.shared.lifecycle.fetch_and(!VISIBLE_BIT, Ordering::AcqRel);
                self.shared.clear_all_frames();
                self.hide();
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        if self
            .graphics
            .as_ref()
            .is_none_or(|graphics| graphics.window.id() != window_id)
        {
            return;
        }

        let command = match &event {
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } if self.active_session.is_some() && !self.composing => {
                translate_keyboard(event, self.modifiers, self.frame.as_ref())
            }
            _ => None,
        };
        {
            let Some(graphics) = self.graphics.as_mut() else {
                return;
            };
            if command.is_none() {
                let response = graphics
                    .egui_state
                    .on_window_event(graphics.window.as_ref(), &event);
                if response.repaint && self.active_session.is_some() {
                    graphics.window.request_redraw();
                }
            }
            // `egui-winit` discards every `WindowEvent::Ime` when it is built
            // for Linux (egui #5008), where an input method that echoes plain
            // keys would otherwise insert their text twice. That reasoning
            // does not cover a character an X input method *composed*: winit
            // emits either a `KeyboardInput` or an `Ime::Commit` for one key
            // event and never both (`x11/event_processor.rs`, the
            // `keycode != 0 && !is_composing` branch returns before the commit
            // path), and the press that finishes a compose sequence is
            // filtered, so the commit is the only delivery of those bytes.
            // Dropping it loses the character outright.
            //
            // The `Enabled` that precedes it is not decoration. egui only
            // accepts a commit whose caret still sits where the composition
            // was anchored (`text_edit/builder.rs`, `ImeEvent::Commit`), and
            // the X11 stream carries no `Ime::Enabled` before a commit -- only
            // `Ime::Preedit("", None)`, which is a preedit *clear*, not an
            // anchor. Anchoring here means the composed text replaces the
            // selection and lands at the caret.
            //
            // Preedit is deliberately not forwarded: egui expects the
            // Windows/macOS ordering, in which the anchor precedes the
            // composition, and replaying X11's `Preedit("", None)` before a
            // commit moves the caret off that anchor and drops the character.
            // A Linux composition is therefore invisible until it commits,
            // which is a display limitation, not a lost keystroke.
            #[cfg(target_os = "linux")]
            if let WindowEvent::Ime(Ime::Commit(text)) = &event {
                let events = &mut graphics.egui_state.egui_input_mut().events;
                events.push(egui::Event::Ime(egui::ImeEvent::Enabled));
                events.push(egui::Event::Ime(egui::ImeEvent::Commit(text.clone())));
            }
            match &event {
                WindowEvent::Resized(size) => graphics.resize(size.width, size.height),
                WindowEvent::ScaleFactorChanged { .. } => {
                    let size = graphics.window.inner_size();
                    graphics.resize(size.width, size.height);
                }
                _ => {}
            }
        }

        match event {
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            // Enter commits a composition and Escape abandons one, so while an
            // input method owns the keyboard those keys are its own and must
            // not also run the selected result or close the launcher.
            WindowEvent::Ime(ime) => {
                self.composing = match ime {
                    Ime::Preedit(text, _) => !text.is_empty(),
                    Ime::Enabled | Ime::Commit(_) | Ime::Disabled => false,
                };
            }
            WindowEvent::KeyboardInput { .. } => {
                if let Some(command) = command {
                    self.dispatch_command(event_loop, command);
                }
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            WindowEvent::CloseRequested => self.dispatch_command(event_loop, UiCommand::Dismiss),
            // A launcher is dismissed by being clicked away from, the way
            // Spotlight and Alfred are. The dismissal is the same one Escape
            // ends at, so it goes through `dispatch_command` and lets the host
            // clear the view model and hide the window; hiding here as well
            // would be a second, divergent path.
            WindowEvent::Focused(focused) => {
                let dismiss =
                    should_dismiss_on_focus_change(focused, self.active_session.is_some(), self.focused);
                self.focused = focused;
                if dismiss {
                    self.dispatch_command(event_loop, UiCommand::Dismiss);
                }
            }
            WindowEvent::Destroyed if !self.exiting => {
                self.fail(event_loop, RendererError::WindowDestroyed);
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.active_session.is_none() {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }
        match self.next_repaint {
            Some(deadline) if deadline <= Instant::now() => {
                self.next_repaint = None;
                self.request_redraw();
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

struct GraphicsState {
    window: Arc<Window>,
    transparent: bool,
    /// The height the window returns to as soon as it has something to show
    /// below the query field, in logical pixels.
    expanded_height: u32,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    egui_context: egui::Context,
    egui_state: egui_winit::State,
}

/// How tall the window has to be to show `model`, in logical pixels.
///
/// The launcher is a query field that grows a list under itself and shrinks
/// back when the list empties. Nothing under the field -- no rows, no settings
/// surface -- is [`theme::COMPACT_WINDOW_HEIGHT`], and that includes a query
/// the providers have not answered yet: a window that grew on the first
/// keystroke would stand there as an empty panel until results arrived.
///
/// With rows, the height is arithmetic over the row metrics rather than a
/// measurement of the frame. egui lays the frame out inside whatever window
/// already exists, so asking it how tall the content is answers with the
/// current window height and the size never changes again. The metrics are
/// pinned instead, by `every_result_row_matches_the_pinned_row_metrics`, which
/// lays real rows out and fails if the drawing and this sum disagree, and by
/// `a_listed_window_leaves_the_status_line_room_to_be_read`, which fails if
/// the sum below stops covering everything the frame actually draws.
fn desired_window_height(model: &ViewModel, expanded_height: u32) -> u32 {
    if model.settings_open {
        return expanded_height;
    }
    let rows = model.rows.len();
    if rows == 0 {
        return theme::COMPACT_WINDOW_HEIGHT;
    }
    // The list, plus the gap above it, added to the compact frame -- the field
    // and the footer inside both panel margins -- which
    // [`theme::COMPACT_WINDOW_HEIGHT`] already is. Nothing here re-derives the
    // field or the footer: the last thing a listed frame draws is the status
    // line, and the panel's bottom margin under it is part of the compact
    // height, so a listed window is the compact one with a list inserted.
    //
    // [`BLOCK_GAP`] is that insertion's own gap: the explicit space
    // [`draw_launcher`] puts above the list plus the item spacing egui adds
    // ahead of it. Leaving the implicit half out of the sum is how the frame
    // used to end lower than the window it was sized for.
    //
    // Saturating throughout: a result set large enough to overflow this is
    // clamped to the expanded height anyway.
    let gaps = (rows.saturating_sub(1) as f32) * theme::ROW_GAP;
    let list = (rows as f32) * theme::ROW_HEIGHT + gaps + BLOCK_GAP;
    // The action list opens between the results and the status line, so it
    // needs room of its own. Without this a single result with alternates
    // opened its actions into a window sized for the list alone and pushed the
    // status line off the bottom -- the same clipping, from the other
    // direction, and one that a long result set hides because it is clamped.
    let actions = if model.actions_open {
        let buttons = model.rows.get(model.selected).map_or(0, |row| {
            usize::from(row.default_action.is_some()) + row.alternate_actions.len()
        });
        BLOCK_GAP + actions_overlay_height(buttons)
    } else {
        0.0
    };
    let total = f32::from(u16::try_from(theme::COMPACT_WINDOW_HEIGHT).unwrap_or(u16::MAX)) + list + actions;
    (total.ceil().max(0.0) as u32).clamp(theme::COMPACT_WINDOW_HEIGHT, expanded_height)
}

/// The physical size the next frame must be built at, given the size the
/// window has now, the height that was just requested of it, and whatever
/// `Window::request_inner_size` answered.
///
/// A backend that resizes synchronously answers `Some` with the size it
/// actually gave, which is not always the size that was asked for -- a
/// minimum size or a tiling constraint can cut it down -- so the granted size
/// wins. A backend that resizes asynchronously (X11, Wayland, and Windows in
/// several situations) answers `None`: the window is about to become the
/// requested height, and the frame has to be built for the window it is
/// becoming rather than the one it is leaving. Building at the old size and
/// presenting into the new window leaves the compositor scaling one frame up
/// -- the single-frame stretch of the query field seen when results arrive --
/// until the `Resized` event lands and a correct frame replaces it.
///
/// Shrinking is susceptible in exactly the same way, with the stretch running
/// the other way: a tall frame squeezed into a short window for one frame when
/// the results are cleared. It is less noticeable but not less wrong, and the
/// requested height is the right answer in both directions.
///
/// When the requested height is the height the window already has, the `None`
/// branch returns that same size, so the caller's `resize` hits its
/// unchanged-size early return and the common frame costs nothing.
fn next_frame_size(
    current: PhysicalSize<u32>,
    target_height: u32,
    granted: Option<PhysicalSize<u32>>,
) -> PhysicalSize<u32> {
    granted.unwrap_or(PhysicalSize::new(current.width, target_height))
}

/// Works out where the window's top-left corner goes on a monitor of
/// `screen`, given the window's current `window_width` and the height it will
/// reach once it is fully expanded, both in physical pixels.
///
/// Split out from [`GraphicsState::center_on_active_monitor`] because that
/// function needs a real monitor from winit and so cannot be exercised in a
/// unit test, while the arithmetic -- and in particular its saturating
/// behaviour on a window larger than the screen -- is exactly the part that
/// can be got wrong silently.
fn centred_origin(screen: PhysicalSize<u32>, window_width: u32, expanded_height_physical: u32) -> (u32, u32) {
    // Saturating on both axes, because a window larger than the monitor must
    // land at the edge rather than wrap around to an enormous coordinate.
    let x = screen.width.saturating_sub(window_width) / 2;
    let y = screen.height.saturating_sub(expanded_height_physical) / 2;
    (x, y)
}

/// Picks the surface compositing mode that matches how the window was created.
///
/// This is deliberately *not* a judgement about how the launcher should look.
/// `transparent` is whatever [`NativeLauncherConfig`] asked for, and the same
/// flag drives `with_transparent` on the window and the renderer's transparent
/// canvas, so the surface, window and frame cannot disagree.
///
/// egui produces premultiplied colours, so a transparent window wants
/// [`CompositeAlphaMode::PreMultiplied`] and an opaque one wants
/// [`CompositeAlphaMode::Opaque`]. `Auto` — which resolves to opaque or inherit
/// against the real surface — is the fallback for both, and the first
/// advertised mode is the last resort so a backend offering neither still
/// starts.
fn preferred_alpha_mode(
    modes: &[wgpu::CompositeAlphaMode],
    transparent: bool,
) -> Option<wgpu::CompositeAlphaMode> {
    let ranked: [wgpu::CompositeAlphaMode; 2] = if transparent {
        [
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::PostMultiplied,
        ]
    } else {
        [wgpu::CompositeAlphaMode::Opaque, wgpu::CompositeAlphaMode::Auto]
    };
    for preferred in ranked {
        if modes.contains(&preferred) {
            return Some(preferred);
        }
    }
    if modes.contains(&wgpu::CompositeAlphaMode::Auto) {
        return Some(wgpu::CompositeAlphaMode::Auto);
    }
    modes.first().copied()
}

impl GraphicsState {
    fn new(
        event_loop: &ActiveEventLoop,
        config: &NativeLauncherConfig,
        proxy: Arc<EventProxy>,
    ) -> Result<Self, RendererError> {
        let attributes = Window::default_attributes()
            .with_title(config.title.clone())
            // The launcher opens on an empty query, which is the compact
            // window. Creating it at the configured height instead would show a
            // tall empty box for the frame it takes to fit itself, and would
            // place the first centring for a height nothing ever displays.
            // `config.height` is what the window expands to, not what it opens
            // at, and it is carried as `expanded_height`.
            .with_inner_size(LogicalSize::new(
                f64::from(config.width),
                f64::from(theme::COMPACT_WINDOW_HEIGHT),
            ))
            .with_min_inner_size(LogicalSize::new(
                f64::from(theme::MIN_WINDOW_WIDTH),
                f64::from(theme::MIN_WINDOW_HEIGHT),
            ))
            .with_resizable(true)
            .with_decorations(false)
            .with_transparent(config.transparent)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_visible(false);
        let window = Arc::new(event_loop.create_window(attributes)?);
        // A hidden launcher is still the process a Windows installer has to
        // replace, and it has to leave when the operating system asks it to.
        // The request cannot travel as a `UiCommand`: `dispatch_command` drops
        // commands that arrive with no active session and an idle launcher has
        // none. `NativeEvent::Exit` is where the settings surface's quit
        // control ends up too, so the host's orderly shutdown -- selection
        // history, plugin children -- is the one that already exists.
        #[cfg(target_os = "windows")]
        {
            let exit_proxy = proxy.clone();
            let _installed = crate::session_end::watch(
                window.as_ref(),
                Box::new(move || {
                    let _ = exit_proxy.send(NativeEvent::Exit);
                }),
            );
        }
        pollster::block_on(Self::initialize(
            window,
            proxy,
            config.transparent,
            config.height,
            config.present_mode,
        ))
    }

    async fn initialize(
        window: Arc<Window>,
        proxy: Arc<EventProxy>,
        transparent: bool,
        expanded_height: u32,
        present_mode: wgpu::PresentMode,
    ) -> Result<Self, RendererError> {
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(Arc::clone(&window))?;
        let preferred = wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        };
        let adapter = match instance.request_adapter(&preferred).await {
            Some(adapter) => adapter,
            None => instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    force_fallback_adapter: true,
                    ..preferred
                })
                .await
                .ok_or(RendererError::NoCompatibleAdapter)?,
        };
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("crikey launcher device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                    memory_hints: wgpu::MemoryHints::MemoryUsage,
                },
                None,
            )
            .await?;

        let error_proxy = proxy.clone();
        device.on_uncaptured_error(Box::new(move |error| {
            let _ = error_proxy.send(NativeEvent::DriverError(error.to_string()));
        }));

        let capabilities = surface.get_capabilities(&adapter);
        let format = egui_wgpu::preferred_framebuffer_format(&capabilities.formats)
            .map_err(|_| RendererError::NoSurfaceFormat)?;
        let alpha_mode = preferred_alpha_mode(&capabilities.alpha_modes, transparent)
            .ok_or(RendererError::NoSurfaceAlphaMode)?;
        // A surface that advertises only `Opaque` ignores the alpha channel
        // altogether, so a transparent canvas would be composited as solid
        // black instead of disappearing. Draw the opaque theme in that case, so
        // the window, the surface and the frame keep agreeing.
        let transparent = transparent && alpha_mode != wgpu::CompositeAlphaMode::Opaque;
        let size = window.inner_size();
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            desired_maximum_frame_latency: 1,
            present_mode,
            alpha_mode,
            view_formats: Vec::new(),
        };
        surface.configure(&device, &surface_config);

        let egui_context = create_launcher_context();
        let repaint_proxy = proxy;
        egui_context.set_request_repaint_callback(move |request| {
            if request.viewport_id == egui::ViewportId::ROOT {
                let _ = repaint_proxy.send(NativeEvent::RepaintAfter(request.delay));
            }
        });
        let egui_state = egui_winit::State::new(
            egui_context.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(device.limits().max_texture_dimension_2d as usize),
        );
        let renderer = Renderer::new(&device, format, None, 1, false);
        Ok(Self {
            window,
            transparent,
            expanded_height,
            surface,
            device,
            queue,
            surface_config,
            renderer,
            egui_context,
            egui_state,
        })
    }

    /// Positions the window so that the *expanded* launcher is centred on the
    /// monitor the user is working on.
    ///
    /// Centring happens on every show rather than once at creation: the window
    /// is summoned by a global hotkey, and between two summons the user may
    /// have moved to another monitor, changed the resolution, or docked the
    /// machine. A launcher that reappears on the display they left is a
    /// launcher they have to go looking for.
    ///
    /// The vertical origin is derived from `expanded_height` rather than from
    /// the compact height or from whatever height the window happens to be
    /// right now. The window only ever grows downwards as results arrive, so
    /// anchoring the top edge where the expanded window's top edge belongs
    /// leaves the whole surface balanced on screen for the state it spends its
    /// working life in -- showing results. The price is that the empty box sits
    /// above the middle of the screen while there is nothing to show, which the
    /// owner prefers to a full list hanging off the bottom half.
    ///
    /// Using the current height instead would be worse on both counts: it would
    /// move the query field for every result count, and the height at the
    /// moment of showing is last session's, not the one the first frame is
    /// about to ask for.
    fn center_on_active_monitor(&self) {
        // The monitor the window is on, falling back to the primary.
        // `current_monitor` answers for a hidden window too, which is the state
        // this is called in.
        let Some(monitor) = self
            .window
            .current_monitor()
            .or_else(|| self.window.primary_monitor())
        else {
            // A backend that will not name a monitor cannot be asked where the
            // middle is; leaving the window where it is beats guessing at 0,0.
            return;
        };
        let screen = monitor.size();
        if screen.width == 0 || screen.height == 0 {
            return;
        }
        let scale = self.window.scale_factor();
        let expanded = (f64::from(self.expanded_height) * scale).round() as u32;
        let width = self.window.outer_size().width;
        // No short-screen clamp is needed any more. The origin is already
        // derived from the expanded height, so `y + expanded` is at most
        // `screen.height` whenever the expanded window fits, and the saturating
        // subtraction pins `y` to 0 when it does not -- which is the lowest the
        // old clamp could ever have pushed it anyway.
        let (x, y) = centred_origin(screen, width, expanded);
        let origin = monitor.position();
        self.window.set_outer_position(PhysicalPosition::new(
            origin.x + i32::try_from(x).unwrap_or(0),
            origin.y + i32::try_from(y).unwrap_or(0),
        ));
    }

    fn show(&self) {
        // Before the window is mapped: a position set afterwards is a visible
        // jump from wherever the compositor first put it.
        self.center_on_active_monitor();
        self.window.set_visible(true);
        self.window.set_ime_allowed(true);
        self.window.focus_window();
        self.window.request_redraw();
    }

    fn hide(&self) {
        self.window.set_ime_allowed(false);
        self.window.set_visible(false);
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if self.surface_config.width == width && self.surface_config.height == height {
            return;
        }
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);
    }

    /// Resizes the window to the height the next frame needs, and answers with
    /// the physical size that frame must be built at.
    ///
    /// Called before the frame is built, so the surface, the egui layout and
    /// the window all describe the same height. The surface is reconfigured to
    /// that size here on both paths, synchronous and asynchronous, rather than
    /// only when the backend grants the resize outright: see [`next_frame_size`]
    /// for why a frame built at the outgoing size is the one-frame stretch the
    /// launcher used to show whenever results arrived.
    ///
    /// The `Resized` event still reconfigures when the real resize lands. On
    /// the asynchronous path it now arrives with the size the surface was
    /// already given, so `resize` early-returns and the event costs nothing;
    /// when the compositor lands on a different size after all, the event is
    /// still what corrects it.
    fn fit_window_height(&mut self, model: &ViewModel) -> PhysicalSize<u32> {
        let desired = desired_window_height(model, self.expanded_height);
        let scale = self.window.scale_factor();
        let current = self.window.inner_size();
        let target = ((f64::from(desired) * scale).round() as u32).max(1);
        if current.height == target {
            return current;
        }

        let granted = self
            .window
            .request_inner_size(PhysicalSize::new(current.width, target));
        let next = next_frame_size(current, target, granted);
        self.resize(next.width, next.height);
        next
    }

    fn draw(&mut self, model: &ViewModel) -> Result<DrawResult, RendererError> {
        let frame_size = self.fit_window_height(model);
        let mut input = self.egui_state.take_egui_input(self.window.as_ref());
        // `take_egui_input` reads the window's *current* inner size, which on a
        // backend that resizes asynchronously is still the outgoing one. Laying
        // the frame out at that size and presenting it into the taller window
        // is what stretched the query field for a frame, so egui is told the
        // size the surface was just configured to instead. egui works in
        // logical points, hence the division by the scale factor; the
        // `ScreenDescriptor` built below reads the same surface configuration,
        // so layout, surface and render all describe one frame.
        let points = (self.window.scale_factor() as f32).max(f32::MIN_POSITIVE);
        input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(
                frame_size.width as f32 / points,
                frame_size.height as f32 / points,
            ),
        ));
        let NativeUiFrame { output, commands } =
            build_launcher_frame_with_transparency(&self.egui_context, input, model, self.transparent);
        let egui::FullOutput {
            platform_output,
            textures_delta,
            shapes,
            pixels_per_point,
            viewport_output,
        } = output;
        let repaint_after = viewport_output
            .get(&egui::ViewportId::ROOT)
            .map_or(Duration::MAX, |output| output.repaint_delay);
        self.egui_state
            .handle_platform_output(self.window.as_ref(), platform_output);

        for (texture_id, image_delta) in &textures_delta.set {
            self.renderer
                .update_texture(&self.device, &self.queue, *texture_id, image_delta);
        }
        let paint_jobs = self.egui_context.tessellate(shapes, pixels_per_point);
        let screen = ScreenDescriptor {
            size_in_pixels: [self.surface_config.width, self.surface_config.height],
            pixels_per_point,
        };

        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.surface_config);
                for texture_id in &textures_delta.free {
                    self.renderer.free_texture(texture_id);
                }
                return Ok(DrawResult {
                    commands,
                    presented_at: None,
                    repaint_after,
                    retry: true,
                });
            }
            Err(wgpu::SurfaceError::Timeout) => {
                for texture_id in &textures_delta.free {
                    self.renderer.free_texture(texture_id);
                }
                return Ok(DrawResult {
                    commands,
                    presented_at: None,
                    repaint_after,
                    retry: true,
                });
            }
            Err(wgpu::SurfaceError::OutOfMemory) => {
                return Err(RendererError::SurfaceOutOfMemory);
            }
        };
        let suboptimal = frame.suboptimal;
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("crikey launcher encoder"),
            });
        let mut command_buffers =
            self.renderer
                .update_buffers(&self.device, &self.queue, &mut encoder, &paint_jobs, &screen);
        {
            let color = clear_color(self.transparent);
            let render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("crikey launcher render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            self.renderer
                .render(&mut render_pass.forget_lifetime(), &paint_jobs, &screen);
        }
        command_buffers.push(encoder.finish());
        self.queue.submit(command_buffers);
        frame.present();
        let presented_at = Instant::now();
        for texture_id in &textures_delta.free {
            self.renderer.free_texture(texture_id);
        }
        if suboptimal {
            self.surface.configure(&self.device, &self.surface_config);
        }

        Ok(DrawResult {
            commands,
            presented_at: Some(presented_at),
            repaint_after,
            retry: false,
        })
    }
}

struct DrawResult {
    commands: Vec<UiCommand>,
    presented_at: Option<Instant>,
    repaint_after: Duration,
    retry: bool,
}

/// Decides whether a `WindowEvent::Focused` means the user clicked away.
///
/// Three conditions have to hold together, and each one rules out a way the
/// launcher could dismiss itself:
///
/// * `focused` must be false. Gaining focus is never a dismissal.
/// * `active` must be true. A `Focused(false)` for a window that is already
///   hidden -- the one the compositor sends as the unmap takes effect -- is not
///   a click-away, and dismissing again would push a second `Dismiss` at the
///   host for a session it has already closed.
/// * `had_focus` must be true. `GraphicsState::show` focuses the window
///   programmatically, so a genuine click-away is always preceded by a
///   `Focused(true)`. Requiring it is what keeps a backend that never grants
///   focus at all, such as Xvfb without a window manager, from closing the
///   launcher the moment it opens.
fn should_dismiss_on_focus_change(focused: bool, active: bool, had_focus: bool) -> bool {
    !focused && active && had_focus
}

fn translate_keyboard(
    event: &KeyEvent,
    modifiers: ModifiersState,
    model: Option<&ViewModel>,
) -> Option<UiCommand> {
    if event.state != ElementState::Pressed {
        return None;
    }
    // While the settings surface is open its editors own the keyboard: Enter
    // commits an edit and Tab walks between rows, so neither may still run a
    // result. Escape is the exception, because closing the surface is what it
    // means there.
    if model.is_some_and(|model| model.settings_open) {
        return matches!(event.logical_key.as_ref(), Key::Named(NamedKey::Escape))
            .then_some(UiCommand::Cancel);
    }
    match event.logical_key.as_ref() {
        Key::Named(NamedKey::ArrowDown) => Some(UiCommand::SelectNext),
        Key::Named(NamedKey::ArrowUp) => Some(UiCommand::SelectPrevious),
        Key::Named(NamedKey::PageDown) => Some(UiCommand::PageDown),
        Key::Named(NamedKey::PageUp) => Some(UiCommand::PageUp),
        Key::Named(NamedKey::Tab) => Some(UiCommand::Complete),
        Key::Named(NamedKey::Escape) => Some(UiCommand::Cancel),
        Key::Named(NamedKey::Enter) if modifiers.alt_key() => Some(UiCommand::ShowActions),
        Key::Named(NamedKey::Enter) => Some(UiCommand::ExecuteDefault),
        Key::Character(text) if model.is_some_and(|model| model.actions_open) => {
            let mut characters = text.chars();
            let digit = characters.next()?.to_digit(10)?;
            if characters.next().is_some() || digit == 0 {
                return None;
            }
            let index = digit as usize - 1;
            let row = model?.rows.get(model?.selected)?;
            (index < row.alternate_actions.len()).then_some(UiCommand::ExecuteAlternate(index))
        }
        _ => None,
    }
}

fn canvas_fill(colors: theme::Palette, transparent: bool) -> egui::Color32 {
    if transparent {
        egui::Color32::TRANSPARENT
    } else {
        colors.canvas
    }
}

/// The vertical distance [`draw_launcher`] puts between two of the blocks it
/// stacks in the central panel, in logical pixels.
///
/// Two things make it up and both are drawn: the explicit
/// [`theme::SPACE_3`] the panel body asks for, and the
/// [`theme::ITEM_SPACING_Y`] egui puts ahead of the block that follows it,
/// because every allocation in a vertical layout is preceded by item spacing.
/// Reserving only the explicit gap is how [`draw_results`] used to end up
/// short of where it thought it would.
const BLOCK_GAP: f32 = theme::ITEM_SPACING_Y + theme::SPACE_3;

fn draw_launcher(
    context: &egui::Context,
    model: &ViewModel,
    commands: &mut Vec<UiCommand>,
    transparent: bool,
) {
    let colors = theme::palette();
    // Ctrl+, is what every desktop means by "open preferences". It is consumed
    // from the egui frame rather than translated out of the raw key event so
    // that the headless frame builder answers to the shortcut exactly as the
    // window does, and so the query field never also receives the comma.
    if context.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
        commands.push(UiCommand::OpenSettings);
    }
    egui::CentralPanel::default()
        .frame(
            Frame::default()
                .fill(canvas_fill(colors, transparent))
                .inner_margin(Margin::same(theme::PANEL_MARGIN)),
        )
        .show(context, |ui| {
            draw_query(ui, model, commands, colors);
            if model.settings_open {
                // The settings surface takes the result area rather than
                // floating over it: the launcher is one column, and a list the
                // user cannot reach behind a panel is only noise.
                ui.add_space(theme::SPACE_3);
                draw_settings(ui, model, commands, colors);
            } else if !model.query.is_empty() {
                // An untyped launcher is the query field and nothing else: no
                // card, no list, and none of the spacing that would hold room
                // for one.
                ui.add_space(theme::SPACE_3);
                draw_results(ui, model, commands, colors);
                if model.actions_open {
                    ui.add_space(theme::SPACE_3);
                    draw_actions(ui, model, commands, colors);
                }
            }
            ui.add_space(theme::SPACE_3);
            draw_status(ui, model, commands, colors);
        });
}

fn draw_query(ui: &mut egui::Ui, model: &ViewModel, commands: &mut Vec<UiCommand>, colors: theme::Palette) {
    // One filled pill, no border: the field is the only thing on the canvas
    // above the list, and at [`theme::TEXT_QUERY`] the text says "type here"
    // more plainly than an outline would. The fill is a whole surface tier
    // above the canvas so that the pill is still unmistakably a field.
    Frame::default()
        .fill(colors.surface)
        .rounding(Rounding::same(theme::RADIUS_MEDIUM))
        .show(ui, |ui| {
            let mut query = model.query.clone();
            // The pill has no margin of its own: the editor's own padding is
            // the field's padding, which keeps [`theme::FIELD_HEIGHT`] the
            // height of what is actually drawn.
            let response = ui.add_sized(
                [ui.available_width(), theme::FIELD_HEIGHT],
                TextEdit::singleline(&mut query)
                    .font(TextStyle::Heading)
                    .hint_text("Search apps, files, and actions")
                    .hint_text_font(TextStyle::Heading)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(theme::SPACE_3, theme::SPACE_1))
                    .frame(false)
                    .lock_focus(true),
            );
            // The query field takes the keyboard back on every frame, which is
            // right for a launcher whose only job is typing -- except while the
            // settings surface is open, where it would tear focus out of the
            // editor the user is typing into.
            if !model.settings_open {
                response.request_focus();
            }
            if response.changed() {
                flatten_line_breaks(&mut query);
                commands.push(UiCommand::SetQuery(query));
            }
        });
}

/// Collapses the line breaks a paste can smuggle into the query field.
///
/// A single-line `TextEdit` only stops the Enter key; a clipboard paste is
/// inserted verbatim, so pasting several lines leaves real line breaks in text
/// the rest of the launcher treats as one line and hands to plugins as a search
/// string. Each break becomes one space, which keeps every following character
/// at the position the text cursor already points at, and the string is only
/// rebuilt when there is something to replace.
fn flatten_line_breaks(text: &mut String) {
    if text.contains(['\n', '\r']) {
        *text = text.replace(['\n', '\r'], " ");
    }
}

/// What the renderer last scrolled the result list to, kept per context.
///
/// Without it the selected row asks to be scrolled into view on every frame,
/// which silently undoes the user's own mouse wheel on the very next repaint.
/// The anchor is renderer state rather than view-model state because it is
/// about where this list is currently scrolled, which no other renderer and no
/// host can answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollAnchor {
    /// Identity of the row set the anchor was taken from. The address of the
    /// shared allocation, because `publish` replaces the row set wholesale:
    /// a different address is a different list, and the same address is the
    /// same list however the user has scrolled it.
    rows: usize,
    selected: usize,
}

fn scroll_anchor_id() -> egui::Id {
    egui::Id::new("crikey-result-scroll-anchor")
}

fn draw_results(ui: &mut egui::Ui, model: &ViewModel, commands: &mut Vec<UiCommand>, colors: theme::Palette) {
    // Nothing below the field until there is something to put there. The card
    // that used to stand here said "Searching" or "No matches" inside a
    // full-width box, so the moment a key was pressed the launcher grew into a
    // large empty panel and stayed that way until results arrived. The status
    // line already carries both states -- a spinner while providers respond, a
    // result count once they have -- without taking the room.
    if model.rows.is_empty() {
        return;
    }

    let anchor = ScrollAnchor {
        rows: Arc::as_ptr(&model.rows).cast::<()>() as usize,
        selected: model.selected,
    };
    let previous = ui.data(|data| data.get_temp::<ScrollAnchor>(scroll_anchor_id()));
    ui.data_mut(|data| data.insert_temp(scroll_anchor_id(), anchor));

    // What the list may occupy is what is left after everything drawn under
    // it, measured rather than guessed. This used to subtract a flat
    // [`theme::SPACE_8`], which was short of the footer: a list long enough to
    // clamp the window to its expanded height pushed the "120 results" row
    // past the bottom edge, and the owner of a 1600x1000 screen saw only its
    // top few pixels.
    //
    // The central panel's bottom inner margin is already outside
    // `available_height`, so it must not be counted again here.
    //
    // The actions overlay stands between the list and the footer, so with it
    // open the list gives up the overlay and a second gap as well. Its height
    // follows the number of buttons the selected row publishes, because a row
    // with three alternates draws an overlay half again as tall as one with
    // none. [`draw_launcher`] spends the gap above the overlay whether or not
    // [`draw_actions`] finds a row to describe.
    let overlay = if model.actions_open {
        BLOCK_GAP
            + model.rows.get(model.selected).map_or(0.0, |row| {
                let buttons = usize::from(row.default_action.is_some()) + row.alternate_actions.len();
                actions_overlay_height(buttons)
            })
    } else {
        0.0
    };
    let reserved = overlay + BLOCK_GAP + STATUS_BLOCK_HEIGHT;
    let list_height = (ui.available_height() - reserved).max(theme::SPACE_8 * 3.0);
    egui::ScrollArea::vertical()
        // Vertically the list is exactly as tall as its rows, up to the cap:
        // filling the cap regardless would put empty space between the last
        // result and the status line, which is what makes a three-result search
        // look like a mostly empty window.
        .auto_shrink([false, true])
        .max_height(list_height)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            // The gap between rows is exactly ROW_GAP, which the window height
            // is computed from. egui's own item spacing would be added on top
            // of it and put the arithmetic out by a few pixels per row.
            ui.spacing_mut().item_spacing.y = 0.0;
            let mut selected_row = None;
            for (index, row) in model.rows.iter().enumerate() {
                let response = draw_result_row(ui, row, index == model.selected, commands, colors);
                if index == model.selected {
                    selected_row = Some(response);
                }
                if index + 1 != model.rows.len() {
                    ui.add_space(theme::ROW_GAP);
                }
            }

            let Some(response) = selected_row else {
                return;
            };
            // Follow the selection, never the scroll position: a wheel gesture
            // that leaves the selected row behind is exactly what the user
            // asked for, and scrolling back would undo it on the next repaint.
            // A list that was replaced under the selection is the one case
            // where nobody asked for the current scroll offset, so there the
            // row is fetched back if it landed out of sight.
            let follow = match previous {
                None => true,
                Some(previous) if previous.selected != anchor.selected => true,
                Some(previous) => {
                    previous.rows != anchor.rows && !ui.clip_rect().contains_rect(response.rect)
                }
            };
            if follow {
                response.scroll_to_me(None);
            }
        });
}

/// Draws one result row and hands back the response covering it, which is what
/// [`draw_results`] needs to decide whether the list should follow the
/// selection. The row itself never scrolls: only the list knows whether the
/// user has scrolled it since.
fn draw_result_row(
    ui: &mut egui::Ui,
    row: &ResultRow,
    selected: bool,
    commands: &mut Vec<UiCommand>,
    colors: theme::Palette,
) -> egui::Response {
    // Only the selected row is a shape at all. A fill and a border on every
    // row is what made the list read as a stack of cards, and it left the
    // selection competing with a dozen other outlines instead of being the
    // only one on screen: the rest of the rows are text on the canvas, and
    // what separates them is their own leading.
    let (fill, stroke) = if selected {
        (colors.accent_soft, Stroke::new(1.0_f32, colors.accent))
    } else {
        (egui::Color32::TRANSPARENT, Stroke::NONE)
    };
    let frame = Frame::default()
        .fill(fill)
        .stroke(stroke)
        .rounding(Rounding::same(theme::RADIUS_SMALL))
        .inner_margin(Margin::symmetric(theme::ROW_PAD_X, theme::ROW_PAD_Y))
        .show(ui, |ui| {
            // Every row is as wide as the list, not as wide as its own text.
            // An egui frame shrinks to its content, so without this the rows
            // end wherever their label happens to end and the list reads as a
            // ragged column of differently sized cards -- and the selected row,
            // which alone is stretched by the scroll area's minimum width,
            // looks like a different kind of thing from the rest.
            ui.set_min_width(ui.available_width());
            // And exactly as tall as every other row, description or not, so
            // the list is a regular column and the window can be sized from the
            // row count. The padding is outside this, hence the subtraction.
            ui.set_min_height(theme::ROW_HEIGHT - 2.0 * theme::ROW_PAD_Y);
            // Horizontally the icon stands away from the text; vertically the
            // label and the line under it are one thing, so they are set
            // tighter than anything else in the window.
            ui.spacing_mut().item_spacing = vec2(theme::SPACE_2, theme::ROW_LINE_GAP);
            ui.horizontal(|ui| {
                draw_row_icon(ui, row);
                ui.vertical(|ui| {
                    // Truncated rather than wrapped: a row that grew a second
                    // line for a long path would break the row arithmetic the
                    // window height is computed from.
                    ui.add(egui::Label::new(highlighted_label(row, colors)).truncate());
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = theme::SPACE_1;
                        ui.add(
                            egui::Label::new(
                                RichText::new(row_metadata(row))
                                    .size(theme::TEXT_SMALL)
                                    .color(colors.text_muted),
                            )
                            .truncate(),
                        );
                        if let Some(hint) = &row.argument_hint {
                            ui.label(RichText::new(hint).size(theme::TEXT_SMALL).color(colors.accent));
                        }
                        if let Some(status) = &row.status {
                            ui.label(
                                RichText::new(status)
                                    .size(theme::TEXT_SMALL)
                                    .color(colors.warning),
                            );
                        }
                    });
                });
                if selected {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if !row.alternate_actions.is_empty() && ui.button("Actions  Alt+Enter").clicked() {
                            commands.push(UiCommand::ShowActions);
                        }
                        if row.default_action.is_some() && ui.button("Run  Enter").clicked() {
                            commands.push(UiCommand::ExecuteDefault);
                        }
                    });
                }
            });
        });
    frame.response
}

/// The one muted line under a row's label: what the result is, and where it
/// came from.
///
/// One line and one galley rather than a stacked description, category and
/// plugin name. Three lines of text needed a row half again as tall as this
/// one, and rows tall enough to hold three lines are what made a five-result
/// search fill the screen.
fn row_metadata(row: &ResultRow) -> String {
    // Interpuncts, because the parts are peers: the description is the most
    // useful of them, so it leads.
    let mut line = String::with_capacity(row.description.len() + row.category.len() + row.plugin_name.len());
    for part in [
        row.description.as_str(),
        row.category.as_str(),
        row.plugin_name.as_str(),
    ] {
        if part.is_empty() {
            continue;
        }
        if !line.is_empty() {
            line.push_str("  ·  ");
        }
        line.push_str(part);
    }
    line
}

/// How many icon textures one context retains before the cache is dropped
/// whole.
///
/// A launcher session can walk past thousands of distinct icons, and a texture
/// nothing draws is still GPU memory. Dropping the whole map rather than
/// evicting one entry keeps the policy to a single branch: the next frame
/// re-uploads only the icons it actually draws, which is at most a screenful,
/// and the decoded pixels are still in the row model, so nothing is re-decoded.
const MAX_ICON_TEXTURES: usize = 256;

/// Uploaded icon textures, keyed by the content identity of the pixels.
///
/// Keyed on content rather than on the icon reference because a reference is not
/// unique to one image: the theme behind a themed name can be replaced while the
/// launcher runs, and two references routinely resolve to the same file. Keying
/// on the reference would draw the stale texture in the first case and upload
/// the same pixels twice in the second.
#[derive(Default)]
struct IconTextures {
    by_content: HashMap<u64, TextureHandle>,
}

/// `egui::TextureHandle` is not `Debug`, and the workspace requires every type
/// to be: the count is what a diagnostic wants anyway, since the handles
/// themselves are opaque ids.
impl fmt::Debug for IconTextures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IconTextures")
            .field("uploaded", &self.by_content.len())
            .finish()
    }
}

/// Draws `row`'s icon, or reserves exactly the space it would have taken.
///
/// The slot is allocated in both branches, and that is the point: a result list
/// whose rows shift sideways because one icon is missing, still loading or
/// undecodable is worse than one with no icons at all. "Absent", "not found" and
/// "failed to decode" all arrive here as `None` and are drawn identically.
fn draw_row_icon(ui: &mut egui::Ui, row: &ResultRow) {
    let slot = vec2(theme::ICON_SIZE, theme::ICON_SIZE);
    match &row.icon {
        Some(icon) => {
            let texture = icon_texture(ui.ctx(), icon);
            ui.add(egui::Image::new(SizedTexture::new(texture.id(), slot)).fit_to_exact_size(slot));
        }
        None => {
            ui.allocate_space(slot);
        }
    }
}

/// The texture for one decoded icon, uploaded on first sight and reused after.
///
/// The cache lives in the context's own frame-persistent store rather than in
/// the renderer, because [`build_launcher_frame`] is a free function over a
/// context: a headless caller gets the same uploads, and therefore the same
/// frame, that the windowed renderer produces. The handle returned here is a
/// clone; the cache holds the one that keeps the texture alive.
fn icon_texture(context: &egui::Context, icon: &crikey_platform::IconImage) -> TextureHandle {
    let cache: Arc<Mutex<IconTextures>> = context.data_mut(|data| {
        Arc::clone(data.get_temp_mut_or_default::<Arc<Mutex<IconTextures>>>(egui::Id::NULL))
    });
    let mut cache = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let content = icon.content_id();
    if let Some(handle) = cache.by_content.get(&content) {
        return handle.clone();
    }
    // Keep the existing working set stable. If a result list is one icon over
    // capacity, evicting (or clearing) would make the next frame miss nearly
    // every texture; the overflow icon is cheap to upload again and does not
    // displace screenfuls of reusable handles.
    if cache.by_content.len() >= MAX_ICON_TEXTURES {
        let image =
            ColorImage::from_rgba_unmultiplied([icon.width() as usize, icon.height() as usize], icon.rgba());
        return context.load_texture(
            format!("crikey-icon-{content:016x}"),
            image,
            TextureOptions::LINEAR,
        );
    }
    let image =
        ColorImage::from_rgba_unmultiplied([icon.width() as usize, icon.height() as usize], icon.rgba());
    // Linear filtering because the slot is smaller than the icons are requested
    // at, so every icon is being minified rather than magnified.
    let handle = context.load_texture(
        format!("crikey-icon-{content:016x}"),
        image,
        TextureOptions::LINEAR,
    );
    cache.by_content.insert(content, handle.clone());
    handle
}

fn display_label(row: &ResultRow) -> &str {
    if row.label.is_empty() {
        "(unnamed result)"
    } else {
        &row.label
    }
}

fn highlighted_label(row: &ResultRow, colors: theme::Palette) -> LayoutJob {
    let regular = TextFormat {
        font_id: FontId::new(theme::TEXT_LABEL, FontFamily::Proportional),
        color: colors.text,
        ..Default::default()
    };
    let highlighted = TextFormat {
        font_id: FontId::new(theme::TEXT_LABEL, FontFamily::Proportional),
        color: colors.accent,
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    if row.label.is_empty() {
        job.append(display_label(row), 0.0, regular);
        return job;
    }

    let mut cursor = 0;
    for &(start, end) in &row.highlights {
        if start < cursor || start >= end {
            continue;
        }
        let Some(prefix) = row.label.get(cursor..start) else {
            continue;
        };
        let Some(matched) = row.label.get(start..end) else {
            continue;
        };
        job.append(prefix, 0.0, regular.clone());
        job.append(matched, 0.0, highlighted.clone());
        cursor = end;
    }
    if let Some(remainder) = row.label.get(cursor..) {
        job.append(remainder, 0.0, regular);
    }
    job
}

/// The height of the header row [`draw_actions`] opens with, in logical
/// pixels: one line of [`theme::TEXT_LABEL`] text, which egui lays out 24 px
/// tall. There is no way to ask a style for that figure without laying the
/// text out, so it is measured instead -- see [`actions_overlay_height`].
const ACTIONS_HEADER_HEIGHT: f32 = 24.0;

/// How tall the overlay [`draw_actions`] draws is, in logical pixels, for a
/// row publishing `buttons` actions.
///
/// [`draw_results`] has to know this before the overlay is drawn, because the
/// overlay comes out of the room the result list would otherwise take. The sum
/// follows the drawing directly: the frame's [`theme::SPACE_3`] inner margin
/// at the top and bottom, the header row, the explicit [`theme::SPACE_2`]
/// under the header plus the one [`theme::ITEM_SPACING_Y`] egui adds ahead of
/// the first button -- `add_space` advances the cursor and nothing else, so
/// the implicit spacing lands after the explicit gap and not before it -- and
/// then one button per action at [`theme::CONTROL_HEIGHT`], separated by item
/// spacing.
///
/// Measured, not guessed:
/// `a_clamped_window_still_leaves_the_status_line_room_to_be_read` lays the
/// overlay out and fails if the frame it draws is not this tall, so a
/// restyled overlay cannot quietly grow over the status line again.
fn actions_overlay_height(buttons: usize) -> f32 {
    let chrome = theme::SPACE_3 * 2.0 + ACTIONS_HEADER_HEIGHT + theme::SPACE_2 + theme::ITEM_SPACING_Y;
    let stack =
        (buttons as f32) * theme::CONTROL_HEIGHT + (buttons.saturating_sub(1) as f32) * theme::ITEM_SPACING_Y;
    chrome + stack
}

fn draw_actions(ui: &mut egui::Ui, model: &ViewModel, commands: &mut Vec<UiCommand>, colors: theme::Palette) {
    let Some(row) = model.rows.get(model.selected) else {
        return;
    };
    // A sheet, not a bordered box: it is the only thing under the field while
    // it is open, so the surface tier is enough to say it stands over the
    // list. The accent outline it used to carry only shouted.
    Frame::default()
        .fill(colors.surface)
        .rounding(Rounding::same(theme::RADIUS_MEDIUM))
        .inner_margin(Margin::same(theme::SPACE_3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Actions")
                        .size(theme::TEXT_LABEL)
                        .strong()
                        .color(colors.text),
                );
                ui.label(RichText::new(display_label(row)).small().color(colors.text_muted));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new("Esc to close").small().color(colors.text_muted));
                });
            });
            ui.add_space(theme::SPACE_2);
            if let Some(action) = &row.default_action {
                if ui
                    .button(format!("Enter  {}", action.label))
                    .on_hover_text(&action.description)
                    .clicked()
                {
                    commands.push(UiCommand::ExecuteDefault);
                }
            }
            for (index, action) in row.alternate_actions.iter().enumerate() {
                if ui
                    .button(format!("{}  {}", index + 1, action.label))
                    .on_hover_text(&action.description)
                    .clicked()
                {
                    commands.push(UiCommand::ExecuteAlternate(index));
                }
            }
        });
}

/// Draws the settings surface (spec 6.3).
///
/// Every row is host-supplied, including the labels, so the renderer decides
/// nothing about the configuration and a key the host stops publishing simply
/// stops appearing. The quit control lives here because this is the one
/// surface a user goes looking for when they want the launcher to stop, and
/// until now there was nowhere to ask.
fn draw_settings(
    ui: &mut egui::Ui,
    model: &ViewModel,
    commands: &mut Vec<UiCommand>,
    colors: theme::Palette,
) {
    Frame::default()
        .fill(colors.surface)
        .rounding(Rounding::same(theme::RADIUS_MEDIUM))
        .inner_margin(Margin::same(theme::SPACE_3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Settings")
                        .size(theme::TEXT_LABEL)
                        .strong()
                        .color(colors.text),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Close  Esc").clicked() {
                        commands.push(UiCommand::CloseSettings);
                    }
                });
            });
            ui.add_space(theme::SPACE_2);
            if model.settings.is_empty() {
                ui.label(
                    RichText::new("The launcher host published no settings.")
                        .size(theme::TEXT_SMALL)
                        .color(colors.text_muted),
                );
            }
            for row in model.settings.iter() {
                draw_setting_row(ui, row, model.settings_focus.as_deref(), commands, colors);
                ui.add_space(theme::SPACE_1);
            }
            // The footer of the sheet is set apart by space rather than by a
            // rule: it is the last thing on a surface that has already ended.
            ui.add_space(theme::SPACE_2);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Enter or Save commits an edit")
                        .size(theme::TEXT_SMALL)
                        .color(colors.text_muted),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Quit CriKey").clicked() {
                        commands.push(UiCommand::Quit);
                    }
                });
            });
        });
}

/// One editable setting.
///
/// The half-typed value has to be kept somewhere between frames: the model
/// carries what the host has stored, so an editor bound straight to it would
/// lose every keystroke on the next repaint. The draft is dropped again the
/// moment the edit is committed, which is what makes the host's answer --
/// validated, normalised, or refused -- the value the row shows next.
fn draw_setting_row(
    ui: &mut egui::Ui,
    row: &SettingRow,
    focus_key: Option<&str>,
    commands: &mut Vec<UiCommand>,
    colors: theme::Palette,
) {
    let draft_id = egui::Id::new(("crikey-setting-draft", row.key.as_str()));
    let mut draft = ui
        .data(|data| data.get_temp::<String>(draft_id))
        .unwrap_or_else(|| row.value.clone());
    let mut committed = false;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(
                RichText::new(&row.label)
                    .size(theme::TEXT_BODY)
                    .color(colors.text),
            );
            ui.label(
                RichText::new(format!("{}  ({})", row.key, row.source))
                    .size(theme::TEXT_SMALL)
                    .color(colors.text_muted),
            );
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Save").clicked() {
                committed = true;
            }
            let response = ui.add(
                TextEdit::singleline(&mut draft)
                    .id(draft_id.with("editor"))
                    .desired_width(theme::SPACE_8 * 6.0),
            );
            // Focus is honoured once per request rather than on every frame:
            // repeating it would pin the keyboard to this row and leave the
            // user unable to reach any other.
            if focus_key == Some(row.key.as_str()) {
                let honoured_id = egui::Id::new("crikey-settings-honoured-focus");
                let honoured = ui.data(|data| data.get_temp::<String>(honoured_id));
                if honoured.as_deref() != Some(row.key.as_str()) {
                    response.request_focus();
                    ui.data_mut(|data| data.insert_temp(honoured_id, row.key.clone()));
                }
            }
            if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                committed = true;
            } else if response.changed() {
                ui.data_mut(|data| data.insert_temp(draft_id, draft.clone()));
            }
        });
    });
    if committed {
        commands.push(UiCommand::SetSetting {
            key: row.key.clone(),
            value: draft,
        });
        ui.data_mut(|data| data.remove::<String>(draft_id));
    }
}

/// The vertical room [`draw_status`] needs, in logical pixels: one status row,
/// which is [`theme::CONTROL_HEIGHT`] tall because the `Settings  Ctrl+,`
/// button's `interact_size` decides the row's height rather than the small
/// text beside it.
///
/// There is no rule above it any more. The footer used to open with a
/// `Separator` drawn edge to edge, which is a border across the one part of
/// the window that should recede; the [`BLOCK_GAP`] over it already says the
/// same thing.
///
/// [`draw_results`] subtracts this from the room the result list may take, so
/// it must be the room the footer actually occupies rather than an estimate of
/// it. Measured, not guessed:
/// `a_clamped_window_still_leaves_the_status_line_room_to_be_read` lays a
/// clamped frame out and fails if the status row stops matching this, which is
/// what let a 120-result list push the "120 results" line off the bottom of
/// the window.
const STATUS_BLOCK_HEIGHT: f32 = theme::CONTROL_HEIGHT;

fn draw_status(ui: &mut egui::Ui, model: &ViewModel, commands: &mut Vec<UiCommand>, colors: theme::Palette) {
    ui.horizontal(|ui| {
        if model.pending_plugins {
            ui.spinner();
            ui.label(
                RichText::new("Providers are still responding")
                    .small()
                    .color(colors.warning),
            );
        } else if !model.query.is_empty() {
            // There is nothing to count before the user has typed, and a
            // "0 results" under an empty field reads as a failed search.
            let count = model.rows.len();
            let suffix = if count == 1 { "result" } else { "results" };
            ui.label(
                RichText::new(format!("{count} {suffix}"))
                    .small()
                    .color(colors.text_muted),
            );
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // The one control that is on screen no matter what the launcher is
            // doing: without it a first-time user has no way of discovering
            // that the launcher can be configured or told to quit at all.
            if ui.button("Settings  Ctrl+,").clicked() {
                commands.push(UiCommand::OpenSettings);
            }
            ui.label(
                RichText::new("Up/Down navigate   Tab complete   Esc cancel")
                    .small()
                    .color(colors.text_muted),
            );
        });
    });
}

fn clear_color(transparent: bool) -> wgpu::Color {
    // A transparent surface is composited premultiplied, where the colour
    // channels are already scaled by alpha: at alpha zero every other channel
    // must be zero too, or the desktop showing through gets the canvas colour
    // added on top of it instead of being left alone.
    if transparent {
        return wgpu::Color::TRANSPARENT;
    }

    let [red, green, blue, alpha] = theme::palette().canvas.to_array();
    const BYTE_MAX: f64 = u8::MAX as f64;
    wgpu::Color {
        r: f64::from(red) / BYTE_MAX,
        g: f64::from(green) / BYTE_MAX,
        b: f64::from(blue) / BYTE_MAX,
        a: f64::from(alpha) / BYTE_MAX,
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod transparency_tests {
    use super::*;

    #[test]
    fn frame_builder_uses_the_selected_canvas_fill() {
        fn contains_fill(shape: &egui::Shape, fill: egui::Color32) -> bool {
            match shape {
                egui::Shape::Rect(rect) => rect.fill == fill,
                egui::Shape::Vec(shapes) => shapes.iter().any(|shape| contains_fill(shape, fill)),
                _ => false,
            }
        }

        let model = ViewModel {
            generation: crikey_core::Generation::ZERO,
            query: String::new(),
            rows: Arc::default(),
            selected: 0,
            pending_plugins: false,
            actions_open: false,
            settings_open: false,
            settings: Arc::default(),
            settings_focus: None,
        };
        let input = RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(720.0, 520.0),
            )),
            ..Default::default()
        };

        let opaque =
            build_launcher_frame_with_transparency(&create_launcher_context(), input.clone(), &model, false);
        let transparent =
            build_launcher_frame_with_transparency(&create_launcher_context(), input, &model, true);
        let colors = theme::palette();

        assert!(opaque
            .output
            .shapes
            .iter()
            .any(|clipped| contains_fill(&clipped.shape, colors.canvas)));
        assert!(transparent
            .output
            .shapes
            .iter()
            .any(|clipped| contains_fill(&clipped.shape, egui::Color32::TRANSPARENT)));
    }

    #[test]
    fn clear_color_tracks_the_selected_alpha_mode() {
        let opaque = clear_color(false);
        let canvas = theme::palette().canvas;
        assert_eq!(opaque.a, 1.0);
        assert_eq!(opaque.r, f64::from(canvas.r()) / 255.0);

        // Premultiplied compositing adds the clear colour straight onto the
        // desktop, so a transparent canvas must be zero in every channel and
        // not merely zero in alpha.
        assert_eq!(clear_color(true), wgpu::Color::TRANSPARENT);
    }

    #[test]
    fn a_pasted_line_break_cannot_reach_the_query() {
        let mut pasted = "first\r\nsecond\nthird".to_owned();
        let characters = pasted.chars().count();

        flatten_line_breaks(&mut pasted);

        assert_eq!(pasted, "first  second third");
        // One space per break keeps every later character at the offset the
        // text cursor already points at.
        assert_eq!(pasted.chars().count(), characters);

        let mut clean = "already one line".to_owned();
        flatten_line_breaks(&mut clean);
        assert_eq!(clean, "already one line");
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn a_hide_from_an_old_session_cannot_claim_a_new_activation() {
        let state = SharedState::default();
        let first = state.claim_activation().expect("first activation is claimed");
        assert!(state.claim_hide(first));

        let second = state.claim_activation().expect("second activation is claimed");
        assert!(second > first);
        assert!(!state.claim_hide(first));
        assert!(state.is_visible_session(second));
    }

    #[test]
    fn a_failed_old_toggle_cannot_restore_visibility_over_a_new_session() {
        let state = SharedState::default();
        let first = state.claim_activation().expect("first activation is claimed");
        assert!(state.claim_hide(first));

        let second = state.claim_activation().expect("second activation is claimed");
        state.restore_visible(first);

        assert!(state.is_visible_session(second));
    }

    /// A `FrameReady` that finds nothing to draw still spends the wake it
    /// carried. If the wake outlived it, `submit_frame` would keep believing a
    /// wake-up was already on its way and the loop would stop being told about
    /// new frames, while the frame itself must survive so the activation that
    /// is still on its way can pick it up.
    #[test]
    fn acknowledging_a_wake_retires_it_without_dropping_the_frame() {
        let state = SharedState::default();
        {
            let mut mailbox = lock_recover(&state.frames);
            mailbox.latest = Some(PendingFrame {
                session: 7,
                model: ViewModel {
                    generation: crikey_core::Generation::ZERO,
                    query: String::new(),
                    rows: Arc::default(),
                    selected: 0,
                    pending_plugins: false,
                    actions_open: false,
                    settings_open: false,
                    settings: Arc::default(),
                    settings_focus: None,
                },
            });
            mailbox.wake_session = Some(7);
        }

        state.acknowledge_wake(6);
        assert_eq!(lock_recover(&state.frames).wake_session, Some(7));

        state.acknowledge_wake(7);
        let mailbox = lock_recover(&state.frames);
        assert_eq!(mailbox.wake_session, None);
        assert!(mailbox.latest.is_some());
    }

    /// The defect: clicking away left the launcher on screen. Losing focus
    /// while a session is on screen and holding the keyboard is the click-away,
    /// and it has to dismiss.
    #[test]
    fn losing_focus_while_shown_dismisses_the_launcher() {
        assert!(should_dismiss_on_focus_change(false, true, true));
    }

    /// Hiding the window makes the compositor send `Focused(false)` after the
    /// session has already been closed. Acting on it would dismiss a second
    /// time, and after a quick re-activation it would close the new session.
    #[test]
    fn the_focus_loss_that_hiding_causes_is_not_a_second_dismissal() {
        assert!(!should_dismiss_on_focus_change(false, false, true));
        assert!(!should_dismiss_on_focus_change(false, true, false));
    }

    /// Xvfb without a window manager never focuses the window, so the launcher
    /// must not read the resulting focus report as the user clicking away.
    #[test]
    fn a_window_that_never_gained_focus_does_not_dismiss_itself() {
        assert!(!should_dismiss_on_focus_change(false, true, false));
    }

    /// Gaining focus is how a session starts, never a reason to end one.
    #[test]
    fn gaining_focus_is_never_a_dismissal() {
        assert!(!should_dismiss_on_focus_change(true, true, false));
        assert!(!should_dismiss_on_focus_change(true, true, true));
    }

    /// One announcement per session until it is consumed.
    ///
    /// A file search answering three times before the loop next runs must buy
    /// one wake, not three: the host drains every driver when it runs, so the
    /// extra events would each turn the loop for nothing.
    #[test]
    fn answer_wakes_coalesce_until_the_loop_consumes_one() {
        let state = SharedState::default();
        let session = state.claim_activation().expect("the session is claimed");

        assert!(state.claim_answer_wake(session), "the first answer is announced");
        assert!(
            !state.claim_answer_wake(session),
            "an answer arriving before the loop ran must not buy a second wake"
        );

        state.acknowledge_answer_wake(session);
        assert!(
            state.claim_answer_wake(session),
            "once the loop has consumed the announcement the next answer must be able to make one"
        );
    }

    /// The promise is per session, so an announcement left outstanding by a
    /// session that ended cannot silence the next one.
    #[test]
    fn an_unconsumed_answer_wake_does_not_silence_the_next_session() {
        let state = SharedState::default();
        let first = state.claim_activation().expect("the first session is claimed");
        assert!(state.claim_answer_wake(first));
        assert!(state.claim_hide(first));

        let second = state.claim_activation().expect("the second session is claimed");
        assert!(
            state.claim_answer_wake(second),
            "a new session starts owing nothing, whatever the last one left behind"
        );
        state.acknowledge_answer_wake(first);
        assert!(
            !state.claim_answer_wake(second),
            "acknowledging the old session's announcement must not retire the live one"
        );
    }
}

#[test]
fn clearing_all_frames_releases_a_queued_frame_for_shutdown() {
    let state = SharedState::default();
    {
        let mut mailbox = lock_recover(&state.frames);
        mailbox.latest = Some(PendingFrame {
            session: 1,
            model: ViewModel {
                generation: crikey_core::Generation::ZERO,
                query: String::new(),
                rows: Arc::default(),
                selected: 0,
                pending_plugins: false,
                actions_open: false,
                settings_open: false,
                settings: Arc::default(),
                settings_focus: None,
            },
        });
        mailbox.wake_session = Some(1);
    }

    state.clear_all_frames();

    let mailbox = lock_recover(&state.frames);
    assert!(mailbox.latest.is_none());
    assert!(mailbox.wake_session.is_none());
}

#[cfg(test)]
mod label_tests {
    use super::*;

    #[test]
    fn an_empty_result_label_gets_a_visible_fallback() {
        let row = ResultRow {
            item: crikey_core::ItemId("untitled".to_owned()),
            label: String::new(),
            description: String::new(),
            icon_reference: None,
            icon: None,
            category: String::new(),
            plugin_name: String::new(),
            highlights: Vec::new(),
            argument_hint: None,
            status: None,
            default_action: None,
            alternate_actions: Vec::new(),
        };

        assert_eq!(highlighted_label(&row, theme::palette()).text, "(unnamed result)");
    }

    #[test]
    fn a_highlight_range_inside_a_utf8_character_is_ignored_safely() {
        let row = ResultRow {
            item: crikey_core::ItemId("cafe".to_owned()),
            label: "café".to_owned(),
            description: String::new(),
            icon_reference: None,
            icon: None,
            category: String::new(),
            plugin_name: String::new(),
            highlights: vec![(4, 5)],
            argument_hint: None,
            status: None,
            default_action: None,
            alternate_actions: Vec::new(),
        };

        assert_eq!(highlighted_label(&row, theme::palette()).text, "café");
    }
}

#[cfg(test)]
mod surface_tests {
    use super::*;

    /// An opaque window takes an advertised `Opaque` mode no matter where the
    /// backend lists it. This is the case `.first()` got right only by luck.
    #[test]
    fn an_opaque_window_chooses_opaque_even_when_listed_late() {
        let modes = [
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::Opaque,
        ];
        assert_eq!(
            preferred_alpha_mode(&modes, false),
            Some(wgpu::CompositeAlphaMode::Opaque)
        );
    }

    /// The renderer must not impose opacity on a theme that asked for
    /// transparency: a transparent window takes a blending mode even when an
    /// opaque one is advertised first.
    #[test]
    fn a_transparent_window_is_not_forced_opaque() {
        let modes = [
            wgpu::CompositeAlphaMode::Opaque,
            wgpu::CompositeAlphaMode::PreMultiplied,
        ];
        assert_eq!(
            preferred_alpha_mode(&modes, true),
            Some(wgpu::CompositeAlphaMode::PreMultiplied)
        );
    }

    /// egui emits premultiplied colours, so that mode is preferred over
    /// postmultiplied when a transparent surface offers both.
    #[test]
    fn a_transparent_window_prefers_premultiplied() {
        let modes = [
            wgpu::CompositeAlphaMode::PostMultiplied,
            wgpu::CompositeAlphaMode::PreMultiplied,
        ];
        assert_eq!(
            preferred_alpha_mode(&modes, true),
            Some(wgpu::CompositeAlphaMode::PreMultiplied)
        );
    }

    /// `Auto` resolves against the real surface and is the shared fallback
    /// when neither preference is advertised.
    #[test]
    fn auto_is_the_fallback_for_either_intent() {
        let modes = [wgpu::CompositeAlphaMode::Auto];
        assert_eq!(
            preferred_alpha_mode(&modes, false),
            Some(wgpu::CompositeAlphaMode::Auto)
        );
        assert_eq!(
            preferred_alpha_mode(&modes, true),
            Some(wgpu::CompositeAlphaMode::Auto)
        );
    }

    /// A surface offering none of the preferred modes still starts rather than
    /// refusing to open.
    #[test]
    fn the_first_advertised_mode_is_the_last_resort() {
        let modes = [wgpu::CompositeAlphaMode::PostMultiplied];
        assert_eq!(
            preferred_alpha_mode(&modes, false),
            Some(wgpu::CompositeAlphaMode::PostMultiplied)
        );
    }

    /// An empty capability list stays an error: there is no mode to configure
    /// the surface with.
    #[test]
    fn no_advertised_mode_is_an_error_rather_than_a_guess() {
        assert_eq!(preferred_alpha_mode(&[], false), None);
        assert_eq!(preferred_alpha_mode(&[], true), None);
    }
}

#[cfg(test)]
mod window_geometry_tests {
    use super::*;

    /// A backend that resizes asynchronously -- X11, Wayland, and Windows in
    /// several situations -- answers `request_inner_size` with `None`. The
    /// frame built in that gap must already be the height the window is
    /// becoming: an old-size image presented into a new-size window is scaled
    /// up by the compositor, which is the single-frame stretch of the query
    /// field the owner reported when results arrive.
    #[test]
    fn an_asynchronous_resize_builds_the_frame_at_the_requested_height() {
        let current = PhysicalSize::new(720, 96);

        assert_eq!(next_frame_size(current, 420, None), PhysicalSize::new(720, 420));
    }

    /// Shrinking is the same artefact with the stretch running the other way:
    /// a tall image squeezed into a short window. Clearing the results back to
    /// the compact box goes through the identical path, so it gets the
    /// identical answer.
    #[test]
    fn an_asynchronous_shrink_builds_the_frame_at_the_requested_height() {
        let current = PhysicalSize::new(720, 420);

        assert_eq!(next_frame_size(current, 96, None), PhysicalSize::new(720, 96));
    }

    /// A backend that resizes synchronously answers with the size it actually
    /// gave, which is not always the size that was asked for -- a minimum size
    /// or a tiling constraint can cut it down. The granted size is what the
    /// window really is, so it wins over the request.
    #[test]
    fn a_granted_resize_wins_over_the_requested_height() {
        let current = PhysicalSize::new(720, 96);

        assert_eq!(
            next_frame_size(current, 420, Some(PhysicalSize::new(720, 300))),
            PhysicalSize::new(720, 300)
        );
    }

    /// The height the window already has asks for no resize at all, so the
    /// frame is built at the size it is: this is the case that must stay a
    /// cheap no-op through `resize`'s unchanged-size early return, every frame
    /// the launcher draws without the result count changing.
    #[test]
    fn a_height_that_is_already_current_leaves_the_frame_size_alone() {
        let current = PhysicalSize::new(720, 96);

        assert_eq!(next_frame_size(current, 96, None), current);
    }

    /// The window is centred horizontally, but vertically the origin comes from
    /// the expanded height, so that the window is centred once it is showing
    /// results rather than while it is still an empty box. Centring the compact
    /// height here instead would put the origin at 1934, which is the bug this
    /// pins.
    #[test]
    fn the_vertical_origin_leaves_room_for_the_expanded_window_below_it() {
        let screen = PhysicalSize::new(2560, 4000);
        let expanded = 1000;

        let (x, y) = centred_origin(screen, 720, expanded);

        assert_eq!(x, (2560 - 720) / 2);
        assert_eq!(y, (4000 - 1000) / 2);
        // The expanded window ends as far above the bottom edge as it starts
        // below the top one, which is what "centred upon expansion" means.
        assert_eq!(y, screen.height - (y + expanded));
    }

    /// A window wider or taller than the monitor must land at the edge. These
    /// are unsigned pixel counts, so an unguarded subtraction would wrap to
    /// roughly four billion and throw the window off the desktop entirely.
    #[test]
    fn a_window_larger_than_the_monitor_lands_at_the_origin_rather_than_wrapping() {
        let screen = PhysicalSize::new(800, 600);

        assert_eq!(centred_origin(screen, 1920, 1080), (0, 0));
        // Each axis saturates on its own: a narrow window on a short screen
        // still gets its horizontal centring.
        assert_eq!(centred_origin(screen, 400, 1080), (200, 0));
        assert_eq!(centred_origin(screen, 1920, 400), (0, 100));
    }

    fn view(query: &str, settings_open: bool) -> ViewModel {
        ViewModel {
            generation: crikey_core::Generation::ZERO,
            query: query.to_owned(),
            rows: Arc::default(),
            selected: 0,
            pending_plugins: false,
            actions_open: false,
            settings_open,
            settings: Arc::default(),
            settings_focus: None,
        }
    }

    /// A model showing `count` results, each with every optional field a row
    /// can draw: the tallest a row gets is what the row metrics must cover.
    fn view_with_rows(count: usize) -> ViewModel {
        let rows: Vec<ResultRow> = (0..count)
            .map(|index| ResultRow {
                item: crikey_core::ItemId(format!("item-{index}")),
                label: format!("Result {index}"),
                description: "A description that occupies the second line".to_owned(),
                icon_reference: None,
                icon: None,
                category: "Application".to_owned(),
                plugin_name: "builtin".to_owned(),
                highlights: Vec::new(),
                argument_hint: None,
                status: None,
                default_action: None,
                alternate_actions: Vec::new(),
            })
            .collect();
        ViewModel {
            query: "q".to_owned(),
            rows: rows.into(),
            ..view("q", false)
        }
    }

    /// A provider driver publishes from its own thread and can only describe
    /// results. If its frame were taken at face value the first suggestion to
    /// arrive would close a settings panel the user is typing into.
    #[test]
    fn a_provider_frame_inherits_the_settings_surface_the_host_last_published() {
        let mut mailbox = FrameMailbox::default();
        let mut host = view("", true);
        host.settings = vec![SettingRow {
            key: "launcher.activation-hotkey".to_owned(),
            label: "Activation hotkey".to_owned(),
            value: "Ctrl+Alt+Space".to_owned(),
            source: "default".to_owned(),
        }]
        .into();
        host.settings_focus = Some("launcher.activation-hotkey".to_owned());
        record_overlay(&mut mailbox, 7, &host);

        let published = with_overlay(&mailbox, 7, &view("term", false));
        assert!(
            published.settings_open,
            "a provider must not close the surface the user is editing"
        );
        assert_eq!(published.settings.len(), 1, "the rows survive the provider frame");
        assert_eq!(
            published.settings_focus.as_deref(),
            Some("launcher.activation-hotkey")
        );
        assert_eq!(
            published.query, "term",
            "the provider still owns the results half"
        );

        record_overlay(&mut mailbox, 7, &view("term", false));
        assert!(
            !with_overlay(&mailbox, 7, &view("term", false)).settings_open,
            "closing the surface is the host's to say, and it sticks"
        );
    }

    /// An overlay belongs to the activation it was published for. A panel left
    /// open when the launcher was dismissed must not reappear over the next
    /// activation, which has published no host frame yet.
    #[test]
    fn a_provider_frame_ignores_an_overlay_from_an_earlier_session() {
        let mut mailbox = FrameMailbox::default();
        record_overlay(&mut mailbox, 7, &view("", true));

        assert!(
            !with_overlay(&mailbox, 8, &view("term", false)).settings_open,
            "a new activation starts with no panel until its host says otherwise"
        );
    }

    #[test]
    fn the_window_is_compact_until_there_are_rows_to_show() {
        let expanded = theme::DEFAULT_WINDOW_HEIGHT;

        assert_eq!(
            desired_window_height(&view("", false), expanded),
            theme::COMPACT_WINDOW_HEIGHT,
            "an untyped launcher must not stand at list height over the desktop"
        );
        assert_eq!(
            desired_window_height(&view("q", false), expanded),
            theme::COMPACT_WINDOW_HEIGHT,
            "typing is not a result: the field must not grow an empty panel under \
             itself while the providers are still answering"
        );
        assert_eq!(
            desired_window_height(&view("", true), expanded),
            expanded,
            "the settings surface needs the room even with nothing typed"
        );
    }

    #[test]
    fn a_listed_window_is_as_tall_as_its_rows_and_no_taller() {
        let expanded = theme::DEFAULT_WINDOW_HEIGHT;

        let one = desired_window_height(&view_with_rows(1), expanded);
        let two = desired_window_height(&view_with_rows(2), expanded);
        assert_eq!(
            two - one,
            (theme::ROW_HEIGHT + theme::ROW_GAP) as u32,
            "each further result adds exactly one row and one gap"
        );
        assert!(
            one > theme::COMPACT_WINDOW_HEIGHT && one < expanded,
            "a single result neither leaves the window compact nor fills it: {one}"
        );
        assert_eq!(
            desired_window_height(&view_with_rows(500), expanded),
            expanded,
            "a long list scrolls inside the window rather than covering the screen"
        );
    }

    /// The lowest pixel a frame draws, ignoring the central panel's own
    /// background: that fills the window by definition and would report the
    /// window height back rather than the height of what was drawn in it.
    fn drawn_bottom(shape: &egui::Shape, canvas: egui::Color32) -> f32 {
        match shape {
            egui::Shape::Vec(shapes) => shapes
                .iter()
                .map(|shape| drawn_bottom(shape, canvas))
                .fold(f32::NEG_INFINITY, f32::max),
            egui::Shape::Rect(rect) if rect.fill == canvas => f32::NEG_INFINITY,
            other => {
                let bounds = other.visual_bounding_rect();
                if bounds.is_finite() {
                    bounds.max.y
                } else {
                    f32::NEG_INFINITY
                }
            }
        }
    }

    /// The frame `model` draws in a window of the launcher's width and
    /// `height`, which is how every clipping check here lays a frame out at
    /// exactly the height the launcher would have asked the window for.
    fn frame_at(model: &ViewModel, height: u32) -> NativeUiFrame {
        let input = RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(
                    f32::from(theme::DEFAULT_WINDOW_WIDTH as u16),
                    f32::from(height as u16),
                ),
            )),
            ..Default::default()
        };
        build_launcher_frame(&create_launcher_context(), input, model)
    }

    /// The lowest pixel `frame` draws.
    fn frame_bottom(frame: &NativeUiFrame) -> f32 {
        let canvas = theme::palette().canvas;
        frame
            .output
            .shapes
            .iter()
            .map(|clipped| drawn_bottom(&clipped.shape, canvas))
            .fold(f32::NEG_INFINITY, f32::max)
    }

    /// The narrowest a result row may be in a 720-wide test window, in logical
    /// pixels: wider than any control the launcher draws inside a row, so a
    /// row's own rectangle can be told apart from theirs by width alone.
    const LIST_WIDTH_FLOOR: f32 = 600.0;

    /// The window height is arithmetic over [`theme::ROW_HEIGHT`] and
    /// [`theme::ROW_GAP`], and nothing in egui enforces that the rows it draws
    /// are that size. This lays real rows out and fails if they are not: the
    /// arithmetic and the drawing must not be able to drift apart.
    #[test]
    fn every_result_row_matches_the_pinned_row_metrics() {
        // A row is the full-width small-radius rectangle its frame paints: the
        // selected row paints it in the selection fill, the rest paint it
        // transparent, so the fill cannot be what identifies one. The width is
        // what separates a row from the buttons inside it and from the
        // footer's Settings button, which share the radius.
        fn row_rects(shape: &egui::Shape, out: &mut Vec<egui::Rect>) {
            match shape {
                egui::Shape::Rect(rect)
                    if rect.rounding.nw == theme::RADIUS_SMALL && rect.rect.width() > LIST_WIDTH_FLOOR =>
                {
                    out.push(rect.rect);
                }
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| row_rects(shape, out)),
                _ => {}
            }
        }

        // Deliberately taller than the window ever is, so the list is not
        // capped and every row is laid out.
        let input = RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(720.0, 2000.0),
            )),
            ..Default::default()
        };
        let frame = build_launcher_frame(&create_launcher_context(), input.clone(), &view_with_rows(3));
        let mut rects = Vec::new();
        for clipped in &frame.output.shapes {
            row_rects(&clipped.shape, &mut rects);
        }
        rects.sort_by(|left, right| left.min.y.total_cmp(&right.min.y));

        assert_eq!(rects.len(), 3, "three results are three rows");
        for rect in &rects {
            assert_eq!(
                rect.height(),
                theme::ROW_HEIGHT,
                "a row that is not ROW_HEIGHT tall puts the window height out"
            );
        }
        // Full width: every row spans the list, so the column is not ragged.
        let width = rects[0].width();
        for rect in &rects {
            assert_eq!(rect.width(), width, "rows must all be the same width");
        }
        assert!(
            width > LIST_WIDTH_FLOOR,
            "rows must span the 720-wide window, not shrink to their text: {width}"
        );
        for pair in rects.windows(2) {
            assert_eq!(
                pair[1].min.y - pair[0].min.y,
                theme::ROW_HEIGHT + theme::ROW_GAP,
                "the gap between rows must be exactly ROW_GAP"
            );
        }

        // A row carrying less text is still a row. The window height is
        // row-count arithmetic, so a short row would silently leave the last
        // result half outside the window.
        let mut sparse = view_with_rows(2);
        let bare: Vec<ResultRow> = sparse
            .rows
            .iter()
            .map(|row| ResultRow {
                description: String::new(),
                category: String::new(),
                ..row.clone()
            })
            .collect();
        sparse.rows = bare.into();
        let frame = build_launcher_frame(&create_launcher_context(), input, &sparse);
        let mut rects = Vec::new();
        for clipped in &frame.output.shapes {
            row_rects(&clipped.shape, &mut rects);
        }
        assert_eq!(rects.len(), 2);
        for rect in &rects {
            assert_eq!(
                rect.height(),
                theme::ROW_HEIGHT,
                "a row with no description must still be ROW_HEIGHT tall"
            );
        }
    }

    /// The selection is the only shape in the list.
    ///
    /// The rows are drawn as a list rather than as a stack of cards, which
    /// means the selection cannot rely on being a slightly different card any
    /// more: it is the one row that is filled and outlined at all. Giving every
    /// row a fill back would take the selection's only affordance away, and
    /// dropping the selected row's fill would leave the user with nothing at
    /// all to look at.
    #[test]
    fn the_selected_row_is_the_only_row_the_list_paints() {
        fn row_shapes<'a>(shape: &'a egui::Shape, out: &mut Vec<&'a egui::epaint::RectShape>) {
            match shape {
                egui::Shape::Rect(rect)
                    if rect.rounding.nw == theme::RADIUS_SMALL && rect.rect.width() > LIST_WIDTH_FLOOR =>
                {
                    out.push(rect);
                }
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| row_shapes(shape, out)),
                _ => {}
            }
        }

        let mut model = view_with_rows(3);
        model.selected = 1;
        let frame = frame_at(&model, theme::DEFAULT_WINDOW_HEIGHT);
        let mut rows = Vec::new();
        for clipped in &frame.output.shapes {
            row_shapes(&clipped.shape, &mut rows);
        }
        rows.sort_by(|left, right| left.rect.min.y.total_cmp(&right.rect.min.y));
        assert_eq!(rows.len(), 3, "three results are three rows");

        let colors = theme::palette();
        for (index, row) in rows.iter().enumerate() {
            if index == model.selected {
                assert_eq!(
                    row.fill, colors.accent_soft,
                    "the selected row must be filled, or nothing on screen says which row \
                     Enter would run"
                );
                assert_eq!(row.stroke.color, colors.accent, "and outlined in the accent");
                assert!(row.stroke.width >= 1.0, "with a stroke that is actually drawn");
            } else {
                assert_eq!(
                    row.fill,
                    egui::Color32::TRANSPARENT,
                    "an unselected row paints nothing: a fill on every row is what made the \
                     list a stack of cards and left the selection competing with it"
                );
                assert_eq!(
                    row.stroke,
                    Stroke::NONE,
                    "and it is not outlined either, for the same reason"
                );
            }
        }
    }

    /// The owner of the v0.1.6 window reported seeing "the top 5% of the 'x
    /// results'": the status line is the last thing [`draw_launcher`] draws,
    /// and the window was being sized to a sum that stopped above it.
    ///
    /// Row arithmetic on its own cannot catch that -- the rows were the right
    /// size all along -- so this measures the frame instead. It lays the frame
    /// out at exactly the height [`desired_window_height`] asks for and
    /// compares the lowest pixel the frame draws against that height.
    #[test]
    fn a_listed_window_leaves_the_status_line_room_to_be_read() {
        for rows in 1_usize..=3 {
            let model = view_with_rows(rows);
            let height = desired_window_height(&model, theme::DEFAULT_WINDOW_HEIGHT);
            let bottom = frame_bottom(&frame_at(&model, height));

            let needed = bottom + theme::PANEL_MARGIN;
            assert!(
                needed <= f32::from(height as u16),
                "{rows} results draw down to {bottom}, so the window needs {needed} and \
                 was only given {height}: the status line is cut off"
            );
            assert!(
                f32::from(height as u16) - needed < theme::ROW_HEIGHT,
                "{rows} results leave {} of empty window under the status line, which is \
                 more than a whole row",
                f32::from(height as u16) - needed
            );
        }
    }

    /// The same clipping check for the window the launcher opens with.
    ///
    /// [`theme::COMPACT_WINDOW_HEIGHT`] is the one height nothing computes: it
    /// is a pinned number that has to cover the query field, the footer and
    /// both panel margins, and every other height in the launcher is that
    /// number plus a list. Shrinking the field or the footer without shrinking
    /// it leaves an empty strip under the footer; shrinking it too far cuts the
    /// footer off in the window the user sees most.
    #[test]
    fn a_compact_window_leaves_room_for_the_field_and_the_footer() {
        let model = view("", false);
        assert_eq!(
            desired_window_height(&model, theme::DEFAULT_WINDOW_HEIGHT),
            theme::COMPACT_WINDOW_HEIGHT,
            "an untyped launcher is the compact window, or this tests the wrong height"
        );

        let window = f32::from(theme::COMPACT_WINDOW_HEIGHT as u16);
        let bottom = frame_bottom(&frame_at(&model, theme::COMPACT_WINDOW_HEIGHT));
        let needed = bottom + theme::PANEL_MARGIN;
        assert!(
            needed <= window,
            "the compact frame draws down to {bottom}, so it needs {needed} once the \
             panel's bottom margin is added and the window is only {window} tall: the \
             footer is cut off"
        );
        assert!(
            window - needed < theme::ROW_GAP,
            "the compact window is {} taller than what it draws, which is a strip of \
             empty canvas under the footer",
            window - needed
        );
    }

    /// The same clipping check with the action list open.
    ///
    /// A long result set hides this: it clamps to the expanded height, which is
    /// tall enough for anything. It is the short list -- one result, its
    /// actions opened -- where the overlay has to be paid for out of the
    /// window's height, and where forgetting it pushes the status line off the
    /// bottom.
    #[test]
    fn opening_the_action_list_makes_room_for_it() {
        use crikey_core::{Action, ActionId};

        // The overlay's own height is checked by the clamped case; here it is
        // only in the way of the status line.

        for rows in 1_usize..=2 {
            let mut model = view_with_rows(rows);
            // A row with something to act on, which is what opens the overlay.
            let mut first = model.rows[0].clone();
            first.default_action = Some(Action {
                action_id: ActionId("run".to_owned()),
                label: "Run".to_owned(),
                description: String::new(),
                applicable_categories: Vec::new(),
                icon_reference: None,
                execution_policy: crikey_core::ExecutionPolicy::HostMediated,
            });
            first.alternate_actions = vec![Action {
                action_id: ActionId("reveal".to_owned()),
                label: "Reveal".to_owned(),
                description: String::new(),
                applicable_categories: Vec::new(),
                icon_reference: None,
                execution_policy: crikey_core::ExecutionPolicy::HostMediated,
            }];
            let mut all = model.rows.to_vec();
            all[0] = first;
            model.rows = all.into();
            model.actions_open = true;

            let height = desired_window_height(&model, theme::DEFAULT_WINDOW_HEIGHT);
            let bottom = frame_bottom(&frame_at(&model, height));

            let needed = bottom + theme::PANEL_MARGIN;
            assert!(
                needed <= f32::from(height as u16),
                "{rows} results with the action list open draw down to {bottom}, so the \
                 window needs {needed} and was only given {height}: the overlay or the \
                 status line is cut off"
            );
        }
    }

    /// The v0.1.6 footer was cut off again, this time only once the list was
    /// long enough to matter: at 1600x1000 with 120 matches the window filled
    /// to [`theme::DEFAULT_WINDOW_HEIGHT`] and the "120 results" line showed
    /// nothing but its top few pixels.
    ///
    /// [`a_listed_window_leaves_the_status_line_room_to_be_read`] cannot see
    /// it: a short list is sized to fit, so the frame ends where it should and
    /// only the clamped case squeezes the footer out. This lays a clamped
    /// frame out at exactly the height [`desired_window_height`] asks for and
    /// asserts on the status line itself rather than on total content, because
    /// what went wrong is that the result list took the status line's room.
    #[test]
    fn a_clamped_window_still_leaves_the_status_line_room_to_be_read() {
        fn leaves<'a>(shape: &'a egui::Shape, out: &mut Vec<&'a egui::Shape>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| leaves(shape, out)),
                other => out.push(other),
            }
        }

        fn action(label: &str) -> crikey_core::Action {
            crikey_core::Action {
                action_id: crikey_core::ActionId(label.to_owned()),
                label: label.to_owned(),
                description: "does a thing".to_owned(),
                applicable_categories: Vec::new(),
                icon_reference: None,
                execution_policy: crikey_core::ExecutionPolicy::HostMediated,
            }
        }

        // Three actions per row, so the overlay is tall enough that reserving
        // its chrome alone would not be enough either.
        const BUTTONS: usize = 3;

        for actions_open in [false, true] {
            // Far more results than the window can hold: this is the case the
            // owner reported, and the one where the list is capped rather than
            // sized to fit.
            let mut model = view_with_rows(120);
            let rows: Vec<ResultRow> = model
                .rows
                .iter()
                .map(|row| ResultRow {
                    default_action: Some(action("Launch")),
                    alternate_actions: vec![action("Open folder"), action("Copy path")],
                    ..row.clone()
                })
                .collect();
            model.rows = rows.into();
            model.actions_open = actions_open;

            let height = desired_window_height(&model, theme::DEFAULT_WINDOW_HEIGHT);
            assert_eq!(
                height,
                theme::DEFAULT_WINDOW_HEIGHT,
                "120 results must clamp the window, or this tests the wrong case"
            );
            let frame = frame_at(&model, height);
            let mut shapes = Vec::new();
            for clipped in &frame.output.shapes {
                leaves(&clipped.shape, &mut shapes);
            }
            let canvas = theme::palette().canvas;
            // Every rectangle the frame paints except the panel's own
            // background, which is the window and would answer every question
            // here with the window's own edges.
            let mut rects: Vec<egui::Rect> = shapes
                .iter()
                .filter_map(|shape| match shape {
                    egui::Shape::Rect(rect) if rect.fill != canvas => Some(rect.rect),
                    _ => None,
                })
                .collect();
            rects.sort_by(|left, right| left.min.y.total_cmp(&right.min.y));

            // The footer is the last thing the frame draws and its one
            // rectangle is the `Settings  Ctrl+,` button, whose interact_size
            // is what makes the status row as tall as it is. There is no
            // separator to find it by any more, so it is found by being last.
            // The unexpanded `rect` is the layout rect; `visual_bounding_rect`
            // would add half the stroke.
            let footer = *rects.last().expect("a listed frame paints the footer button");
            // Where a rectangle *starts*, not where it ends: the last row of a
            // clamped list is cut off by the scroll area's clip rect, so its
            // rectangle reaches under the footer while none of it is drawn
            // there.
            assert!(
                rects.iter().filter(|rect| rect.min.y >= footer.min.y).count() == 1,
                "the footer is one control, not {:?}",
                rects
                    .iter()
                    .filter(|rect| rect.min.y >= footer.min.y)
                    .collect::<Vec<_>>()
            );
            let status_bottom = footer.max.y;

            let window = f32::from(height as u16);
            let needed = status_bottom + theme::PANEL_MARGIN;
            assert!(
                needed <= window,
                "with actions_open={actions_open} the status row ends at {status_bottom}, so \
                 the window needs {needed} once the panel's bottom margin is added and it is \
                 only {window} tall: the status line is cut off"
            );
            assert!(
                window - needed < theme::ROW_HEIGHT,
                "with actions_open={actions_open} the list gave up {} px it could have shown \
                 results in, which is more than a whole row",
                window - needed
            );

            // The room [`draw_results`] gives up for the footer is only honest
            // while the footer is that tall. The button is snapped to a pixel
            // grid, hence the tolerance.
            assert!(
                (footer.height() - STATUS_BLOCK_HEIGHT).abs() <= 1.0,
                "the footer occupies {}, not the STATUS_BLOCK_HEIGHT of {} that \
                 draw_results reserves for it",
                footer.height(),
                STATUS_BLOCK_HEIGHT
            );
            if actions_open {
                // Two sheets stand on the canvas: the query field and, under
                // the list, the action overlay. Neither is outlined any more,
                // so they are found by fill and radius, and the overlay is the
                // lower of the two.
                let surface = theme::palette().surface;
                let mut sheets: Vec<egui::Rect> = shapes
                    .iter()
                    .filter_map(|shape| match shape {
                        egui::Shape::Rect(rect)
                            if rect.fill == surface && rect.rounding.nw == theme::RADIUS_MEDIUM =>
                        {
                            Some(rect.rect)
                        }
                        _ => None,
                    })
                    .collect();
                sheets.sort_by(|left, right| left.min.y.total_cmp(&right.min.y));
                assert_eq!(
                    sheets.len(),
                    2,
                    "a frame with the action list open draws the query field and the \
                     overlay, not {sheets:?}"
                );
                assert_eq!(
                    sheets[1].height(),
                    actions_overlay_height(BUTTONS),
                    "the actions overlay for {BUTTONS} actions is not the height \
                     draw_results reserves for it"
                );
            }
        }
    }
}
