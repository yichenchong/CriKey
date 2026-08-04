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
    dpi::LogicalSize,
    event::{ElementState, Ime, KeyEvent, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{Window, WindowId, WindowLevel},
};

use crate::{theme, LauncherWindow, ResultRow, UiCommand, ViewModel};

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

    /// Replaces the frame waiting for the UI thread with the newest immutable
    /// view model and wakes the loop at most once for that session.
    ///
    /// Replacing a pending frame preserves the view model's coalescing
    /// semantics; rows remain shared through their `Arc` when `model` is cloned.
    pub fn submit_frame(&self, model: &ViewModel) -> Result<(), RendererError> {
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
            mailbox.latest = Some(PendingFrame {
                session,
                model: model.clone(),
            });
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
            NativeEvent::RepaintAfter(delay) => self.schedule_repaint(delay),
            NativeEvent::DriverError(message) => {
                self.fail(event_loop, RendererError::Driver(message));
            }
            NativeEvent::Exit => {
                self.exiting = true;
                self.active_session = None;
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
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    egui_context: egui::Context,
    egui_state: egui_winit::State,
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
            .with_inner_size(LogicalSize::new(
                f64::from(config.width),
                f64::from(config.height),
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
        pollster::block_on(Self::initialize(
            window,
            proxy,
            config.transparent,
            config.present_mode,
        ))
    }

    async fn initialize(
        window: Arc<Window>,
        proxy: Arc<EventProxy>,
        transparent: bool,
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
            surface,
            device,
            queue,
            surface_config,
            renderer,
            egui_context,
            egui_state,
        })
    }

    fn show(&self) {
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

    fn draw(&mut self, model: &ViewModel) -> Result<DrawResult, RendererError> {
        let input = self.egui_state.take_egui_input(self.window.as_ref());
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

fn translate_keyboard(
    event: &KeyEvent,
    modifiers: ModifiersState,
    model: Option<&ViewModel>,
) -> Option<UiCommand> {
    if event.state != ElementState::Pressed {
        return None;
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

fn draw_launcher(
    context: &egui::Context,
    model: &ViewModel,
    commands: &mut Vec<UiCommand>,
    transparent: bool,
) {
    let colors = theme::palette();
    egui::CentralPanel::default()
        .frame(
            Frame::default()
                .fill(canvas_fill(colors, transparent))
                .inner_margin(Margin::same(theme::SPACE_4)),
        )
        .show(context, |ui| {
            draw_query(ui, model, commands, colors);
            ui.add_space(theme::SPACE_3);
            draw_results(ui, model, commands, colors);
            if model.actions_open {
                ui.add_space(theme::SPACE_3);
                draw_actions(ui, model, commands, colors);
            }
            ui.add_space(theme::SPACE_3);
            draw_status(ui, model, colors);
        });
}

fn draw_query(ui: &mut egui::Ui, model: &ViewModel, commands: &mut Vec<UiCommand>, colors: theme::Palette) {
    Frame::default()
        .fill(colors.input)
        .stroke(Stroke::new(1.0_f32, colors.border))
        .rounding(Rounding::same(theme::RADIUS_MEDIUM))
        .inner_margin(Margin::symmetric(theme::SPACE_2, theme::SPACE_1))
        .show(ui, |ui| {
            let mut query = model.query.clone();
            let response = ui.add_sized(
                [ui.available_width(), theme::SPACE_8 + theme::SPACE_2],
                TextEdit::singleline(&mut query)
                    .font(TextStyle::Heading)
                    .hint_text("Search apps, files, and actions")
                    .hint_text_font(TextStyle::Heading)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(theme::SPACE_2, theme::SPACE_1))
                    .frame(false)
                    .lock_focus(true),
            );
            response.request_focus();
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

fn draw_results(ui: &mut egui::Ui, model: &ViewModel, commands: &mut Vec<UiCommand>, colors: theme::Palette) {
    if model.rows.is_empty() {
        Frame::default()
            .fill(colors.surface)
            .stroke(Stroke::new(1.0_f32, colors.border))
            .rounding(Rounding::same(theme::RADIUS_MEDIUM))
            .inner_margin(Margin::same(theme::SPACE_6))
            .show(ui, |ui| {
                let (title, detail) = if model.pending_plugins {
                    ("Searching", "Results will appear as providers respond.")
                } else if model.query.is_empty() {
                    ("Ready", "Type a name, path, or action to begin.")
                } else {
                    ("No matches", "Try fewer words or a different spelling.")
                };
                ui.label(RichText::new(title).size(theme::TEXT_LABEL).strong());
                ui.add_space(theme::SPACE_1);
                ui.label(RichText::new(detail).small().color(colors.text_muted));
            });
        return;
    }

    let reserved = if model.actions_open {
        theme::SPACE_8 * 4.0
    } else {
        theme::SPACE_8
    };
    let list_height = (ui.available_height() - reserved).max(theme::SPACE_8 * 3.0);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height(list_height)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            for (index, row) in model.rows.iter().enumerate() {
                draw_result_row(ui, row, index == model.selected, commands, colors);
                if index + 1 != model.rows.len() {
                    ui.add_space(theme::SPACE_1);
                }
            }
        });
}

fn draw_result_row(
    ui: &mut egui::Ui,
    row: &ResultRow,
    selected: bool,
    commands: &mut Vec<UiCommand>,
    colors: theme::Palette,
) {
    let fill = if selected {
        colors.accent_soft
    } else {
        colors.surface
    };
    let stroke = if selected {
        Stroke::new(1.0_f32, colors.accent)
    } else {
        Stroke::new(1.0_f32, colors.border)
    };
    let frame = Frame::default()
        .fill(fill)
        .stroke(stroke)
        .rounding(Rounding::same(theme::RADIUS_SMALL))
        .inner_margin(Margin::symmetric(theme::SPACE_3, theme::SPACE_2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                draw_row_icon(ui, row);
                ui.vertical(|ui| {
                    ui.add(egui::Label::new(highlighted_label(row, colors)));
                    if !row.description.is_empty() {
                        ui.label(
                            RichText::new(&row.description)
                                .size(theme::TEXT_SMALL)
                                .color(colors.text_muted),
                        );
                    }
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(&row.category)
                                .size(theme::TEXT_SMALL)
                                .color(colors.text_muted),
                        );
                        if !row.plugin_name.is_empty() {
                            ui.label(
                                RichText::new("/")
                                    .size(theme::TEXT_SMALL)
                                    .color(colors.text_muted),
                            );
                            ui.label(
                                RichText::new(&row.plugin_name)
                                    .size(theme::TEXT_SMALL)
                                    .color(colors.text_muted),
                            );
                        }
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
    if selected {
        // Keyboard navigation can walk the selection past the bottom of the
        // visible list; `None` scrolls the least amount that brings the row
        // back into view and does nothing at all once it is already visible.
        frame.response.scroll_to_me(None);
    }
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

fn draw_actions(ui: &mut egui::Ui, model: &ViewModel, commands: &mut Vec<UiCommand>, colors: theme::Palette) {
    let Some(row) = model.rows.get(model.selected) else {
        return;
    };
    Frame::default()
        .fill(colors.surface)
        .stroke(Stroke::new(1.0_f32, colors.accent))
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

fn draw_status(ui: &mut egui::Ui, model: &ViewModel, colors: theme::Palette) {
    ui.separator();
    ui.horizontal(|ui| {
        if model.pending_plugins {
            ui.spinner();
            ui.label(
                RichText::new("Providers are still responding")
                    .small()
                    .color(colors.warning),
            );
        } else {
            let count = model.rows.len();
            let suffix = if count == 1 { "result" } else { "results" };
            ui.label(
                RichText::new(format!("{count} {suffix}"))
                    .small()
                    .color(colors.text_muted),
            );
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
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
