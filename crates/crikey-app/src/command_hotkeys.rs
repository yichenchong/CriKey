//! Process-scoped, asynchronous registration authority. Native callbacks only
//! enqueue bounded receipts; the owner thread performs every platform operation.
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
};

use crate::{command_preferences::CommandPreferencesSnapshot, command_registry::CommandRegistrySnapshot};
use crikey_core::{
    command::{CommandAvailability, CommandRef},
    ArgumentPolicy,
};
use crikey_platform::{Accelerator, HotkeyBinding, HotkeyCancellation, HotkeyService};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum BindingOwner {
    Launcher,
    Command(CommandRef),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyProblem {
    Invalid(String),
    Unavailable,
    Conflict {
        accelerator: String,
        owners: Vec<BindingOwner>,
    },
    Backend(Vec<HotkeyFailure>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyFailure {
    pub accelerator: String,
    pub operation: &'static str,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingStatus {
    pub requested: Option<String>,
    /// A previous binding can remain active after a refused replacement.
    pub active: Option<String>,
    pub problem: Option<HotkeyProblem>,
}

#[derive(Debug, Clone, Default)]
pub struct ReconcileReport {
    pub publication: u64,
    pub registry_revision: u64,
    pub preference_revision: u64,
    pub bindings: BTreeMap<BindingOwner, BindingStatus>,
    /// Failed releases are never forgotten; subsequent reconciliation/shutdown retries them.
    pub cleanup_failures: Vec<HotkeyFailure>,
}

/// Opaque receipt captured on the native callback thread, not an executable request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedHotkey {
    owner: BindingOwner,
    accelerator: String,
    binding_revision: u64,
    incarnation: Option<u64>,
    registry_revision: u64,
    preference_revision: u64,
}

impl QueuedHotkey {
    pub fn accelerator(&self) -> &str {
        &self.accelerator
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyDispatch {
    Launcher,
    /// The host must still resolve the item/action and check current permissions.
    Execute {
        reference: CommandRef,
    },
    /// Required arguments use the existing input workflow without admitting work.
    OpenInput {
        reference: CommandRef,
    },
}

#[derive(Debug, Clone)]
struct ActiveBinding {
    accelerator: String,
    incarnation: Option<u64>,
}

type Routes = Arc<Mutex<BTreeMap<String, QueuedHotkey>>>;

/// Owner-thread state; never shared with event/paint handling.
#[derive(Default)]
struct RegistrationState {
    active: BTreeMap<BindingOwner, ActiveBinding>,
    cleanup: BTreeSet<String>,
    routes: Routes,
    revision: u64,
    cancellation: Option<(HotkeyCancellation, u64)>,
}

impl RegistrationState {
    fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    /// Install exactly one callback, including launcher activation. `enqueue` must only
    /// wake/enqueue into the UI loop; do not execute commands on the native thread.
    pub fn install_handler(
        &self,
        service: &mut dyn HotkeyService,
        enqueue: impl Fn(QueuedHotkey) + Send + Sync + 'static,
    ) {
        let routes = Arc::clone(&self.routes);
        service.set_activation_handler(Some(Box::new(move |binding| {
            let Ok(accelerator) = Accelerator::parse(&binding.accelerator) else {
                return;
            };
            let event = routes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&accelerator.canonical())
                .cloned();
            if let Some(event) = event {
                enqueue(event);
            }
        })));
    }

    /// Canonical conflicts reject command claims; launcher reachability wins
    /// fresh desired-only conflicts but never steals actual cross-owner grabs.
    /// Replacements acquire the new key before releasing the old one. A failed
    /// release becomes an unrouted cleanup obligation, never fictional success.
    pub fn reconcile(
        &mut self,
        service: &mut dyn HotkeyService,
        registry: &CommandRegistrySnapshot,
        preferences: &CommandPreferencesSnapshot,
        activation: Option<&str>,
    ) -> ReconcileReport {
        let mut report = ReconcileReport {
            registry_revision: registry.revision,
            preference_revision: preferences.revision,
            ..Default::default()
        };
        let Some(revision) = self.revision.checked_add(1) else {
            report.cleanup_failures.push(HotkeyFailure {
                accelerator: String::new(),
                operation: "publish",
                error: "hotkey binding revision exhausted".into(),
            });
            return report;
        };
        self.retry_cleanup(service, &mut report.cleanup_failures);
        let mut desired = BTreeMap::new();
        if let Some(binding) = activation {
            desired.insert(BindingOwner::Launcher, binding.to_owned());
        }
        for record in &preferences.preferences.records {
            if let Some(binding) = &record.hotkey {
                desired.insert(BindingOwner::Command(record.reference.clone()), binding.clone());
            }
        }
        let mut canonical = BTreeMap::new();
        let mut claims: BTreeMap<String, Vec<BindingOwner>> = BTreeMap::new();
        for (owner, requested) in &desired {
            let status = BindingStatus {
                requested: Some(requested.clone()),
                active: self.active.get(owner).map(|binding| binding.accelerator.clone()),
                problem: None,
            };
            report.bindings.insert(owner.clone(), status);
            if let BindingOwner::Command(reference) = owner {
                if !registry
                    .commands
                    .get(reference)
                    .is_some_and(|command| command.declaration.availability == CommandAvailability::Available)
                {
                    report.bindings.get_mut(owner).expect("inserted status").problem =
                        Some(HotkeyProblem::Unavailable);
                    continue;
                }
                // Revalidate preserved physical bindings against this available
                // command publication even if the desired chord later conflicts
                // or registration is refused.
                if let Some(active) = self.active.get_mut(owner) {
                    active.incarnation = registry
                        .commands
                        .get(reference)
                        .map(|command| command.incarnation);
                }
            }
            match Accelerator::parse(requested) {
                Ok(accelerator) => {
                    let key = accelerator.canonical();
                    claims.entry(key.clone()).or_default().push(owner.clone());
                    canonical.insert(owner.clone(), key);
                }
                Err(error) => {
                    report.bindings.get_mut(owner).expect("inserted status").problem =
                        Some(HotkeyProblem::Invalid(error.to_string()))
                }
            }
        }
        // Removed/unavailable owners must stop dispatching even when native cleanup fails.
        let removed: Vec<_> = self
            .active
            .keys()
            .filter(|owner| {
                !desired.contains_key(*owner)
                    || matches!(
                        report
                            .bindings
                            .get(*owner)
                            .and_then(|status| status.problem.as_ref()),
                        Some(HotkeyProblem::Unavailable)
                    )
            })
            .cloned()
            .collect();
        for owner in removed {
            if let Some(old) = self.active.remove(&owner) {
                if let Err(error) = service.unregister(&binding(&old.accelerator)) {
                    self.cleanup.insert(old.accelerator.clone());
                    report
                        .cleanup_failures
                        .push(failure(&old.accelerator, "remove", error));
                }
            }
            report
                .bindings
                .entry(owner)
                .or_insert(BindingStatus {
                    requested: None,
                    active: None,
                    problem: None,
                })
                .active = None;
        }
        for (owner, key) in &canonical {
            // Include surviving physical registrations, including failed replacements,
            // so another owner cannot steal a key merely by changing desired state.
            let mut owners = if *owner == BindingOwner::Launcher {
                // Desired command conflicts cannot strand launcher activation on
                // startup. Actual cross-owner occupancy still forbids stealing.
                vec![BindingOwner::Launcher]
            } else {
                claims.get(key).cloned().unwrap_or_default()
            };
            for (held_owner, held) in &self.active {
                if held.accelerator == *key && !owners.contains(held_owner) {
                    owners.push(held_owner.clone());
                }
            }
            owners.sort();
            if owners.len() > 1 {
                report.bindings.get_mut(owner).expect("inserted status").problem =
                    Some(HotkeyProblem::Conflict {
                        accelerator: key.clone(),
                        owners,
                    });
            } else if self.cleanup.contains(key) {
                report.bindings.get_mut(owner).expect("inserted status").problem =
                    Some(HotkeyProblem::Backend(vec![HotkeyFailure {
                        accelerator: key.clone(),
                        operation: "reserve",
                        error: "previous release is unresolved".into(),
                    }]));
            }
        }
        for (owner, key) in canonical {
            if self
                .cancellation
                .as_ref()
                .is_some_and(|(cancel, epoch)| cancel.cancelled(*epoch))
            {
                break;
            }
            if report.bindings[&owner].problem.is_some() {
                continue;
            }
            let incarnation = match &owner {
                BindingOwner::Launcher => None,
                BindingOwner::Command(reference) => registry
                    .commands
                    .get(reference)
                    .map(|command| command.incarnation),
            };
            if self.active.get(&owner).is_some_and(|old| old.accelerator == key) {
                self.active.get_mut(&owner).expect("active binding").incarnation = incarnation;
                continue;
            }
            let registration = match &self.cancellation {
                Some((_, epoch)) => service.register_at(&binding(&key), *epoch),
                None => service.register(&binding(&key)),
            };
            if let Err(error) = registration {
                report.bindings.get_mut(&owner).expect("inserted status").problem = Some(
                    HotkeyProblem::Backend(vec![failure(&key, "register replacement", error)]),
                );
                continue;
            }
            if let Some(old) = self.active.get(&owner).cloned() {
                if let Err(error) = service.unregister(&binding(&old.accelerator)) {
                    let mut errors = vec![failure(&old.accelerator, "release previous", error)];
                    // Release refusal leaves physical ownership unresolved. Do not
                    // claim re-registering an idempotent backend proves it live.
                    self.active.remove(&owner);
                    self.cleanup.insert(old.accelerator);
                    if let Err(error) = service.unregister(&binding(&key)) {
                        errors.push(failure(&key, "rollback replacement", error));
                        self.cleanup.insert(key.clone());
                    }
                    report.cleanup_failures.extend(errors.iter().cloned());
                    report.bindings.get_mut(&owner).expect("inserted status").problem =
                        Some(HotkeyProblem::Backend(errors));
                    continue;
                }
            }
            self.active.insert(
                owner,
                ActiveBinding {
                    accelerator: key,
                    incarnation,
                },
            );
        }
        self.revision = revision;
        let mut routes = self.routes.lock().unwrap_or_else(|error| error.into_inner());
        routes.clear();
        for (owner, active) in &self.active {
            routes.insert(
                active.accelerator.clone(),
                QueuedHotkey {
                    owner: owner.clone(),
                    accelerator: active.accelerator.clone(),
                    binding_revision: revision,
                    incarnation: active.incarnation,
                    registry_revision: registry.revision,
                    preference_revision: preferences.revision,
                },
            );
            if let Some(status) = report.bindings.get_mut(owner) {
                status.active = Some(active.accelerator.clone());
            }
        }
        for (owner, status) in &mut report.bindings {
            if !self.active.contains_key(owner) {
                status.active = None;
            }
        }
        report
    }

    #[cfg(test)]
    /// Rejects stale queued receipts after any reconciliation, preference publication,
    /// registry publication, owner restart, removal or shutdown. Resolve only against
    /// the latest store/registry snapshots; do not retain the returned dispatch.
    pub fn resolve_event(
        &self,
        event: &QueuedHotkey,
        registry: &CommandRegistrySnapshot,
        preferences: &CommandPreferencesSnapshot,
    ) -> Option<HotkeyDispatch> {
        if event.binding_revision != self.revision
            || registry.revision != event.registry_revision
            || preferences.revision != event.preference_revision
        {
            return None;
        }
        let active = self.active.get(&event.owner)?;
        if active.accelerator != event.accelerator || active.incarnation != event.incarnation {
            return None;
        }
        match &event.owner {
            BindingOwner::Launcher => Some(HotkeyDispatch::Launcher),
            BindingOwner::Command(reference) => {
                let command = registry.commands.get(reference)?;
                if Some(command.incarnation) != event.incarnation
                    || command.declaration.availability != CommandAvailability::Available
                {
                    return None;
                }
                if command.declaration.argument_policy == ArgumentPolicy::Required {
                    Some(HotkeyDispatch::OpenInput {
                        reference: reference.clone(),
                    })
                } else {
                    Some(HotkeyDispatch::Execute {
                        reference: reference.clone(),
                    })
                }
            }
        }
    }

    /// Clear delivery first, then release every known registration. Retry if this
    /// returns errors, and finally drop the native service to close its OS session.
    pub fn shutdown(&mut self, service: &mut dyn HotkeyService) -> Vec<HotkeyFailure> {
        service.set_activation_handler(None);
        self.routes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        for (_, active) in std::mem::take(&mut self.active) {
            self.cleanup.insert(active.accelerator);
        }
        let bindings: Vec<_> = self.cleanup.iter().map(|key| binding(key)).collect();
        match service.shutdown(&bindings) {
            Ok(()) => {
                self.cleanup.clear();
                Vec::new()
            }
            Err(error) => vec![failure("", "shutdown", error)],
        }
    }

    fn retry_cleanup(&mut self, service: &mut dyn HotkeyService, errors: &mut Vec<HotkeyFailure>) {
        self.cleanup
            .retain(|key| match service.unregister(&binding(key)) {
                Ok(()) => false,
                Err(error) => {
                    errors.push(failure(key, "cleanup", error));
                    true
                }
            });
    }
}

impl Drop for RegistrationState {
    fn drop(&mut self) {
        // Even an incorrectly ordered host teardown must stop callback dispatch.
        self.routes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }
}

fn binding(accelerator: &str) -> HotkeyBinding {
    HotkeyBinding {
        accelerator: accelerator.into(),
    }
}
fn failure(accelerator: &str, operation: &'static str, error: impl std::fmt::Display) -> HotkeyFailure {
    HotkeyFailure {
        accelerator: accelerator.into(),
        operation,
        error: error.to_string(),
    }
}

struct Desired {
    publication: u64,
    registry: Arc<CommandRegistrySnapshot>,
    preferences: Arc<CommandPreferencesSnapshot>,
    activation: Option<String>,
}

#[derive(Default)]
struct Mailbox {
    latest: Option<Desired>,
    publication: u64,
    stop: bool,
    cancellation: Option<HotkeyCancellation>,
    routes: BTreeMap<String, QueuedHotkey>,
    events: VecDeque<QueuedHotkey>,
    report: Option<ReconcileReport>,
    initialization_error: Option<String>,
}

/// Exactly one instance owns the process hotkey handler. Construct the native
/// service inside `factory` so thread-affine APIs are created and destroyed on
/// the same worker. One latest desired snapshot and one latest report are kept.
pub struct CommandHotkeys {
    mailbox: Arc<(Mutex<Mailbox>, Condvar)>,
    worker: Option<JoinHandle<Vec<HotkeyFailure>>>,
}

impl std::fmt::Debug for CommandHotkeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandHotkeys")
            .field("owner_thread_present", &self.worker.is_some())
            .finish_non_exhaustive()
    }
}

impl CommandHotkeys {
    pub fn new(
        factory: impl FnOnce() -> Result<Box<dyn HotkeyService>, String> + Send + 'static,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let mailbox = Arc::new((Mutex::new(Mailbox::default()), Condvar::new()));
        let shared = Arc::clone(&mailbox);
        let wake = Arc::new(wake);
        let worker = thread::spawn(move || {
            let mut service = match factory() {
                Ok(service) => service,
                Err(error) => {
                    let mut state = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                    state.initialization_error = Some(error.clone());
                    let (registry_revision, preference_revision) = state
                        .latest
                        .as_ref()
                        .map(|desired| (desired.registry.revision, desired.preferences.revision))
                        .unwrap_or_default();
                    state.report = Some(ReconcileReport {
                        publication: state.publication,
                        registry_revision,
                        preference_revision,
                        cleanup_failures: vec![failure("", "initialize", error)],
                        ..Default::default()
                    });
                    drop(state);
                    wake();
                    return Vec::new();
                }
            };
            let mut registrations = RegistrationState::new();
            {
                let mut state = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                state.cancellation = service.cancellation_handle();
                if state.stop {
                    if let Some(cancel) = &state.cancellation {
                        cancel.stop();
                    }
                }
            }
            let callback = Arc::clone(&shared);
            let notify = Arc::clone(&wake);
            // Resolve through the admitted publication, not worker-local routes.
            service.set_activation_handler(Some(Box::new(move |binding| {
                let mut state = callback.0.lock().unwrap_or_else(|e| e.into_inner());
                if state.stop {
                    return;
                }
                let Some(event) = state.routes.get(&binding.accelerator).cloned() else {
                    return;
                };
                if state.events.len() == 64 {
                    return;
                }
                state.events.push_back(event);
                drop(state);
                notify();
            })));
            loop {
                let desired = {
                    let mut state = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                    while state.latest.is_none() && !state.stop {
                        state = shared.1.wait(state).unwrap_or_else(|e| e.into_inner());
                    }
                    if state.stop {
                        break;
                    }
                    registrations.cancellation = state
                        .cancellation
                        .as_ref()
                        .map(|cancel| (cancel.clone(), cancel.epoch()));
                    state.latest.take().expect("desired snapshot")
                };
                let report = registrations.reconcile(
                    service.as_mut(),
                    &desired.registry,
                    &desired.preferences,
                    desired.activation.as_deref(),
                );
                let mut state = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                // Superseded work can still have changed physical ownership.
                // Retire routes it released even though its report cannot publish.
                state.routes.retain(|key, event| {
                    registrations.active.get(&event.owner).is_some_and(|active| {
                        active.accelerator == *key && active.incarnation == event.incarnation
                    })
                });
                let mut report = report;
                report.publication = desired.publication;
                report.registry_revision = desired.registry.revision;
                report.preference_revision = desired.preferences.revision;
                if !state.stop && state.publication == desired.publication {
                    state.routes = registrations
                        .routes
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .iter()
                        .map(|(key, receipt)| {
                            let mut receipt = receipt.clone();
                            receipt.binding_revision = desired.publication;
                            (key.clone(), receipt)
                        })
                        .collect();
                    state.report = Some(report);
                    drop(state);
                    wake();
                }
            }
            registrations.shutdown(service.as_mut())
        });
        Self {
            mailbox,
            worker: Some(worker),
        }
    }

    /// Bounded admission only. Every publication invalidates queued receipts;
    /// routes are retired immediately, before any pending OS approval completes.
    pub fn reconcile(
        &self,
        registry: Arc<CommandRegistrySnapshot>,
        preferences: Arc<CommandPreferencesSnapshot>,
        activation: Option<String>,
    ) {
        let mut state = self.mailbox.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.stop {
            return;
        }
        let Some(publication) = state.publication.checked_add(1) else {
            state.routes.clear();
            state.report = Some(ReconcileReport {
                cleanup_failures: vec![failure("", "publish", "hotkey publication exhausted")],
                ..Default::default()
            });
            return;
        };
        state.publication = publication;
        state.report = None;
        let assigned: BTreeSet<_> = if state.routes.is_empty() {
            BTreeSet::new()
        } else {
            preferences
                .preferences
                .records
                .iter()
                .filter(|record| record.hotkey.is_some())
                .map(|record| &record.reference)
                .collect()
        };
        state.routes.retain(|_, receipt| {
            let valid = match &receipt.owner {
                BindingOwner::Launcher => activation.is_some(),
                BindingOwner::Command(reference) => {
                    assigned.contains(reference)
                        && registry.commands.get(reference).is_some_and(|command| {
                            command.declaration.availability == CommandAvailability::Available
                                && Some(command.incarnation) == receipt.incarnation
                        })
                }
            };
            if valid {
                receipt.binding_revision = publication;
                receipt.registry_revision = registry.revision;
                receipt.preference_revision = preferences.revision;
            }
            valid
        });
        state.events.clear();
        if let Some(error) = state.initialization_error.clone() {
            state.report = Some(ReconcileReport {
                publication,
                registry_revision: registry.revision,
                preference_revision: preferences.revision,
                cleanup_failures: vec![failure("", "initialize", error)],
                ..Default::default()
            });
            return;
        }
        if let Some(cancel) = &state.cancellation {
            cancel.cancel();
        }
        state.latest = Some(Desired {
            publication,
            registry,
            preferences,
            activation,
        });
        self.mailbox.1.notify_one();
    }

    pub fn drain_events(&self) -> Vec<QueuedHotkey> {
        self.mailbox
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .events
            .drain(..)
            .collect()
    }

    pub fn take_report(&self) -> Option<ReconcileReport> {
        self.mailbox
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .report
            .take()
    }

    pub fn active_binding(&self, owner: &BindingOwner) -> Option<String> {
        self.mailbox
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .routes
            .values()
            .find(|event| &event.owner == owner)
            .map(|event| event.accelerator.clone())
    }

    pub fn resolve_event(
        &self,
        event: &QueuedHotkey,
        registry: &CommandRegistrySnapshot,
        preferences: &CommandPreferencesSnapshot,
    ) -> Option<HotkeyDispatch> {
        let state = self.mailbox.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.stop
            || event.binding_revision != state.publication
            || state.routes.get(&event.accelerator) != Some(event)
        {
            return None;
        }
        if registry.revision != event.registry_revision || preferences.revision != event.preference_revision {
            return None;
        }
        match &event.owner {
            BindingOwner::Launcher => Some(HotkeyDispatch::Launcher),
            BindingOwner::Command(reference) => {
                // Desired presence is authority to keep a preserved old chord,
                // not a requirement that it equal the newly requested chord.
                if !preferences
                    .preferences
                    .records
                    .iter()
                    .any(|record| record.reference == *reference && record.hotkey.is_some())
                {
                    return None;
                }
                let command = registry.commands.get(reference)?;
                if command.declaration.availability != CommandAvailability::Available
                    || Some(command.incarnation) != event.incarnation
                {
                    return None;
                }
                Some(
                    if command.declaration.argument_policy == ArgumentPolicy::Required {
                        HotkeyDispatch::OpenInput {
                            reference: reference.clone(),
                        }
                    } else {
                        HotkeyDispatch::Execute {
                            reference: reference.clone(),
                        }
                    },
                )
            }
        }
    }

    pub fn shutdown(&mut self) -> Vec<HotkeyFailure> {
        {
            let mut state = self.mailbox.0.lock().unwrap_or_else(|e| e.into_inner());
            state.stop = true;
            state.routes.clear();
            state.events.clear();
            state.latest = None;
            if let Some(cancel) = &state.cancellation {
                cancel.stop();
            }
            self.mailbox.1.notify_one();
        }
        self.worker
            .take()
            .map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|_| vec![failure("", "shutdown", "hotkey owner thread panicked")])
            })
            .unwrap_or_default()
    }
}

