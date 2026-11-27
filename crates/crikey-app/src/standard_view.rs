//! Exact physical-producer route authority; navigation and retention stay in P3.
use crate::plugin_action::{ActionEffect, ActionRequestId, PluginActionCompletion};
use crate::preview_action_budget::{ForegroundActionAdmission, PreviewActionGate};
use crikey_core::structured_arguments::{ArgumentSchema, ArgumentValues, ExecutableKey};
use crikey_core::structured_view::{
    RouteAdmission, RouteIdentity, ViewExecutionContext, ViewReply, ViewRequest, ViewRequestMode,
    ViewSessionIdentity,
};
use crikey_core::{CoreError, Item, ItemId, PluginId, Result};
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) enum ForegroundRequest<T> {
    Action(T),
    View(QueuedView),
}

#[derive(Debug)]
struct ActiveRoute {
    owner: PluginId,
    producer: u64,
    session: ViewSessionIdentity,
    reply: Option<ViewReply>,
    pending: Option<Arc<AtomicBool>>,
    published_from: Option<(ViewExecutionContext, ActionRequestId)>,
}

/// Contains only the currently admitted descriptor, never editor values/drafts.
#[derive(Debug, Default)]
pub(crate) struct ViewAuthority {
    active: Option<ActiveRoute>,
}

