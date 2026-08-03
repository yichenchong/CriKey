//! Shared action-dispatch seam for plugin-owned result actions.
//!
//! The search service validates the selected item and action before handing the
//! request to this router. Providers register their exact loaded plugin ids;
//! dispatch therefore cannot fall through to a sibling runtime or reinterpret
//! a stale owner as another plugin.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use crikey_core::{ActionId, CoreError, Item, ItemId, PluginId, Result as CoreResult};

/// Identifier assigned when a plugin action is admitted to an endpoint.
///
/// The plugin namespace is part of the id so a router can cancel an action or
/// attribute its completion without relying on a process-global counter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActionRequestId {
    pub plugin: PluginId,
    pub sequence: u64,
}

/// Terminal result emitted by a plugin action endpoint.
///
/// The completion owns its error, so polling is the only operation that
/// transfers a worker outcome back to the UI thread.
#[derive(Debug)]
pub struct PluginActionCompletion {
    pub request_id: ActionRequestId,
    pub plugin: PluginId,
    pub item_id: ItemId,
    pub action_id: ActionId,
    pub outcome: CoreResult<()>,
}

/// What a validated action did at submission time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionSubmission {
    /// A host-mediated action completed synchronously.
    Completed,
    /// A plugin-owned action was admitted and will complete asynchronously.
    Pending(ActionRequestId),
}