impl Drop for CommandHotkeys {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_registry::RegisteredCommand;
    use crikey_config::command_preferences::{CommandPreference, CommandPreferences};
    use crikey_core::{
        command::{CommandDeclaration, ExecutableTarget, HostCommand},
        CoreError, PluginId,
    };

    /// Deterministic transaction consumer; deliberately makes no native OS claims.
    #[derive(Default)]
    struct RecordingService {
        held: BTreeSet<String>,
        calls: Vec<(bool, String)>,
        refusals: BTreeSet<(bool, String)>,
        handler: Option<crikey_platform::HotkeyActivationHandler>,
    }
    impl HotkeyService for RecordingService {
        fn register(&mut self, binding: &HotkeyBinding) -> crikey_core::Result<()> {
            self.calls.push((true, binding.accelerator.clone()));
            if self.refusals.contains(&(true, binding.accelerator.clone())) {
                return Err(CoreError::Invalid("registration refused".into()));
            }
            self.held.insert(binding.accelerator.clone());
            Ok(())
        }
        fn unregister(&mut self, binding: &HotkeyBinding) -> crikey_core::Result<()> {
            self.calls.push((false, binding.accelerator.clone()));
            if self.refusals.contains(&(false, binding.accelerator.clone())) {
                return Err(CoreError::Invalid("release refused".into()));
            }
            if !self.held.remove(&binding.accelerator) {
                return Err(CoreError::Invalid("not registered".into()));
            }
            Ok(())
        }
        fn set_activation_handler(&mut self, handler: Option<crikey_platform::HotkeyActivationHandler>) {
            self.handler = handler;
        }
    }
    fn reference() -> CommandRef {
        CommandRef {
            provider_id: PluginId("crikey.host".into()),
            command_id: "open-settings".into(),
        }
    }
    fn snapshots(hotkey: &str) -> (CommandRegistrySnapshot, CommandPreferencesSnapshot) {
        let reference = reference();
        let registry = CommandRegistrySnapshot {
            revision: 1,
            commands: BTreeMap::from([(
                reference.clone(),
                RegisteredCommand {
                    incarnation: 1,
                    declaration: CommandDeclaration {
                        reference: reference.clone(),
                        label: "Settings".into(),
                        executable_target: ExecutableTarget::Host(HostCommand::OpenSettings),
                        availability: CommandAvailability::Available,
                        argument_policy: ArgumentPolicy::Forbidden,
                    },
                },
            )]),
        };
        let preferences = CommandPreferencesSnapshot {
            revision: 1,
            layer: None,
            preferences: CommandPreferences {
                records: vec![CommandPreference {
                    reference,
                    hidden: false,
                    alias: None,
                    hotkey: Some(hotkey.into()),
                }],
                ..Default::default()
            },
        };
        (registry, preferences)
    }
    fn replacement(preferences: &mut CommandPreferencesSnapshot, key: &str) {
        preferences.revision += 1;
        preferences.preferences.records[0].hotkey = Some(key.into());
    }
    fn events(
        authority: &RegistrationState,
        service: &mut RecordingService,
    ) -> Arc<Mutex<Vec<QueuedHotkey>>> {
        let queue = Arc::new(Mutex::new(Vec::new()));
        let sender = Arc::clone(&queue);
        authority.install_handler(service, move |event| sender.lock().unwrap().push(event));
        queue
    }
    fn fire(service: &RecordingService, key: &str, queue: &Arc<Mutex<Vec<QueuedHotkey>>>) -> QueuedHotkey {
        service.handler.as_ref().unwrap()(&binding(key));
        queue.lock().unwrap().pop().unwrap()
    }