fn unavailable() -> CoreError {
    CoreError::Invalid("standard view is unavailable".into())
}
impl ViewAuthority {
    pub(crate) fn owns_source(&self, owner: &PluginId, producer: u64) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| &active.owner == owner && active.producer == producer)
    }
    pub(crate) fn retain_source(&mut self, source: impl FnOnce(&PluginId) -> Option<u64>) {
        if let Some(active) = &self.active {
            if source(&active.owner) != Some(active.producer) {
                let owner = active.owner.clone();
                let session = active.session;
                self.retire(&owner, session);
            }
        }
    }
    pub(crate) fn validate_link(
        &self,
        owner: &PluginId,
        context: &ViewExecutionContext,
        state: &crikey_core::structured_view::HostViewState,
        producer: u64,
        url: &str,
    ) -> Result<()> {
        if !self.current(owner, context, producer) {
            return Err(unavailable());
        }
        let descriptor = &self
            .active
            .as_ref()
            .and_then(|a| a.reply.as_ref())
            .ok_or_else(unavailable)?
            .descriptor;
        let admission = RouteAdmission::worker(
            RouteIdentity {
                owner: owner.clone(),
                route: context.route.clone(),
                incarnation: NonZeroU64::new(producer).ok_or_else(unavailable)?,
            },
            context.surface(),
        )
        .map_err(|_| unavailable())?;
        let event = crikey_core::structured_view::ViewEvent {
            route: admission.identity().clone(),
            surface: context.surface(),
            kind: crikey_core::structured_view::ViewEventKind::OpenLink(url.to_owned()),
        };
        descriptor
            .validate_event(&admission, state, &event)
            .map_err(|_| unavailable())
    }
    pub(crate) fn retire(&mut self, owner: &PluginId, session: ViewSessionIdentity) {
        if self
            .active
            .as_ref()
            .is_some_and(|a| &a.owner == owner && a.session == session)
        {
            if let Some(active) = self.active.take() {
                if let Some(cancelled) = active.pending {
                    cancelled.store(true, Ordering::Release);
                }
            }
        }
    }
    pub(crate) fn current(&self, owner: &PluginId, context: &ViewExecutionContext, producer: u64) -> bool {
        self.active.as_ref().is_some_and(|a| {
            &a.owner == owner
                && a.producer == producer
                && a.reply.as_ref().is_some_and(|r| &r.context == context)
        })
    }
    /// Presentation grace is tied to the exact refresh whose completion the UI
    /// has not drained. It never grants execution or link authority.
    pub(crate) fn presentation_available(
        &self,
        owner: &PluginId,
        context: &ViewExecutionContext,
        producer: u64,
        pending: Option<&ActionRequestId>,
    ) -> bool {
        self.current(owner, context, producer)
            || self.active.as_ref().is_some_and(|active| {
                &active.owner == owner
                    && active.producer == producer
                    && active.session == context.session
                    && active
                        .published_from
                        .as_ref()
                        .is_some_and(|(origin, request)| origin == context && Some(request) == pending)
            })
    }
    fn begin(
        &mut self,
        owner: &PluginId,
        producer: u64,
        request: &ViewRequest,
        cancelled: Arc<AtomicBool>,
    ) -> Result<()> {
        request.validate().map_err(|_| unavailable())?;
        if producer == 0 {
            return Err(unavailable());
        }
        match request.mode {
            ViewRequestMode::Open => {
                // A fresh physical surface is mandatory even when reopening the same route.
                if self.active.as_ref().is_some_and(|a| a.session == request.session) {
                    return Err(unavailable());
                }
                if let Some(old) = self.active.take() {
                    if let Some(cancelled) = old.pending {
                        cancelled.store(true, Ordering::Release);
                    }
                }
                self.active = Some(ActiveRoute {
                    owner: owner.clone(),
                    producer,
                    session: request.session,
                    reply: None,
                    pending: Some(cancelled),
                    published_from: None,
                });
            }
            ViewRequestMode::Refresh => {
                let active = self.active.as_mut().ok_or_else(unavailable)?;
                if &active.owner != owner
                    || active.producer != producer
                    || active.session != request.session
                    || active.pending.is_some()
                    || !active.reply.as_ref().is_some_and(|r| {
                        r.context.route == request.route
                            && Some(r.context.revision) == request.previous_revision
                    })
                {
                    return Err(unavailable());
                }
                active.pending = Some(cancelled);
                active.published_from = None;
            }
        }
        Ok(())
    }
    pub(crate) fn request_current(&self, queued: &QueuedView, request: &ViewRequest) -> bool {
        self.active.as_ref().is_some_and(|a| {
            a.owner == queued.owner
                && a.producer == queued.producer
                && a.session == request.session
                && a.pending
                    .as_ref()
                    .is_some_and(|c| Arc::ptr_eq(c, &queued.cancelled))
        }) && !queued.cancelled.load(Ordering::Acquire)
            && Instant::now() < queued.deadline
    }
    pub(crate) fn publish(
        &mut self,
        queued: &QueuedView,
        request: &ViewRequest,
        reply: ViewReply,
    ) -> Result<ActionEffect> {
        if !self.request_current(queued, request) {
            return Err(unavailable());
        }
        let admission = RouteAdmission::worker(
            RouteIdentity {
                owner: queued.owner.clone(),
                route: request.route.clone(),
                incarnation: NonZeroU64::new(queued.producer).ok_or_else(unavailable)?,
            },
            reply.context.surface(),
        )
        .map_err(|_| unavailable())?;
        if reply
            .validate(request, reply.context.revision, &admission)
            .is_err()
        {
            self.retire(&queued.owner, request.session);
            return Err(unavailable());
        }
        let active = self.active.as_mut().ok_or_else(unavailable)?;
        let previous = active.reply.replace(reply.clone());
        active.published_from = if request.mode == ViewRequestMode::Refresh {
            previous.map(|previous| (previous.context, queued.request_id.clone()))
        } else {
            None
        };
        active.pending = None;
        Ok(ActionEffect::View(reply))
    }
    fn validate_execution(
        &self,
        owner: &PluginId,
        context: &ViewExecutionContext,
        state: &crikey_core::structured_view::HostViewState,
        executable: &ExecutableKey,
        producer: u64,
        typed: bool,
    ) -> Result<()> {
        if !self.current(owner, context, producer) || (typed && state.input.as_ref() != Some(executable)) {
            return Err(unavailable());
        }
        let descriptor = &self
            .active
            .as_ref()
            .and_then(|a| a.reply.as_ref())
            .ok_or_else(unavailable)?
            .descriptor;
        let admission = RouteAdmission::worker(
            RouteIdentity {
                owner: owner.clone(),
                route: context.route.clone(),
                incarnation: NonZeroU64::new(producer).ok_or_else(unavailable)?,
            },
            context.surface(),
        )
        .map_err(|_| unavailable())?;
        let event = crikey_core::structured_view::ViewEvent {
            route: admission.identity().clone(),
            surface: context.surface(),
            kind: crikey_core::structured_view::ViewEventKind::Activate(executable.clone()),
        };
        descriptor
            .validate_event(&admission, state, &event)
            .map_err(|_| unavailable())
    }
    fn executable(
        &self,
        owner: &PluginId,
        context: &ViewExecutionContext,
        state: &crikey_core::structured_view::HostViewState,
        executable: &ExecutableKey,
        producer: u64,
        typed: bool,
    ) -> Result<(Item, Option<Arc<ArgumentSchema>>)> {
        self.validate_execution(owner, context, state, executable, producer, typed)?;
        if &executable.target.owner != owner {
            return Err(unavailable());
        }
        let descriptor = &self
            .active
            .as_ref()
            .and_then(|a| a.reply.as_ref())
            .ok_or_else(unavailable)?
            .descriptor;
        let (policy, presentation) = descriptor.executable(executable).ok_or_else(unavailable)?;
        if policy != crikey_core::ExecutionPolicy::Plugin {
            return Err(unavailable());
        }
        let item = descriptor
            .definition(&executable.target)
            .ok_or_else(unavailable)?
            .item
            .clone();
        Ok((item, presentation.schema.clone()))
    }
}

