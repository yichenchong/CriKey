//! Shared action-dispatch seam for plugin-owned result actions.
//!
//! The search service validates the selected item and action before handing the
//! request to this router. Providers register their exact loaded plugin ids;
//! dispatch therefore cannot fall through to a sibling runtime or reinterpret
//! a stale owner as another plugin.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use crikey_core::{ActionId, CoreError, Item, ItemId, PageInput, PluginId, Result as CoreResult};
use crikey_plugin_model::{FilesystemScope, Permissions};

use crate::plugin_page::PageUpdate;

/// Identifier assigned when a plugin action is admitted to an endpoint.
///
/// The plugin namespace is part of the id so a router can cancel an action or
/// attribute its completion without relying on a process-global counter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActionRequestId {
    pub plugin: PluginId,
    pub sequence: u64,
}

/// What a plugin action did, beyond succeeding.
///
/// An action that opens a page is not a plain success: the launcher must hand
/// the screen to that plugin rather than dismiss, and it can only do that if
/// the completion says so and names the surface. Reporting it as `Ok(())`
/// would leave the caller guessing from a side channel which action, if any,
/// wanted the window kept open (spec 27.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionEffect {
    /// The action ran and is finished.
    Completed,
    /// The action asked the host to open this plugin-drawn page. The owning
    /// plugin is [`PluginActionCompletion::plugin`].
    ShowPage { page_id: String },
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
    pub outcome: CoreResult<ActionEffect>,
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

    /// Opens the named plugin-drawn page and starts asking for frames.
    ///
    /// Returning immediately is part of the contract: a page is a stream of
    /// round trips to a child process, and none of them may happen on the
    /// caller's thread. A runtime with no page support refuses rather than
    /// pretending to open one, so the launcher never shows an empty surface
    /// nothing will ever draw into.
    fn open_plugin_page(
        &self,
        _plugin: &PluginId,
        _page_id: &str,
        _width: u32,
        _height: u32,
        _palette: crikey_core::PagePalette,
    ) -> CoreResult<()> {
        Err(CoreError::Invalid(
            "this plugin runtime cannot draw pages".to_owned(),
        ))
    }

    /// Queues one host-hit-tested page event for the open page.
    fn send_plugin_page_input(&self, _input: PageInput) -> CoreResult<()> {
        Err(CoreError::Invalid("no plugin page is open".to_owned()))
    }

    /// Tells the open page the viewport changed.
    fn resize_plugin_page(&self, _width: u32, _height: u32) {}

    /// Closes the open page, telling its plugin the surface is gone.
    fn close_plugin_page(&self) {}

    /// Takes the newest finished page frame, if one is waiting.
    fn poll_plugin_page(&self) -> Option<PageUpdate> {
        None
    }
}
/// One privileged operation the host performs on a plugin's behalf.
///
/// The variants are exactly the operations a plugin can reach through the
/// host today, and no more. A permission with no operation behind it is not
/// given a variant here: a gate nothing can call would read as enforcement
/// while enforcing nothing, which is the defect this type exists to close.
/// Such permissions are named by `Manifest::unhonoured_declarations` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCapability {
    /// Starting an application through the platform launcher.
    ProcessLaunch,
    /// Handing a path the *user* picked to whatever their desktop registered
    /// for it, through the platform opener.
    ///
    /// Its own variant rather than a second use of [`Self::ProcessLaunch`]
    /// because the two are different questions and a gate that cannot tell
    /// them apart cannot ever be tightened: "start this program with this
    /// argument vector" is a decision this workspace made, while "open this
    /// document with whatever is registered" is a decision the user's desktop
    /// made. A refusal has to be able to name which one was refused.
    ///
    /// It is nonetheless backed by the same `process` permission, and that is
    /// deliberate rather than a shortcut: `xdg-open`, `/usr/bin/open` and
    /// `ShellExecuteExW` all run a `.desktop`, a `.app` or an `.exe` when
    /// handed one, so the authority is genuinely the same authority. Giving it
    /// a weaker field of its own would be a gate that reads as a restriction
    /// while restricting nothing.
    DocumentOpen,
    /// Reading a resource file shipped inside the plugin's own package.
    PackageFileRead,
}