    #[test]
    fn registry_republication_refreshes_preserved_receipts_after_refusal_and_conflict() {
        let (mut registry, mut preferences) = snapshots("Ctrl+A");
        let mut service = RecordingService::default();
        let mut authority = RegistrationState::new();
        let queue = events(&authority, &mut service);
        authority.reconcile(&mut service, &registry, &preferences, Some("Ctrl+C"));
        let old = fire(&service, "Ctrl+A", &queue);
        replacement(&mut preferences, "Ctrl+B");
        registry.revision += 1;
        registry.commands.get_mut(&reference()).unwrap().incarnation += 1;
        service.refusals.insert((true, "Ctrl+B".into()));
        let report = authority.reconcile(&mut service, &registry, &preferences, Some("Ctrl+C"));
        assert_eq!(
            report.bindings[&BindingOwner::Command(reference())]
                .active
                .as_deref(),
            Some("Ctrl+A")
        );
        assert_eq!(service.held, BTreeSet::from(["Ctrl+A".into(), "Ctrl+C".into()]));
        assert!(authority.resolve_event(&old, &registry, &preferences).is_none());
        let preserved = fire(&service, "Ctrl+A", &queue);
        assert_eq!(
            authority.resolve_event(&preserved, &registry, &preferences),
            Some(HotkeyDispatch::Execute {
                reference: reference()
            })
        );
        replacement(&mut preferences, "Ctrl+C");
        registry.revision += 1;
        registry.commands.get_mut(&reference()).unwrap().incarnation += 1;
        let report = authority.reconcile(&mut service, &registry, &preferences, Some("Ctrl+C"));
        assert!(matches!(
            report.bindings[&BindingOwner::Command(reference())].problem,
            Some(HotkeyProblem::Conflict { .. })
        ));
        assert_eq!(
            report.bindings[&BindingOwner::Command(reference())]
                .active
                .as_deref(),
            Some("Ctrl+A")
        );
        assert!(authority
            .resolve_event(&preserved, &registry, &preferences)
            .is_none());
        assert_eq!(
            authority.resolve_event(&fire(&service, "Ctrl+A", &queue), &registry, &preferences),
            Some(HotkeyDispatch::Execute {
                reference: reference()
            })
        );
        assert!(authority.shutdown(&mut service).is_empty());
        assert!(service.held.is_empty());
    }