#[derive(Debug)]
pub(crate) enum ViewOperation {
    Request(ViewRequest),
    Execute {
        context: ViewExecutionContext,
        state: crikey_core::structured_view::HostViewState,
        executable: ExecutableKey,
        item: Item,
        schema: Option<Arc<ArgumentSchema>>,
        values: Option<ArgumentValues>,
        argument: Option<String>,
    },
}
#[derive(Debug)]
pub(crate) struct QueuedView {
    pub request_id: ActionRequestId,
    pub owner: PluginId,
    pub producer: u64,
    pub operation: ViewOperation,
    pub authority: Arc<Mutex<ViewAuthority>>,
    pub admission: Option<ForegroundActionAdmission>,
    pub cancelled: Arc<AtomicBool>,
    pub deadline: Instant,
    #[cfg(test)]
    pub before_send: Option<(
        std::sync::mpsc::SyncSender<()>,
        Mutex<std::sync::mpsc::Receiver<()>>,
    )>,
}
impl QueuedView {
    pub(crate) fn deliver(&self, send: &mut dyn FnMut() -> bool) -> bool {
        #[cfg(test)]
        if let Some((ready, release)) = &self.before_send {
            ready.send(()).expect("writer barrier receiver");
            release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(60))
                .expect("writer barrier release");
        }
        if self.cancelled.load(Ordering::Acquire) || Instant::now() >= self.deadline {
            return false;
        }
        let authority = self.authority.lock().unwrap_or_else(|e| e.into_inner());
        let current = match &self.operation {
            ViewOperation::Request(request) => authority.request_current(self, request),
            ViewOperation::Execute {
                context,
                state,
                executable,
                schema,
                ..
            } => authority
                .validate_execution(
                    &self.owner,
                    context,
                    state,
                    executable,
                    self.producer,
                    schema.is_some(),
                )
                .is_ok(),
        };
        current && !self.cancelled.load(Ordering::Acquire) && Instant::now() < self.deadline && send()
    }
    pub(crate) fn current(&self) -> bool {
        if self.cancelled.load(Ordering::Acquire) || Instant::now() >= self.deadline {
            return false;
        }
        let authority = self.authority.lock().unwrap_or_else(|e| e.into_inner());
        match &self.operation {
            ViewOperation::Request(request) => authority.request_current(self, request),
            ViewOperation::Execute { context, .. } => authority.current(&self.owner, context, self.producer),
        }
    }
    pub(crate) fn finish(&self, result: Result<ActionEffect>) -> Result<ActionEffect> {
        let valid = match &result {
            Ok(ActionEffect::View(reply)) => {
                !self.cancelled.load(Ordering::Acquire)
                    && Instant::now() < self.deadline
                    && self.authority.lock().unwrap_or_else(|e| e.into_inner()).current(
                        &self.owner,
                        &reply.context,
                        self.producer,
                    )
            }
            _ => self.current(),
        };
        if !valid || result.is_err() {
            if let ViewOperation::Request(request) = &self.operation {
                self.authority
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retire(&self.owner, request.session);
            }
        }
        if valid {
            result
        } else {
            Err(CoreError::Cancelled)
        }
    }
    pub(crate) fn completion(&self, outcome: Result<ActionEffect>) -> PluginActionCompletion {
        let (item_id, action_id) = match &self.operation {
            ViewOperation::Request(request) => (ItemId(request.route.0.clone()), None),
            ViewOperation::Execute { executable, .. } => {
                (executable.target.item.clone(), executable.action.clone())
            }
        };
        PluginActionCompletion {
            request_id: self.request_id.clone(),
            plugin: self.owner.clone(),
            item_id,
            action_id,
            outcome,
        }
    }
}