/// Runtime entry point for one provider's plugin-owned actions.
///
/// Implementations own the worker and the shared per-plugin budget. Submission
/// must only validate/admit and enqueue work; it must never wait for the plugin
/// callback. The admitted action slot remains owned by the endpoint until a
/// terminal completion is emitted, including transport failures, timeout,
/// cancellation and worker reaping.
pub trait PluginActionExecutor: Send + Sync {
    /// Admits `action_id` for an item belonging to `plugin`.
    fn submit_plugin_action(
        &self,
        plugin: &PluginId,
        item: &Item,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> CoreResult<ActionRequestId>;

    /// Drains terminal outcomes without waiting for plugin work.
    fn poll_plugin_actions(&self) -> Vec<PluginActionCompletion> {
        Vec::new()
    }

    /// Requests cancellation of an admitted action. Cancellation is
    /// cooperative once a provider worker has started the host call.
    fn cancel_plugin_action(&self, _request_id: &ActionRequestId) -> bool {
        false
    }

    /// Whether this endpoint has a current item snapshot with exact ownership.
    ///
    /// The live UI receives async provider rows separately from
    /// `SearchService`; this lookup lets the composition root route one of
    /// those rows without guessing an owner from an opaque stable id.
    fn owns_item(&self, _plugin: &PluginId, _item_id: &ItemId) -> bool {
        false
    }

    /// Admits an action by an item id retained by the provider runtime.
    fn submit_plugin_action_by_id(
        &self,
        _plugin: &PluginId,
        _item_id: &ItemId,
        _action_id: &ActionId,
        _argument: Option<&str>,
    ) -> CoreResult<ActionRequestId> {
        Err(CoreError::Invalid(
            "plugin action item snapshot is unavailable".to_owned(),
        ))
    }
}

/// Exact-owner action registry used by [`crate::SearchService`].
///
/// A provider endpoint is registered once for every plugin it loaded. The
/// lookup is exact on the namespaced `PluginId`; no prefix or fallback routing
/// is permitted.
#[derive(Default)]
pub struct PluginActionRouter {
    providers: BTreeMap<PluginId, Arc<dyn PluginActionExecutor>>,
}

impl fmt::Debug for PluginActionRouter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginActionRouter")
            .field("plugins", &self.providers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl PluginActionRouter {
    /// Registers one endpoint for each exact plugin id it owns.
    pub fn register<I>(&mut self, plugins: I, executor: Arc<dyn PluginActionExecutor>) -> CoreResult<()>
    where
        I: IntoIterator<Item = PluginId>,
    {
        let plugins = plugins.into_iter().collect::<Vec<_>>();
        let mut unique = BTreeSet::new();
        if plugins.is_empty() {
            return Ok(());
        }
        if plugins
            .iter()
            .any(|plugin| !unique.insert(plugin) || self.providers.contains_key(plugin))
        {
            return Err(CoreError::Invalid(
                "plugin action provider is already registered".to_owned(),
            ));
        }
        for plugin in plugins {
            self.providers.insert(plugin, Arc::clone(&executor));
        }
        Ok(())
    }

    /// Admits an action only to the endpoint registered for `plugin`.
    pub fn submit(
        &self,
        plugin: &PluginId,
        item: &Item,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> CoreResult<ActionRequestId> {
        if item.plugin_id != *plugin {
            return Err(CoreError::Invalid(format!(
                "plugin action item is owned by `{}`, not `{}`",
                item.plugin_id.0, plugin.0
            )));
        }
        self.providers
            .get(plugin)
            .ok_or_else(|| CoreError::Invalid(format!("no action runtime owns plugin `{}`", plugin.0)))?
            .submit_plugin_action(plugin, item, action_id, argument)
    }

    /// Drains all terminal action outcomes without waiting for a plugin.
    pub fn poll(&self) -> Vec<PluginActionCompletion> {
        let mut completions = Vec::new();
        let mut seen = BTreeSet::new();
        for executor in self.providers.values() {
            let key = Arc::as_ptr(executor) as *const () as usize;
            if seen.insert(key) {
                completions.extend(executor.poll_plugin_actions());
            }
        }
        completions
    }

    /// Requests cancellation of one exact-owner action.
    pub fn cancel(&self, request_id: &ActionRequestId) -> bool {
        self.providers
            .get(&request_id.plugin)
            .is_some_and(|executor| executor.cancel_plugin_action(request_id))
    }

    /// Finds a current provider-owned item snapshot by exact item id and
    /// submits to its unique owner.
    ///
    /// Stable ids are opaque and may collide across independently loaded
    /// providers, so an ambiguous match is refused rather than routed to an
    /// arbitrary sibling.
    pub fn submit_by_item_id(
        &self,
        item_id: &ItemId,
        action_id: &ActionId,
        argument: Option<&str>,
    ) -> CoreResult<ActionRequestId> {
        let mut matches = self
            .providers
            .iter()
            .filter(|(plugin, executor)| executor.owns_item(plugin, item_id));
        let Some((plugin, executor)) = matches.next() else {
            return Err(CoreError::Invalid(
                "selected plugin result is no longer current".to_owned(),
            ));
        };
        if matches.next().is_some() {
            return Err(CoreError::Invalid(
                "selected plugin result has ambiguous ownership".to_owned(),
            ));
        }
        executor.submit_plugin_action_by_id(plugin, item_id, action_id, argument)
    }

    /// Returns whether this registry has an exact endpoint for `plugin`.
    pub fn owns(&self, plugin: &PluginId) -> bool {
        self.providers.contains_key(plugin)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    use crikey_core::{
        ActionId, ArgumentPolicy, Category, HitPolicy, Item, ItemId, PluginId, Result as CoreResult,
    };
    use crikey_plugin_model::ConcurrencySection;
    use crikey_plugin_supervisor::{shared_budget_from_section, BudgetKind, PluginBudgetHandle};

    use super::{PluginActionExecutor, PluginActionRouter};

    fn item(plugin: &str) -> Item {
        Item {
            stable_id: ItemId("item".to_owned()),
            plugin_id: PluginId(plugin.to_owned()),
            category: Category::Keyword,
            label: "item".to_owned(),
            description: String::new(),
            target: String::new(),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: HitPolicy::Recorded,
            score_hint: 0,
            metadata: BTreeMap::new(),
            actions: Vec::new(),
        }
    }

    #[derive(Debug, Default)]
    struct RecordingExecutor {
        calls: Mutex<Vec<PluginId>>,
        next: AtomicU64,
    }

    impl PluginActionExecutor for RecordingExecutor {
        fn submit_plugin_action(
            &self,
            plugin: &PluginId,
            _item: &Item,
            _action_id: &ActionId,
            _argument: Option<&str>,
        ) -> CoreResult<super::ActionRequestId> {
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(plugin.clone());
            Ok(super::ActionRequestId {
                plugin: plugin.clone(),
                sequence: self.next.fetch_add(1, Ordering::Relaxed) + 1,
            })
        }
    }

    #[test]
    fn routes_only_to_exact_registered_owner() {
        let executor = Arc::new(RecordingExecutor::default());
        let mut router = PluginActionRouter::default();
        router
            .register([PluginId("modern.alpha".to_owned())], executor.clone())
            .unwrap();
        assert!(router.owns(&PluginId("modern.alpha".to_owned())));
        assert!(!router.owns(&PluginId("native.alpha".to_owned())));
        let request = router
            .submit(
                &PluginId("modern.alpha".to_owned()),
                &item("modern.alpha"),
                &ActionId("run".to_owned()),
                None,
            )
            .unwrap();
        assert_eq!(request.plugin, PluginId("modern.alpha".to_owned()));
        let error = router
            .submit(
                &PluginId("native.alpha".to_owned()),
                &item("native.alpha"),
                &ActionId("run".to_owned()),
                None,
            )
            .expect_err("an unknown owner must not fall through to another provider");
        assert!(error.to_string().contains("no action runtime owns plugin"));
        assert_eq!(
            executor
                .calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_slice(),
            &[PluginId("modern.alpha".to_owned())]
        );
    }

    #[derive(Debug)]
    struct BudgetExecutor {
        budget: PluginBudgetHandle,
        entered: AtomicBool,
        release: AtomicBool,
    }

    impl PluginActionExecutor for BudgetExecutor {
        fn submit_plugin_action(
            &self,
            plugin: &PluginId,
            _item: &Item,
            _action_id: &ActionId,
            _argument: Option<&str>,
        ) -> CoreResult<super::ActionRequestId> {
            let guard = self
                .budget
                .try_acquire_owned(BudgetKind::Action)
                .ok_or(crikey_core::CoreError::CapacityExceeded("action budget"))?;
            self.entered.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                thread::yield_now();
            }
            drop(guard);
            Ok(super::ActionRequestId {
                plugin: plugin.clone(),
                sequence: 1,
            })
        }
    }

    #[test]
    fn action_budget_refuses_a_concurrent_second_call() {
        let budget = shared_budget_from_section(&ConcurrencySection {
            max_action_requests: Some(1),
            ..ConcurrencySection::default()
        });
        let executor = Arc::new(BudgetExecutor {
            budget,
            entered: AtomicBool::new(false),
            release: AtomicBool::new(false),
        });
        let mut router = PluginActionRouter::default();
        router
            .register([PluginId("modern.alpha".to_owned())], executor.clone())
            .unwrap();
        let router = Arc::new(router);
        let worker_router = Arc::clone(&router);
        let worker = thread::spawn(move || {
            worker_router
                .submit(
                    &PluginId("modern.alpha".to_owned()),
                    &item("modern.alpha"),
                    &ActionId("run".to_owned()),
                    None,
                )
                .unwrap();
        });
        while !executor.entered.load(Ordering::Acquire) {
            thread::yield_now();
        }
        let refused = router
            .submit(
                &PluginId("modern.alpha".to_owned()),
                &item("modern.alpha"),
                &ActionId("run".to_owned()),
                None,
            )
            .expect_err("the configured action budget must refuse concurrent work");
        assert!(refused.to_string().contains("action budget"));
        executor.release.store(true, Ordering::Release);
        worker.join().unwrap();
        assert_eq!(executor.budget.in_flight(BudgetKind::Action), 0);
    }

    #[derive(Debug)]
    struct SlowExecutor {
        release: Arc<AtomicBool>,
        completions: Mutex<std::sync::mpsc::Receiver<super::PluginActionCompletion>>,
        sender: std::sync::mpsc::SyncSender<super::PluginActionCompletion>,
        next: AtomicU64,
    }

    impl SlowExecutor {
        fn new() -> Arc<Self> {
            let (sender, completions) = std::sync::mpsc::sync_channel(2);
            Arc::new(Self {
                release: Arc::new(AtomicBool::new(false)),
                completions: Mutex::new(completions),
                sender,
                next: AtomicU64::new(0),
            })
        }
    }

    impl PluginActionExecutor for SlowExecutor {
        fn submit_plugin_action(
            &self,
            plugin: &PluginId,
            item: &Item,
            action_id: &ActionId,
            _argument: Option<&str>,
        ) -> CoreResult<super::ActionRequestId> {
            let request_id = super::ActionRequestId {
                plugin: plugin.clone(),
                sequence: self.next.fetch_add(1, Ordering::Relaxed) + 1,
            };
            let release = Arc::clone(&self.release);
            let sender = self.sender.clone();
            let request_id_for_thread = request_id.clone();
            let plugin = plugin.clone();
            let item_id = item.stable_id.clone();
            let action_id = action_id.clone();
            thread::spawn(move || {
                while !release.load(Ordering::Acquire) {
                    thread::yield_now();
                }
                let _ = sender.send(super::PluginActionCompletion {
                    request_id: request_id_for_thread,
                    plugin,
                    item_id,
                    action_id,
                    outcome: Ok(()),
                });
            });
            Ok(request_id)
        }

        fn poll_plugin_actions(&self) -> Vec<super::PluginActionCompletion> {
            let receiver = self.completions.lock().unwrap_or_else(|error| error.into_inner());
            std::iter::from_fn(|| receiver.try_recv().ok()).collect()
        }
    }

    #[test]
    fn slow_action_submission_does_not_block_following_event() {
        let executor = SlowExecutor::new();
        let mut router = PluginActionRouter::default();
        router
            .register([PluginId("modern.slow".to_owned())], executor.clone())
            .unwrap();

        let request = router
            .submit(
                &PluginId("modern.slow".to_owned()),
                &item("modern.slow"),
                &ActionId("slow".to_owned()),
                None,
            )
            .unwrap();
        // A second event/submission is accepted while the first action is
        // deliberately held before completion.
        let second_request = router
            .submit(
                &PluginId("modern.slow".to_owned()),
                &item("modern.slow"),
                &ActionId("second".to_owned()),
                None,
            )
            .unwrap();
        assert_ne!(second_request, request);
        assert!(router.poll().is_empty());

        executor.release.store(true, Ordering::Release);
        let mut completions = Vec::new();
        for _ in 0..10_000 {
            completions.extend(router.poll());
            if completions.len() == 2 {
                break;
            }
            thread::yield_now();
        }
        assert_eq!(completions.len(), 2);
        assert!(completions.iter().all(|completion| completion.outcome.is_ok()));
        assert!(completions
            .iter()
            .any(|completion| completion.request_id == request));
        assert!(completions
            .iter()
            .any(|completion| completion.request_id == second_request));
    }
}