    #[test]
    fn replacement_acquires_before_release_and_queued_old_receipts_expire() {
        let (registry, mut preferences) = snapshots("Ctrl+A");
        let mut service = RecordingService::default();
        let mut authority = RegistrationState::new();
        let queue = events(&authority, &mut service);
        authority.reconcile(&mut service, &registry, &preferences, None);
        let old = fire(&service, "Ctrl+A", &queue);
        assert!(matches!(
            authority.resolve_event(&old, &registry, &preferences),
            Some(HotkeyDispatch::Execute { .. })
        ));
        replacement(&mut preferences, "Ctrl+B");
        authority.reconcile(&mut service, &registry, &preferences, None);
        service.handler.as_ref().unwrap()(&binding("Ctrl+A"));
        assert!(queue.lock().unwrap().is_empty());
        assert!(authority
            .resolve_event(&fire(&service, "Ctrl+B", &queue), &registry, &preferences)
            .is_some());
        assert!(authority.resolve_event(&old, &registry, &preferences).is_none());
        assert_eq!(service.held, BTreeSet::from(["Ctrl+B".into()]));
    }

    #[test]
    fn refused_replacement_preserves_previous_registration_and_dispatch() {
        let (registry, mut preferences) = snapshots("Ctrl+A");
        let mut service = RecordingService::default();
        let mut authority = RegistrationState::new();
        let queue = events(&authority, &mut service);
        authority.reconcile(&mut service, &registry, &preferences, None);
        service.refusals.insert((true, "Ctrl+B".into()));
        replacement(&mut preferences, "Ctrl+B");
        let report = authority.reconcile(&mut service, &registry, &preferences, None);
        let status = &report.bindings[&BindingOwner::Command(reference())];
        assert_eq!(status.active.as_deref(), Some("Ctrl+A"));
        assert!(matches!(status.problem, Some(HotkeyProblem::Backend(_))));
        assert_eq!(service.held, BTreeSet::from(["Ctrl+A".into()]));
        let event = fire(&service, "Ctrl+A", &queue);
        assert!(authority.resolve_event(&event, &registry, &preferences).is_some());
        service.handler.as_ref().unwrap()(&binding("Ctrl+B"));
        assert!(queue.lock().unwrap().is_empty());
        service.refusals.clear();
        authority.reconcile(&mut service, &registry, &preferences, None);
        service.handler.as_ref().unwrap()(&binding("Ctrl+A"));
        assert!(queue.lock().unwrap().is_empty());
        assert!(authority
            .resolve_event(&fire(&service, "Ctrl+B", &queue), &registry, &preferences)
            .is_some());
    }