pub(crate) struct ViewEventContext<'a> {
    pub context: &'a ViewExecutionContext,
    pub state: &'a crikey_core::structured_view::HostViewState,
}

pub(crate) fn execution(
    authority: &Mutex<ViewAuthority>,
    owner: &PluginId,
    producer: u64,
    event: ViewEventContext<'_>,
    executable: &ExecutableKey,
    values: Option<ArgumentValues>,
    argument: Option<&str>,
) -> Result<ViewOperation> {
    let ViewEventContext { context, state } = event;
    let (item, schema) = authority.lock().unwrap_or_else(|e| e.into_inner()).executable(
        owner,
        context,
        state,
        executable,
        producer,
        values.is_some(),
    )?;
    match (&schema, &values) {
        (Some(schema), Some(values)) if argument.is_none() => {
            schema.validate_values(values).map_err(|_| unavailable())?
        }
        (None, None) => {
            if let Some(action) = &executable.action {
                crate::plugin_action::validate_action_item(&item, action, argument)?;
            } else if matches!(
                (item.argument_policy, argument),
                (crikey_core::ArgumentPolicy::Forbidden, Some(_))
                    | (crikey_core::ArgumentPolicy::Required, None | Some(""))
            ) {
                return Err(unavailable());
            }
        }
        _ => return Err(unavailable()),
    }
    Ok(ViewOperation::Execute {
        context: context.clone(),
        state: state.clone(),
        executable: executable.clone(),
        item,
        schema,
        values,
        argument: argument.map(str::to_owned),
    })
}

/// Shares the existing foreground mailbox, physical slots, lease and terminal drain.
#[allow(clippy::too_many_arguments)]
pub(crate) fn enqueue<T>(
    sender: &SyncSender<ForegroundRequest<T>>,
    gates: &BTreeMap<PluginId, Arc<PreviewActionGate>>,
    pending: &Mutex<BTreeMap<ActionRequestId, Arc<AtomicBool>>>,
    in_flight: &AtomicUsize,
    next_id: &AtomicU64,
    authority: &Arc<Mutex<ViewAuthority>>,
    owner: &PluginId,
    producer: u64,
    operation: ViewOperation,
    capacity: usize,
    timeout_ms: u64,
) -> Result<ActionRequestId> {
    let gate = gates.get(owner).ok_or_else(unavailable)?;
    if in_flight
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < capacity).then_some(n + 1)
        })
        .is_err()
    {
        return Err(unavailable());
    }
    let admission = match gate.reserve_foreground() {
        Ok(v) => v,
        Err(e) => {
            in_flight.fetch_sub(1, Ordering::AcqRel);
            return Err(e);
        }
    };
    let sequence = next_id
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            Some(n.wrapping_add(1).max(1))
        })
        .unwrap_or(1);
    let request_id = ActionRequestId {
        plugin: owner.clone(),
        sequence,
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    if let ViewOperation::Request(request) = &operation {
        if let Err(error) = authority.lock().unwrap_or_else(|e| e.into_inner()).begin(
            owner,
            producer,
            request,
            Arc::clone(&cancelled),
        ) {
            in_flight.fetch_sub(1, Ordering::AcqRel);
            return Err(error);
        }
    }
    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(request_id.clone(), Arc::clone(&cancelled));
    let queued = QueuedView {
        request_id: request_id.clone(),
        owner: owner.clone(),
        producer,
        operation,
        authority: Arc::clone(authority),
        admission: Some(admission),
        cancelled,
        deadline: Instant::now() + Duration::from_millis(timeout_ms),
        #[cfg(test)]
        before_send: None,
    };
    if let Err(error) = sender.try_send(ForegroundRequest::View(queued)) {
        let queued = match error {
            std::sync::mpsc::TrySendError::Full(v) | std::sync::mpsc::TrySendError::Disconnected(v) => v,
        };
        if let ForegroundRequest::View(queued) = queued {
            if let ViewOperation::Request(request) = queued.operation {
                authority
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retire(owner, request.session);
            }
        }
        pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&request_id);
        in_flight.fetch_sub(1, Ordering::AcqRel);
        return Err(unavailable());
    }
    Ok(request_id)
}

pub(crate) fn typed_effect(
    outcome: crikey_core::structured_view::TypedExecutionOutcome,
    producer: u64,
) -> ActionEffect {
    match outcome {
        crikey_core::structured_view::TypedExecutionOutcome::Submission(outcome) => {
            ActionEffect::Typed(outcome)
        }
        crikey_core::structured_view::TypedExecutionOutcome::Navigate(outcome) => {
            ActionEffect::Navigate { outcome, producer }
        }
    }
}