impl HostCapability {
    /// The manifest field an author has to change to grant this.
    fn permission(self) -> &'static str {
        match self {
            Self::ProcessLaunch | Self::DocumentOpen => "process",
            Self::PackageFileRead => "filesystem",
        }
    }

    /// The operation as an operator reading a refusal would name it.
    fn operation(self) -> &'static str {
        match self {
            Self::ProcessLaunch => "process launch",
            Self::DocumentOpen => "document open",
            Self::PackageFileRead => "package resource read",
        }
    }

    fn granted_by(self, permissions: &Permissions) -> bool {
        match self {
            Self::ProcessLaunch | Self::DocumentOpen => permissions.process,
            Self::PackageFileRead => permissions.allows_filesystem_read(FilesystemScope::Package),
        }
    }
}

/// Exact-owner action registry used by [`crate::SearchService`].
///
/// A provider endpoint is registered once for every plugin it loaded. The
/// lookup is exact on the namespaced `PluginId`; no prefix or fallback routing
/// is permitted. Every owner carries a [`Permissions`] value, including the
/// legacy ones that ship no manifest: "no declaration" must not resolve to a
/// skipped check, so the legacy compatibility baseline is written down and
/// consulted like any other grant.
#[derive(Default)]
pub struct PluginActionRouter {
    providers: BTreeMap<PluginId, Arc<dyn PluginActionExecutor>>,
    host_permissions: BTreeMap<PluginId, Permissions>,
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
    /// Registers a legacy endpoint under the legacy compatibility baseline.
    ///
    /// A Keypirinha package has no `crikey.toml` and therefore no author
    /// declaration. It is still registered with an explicit grant set —
    /// [`Permissions::legacy_compatibility_baseline`] — rather than with a
    /// bypass, so a legacy owner travels the same gate as a manifest-governed
    /// one and an operator can read the posture out of `crikey plugin doctor`.
    pub fn register<I>(&mut self, plugins: I, executor: Arc<dyn PluginActionExecutor>) -> CoreResult<()>
    where
        I: IntoIterator<Item = PluginId>,
    {
        self.register_inner(
            plugins
                .into_iter()
                .map(|plugin| (plugin, Permissions::legacy_compatibility_baseline())),
            executor,
        )
    }

    /// Registers a manifest-governed endpoint with its host-mediated grants.
    ///
    /// The grants are checked before a plugin-owned result asks the host to
    /// perform a privileged operation. Keeping this map at the composition
    /// root prevents a provider worker from bypassing the host decision.
    pub fn register_with_permissions<I>(
        &mut self,
        permissions: I,
        executor: Arc<dyn PluginActionExecutor>,
    ) -> CoreResult<()>
    where
        I: IntoIterator<Item = (PluginId, Permissions)>,
    {
        self.register_inner(permissions, executor)
    }

    fn register_inner<I>(&mut self, plugins: I, executor: Arc<dyn PluginActionExecutor>) -> CoreResult<()>
    where
        I: IntoIterator<Item = (PluginId, Permissions)>,
    {
        let plugins = plugins.into_iter().collect::<Vec<_>>();
        let mut unique = BTreeSet::new();
        if plugins.is_empty() {
            return Ok(());
        }
        // A collision on either map is a duplicate: grants can exist without a
        // provider (a host catalog), so checking only the provider map would
        // let a plugin registration silently overwrite the host's own entry.
        if plugins.iter().any(|(plugin, _)| {
            !unique.insert(plugin)
                || self.providers.contains_key(plugin)
                || self.host_permissions.contains_key(plugin)
        }) {
            return Err(CoreError::Invalid(
                "plugin action provider is already registered".to_owned(),
            ));
        }
        for (plugin, permissions) in plugins {
            self.providers.insert(plugin.clone(), Arc::clone(&executor));
            self.host_permissions.insert(plugin, permissions);
        }
        Ok(())
    }