    #[test]
    fn release_failure_rolls_back_and_reports_rollback_failure_without_routing_orphan() {
        let (registry, mut preferences) = snapshots("Ctrl+A");
        let mut service = RecordingService::default();
        let mut authority = RegistrationState::new();
        let queue = events(&authority, &mut service);
        authority.reconcile(&mut service, &registry, &preferences, None);
        replacement(&mut preferences, "Ctrl+B");
        service
            .refusals
            .extend([(false, "Ctrl+A".into()), (false, "Ctrl+B".into())]);
        let report = authority.reconcile(&mut service, &registry, &preferences, None);
        let Some(HotkeyProblem::Backend(errors)) =
            &report.bindings[&BindingOwner::Command(reference())].problem
        else {
            panic!("expected transaction failure");
        };
        assert_eq!(
            errors.iter().map(|error| error.operation).collect::<Vec<_>>(),
            ["release previous", "rollback replacement"]
        );
        service.handler.as_ref().unwrap()(&binding("Ctrl+B"));
        assert!(queue.lock().unwrap().is_empty());
        assert_eq!(
            authority.cleanup,
            BTreeSet::from(["Ctrl+A".into(), "Ctrl+B".into()])
        );
        assert_eq!(service.held, authority.cleanup);
        assert!(!authority.shutdown(&mut service).is_empty());
        assert!(service.handler.is_none());
        service.refusals.clear();
        assert!(authority.shutdown(&mut service).is_empty());
        assert!(service.held.is_empty());
    }

