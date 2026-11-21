//! Native previews pin canonical catalog provenance while sharing provider budgets.
use super::*;
use crate::plugin_preview::{
    PreviewDispatcher, PreviewIdentity, PreviewPublish, PreviewRequest, PreviewState, PreviewUpdate,
};
struct Job {
    request: PreviewRequest,
    publish: PreviewPublish,
    control: Arc<CallControl>,
}
pub(super) struct NativePreviewEndpoint {
    sender: SyncSender<Job>,
    gates: BTreeMap<PluginId, Arc<PreviewActionGate>>,
    incarnations: NativeIncarnations,
    items: Arc<Mutex<BTreeMap<(PluginId, ItemId), (Item, u64)>>>,
    catalog_items: Arc<Mutex<BTreeMap<(PluginId, ItemId), (Item, u64)>>>,
    static_items: Arc<Mutex<BTreeMap<(PluginId, ItemId), Item>>>,
    commands: Arc<Mutex<NativeCommandPublications>>,
    pending: Arc<Mutex<Vec<(PreviewIdentity, Arc<CallControl>)>>>,
    stopped: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
    wake: WakeSlot,
}
impl std::fmt::Debug for NativePreviewEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativePreviewEndpoint").finish_non_exhaustive()
    }
}
impl NativePreviewEndpoint {
    pub(super) fn spawn(provider: &NativeProvider) -> Arc<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Job>(1);
        let pending = Arc::new(Mutex::new(Vec::<(PreviewIdentity, Arc<CallControl>)>::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_supervisors = provider.page_supervisors();
        let endpoint_gates = provider.action_gates();
        let gates = endpoint_gates.clone();
        let incarnations = Arc::clone(&provider.incarnations);
        let thread_incarnations = Arc::clone(&incarnations);
        let thread_pending = Arc::clone(&pending);
        let thread_stopped = Arc::clone(&stopped);
        let thread_commands = Arc::clone(&provider.catalog.commands);
        let thread_catalog_items = Arc::clone(&provider.catalog.items);
        let thread_static_items = Arc::clone(&provider.catalog.static_items);
        let spawned = std::thread::Builder::new()
            .name("crikey-native-preview".into())
            .spawn(move || {
                while !thread_stopped.load(Ordering::Acquire) {
                    let job = match receiver.recv_timeout(Duration::from_millis(10)) {
                        Ok(job) => job,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    let identity = job.request.identity.clone();
                    let state = (|| -> Result<PreviewState, String> {
                        if job.control.requested.load(Ordering::Acquire) {
                            return Ok(PreviewState::Cancelled);
                        }
                        let Some(_guard) = gates
                            .get(&identity.plugin)
                            .and_then(|gate| gate.try_preview(job.control.clone()))
                        else {
                            return Ok(PreviewState::Cancelled);
                        };
                        let supervisor = thread_supervisors
                            .get(&identity.plugin)
                            .ok_or("native preview owner unavailable")?;
                        let started = std::time::Instant::now();
                        let mut supervisor = loop {
                            if job.control.requested.load(Ordering::Acquire) {
                                return Ok(PreviewState::Cancelled);
                            }
                            match supervisor.try_lock() {
                                Ok(supervisor) => break supervisor,
                                Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
                                Err(std::sync::TryLockError::WouldBlock) => {
                                    if started.elapsed() >= Duration::from_secs(5) {
                                        return Err("native preview admission timed out".into());
                                    }
                                    std::thread::sleep(Duration::from_millis(2));
                                }
                            }
                        };
                        refresh_incarnation(&mut supervisor, &identity.plugin, &thread_incarnations);
                        let worker = supervisor
                            .live_worker(&identity.plugin)
                            .ok_or("native preview originating worker unavailable")?;
                        if worker.connection_id() != identity.incarnation {
                            return Ok(PreviewState::Unavailable(
                                "Preview originating worker retired".into(),
                            ));
                        }
                        if let Some(source) = identity.catalog_source.as_ref() {
                            let commands = thread_commands.lock().unwrap_or_else(|error| error.into_inner());
                            let items = thread_catalog_items
                                .lock()
                                .unwrap_or_else(|error| error.into_inner());
                            let static_items = thread_static_items
                                .lock()
                                .unwrap_or_else(|error| error.into_inner());
                            if canonical_preview_item(
                                &commands,
                                &items,
                                &static_items,
                                &identity.plugin,
                                &identity.item,
                                source,
                            )
                            .is_none()
                            {
                                return Ok(PreviewState::Unavailable(
                                    "Preview catalog source retired".into(),
                                ));
                            }
                        }
                        worker.cancel_handle().reset();
                        job.control.install(worker.cancel_handle());
                        let control = Arc::clone(&job.control);
                        let watcher = std::thread::Builder::new()
                            .name("crikey-preview-cancel".into())
                            .spawn(move || control.watch_cancel())
                            .map_err(|error| error.to_string())?;
                        let outcome = worker.preview_with_cancel_latched(
                            &identity.item.0,
                            identity.request,
                            &job.control.requested,
                        );
                        job.control.finish();
                        let _ = watcher.join();
                        refresh_incarnation(&mut supervisor, &identity.plugin, &thread_incarnations);
                        if job.control.requested.load(Ordering::Acquire) {
                            return Ok(PreviewState::Cancelled);
                        }
                        match outcome.map_err(|error| error.to_string())? {
                            crikey_core::preview::PreviewOutcome::Content(content) => {
                                content.validate().map_err(|error| error.to_string())?;
                                Ok(PreviewState::Content(Arc::new(crikey_ui::prepare_preview(
                                    Arc::new(content),
                                )?)))
                            }
                            crikey_core::preview::PreviewOutcome::Empty => Ok(PreviewState::Empty),
                            crikey_core::preview::PreviewOutcome::Unsupported => Ok(
                                PreviewState::Unavailable("worker does not support preview_v1".into()),
                            ),
                            crikey_core::preview::PreviewOutcome::Cancelled => Ok(PreviewState::Cancelled),
                        }
                    })()
                    .unwrap_or_else(PreviewState::Error);
                    thread_pending
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .retain(|(candidate, _)| candidate != &identity);
                    (job.publish)(PreviewUpdate { identity, state });
                }
                while let Ok(job) = receiver.try_recv() {
                    (job.publish)(PreviewUpdate {
                        identity: job.request.identity,
                        state: PreviewState::Cancelled,
                    });
                }
            });
        if spawned.is_err() {
            stopped.store(true, Ordering::Release);
        }
        Arc::new(Self {
            sender,
            gates: endpoint_gates,
            incarnations,
            items: Arc::clone(&provider.action_items),
            catalog_items: Arc::clone(&provider.catalog.items),
            static_items: Arc::clone(&provider.catalog.static_items),
            commands: Arc::clone(&provider.catalog.commands),
            pending,
            stopped,
            worker: Mutex::new(spawned.ok()),
            wake: WakeSlot::default(),
        })
    }
    pub(super) fn lifecycle_wake(&self) -> PageWake {
        self.wake.handle()
    }
    pub(super) fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        for (_, control) in self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
        {
            control.cancel();
        }
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            let _ = worker.join();
        }
    }
}
impl PreviewDispatcher for NativePreviewEndpoint {
    fn foreground_blocked(&self, owner: &PluginId) -> bool {
        self.gates.get(owner).is_some_and(|gate| gate.has_foreground())
    }