    /// Records the grants of a catalog the host itself produces.
    ///
    /// Discovered applications are published under a builtin owner that has no
    /// plugin runtime and must never receive plugin-owned dispatch, so it gets
    /// no executor here. It still needs an entry: an owner absent from the
    /// grant map is refused, and refusing the host's own launch action would
    /// leave the launcher unable to launch anything. Its one grant is
    /// `process`, written down for the same reason the legacy baseline is —
    /// so the exception is a line of code an auditor can find.
    pub fn register_host_catalog(&mut self, plugin: PluginId) -> CoreResult<()> {
        if self.host_permissions.contains_key(&plugin) {
            return Err(CoreError::Invalid(format!(
                "action grants for plugin `{}` are already registered",
                plugin.0
            )));
        }
        self.host_permissions.insert(
            plugin,
            Permissions {
                process: true,
                ..Permissions::default()
            },
        );
        Ok(())
    }

    /// Whether the exact owner may have the host perform `capability`.
    ///
    /// An owner this router does not know is denied: an unattributable request
    /// is refused rather than resolved to some other plugin's grants.
    pub fn permits(&self, plugin: &PluginId, capability: HostCapability) -> bool {
        self.host_permissions
            .get(plugin)
            .is_some_and(|permissions| capability.granted_by(permissions))
    }