    #[test]
    fn release_unknown_is_not_misrepresented_as_active() {
        let (registry, mut preferences) = snapshots("Ctrl+A");
        let mut service = RecordingService::default();
        let mut authority = RegistrationState::new();
        let queue = events(&authority, &mut service);
        authority.reconcile(&mut service, &registry, &preferences, None);
        replacement(&mut preferences, "Ctrl+B");
        service
            .refusals
            .extend([(false, "Ctrl+A".into()), (true, "Ctrl+A".into())]);
        let report = authority.reconcile(&mut service, &registry, &preferences, None);
        let status = &report.bindings[&BindingOwner::Command(reference())];
        assert_eq!(status.active, None);
        assert!(matches!(&status.problem, Some(HotkeyProblem::Backend(errors)) if errors.len() == 1));
        assert!(authority.cleanup.contains("Ctrl+A"));
        service.handler.as_ref().unwrap()(&binding("Ctrl+A"));
        assert!(queue.lock().unwrap().is_empty());
    }

    #[test]
    fn fresh_canonical_conflict_prioritizes_launcher_and_reports_command_inactive() {
        let (registry, preferences) = snapshots("alt+ctrl+SPACE");
        let mut service = RecordingService::default();
        let mut authority = RegistrationState::new();
        let queue = events(&authority, &mut service);
        let report = authority.reconcile(&mut service, &registry, &preferences, Some("Ctrl+Alt+Space"));
        assert_eq!(service.held, BTreeSet::from(["Ctrl+Alt+Space".into()]));
        assert_eq!(
            report.bindings[&BindingOwner::Launcher].active.as_deref(),
            Some("Ctrl+Alt+Space")
        );
        assert!(report.bindings[&BindingOwner::Launcher].problem.is_none());
        assert!(
            matches!(&report.bindings[&BindingOwner::Command(reference())].problem, Some(HotkeyProblem::Conflict { owners, .. }) if owners.len() == 2)
        );
        assert_eq!(report.bindings[&BindingOwner::Command(reference())].active, None);
        assert_eq!(
            authority.resolve_event(&fire(&service, "Ctrl+Alt+Space", &queue), &registry, &preferences),
            Some(HotkeyDispatch::Launcher)
        );
    }

    #[test]
    fn occupied_cross_owner_swap_keeps_existing_launcher_and_command_without_stealing() {
        let (registry, mut preferences) = snapshots("Ctrl+A");
        let mut service = RecordingService::default();
        let mut authority = RegistrationState::new();
        let queue = events(&authority, &mut service);
        authority.reconcile(&mut service, &registry, &preferences, Some("Ctrl+B"));
        replacement(&mut preferences, "Ctrl+B");
        let report = authority.reconcile(&mut service, &registry, &preferences, Some("Ctrl+A"));
        assert_eq!(service.held, BTreeSet::from(["Ctrl+A".into(), "Ctrl+B".into()]));
        assert_eq!(
            report.bindings[&BindingOwner::Launcher].active.as_deref(),
            Some("Ctrl+B")
        );
        assert_eq!(
            report.bindings[&BindingOwner::Command(reference())]
                .active
                .as_deref(),
            Some("Ctrl+A")
        );
        for status in report.bindings.values() {
            assert!(matches!(status.problem, Some(HotkeyProblem::Conflict { .. })));
        }
        assert_eq!(
            authority.resolve_event(&fire(&service, "Ctrl+B", &queue), &registry, &preferences),
            Some(HotkeyDispatch::Launcher)
        );
        assert!(matches!(
            authority.resolve_event(&fire(&service, "Ctrl+A", &queue), &registry, &preferences),
            Some(HotkeyDispatch::Execute { .. })
        ));
    }