    fn foreground_conflicts(&self, preview_owner: &PluginId, action_owner: &PluginId) -> bool {
        preview_owner == action_owner && self.gates.contains_key(preview_owner)
    }

    fn lookup(
        &self,
        plugin: &PluginId,
        id: &ItemId,
        source: Option<&crate::plugin_action::DeclaredCommandSource>,
    ) -> Option<(Item, u64)> {
        if let Some(source) = source {
            let commands = self.commands.lock().unwrap_or_else(|error| error.into_inner());
            let items = self
                .catalog_items
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let static_items = self
                .static_items
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            return canonical_preview_item(&commands, &items, &static_items, plugin, id, source)
                .map(|(item, producer)| (item.clone(), producer));
        }
        let key = (plugin.clone(), id.clone());
        let query = self
            .items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .cloned();
        if query
            .as_ref()
            .is_some_and(|(_, origin)| self.incarnation(plugin) == Some(*origin))
        {
            return query;
        }
        self.catalog_items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .cloned()
            .or(query)
    }
    fn origin(
        &self,
        plugin: &PluginId,
        id: &ItemId,
        source: Option<&crate::plugin_action::DeclaredCommandSource>,
    ) -> Option<u64> {
        if let Some(source) = source {
            let commands = self.commands.lock().unwrap_or_else(|error| error.into_inner());
            let items = self
                .catalog_items
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let static_items = self
                .static_items
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            return canonical_preview_item(&commands, &items, &static_items, plugin, id, source)
                .map(|(_, producer)| producer);
        }
        let key = (plugin.clone(), id.clone());
        let query = self
            .items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .map(|(_, producer)| *producer);
        if query.is_some() && query == self.incarnation(plugin) {
            return query;
        }
        self.catalog_items
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .map(|(_, producer)| *producer)
            .or(query)
    }
    fn incarnation(&self, plugin: &PluginId) -> Option<u64> {
        if self.stopped.load(Ordering::Acquire) {
            return None;
        }
        // UI readers never acquire a supervisor or poll/reap a process.
        self.incarnations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(plugin)
            .copied()
    }
    fn set_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.wake.install(wake);
    }
    fn submit(&self, request: PreviewRequest, publish: PreviewPublish) -> Result<(), String> {
        let identity = request.identity.clone();
        if self.incarnation(&identity.plugin) != Some(identity.incarnation) {
            return Err("native preview incarnation unavailable".into());
        }
        let (item, producer) = self
            .lookup(&identity.plugin, &identity.item, identity.catalog_source.as_ref())
            .ok_or("native preview item is no longer current")?;
        if producer != identity.incarnation {
            return Err("native preview originating worker retired".into());
        }
        if item.metadata.get("crikey.preview").map(String::as_str) != Some("deferred") {
            return Err("item does not declare deferred preview".into());
        }
        let mut pending = self.pending.lock().unwrap_or_else(|error| error.into_inner());
        if pending.len() >= 2 {
            return Err("native preview mailbox is full".into());
        }
        let control = Arc::new(CallControl::new());
        pending.push((identity.clone(), Arc::clone(&control)));
        if self
            .sender
            .try_send(Job {
                request,
                publish,
                control,
            })
            .is_err()
        {
            pending.retain(|(candidate, _)| candidate != &identity);
            return Err("native preview mailbox is full or unavailable".into());
        }
        Ok(())
    }
    fn cancel(&self, identity: &PreviewIdentity) {
        for (candidate, control) in self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
        {
            if candidate == identity {
                control.cancel();
            }
        }
    }
}