    /// [`Self::permits`] as the refusal the caller should propagate.
    ///
    /// One spelling for every host-mediated seam, so a refusal names the owner
    /// and the manifest field to change wherever it came from, and so no two
    /// call sites drift into two different diagnostics for one decision.
    pub fn authorize(&self, plugin: &PluginId, capability: HostCapability) -> CoreResult<()> {
        if self.permits(plugin, capability) {
            return Ok(());
        }
        Err(CoreError::Invalid(format!(
            "plugin `{}` lacks the {} permission for host-mediated {}",
            plugin.0,
            capability.permission(),
            capability.operation()
        )))
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

    /// Opens a page on the runtime registered for `plugin`.
    ///
    /// An owner this router does not know is refused rather than routed to a
    /// sibling runtime, for the same reason action dispatch is: a page is a
    /// plugin drawing on the user's screen, and the wrong plugin drawing is
    /// worse than no page at all.
    pub fn open_page(
        &self,
        plugin: &PluginId,
        page_id: &str,
        width: u32,
        height: u32,
        palette: crikey_core::PagePalette,
    ) -> CoreResult<()> {
        self.providers
            .get(plugin)
            .ok_or_else(|| CoreError::Invalid(format!("no action runtime owns plugin `{}`", plugin.0)))?
            .open_plugin_page(plugin, page_id, width, height, palette)
    }

    /// Queues one page event for the page `plugin` has open.
    pub fn send_page_input(&self, plugin: &PluginId, input: PageInput) -> CoreResult<()> {
        self.providers
            .get(plugin)
            .ok_or_else(|| CoreError::Invalid(format!("no action runtime owns plugin `{}`", plugin.0)))?
            .send_plugin_page_input(input)
    }

    /// Tells the page `plugin` has open that the viewport changed.
    pub fn resize_page(&self, plugin: &PluginId, width: u32, height: u32) {
        if let Some(executor) = self.providers.get(plugin) {
            executor.resize_plugin_page(width, height);
        }
    }

    /// Closes the page `plugin` has open.
    pub fn close_page(&self, plugin: &PluginId) {
        if let Some(executor) = self.providers.get(plugin) {
            executor.close_plugin_page();
        }
    }

    /// Takes the newest frame the page `plugin` has open produced.
    pub fn poll_page(&self, plugin: &PluginId) -> Option<PageUpdate> {
        self.providers.get(plugin)?.poll_plugin_page()
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
    use crikey_plugin_model::{
        ConcurrencySection, FilesystemAccess, FilesystemPermission, FilesystemScope, Permissions,
    };
    use crikey_plugin_supervisor::{shared_budget_from_section, BudgetKind, PluginBudgetHandle};

    use super::{HostCapability, PluginActionExecutor, PluginActionRouter};

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
    #[test]
    fn host_process_permission_is_enforced_per_manifest_owner() {
        let executor = Arc::new(RecordingExecutor::default());
        let modern = PluginId("modern.denied".to_owned());
        let native = PluginId("native.granted".to_owned());
        let legacy = PluginId("legacy.trusted".to_owned());
        let mut router = PluginActionRouter::default();
        router
            .register_with_permissions([(modern.clone(), Permissions::default())], executor.clone())
            .unwrap();
        router
            .register_with_permissions(
                [(
                    native.clone(),
                    Permissions {
                        process: true,
                        ..Permissions::default()
                    },
                )],
                executor.clone(),
            )
            .unwrap();
        router.register([legacy.clone()], executor).unwrap();

        assert!(!router.permits(&modern, HostCapability::ProcessLaunch));
        assert!(router.permits(&native, HostCapability::ProcessLaunch));
        // A legacy package declares nothing, and the baseline the host applies
        // in its place grants exactly this.
        assert!(router.permits(&legacy, HostCapability::ProcessLaunch));
        assert!(!router.permits(&PluginId("unknown".to_owned()), HostCapability::ProcessLaunch));

        let refusal = router
            .authorize(&modern, HostCapability::ProcessLaunch)
            .expect_err("a plugin without the process grant must be refused");
        assert_eq!(
            refusal.to_string(),
            "plugin `modern.denied` lacks the process permission for host-mediated process launch",
            "a refusal must name the owner and the manifest field that would grant it"
        );
        router
            .authorize(&native, HostCapability::ProcessLaunch)
            .expect("a plugin that declared the process grant must be admitted");
    }

    /// The refusal has to be attributable to one owner. A router that answered
    /// per provider endpoint would deny two plugins sharing one worker because
    /// the stricter of the two declared nothing.
    #[test]
    fn a_refusal_is_scoped_to_one_owner_and_not_to_its_provider_endpoint() {
        let executor = Arc::new(RecordingExecutor::default());
        let denied = PluginId("modern.denied".to_owned());
        let granted = PluginId("modern.granted".to_owned());
        let mut router = PluginActionRouter::default();
        router
            .register_with_permissions(
                [
                    (denied.clone(), Permissions::default()),
                    (
                        granted.clone(),
                        Permissions {
                            process: true,
                            ..Permissions::default()
                        },
                    ),
                ],
                executor,
            )
            .unwrap();

        assert!(router.authorize(&denied, HostCapability::ProcessLaunch).is_err());
        assert!(router.authorize(&granted, HostCapability::ProcessLaunch).is_ok());
    }

    /// The one filesystem read the host performs for a plugin. Undeclared must
    /// keep working, or every plugin written before this gate loses its icons;
    /// an author who declares `none` and nothing else is taken at their word.
    #[test]
    fn the_package_read_grant_is_implicit_but_an_explicit_none_scope_refuses_it() {
        let executor = Arc::new(RecordingExecutor::default());
        let silent = PluginId("modern.silent".to_owned());
        let renouncing = PluginId("modern.renouncing".to_owned());
        let scoped = PluginId("modern.scoped".to_owned());
        let mut router = PluginActionRouter::default();
        router
            .register_with_permissions(
                [
                    (silent.clone(), Permissions::default()),
                    (
                        renouncing.clone(),
                        Permissions {
                            filesystem: vec![FilesystemPermission {
                                scope: FilesystemScope::None,
                                access: FilesystemAccess::Read,
                            }],
                            ..Permissions::default()
                        },
                    ),
                    (
                        scoped.clone(),
                        Permissions {
                            filesystem: vec![FilesystemPermission {
                                scope: FilesystemScope::Package,
                                access: FilesystemAccess::Read,
                            }],
                            ..Permissions::default()
                        },
                    ),
                ],
                executor,
            )
            .unwrap();

        assert!(router.permits(&silent, HostCapability::PackageFileRead));
        assert!(router.permits(&scoped, HostCapability::PackageFileRead));
        let refusal = router
            .authorize(&renouncing, HostCapability::PackageFileRead)
            .expect_err("a plugin that declared no filesystem scope must be refused");
        assert_eq!(
            refusal.to_string(),
            "plugin `modern.renouncing` lacks the filesystem permission for host-mediated \
             package resource read"
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
                    outcome: Ok(super::ActionEffect::Completed),
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