pub(crate) fn with_cancellation<T>(
    queued: &QueuedView,
    cancel: impl Fn() + Send,
    call: impl FnOnce() -> T,
) -> T {
    let finished = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let finished_ref = &finished;
        let watcher = scope.spawn(move || {
            while !finished_ref.load(Ordering::Acquire) {
                if !queued.current() {
                    cancel();
                    break;
                }
                std::thread::park_timeout(Duration::from_millis(5));
            }
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(call));
        finished.store(true, Ordering::Release);
        watcher.thread().unpark();
        let _ = watcher.join();
        match result {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

#[cfg(test)]
pub(crate) struct ActionReceiver<T>(pub std::sync::mpsc::Receiver<ForegroundRequest<T>>);
#[cfg(test)]
impl<T> ActionReceiver<T> {
    pub(crate) fn try_recv(&self) -> std::result::Result<T, std::sync::mpsc::TryRecvError> {
        self.0.try_recv().map(|request| match request {
            ForegroundRequest::Action(request) => request,
            ForegroundRequest::View(_) => panic!("expected an ordinary action"),
        })
    }
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    use crate::{App, PluginActionExecutor, PluginActionRouter, SearchService};
    use crikey_core::structured_view::*;
    use crikey_ui::LauncherViewModel;

    // Only the transport is absent: admission, publication, source retention,
    // routing and the launcher's retirement consumer use their real authorities.
    struct AuthorityEndpoint {
        authority: Arc<Mutex<ViewAuthority>>,
        producer: AtomicU64,
    }
    impl PluginActionExecutor for AuthorityEndpoint {
        fn submit_plugin_action(
            &self,
            _: &PluginId,
            _: &Item,
            _: &crikey_core::ActionId,
            _: Option<&str>,
        ) -> Result<ActionRequestId> {
            panic!("this test must not submit an unrelated root action")
        }
        fn retire_view(&self, owner: &PluginId, session: ViewSessionIdentity) {
            self.authority.lock().unwrap().retire(owner, session);
        }
        fn view_presentation_available(
            &self,
            owner: &PluginId,
            context: &ViewExecutionContext,
            producer: u64,
            pending: Option<&ActionRequestId>,
        ) -> bool {
            let source = self.producer.load(Ordering::Acquire);
            let mut authority = self.authority.lock().unwrap();
            authority.retain_source(|_| (source != 0).then_some(source));
            source == producer && authority.presentation_available(owner, context, producer, pending)
        }
    }

    #[test]
    fn publication_before_completion_drain_retains_only_exact_refresh_presentation() {
        for failure in [
            "none",
            "untracked",
            "request",
            "owner",
            "producer",
            "route",
            "revision",
            "logical-view",
            "surface",
            "retired-session",
            "dead-producer",
            "replaced-producer",
            "replaced-session",
        ] {
            let owner = PluginId("publication".into());
            let producer = 7;
            let authority = Arc::new(Mutex::new(ViewAuthority::default()));
            let endpoint = Arc::new(AuthorityEndpoint {
                authority: authority.clone(),
                producer: AtomicU64::new(producer),
            });
            let mut router = PluginActionRouter::default();
            router.register([owner.clone()], endpoint.clone()).unwrap();
            let mut search = SearchService::new(App::new());
            search.set_plugin_action_router(Arc::new(router));
            let mut model = LauncherViewModel::new();
            model.activate();
            let open = model
                .open_standard_route(
                    owner.clone(),
                    producer,
                    StructuredRouteOutcome::Push(RouteId("list".into())),
                )
                .unwrap();
            let descriptor = Arc::new(ViewDescriptor {
                title: "Publication".into(),
                definitions: vec![].into(),
                view: StandardView::List(CollectionView {
                    items: vec![].into(),
                    sections: vec![],
                    scopes: vec![],
                    initial_selection: None,
                    initial_scope: None,
                }),
            });
            let publish = |request: ViewRequest, revision: u64| {
                let cancelled = Arc::new(AtomicBool::new(false));
                authority
                    .lock()
                    .unwrap()
                    .begin(&owner, producer, &request, cancelled.clone())
                    .unwrap();
                let reply = ViewReply {
                    context: ViewExecutionContext {
                        route: request.route.clone(),
                        session: request.session,
                        revision: NonZeroU64::new(revision).unwrap(),
                    },
                    descriptor: descriptor.clone(),
                };
                let queued = QueuedView {
                    request_id: ActionRequestId {
                        plugin: owner.clone(),
                        sequence: revision,
                    },
                    owner: owner.clone(),
                    producer,
                    operation: ViewOperation::Request(request),
                    authority: authority.clone(),
                    admission: None,
                    cancelled,
                    deadline: Instant::now() + Duration::from_secs(60),
                    before_send: None,
                };
                let ViewOperation::Request(request) = &queued.operation else {
                    unreachable!()
                };
                let effect = authority
                    .lock()
                    .unwrap()
                    .publish(&queued, request, reply)
                    .unwrap();
                queued.completion(queued.finish(Ok(effect)))
            };
            let seed = publish(open, 9);
            let Ok(ActionEffect::View(old)) = seed.outcome else {
                panic!("Open failed")
            };
            assert!(model.publish_standard_view(owner.clone(), producer, old.clone()));
            let refresh = model
                .standard_refresh_request(ViewRefreshReason::Selection)
                .unwrap();
            model.set_standard_refresh_pending(true);
            // Deliberately publish authority before the UI drains the completion.
            // No thread timing, sleeping or fabricated authority advancement.
            let completion = publish(refresh, 10);
            let request_id = completion.request_id.clone();
            let mut context = old.context.clone();
            let mut pending = Some(request_id.clone());
            let mut presented_owner = owner.clone();
            let mut presented_producer = producer;
            match failure {
                "none" => {}
                "untracked" => pending = None,
                "request" => pending.as_mut().unwrap().sequence += 1,
                "owner" => presented_owner = PluginId("other".into()),
                "producer" => presented_producer += 1,
                "route" => context.route = RouteId("other".into()),
                "revision" => context.revision = NonZeroU64::new(8).unwrap(),
                "logical-view" => context.session.logical_view = NonZeroU64::new(99).unwrap(),
                "surface" => context.session.surface = NonZeroU64::new(99).unwrap(),
                "retired-session" => search.retire_view(&owner, context.session),
                "dead-producer" => endpoint.producer.store(0, Ordering::Release),
                "replaced-producer" => endpoint.producer.store(producer + 1, Ordering::Release),
                "replaced-session" => {
                    let replacement = ViewRequest {
                        mode: ViewRequestMode::Open,
                        route: context.route.clone(),
                        session: ViewSessionIdentity {
                            logical_view: NonZeroU64::new(99).unwrap(),
                            surface: NonZeroU64::new(99).unwrap(),
                        },
                        previous_revision: None,
                        state: None,
                        query: String::new(),
                        scroll: None,
                        reason: ViewRefreshReason::Explicit,
                    };
                    authority
                        .lock()
                        .unwrap()
                        .begin(&owner, producer, &replacement, Arc::new(AtomicBool::new(false)))
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let retired = search.retire_unavailable_view(
                &presented_owner,
                &context,
                presented_producer,
                pending.as_ref(),
            );
            assert_eq!(retired, failure != "none", "{failure}");
            if retired {
                model.standard_unavailable();
                assert!(model.standard_surface().is_none(), "{failure}");
                continue;
            }
            assert!(model.standard_surface().is_some());
            let state = HostViewState {
                selected: None,
                scope: None,
                input: None,
            };
            let executable = ExecutableKey {
                target: crikey_core::structured_arguments::OwnedItem {
                    owner: owner.clone(),
                    item: ItemId("absent".into()),
                },
                action: None,
            };
            let locked = authority.lock().unwrap();
            assert!(!locked.current(&owner, &old.context, producer));
            assert!(locked
                .validate_execution(&owner, &old.context, &state, &executable, producer, false,)
                .is_err());
            assert!(locked
                .validate_link(&owner, &old.context, &state, producer, "https://example.org/",)
                .is_err());
            drop(locked);
            let Ok(ActionEffect::View(reply)) = completion.outcome else {
                panic!("Refresh failed")
            };
            assert!(authority
                .lock()
                .unwrap()
                .current(&owner, &reply.context, producer));
            assert!(model.publish_standard_view(owner.clone(), producer, reply));
            let surface = model.standard_surface().unwrap();
            assert_eq!(surface.context.revision.get(), 10);
            assert!(!search.retire_unavailable_view(&owner, &surface.context, producer, None));
        }
    }
}