pub(super) fn canonical_preview_item<'a>(
    commands: &NativeCommandPublications,
    items: &'a BTreeMap<(PluginId, ItemId), (Item, u64)>,
    static_items: &'a BTreeMap<(PluginId, ItemId), Item>,
    owner: &PluginId,
    item: &ItemId,
    source: &crate::plugin_action::DeclaredCommandSource,
) -> Option<(&'a Item, u64)> {
    let publication = commands.owners.get(owner)?;
    if publication.status != NativeCommandStatus::Ready
        || publication.instance != source.instance
        || publication.generation != source.generation
        || publication.producer != source.producer
        || publication.revision != source.publication
    {
        return None;
    }
    let key = (owner.clone(), item.clone());
    items
        .get(&key)
        .filter(|(item, producer)| item.plugin_id == *owner && *producer == source.producer)
        .map(|(item, producer)| (item, *producer))
        .or_else(|| {
            static_items
                .get(&key)
                .filter(|item| item.plugin_id == *owner)
                .map(|item| (item, source.producer))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{App, ModernDriver, ModernProvider, PipelineConfig, PreviewService, SearchService};
    use crikey_core::PreviewText;
    use crikey_ui::{LauncherViewModel, PreviewStatus, UiCommand};
    use std::process::Command;
    use std::sync::LazyLock;

    #[test]
    fn canonical_preview_excludes_same_id_suggestion_and_pins_publication() {
        let owner = PluginId("native.preview".into());
        let id = ItemId("shared".into());
        let source = crate::plugin_action::DeclaredCommandSource {
            instance: 3,
            generation: Generation::ZERO,
            producer: 7,
            publication: 99,
        };
        let catalog = Item {
            stable_id: id.clone(),
            plugin_id: owner.clone(),
            category: crikey_core::Category::Application,
            label: "Canonical".into(),
            description: String::new(),
            target: String::new(),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: crikey_core::ArgumentPolicy::Forbidden,
            hit_policy: crikey_core::HitPolicy::Recorded,
            score_hint: 0,
            metadata: BTreeMap::from([("crikey.preview".into(), "deferred".into())]),
            actions: Vec::new(),
        };
        let mut suggestion = catalog.clone();
        suggestion.label = "Suggestion".into();
        suggestion.metadata.clear();
        let mut commands = NativeCommandPublications::default();
        commands.owners.insert(
            owner.clone(),
            Arc::new(NativeCommandSnapshot {
                plugin: owner.clone(),
                revision: source.publication,
                instance: source.instance,
                generation: source.generation,
                producer: source.producer,
                status: NativeCommandStatus::Ready,
                released: false,
                declarations: Arc::from([]),
            }),
        );
        let mut incarnations = NativeIncarnationState::default();
        incarnations.insert(owner.clone(), source.producer);
        let (sender, receiver) = mpsc::sync_channel(1);
        let endpoint = NativePreviewEndpoint {
            sender,
            gates: BTreeMap::from([(
                owner.clone(),
                PreviewActionGate::new(crikey_plugin_supervisor::shared_budget_from_section(
                    &Default::default(),
                )),
            )]),
            incarnations: Arc::new(Mutex::new(incarnations)),
            items: Arc::new(Mutex::new(BTreeMap::from([(
                (owner.clone(), id.clone()),
                (suggestion, source.producer),
            )]))),
            catalog_items: Arc::new(Mutex::new(BTreeMap::from([(
                (owner.clone(), id.clone()),
                (catalog, source.producer),
            )]))),
            static_items: Arc::new(Mutex::new(BTreeMap::new())),
            commands: Arc::new(Mutex::new(commands)),
            pending: Arc::new(Mutex::new(Vec::new())),
            stopped: Arc::new(AtomicBool::new(false)),
            worker: Mutex::new(None),
            wake: WakeSlot::default(),
        };
        assert_eq!(endpoint.lookup(&owner, &id, None).unwrap().0.label, "Suggestion");
        assert_eq!(
            endpoint.lookup(&owner, &id, Some(&source)).unwrap().0.label,
            "Canonical"
        );
        assert_eq!(endpoint.origin(&owner, &id, Some(&source)), Some(7));
        let identity = PreviewIdentity {
            view: 1,
            query: Generation::ZERO,
            plugin: owner.clone(),
            incarnation: 7,
            item: id.clone(),
            target: crikey_ui::ActionTarget::Command(crikey_core::CommandRef {
                provider_id: owner.clone(),
                command_id: "canonical".into(),
            }),
            catalog_source: Some(source.clone()),
            selection: 1,
            request: 1,
        };
        endpoint
            .submit(
                PreviewRequest {
                    identity: identity.clone(),
                },
                Arc::new(|_| {}),
            )
            .unwrap();
        assert_eq!(receiver.try_recv().unwrap().request.identity, identity);
        for field in 0..4 {
            let mut stale = source.clone();
            match field {
                0 => stale.instance += 1,
                1 => stale.generation = Generation::from_raw(1),
                2 => stale.producer += 1,
                3 => stale.publication += 1,
                _ => unreachable!(),
            }
            assert!(endpoint.lookup(&owner, &id, Some(&stale)).is_none());
        }
        let (static_item, _) = endpoint
            .catalog_items
            .lock()
            .unwrap()
            .remove(&(owner.clone(), id.clone()))
            .unwrap();
        endpoint
            .static_items
            .lock()
            .unwrap()
            .insert((owner.clone(), id.clone()), static_item);
        endpoint.incarnations.lock().unwrap().remove(&owner);
        assert_eq!(
            endpoint.lookup(&owner, &id, Some(&source)).unwrap().0.label,
            "Canonical"
        );
        assert_eq!(endpoint.origin(&owner, &id, Some(&source)), Some(source.producer));
        assert!(
            endpoint
                .submit(
                    PreviewRequest {
                        identity: identity.clone()
                    },
                    Arc::new(|_| {})
                )
                .is_err(),
            "retained static metadata never grants deferred preview to a replacement"
        );
        endpoint.commands.lock().unwrap().owners.clear();
        assert!(endpoint.lookup(&owner, &id, Some(&source)).is_none());
        assert!(endpoint.lookup(&owner, &id, None).is_some());
        assert!(endpoint
            .submit(PreviewRequest { identity }, Arc::new(|_| {}))
            .is_err());
    }

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn preview_executable() -> PathBuf {
        static BINARY: LazyLock<PathBuf> = LazyLock::new(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
            let target = root.join("target/preview-conformance");
            let output = Command::new("cargo")
                .current_dir(&root)
                .args([
                    "build",
                    "-p",
                    "crikey-plugin-sdk",
                    "--example",
                    "preview",
                    "--target-dir",
                ])
                .arg(&target)
                .output()
                .expect("build the real SDK preview example");
            assert!(
                output.status.success(),
                "SDK example build failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            target
                .join("debug/examples")
                .join(if cfg!(windows) { "preview.exe" } else { "preview" })
        });
        BINARY.clone()
    }

    fn wait_for<T>(stage: &str, mut observe: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(value) = observe() {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "native preview consumer did not finish: {stage}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn selected_worker_text(frame: &crikey_ui::ViewModel) -> Option<String> {
        let PreviewStatus::Content(content) = frame.preview.status.as_ref()? else {
            return None;
        };
        let Some(PreviewText::Plain(text)) = &content.content.text else {
            return None;
        };
        text.starts_with("Selected text; worker process ")
            .then(|| text.clone())
    }

    fn expected_worker_text(frame: &crikey_ui::ViewModel, stage: &str) -> Option<String> {
        match frame.preview.status.as_ref() {
            None | Some(PreviewStatus::Loading) => None,
            Some(PreviewStatus::Content(_)) => {
                Some(selected_worker_text(frame).unwrap_or_else(|| {
                    panic!("{stage}: unexpected worker content: {:?}", frame.preview.status)
                }))
            }
            Some(status) => panic!("{stage}: terminal worker preview failure: {status:?}"),
        }
    }

    #[test]
    fn retired_query_item_cannot_preview_in_action_replacement_but_fresh_item_can() {
        let executable = preview_executable().canonicalize().unwrap();
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let scratch = Scratch(std::env::temp_dir().join(format!(
            "crikey-native-preview-origin-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        fs::create_dir_all(&scratch.0).unwrap();
        let package = scratch.0.join("preview-demo");
        fs::create_dir_all(&package).unwrap();
        let executable = executable
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        fs::write(package.join("crikey.toml"), format!(
            "manifest-version = 1\n[plugin]\nid = \"preview-demo\"\nname = \"Preview\"\nversion = \"0.1.0\"\nruntime = \"native\"\nentrypoint = \"{executable}\"\n"
        )).unwrap();

        let mut pipeline = QueryPipeline::new(PipelineConfig::default());
        let mut provider = NativeProvider::load_with_collection_window(
            &mut pipeline,
            &[scratch.0.clone()],
            Duration::from_secs(10),
            &DisabledPlugins::default(),
        );
        assert_eq!(
            provider.plugins(),
            &[PluginId("native.preview-demo".into())],
            "real preview package must load: {:?}",
            provider.unavailable()
        );
        let plugin = PluginId("native.preview-demo".into());
        provider
            .request_catalog_build(&plugin, 1, Generation::ZERO)
            .unwrap();
        let build = wait_for("initial catalog", || {
            provider.take_catalog_results().into_iter().next()
        });
        let crate::CatalogBuildResult::Complete(build) = build else {
            panic!("real catalog must complete");
        };
        let item = build
            .items
            .into_iter()
            .find(|item| item.stable_id.0 == "text")
            .unwrap();
        let frame = provider
            .drive_query(&mut pipeline, "catalog-only", 1)
            .expect("empty suggestion frame");
        assert!(
            frame.rows.is_empty(),
            "only catalog construction may initialize this preview"
        );
        let old_row = crikey_ui::ResultRow {
            target: crikey_ui::ActionTarget::Result {
                owner: item.plugin_id.clone(),
                item: item.stable_id.clone(),
            },
            source: Some(crikey_ui::ResultSource {
                owner: item.plugin_id,
                item: item.stable_id,
                kind: crikey_ui::ResultSourceKind::Ordinary,
            }),
            command_state: None,
            label: item.label,
            description: item.description,
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
        let driver = NativeDriver::spawn(provider, pipeline, Box::new(|_| {}));
        let endpoint = &driver.preview_endpoint;
        let (old_item, old_origin) = endpoint
            .lookup(
                &old_row.source.as_ref().unwrap().owner,
                &old_row.source.as_ref().unwrap().item,
                None,
            )
            .expect("published producer snapshot");
        let supervisor = driver
            .action_endpoint
            .page_supervisors
            .get(&old_item.plugin_id)
            .unwrap();
        {
            // The live snapshot remains available while a query/action owns the
            // supervisor: looking up an item never waits for that worker lock.
            let _busy = supervisor.lock().unwrap();
            assert_eq!(endpoint.incarnation(&old_item.plugin_id), Some(old_origin));
            assert_eq!(
                endpoint
                    .lookup(&old_item.plugin_id, &old_item.stable_id, None)
                    .unwrap()
                    .1,
                old_origin
            );
        }

        let mut modern_pipeline = QueryPipeline::new(PipelineConfig::default());
        let modern_provider = ModernProvider::load(
            &mut modern_pipeline,
            &[],
            None,
            scratch.0.join("modern-cache"),
            &DisabledPlugins::default(),
        );
        let modern = ModernDriver::spawn(modern_provider, modern_pipeline, |_| {});
        let mut service = PreviewService::new(&driver, &modern, || {});
        let search = SearchService::new(App::new());
        let mut model = LauncherViewModel::new();
        model.activate();
        let _ = model.apply(UiCommand::SetQuery("text".into()));
        model.begin_generation(frame.generation);
        model.publish(frame.generation, vec![old_row.clone()], false);
        let old_text = wait_for("catalog-only selected preview", || {
            service.synchronize(&mut model, &search);
            model
                .frame()
                .and_then(|frame| expected_worker_text(&frame, "catalog-only selected preview"))
        });
        // Only after proving catalog-only preview, wait for the actual final
        // query result to establish the action snapshot before retiring it.
        let action_generation = Generation::from_raw(frame.generation.get() + 1);
        driver.submit(action_generation, "text", 2, Vec::new(), false, 0);
        wait_for("completed action query rendered item", || {
            driver.take_outcome().filter(|frame| {
                frame.generation == action_generation
                    && !frame.pending_plugins
                    && frame.rows.iter().any(|row| row.target == old_row.target)
            })
        });

        {
            let mut supervisor = supervisor.lock().unwrap();
            supervisor.live_worker(&old_item.plugin_id).unwrap().kill();
            // Observe failure at deterministic time zero, then advance the
            // existing supervisor clock exactly to its admissible retry.
            assert!(supervisor.worker(&old_item.plugin_id, 0).is_err());
            let retry = supervisor
                .next_retry_at_ms(&old_item.plugin_id)
                .expect("restart backoff");
            supervisor
                .worker(&old_item.plugin_id, retry)
                .expect("admissible replacement");
        }
        let actions = driver.action_executor();
        let action_id = old_item
            .actions
            .first()
            .expect("real plugin-owned fixture action")
            .action_id
            .clone();
        let action = actions
            .submit_plugin_action(&old_item.plugin_id, &old_item, &action_id, None)
            .unwrap();
        let completion = wait_for("replacement action completion", || {
            actions
                .poll_plugin_actions()
                .into_iter()
                .find(|completion| completion.request_id == action)
        });
        let refusal = completion
            .outcome
            .expect_err("a retired dynamic origin must not execute on its replacement");
        assert!(
            refusal.to_string().contains("originating worker was replaced"),
            "{refusal}"
        );
        assert!(
            !supervisor
                .lock()
                .unwrap()
                .live_worker(&old_item.plugin_id)
                .unwrap()
                .observations()
                .iter()
                .any(|event| event.kind == "execute"),
            "refused stale admission must not send execution to the replacement"
        );
        let replacement = endpoint
            .incarnation(&old_item.plugin_id)
            .expect("action replacement alive");
        assert_ne!(replacement, old_origin);
        assert_eq!(
            endpoint
                .lookup(&old_item.plugin_id, &old_item.stable_id, None)
                .unwrap()
                .1,
            old_origin,
            "retained item must preserve its retired producer, never acquire replacement ownership"
        );
        service.synchronize(&mut model, &search);
        let retired_frame = model.frame().expect("retirement must publish a frame");
        assert_eq!(
            retired_frame.preview.status,
            Some(crikey_ui::PreviewStatus::Unavailable(
                "Preview owner unavailable".into()
            ))
        );
        assert!(retired_frame.preview.link_target.is_none());
        let selection = model.preview_selection().unwrap();
        let old_identity = PreviewIdentity {
            view: selection.view.get(),
            query: selection.generation,
            plugin: old_item.plugin_id.clone(),
            incarnation: old_origin,
            item: old_item.stable_id.clone(),
            target: selection.target.clone(),
            catalog_source: None,
            selection: selection.revision,
            request: 91,
        };
        let publish: PreviewPublish =
            Arc::new(|_| panic!("rejected old item must not publish a callback result"));
        assert!(endpoint
            .submit(
                PreviewRequest {
                    identity: old_identity.clone()
                },
                Arc::clone(&publish)
            )
            .is_err());
        let mut retagged = old_identity;
        retagged.incarnation = replacement;
        assert!(
            endpoint
                .submit(PreviewRequest { identity: retagged }, publish)
                .is_err(),
            "even a replacement-tagged request cannot inherit the old item"
        );

        // Reopening the old selected row cannot display worker-produced content
        // from the replacement. Restore the query and use a live generation:
        // dismissal clears the query but preserves the generation floor.
        model.dismiss();
        model.activate();
        let _ = model.apply(UiCommand::SetQuery("text".into()));
        model.begin_generation(action_generation);
        model.publish(action_generation, vec![old_row.clone()], false);
        let reopened_selection = model
            .preview_selection()
            .expect("old row must really be selected");
        assert_eq!(reopened_selection.generation, action_generation);
        assert_eq!(reopened_selection.item, old_item.stable_id);
        service.synchronize(&mut model, &search);
        let reopened_frame = model.frame().expect("reopening must publish a frame");
        assert!(selected_worker_text(&reopened_frame).is_none());
        assert!(
            !supervisor
                .lock()
                .unwrap()
                .live_worker(&old_item.plugin_id)
                .unwrap()
                .observations()
                .iter()
                .any(|event| event.kind == "preview_request"),
            "the replacement must never receive a preview for a retired producer"
        );

        let fresh_generation = Generation::from_raw(action_generation.get() + 1);
        driver.submit(fresh_generation, "text", 2, Vec::new(), false, 0);
        let fresh = wait_for("fresh query rendered item", || {
            driver.take_outcome().filter(|frame| {
                frame.generation == fresh_generation
                    && frame.rows.iter().any(|row| row.target == old_row.target)
            })
        });
        assert_eq!(
            endpoint
                .lookup(&old_item.plugin_id, &old_item.stable_id, None)
                .unwrap()
                .1,
            replacement,
            "fresh publication must carry the actual replacement producer"
        );
        model.begin_generation(fresh.generation);
        model.publish(
            fresh.generation,
            vec![fresh
                .rows
                .iter()
                .find(|row| row.target == old_row.target)
                .unwrap()
                .clone()],
            false,
        );
        let fresh_text = wait_for("fresh replacement selected preview", || {
            service.synchronize(&mut model, &search);
            model
                .frame()
                .and_then(|frame| expected_worker_text(&frame, "fresh replacement selected preview"))
        });
        assert_ne!(
            fresh_text, old_text,
            "visible content must come from the fresh producing process"
        );
        drop(service);
        drop(modern);
        drop(driver);
    }
}