    #[test]
    fn fresh_launcher_desire_cannot_take_an_existing_command_registration() {
        let (registry, preferences) = snapshots("Ctrl+A");
        let mut service = RecordingService::default();
        let mut authority = RegistrationState::new();
        authority.reconcile(&mut service, &registry, &preferences, None);
        let report = authority.reconcile(&mut service, &registry, &preferences, Some("Ctrl+A"));
        assert_eq!(service.held, BTreeSet::from(["Ctrl+A".into()]));
        assert_eq!(report.bindings[&BindingOwner::Launcher].active, None);
        assert!(matches!(
            report.bindings[&BindingOwner::Launcher].problem,
            Some(HotkeyProblem::Conflict { .. })
        ));
        assert_eq!(
            report.bindings[&BindingOwner::Command(reference())]
                .active
                .as_deref(),
            Some("Ctrl+A")
        );
    }

    #[test]
    fn unavailable_removed_and_shutdown_reject_pending_events() {
        let (mut registry, mut preferences) = snapshots("Ctrl+A");
        let mut service = RecordingService::default();
        let mut authority = RegistrationState::new();
        let queue = events(&authority, &mut service);
        authority.reconcile(&mut service, &registry, &preferences, Some("Ctrl+Space"));
        let old = fire(&service, "Ctrl+A", &queue);
        registry.revision += 1;
        registry
            .commands
            .get_mut(&reference())
            .unwrap()
            .declaration
            .availability = CommandAvailability::Unavailable;
        assert!(authority.resolve_event(&old, &registry, &preferences).is_none());
        authority.reconcile(&mut service, &registry, &preferences, Some("Ctrl+Space"));
        assert!(!service.held.contains("Ctrl+A"));
        assert!(authority.resolve_event(&old, &registry, &preferences).is_none());
        registry
            .commands
            .get_mut(&reference())
            .unwrap()
            .declaration
            .availability = CommandAvailability::Available;
        registry.revision += 1;
        authority.reconcile(&mut service, &registry, &preferences, Some("Ctrl+Space"));
        preferences.preferences.records.clear();
        preferences.revision += 1;
        authority.reconcile(&mut service, &registry, &preferences, Some("Ctrl+Space"));
        assert_eq!(service.held, BTreeSet::from(["Ctrl+Space".into()]));
        let launcher = fire(&service, "Ctrl+Space", &queue);
        assert_eq!(
            authority.resolve_event(&launcher, &registry, &preferences),
            Some(HotkeyDispatch::Launcher)
        );
        assert!(authority.shutdown(&mut service).is_empty());
        assert!(authority
            .resolve_event(&launcher, &registry, &preferences)
            .is_none());
    }

    #[test]
    fn current_revisions_and_incarnation_are_revalidated_and_arguments_open_input() {
        let (mut registry, mut preferences) = snapshots("Ctrl+A");
        let mut service = RecordingService::default();
        let mut authority = RegistrationState::new();
        let queue = events(&authority, &mut service);
        authority.reconcile(&mut service, &registry, &preferences, None);
        let event = fire(&service, "Ctrl+A", &queue);
        preferences.revision += 1;
        assert!(authority.resolve_event(&event, &registry, &preferences).is_none());
        registry.commands.get_mut(&reference()).unwrap().incarnation += 1;
        assert!(authority.resolve_event(&event, &registry, &preferences).is_none());
        for policy in [ArgumentPolicy::Required, ArgumentPolicy::Optional] {
            registry
                .commands
                .get_mut(&reference())
                .unwrap()
                .declaration
                .argument_policy = policy;
            registry.revision += 1;
            authority.reconcile(&mut service, &registry, &preferences, None);
            let expected = if policy == ArgumentPolicy::Required {
                HotkeyDispatch::OpenInput {
                    reference: reference(),
                }
            } else {
                HotkeyDispatch::Execute {
                    reference: reference(),
                }
            };
            assert_eq!(
                authority.resolve_event(&fire(&service, "Ctrl+A", &queue), &registry, &preferences),
                Some(expected)
            );
        }
    }

    struct SharedService(Arc<Mutex<RecordingService>>);
    impl HotkeyService for SharedService {
        fn register(&mut self, binding: &HotkeyBinding) -> crikey_core::Result<()> {
            self.0.lock().unwrap().register(binding)
        }
        fn unregister(&mut self, binding: &HotkeyBinding) -> crikey_core::Result<()> {
            self.0.lock().unwrap().unregister(binding)
        }
        fn set_activation_handler(&mut self, handler: Option<crikey_platform::HotkeyActivationHandler>) {
            self.0.lock().unwrap().set_activation_handler(handler);
        }
    }

    #[test]
    fn process_consumer_fences_publications_bounds_receipts_and_retires_failed_removal() {
        use std::{sync::mpsc, time::Duration};
        let resources = Arc::new(Mutex::new(RecordingService::default()));
        let backend = Arc::clone(&resources);
        let (wake, wakes) = mpsc::channel();
        let mut runtime = CommandHotkeys::new(
            move || Ok(Box::new(SharedService(backend))),
            move || {
                let _ = wake.send(());
            },
        );
        let complete = |runtime: &CommandHotkeys| loop {
            if let Some(report) = runtime.take_report() {
                break report;
            }
            wakes.recv_timeout(Duration::from_secs(3)).unwrap();
        };
        let emit = |key: &str| resources.lock().unwrap().handler.as_ref().unwrap()(&binding(key));
        let (mut registry, mut preferences) = snapshots("Ctrl+A");
        runtime.reconcile(Arc::new(registry.clone()), Arc::new(preferences.clone()), None);
        complete(&runtime);
        emit("Ctrl+A");
        let stale = runtime.drain_events().pop().unwrap();
        resources.lock().unwrap().refusals.insert((true, "Ctrl+B".into()));
        replacement(&mut preferences, "Ctrl+B");
        registry.revision += 1;
        registry.commands.get_mut(&reference()).unwrap().incarnation += 1;
        runtime.reconcile(Arc::new(registry.clone()), Arc::new(preferences.clone()), None);
        assert!(runtime.resolve_event(&stale, &registry, &preferences).is_none());
        let report = complete(&runtime);
        assert_eq!(report.preference_revision, preferences.revision);
        assert_eq!(report.registry_revision, registry.revision);
        assert_eq!(
            report.bindings[&BindingOwner::Command(reference())]
                .active
                .as_deref(),
            Some("Ctrl+A")
        );
        emit("Ctrl+B");
        assert!(runtime.drain_events().is_empty());
        emit("Ctrl+A");
        let preserved = runtime.drain_events().pop().unwrap();
        assert_eq!(
            runtime.resolve_event(&preserved, &registry, &preferences),
            Some(HotkeyDispatch::Execute {
                reference: reference()
            })
        );
        for _ in 0..100 {
            emit("Ctrl+A");
        }
        assert_eq!(runtime.drain_events().len(), 64);
        resources
            .lock()
            .unwrap()
            .refusals
            .insert((false, "Ctrl+A".into()));
        preferences.preferences.records.clear();
        preferences.revision += 1;
        runtime.reconcile(Arc::new(registry.clone()), Arc::new(preferences.clone()), None);
        assert_eq!(runtime.active_binding(&BindingOwner::Command(reference())), None);
        emit("Ctrl+A");
        assert!(runtime.drain_events().is_empty());
        assert!(runtime
            .resolve_event(&preserved, &registry, &preferences)
            .is_none());
        assert!(!complete(&runtime).cleanup_failures.is_empty());
        assert!(resources.lock().unwrap().held.contains("Ctrl+A"));
        resources.lock().unwrap().refusals.clear();
        assert!(runtime.shutdown().is_empty());
        assert!(resources.lock().unwrap().held.is_empty());
        assert!(resources.lock().unwrap().handler.is_none());
    }

    struct ApprovalService {
        held: Arc<Mutex<BTreeSet<String>>>,
        cancel: HotkeyCancellation,
        entered: Option<std::sync::mpsc::Sender<()>>,
        owner: thread::ThreadId,
        dropped: Arc<Mutex<bool>>,
    }
    impl HotkeyService for ApprovalService {
        fn register(&mut self, binding: &HotkeyBinding) -> crikey_core::Result<()> {
            assert_eq!(self.owner, thread::current().id());
            if let Some(entered) = self.entered.take() {
                let epoch = self.cancel.epoch();
                entered.send(()).unwrap();
                while !self.cancel.cancelled(epoch) {
                    thread::park_timeout(std::time::Duration::from_millis(1));
                }
                return Err(CoreError::Invalid("approval cancelled".into()));
            }
            self.held.lock().unwrap().insert(binding.accelerator.clone());
            Ok(())
        }
        fn unregister(&mut self, binding: &HotkeyBinding) -> crikey_core::Result<()> {
            assert_eq!(self.owner, thread::current().id());
            assert!(self.held.lock().unwrap().remove(&binding.accelerator));
            Ok(())
        }
        fn set_activation_handler(&mut self, _: Option<crikey_platform::HotkeyActivationHandler>) {}
        fn cancellation_handle(&self) -> Option<HotkeyCancellation> {
            Some(self.cancel.clone())
        }
    }
    impl Drop for ApprovalService {
        fn drop(&mut self) {
            assert_eq!(self.owner, thread::current().id());
            *self.dropped.lock().unwrap() = true;
        }
    }

    #[test]
    fn pending_approval_cancellation_reconciles_latest_and_releases_resources() {
        use std::{sync::mpsc, time::Duration};
        let held = Arc::new(Mutex::new(BTreeSet::new()));
        let resources = Arc::clone(&held);
        let dropped = Arc::new(Mutex::new(false));
        let retired = Arc::clone(&dropped);
        let (entered, blocked) = mpsc::channel();
        let (wake, wakes) = mpsc::channel();
        let mut runtime = CommandHotkeys::new(
            move || {
                Ok(Box::new(ApprovalService {
                    held: resources,
                    cancel: HotkeyCancellation::default(),
                    entered: Some(entered),
                    owner: thread::current().id(),
                    dropped: retired,
                }))
            },
            move || {
                let _ = wake.send(());
            },
        );
        let (registry, mut preferences) = snapshots("Ctrl+A");
        runtime.reconcile(Arc::new(registry.clone()), Arc::new(preferences.clone()), None);
        blocked.recv_timeout(Duration::from_secs(3)).unwrap();
        replacement(&mut preferences, "Ctrl+C");
        runtime.reconcile(Arc::new(registry), Arc::new(preferences), None);
        let report = loop {
            if let Some(report) = runtime.take_report() {
                break report;
            }
            wakes.recv_timeout(Duration::from_secs(3)).unwrap();
        };
        assert_eq!(
            report.bindings[&BindingOwner::Command(reference())]
                .active
                .as_deref(),
            Some("Ctrl+C")
        );
        assert_eq!(*held.lock().unwrap(), BTreeSet::from(["Ctrl+C".into()]));
        assert!(runtime.shutdown().is_empty());
        assert!(held.lock().unwrap().is_empty());
        assert!(*dropped.lock().unwrap());
    }

    #[test]
    fn shutdown_interrupts_approval_and_destroys_service_on_owner_thread() {
        use std::{sync::mpsc, time::Duration};
        let held = Arc::new(Mutex::new(BTreeSet::new()));
        let resources = Arc::clone(&held);
        let dropped = Arc::new(Mutex::new(false));
        let retired = Arc::clone(&dropped);
        let (entered, blocked) = mpsc::channel();
        let mut runtime = CommandHotkeys::new(
            move || {
                Ok(Box::new(ApprovalService {
                    held: resources,
                    cancel: HotkeyCancellation::default(),
                    entered: Some(entered),
                    owner: thread::current().id(),
                    dropped: retired,
                }))
            },
            || {},
        );
        let (registry, preferences) = snapshots("Ctrl+A");
        runtime.reconcile(Arc::new(registry), Arc::new(preferences), None);
        blocked.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(runtime.shutdown().is_empty());
        assert!(runtime.drain_events().is_empty());
        assert!(held.lock().unwrap().is_empty());
        assert!(*dropped.lock().unwrap());
    }
}
