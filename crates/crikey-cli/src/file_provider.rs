//! The launcher's own file search, driven off the UI thread (spec 18.1).
//!
//! The platform backends behind [`crikey_app::FileItemSearch`] are the part
//! that finds files; this module is the part that decides *when* to ask them,
//! what to do with a late answer, and how a row the user selects finds its way
//! back to the item it came from.
//!
//! Three things shape it:
//!
//! * **Off the UI thread.** A search crosses a process boundary — a desktop
//!   index, or a directory walk — so it is handed to a worker thread and the
//!   caller returns immediately, exactly as the legacy, modern and native
//!   providers do. That is what makes a one-second deadline affordable.
//! * **One in-flight search.** Replace-oldest with a single slot: while a
//!   search is running the user is still typing, and only the newest query is
//!   worth answering. Each new query cancels the previous one's token and any
//!   answer that arrives for a superseded generation is dropped rather than
//!   shown.
//! * **The items are retained.** A file item is not in the catalog, so
//!   `SearchService::execute` cannot find it: the rows would render and then
//!   refuse to open. The driver therefore keeps the items it published, keyed
//!   by owner and item id and scoped to the generation that produced them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crikey_app::{BatchState, FileItemSearch, QueryPipeline, ResultBatch};
use crikey_core::{Generation, Item, ItemId, PluginId};
use crikey_input_scheduler::{
    ActivationPolicy, DebouncePolicy, PluginPolicy, QueuePolicy, SchedulingProfile,
};
use crikey_platform::{CancelToken, FileSearchCoverage, FileSearchQuery, MAX_FILE_HITS};
use crikey_ui::{ResultRow, ViewModel};

/// Owner of the host's own file-search results (spec 10.2 namespacing).
pub(crate) const FILE_SEARCH_PLUGIN: &str = "builtin.crikey.files";

/// How long a backend may spend on one search.
///
/// A whole second, which would be indefensible anywhere else in the query
/// path: it is affordable here and only here because the search runs on this
/// module's worker thread. The UI thread hands the query over and returns
/// within microseconds, so the deadline bounds the *search* rather than the
/// frame. What keeps a generous deadline from wasting work is the cancel token
/// — every keystroke cancels the one before it — not a short clock.
const FILE_SEARCH_DEADLINE: Duration = Duration::from_secs(1);

/// Characters of the query the backend is actually asked about.
///
/// A backend matches a name by case-insensitive substring, so the hits for a
/// two-character prefix are a superset of the hits for every longer query that
/// extends it. One call per prefix therefore serves a whole typing burst: the
/// answer is cached and each further keystroke filters it in memory. Two rather
/// than one because a single character selects most of a home directory, and
/// three would re-query on the third keystroke of every word.
const PREFIX_CHARS: usize = 2;

/// Scheduling policy for the file provider.
///
/// Deliberately not the application provider's leading-edge, zero-debounce
/// policy: that one scans memory, this one crosses a process boundary. The
/// leading edge is off because it would fire on the first character — the
/// broadest and most expensive query there is, and the one most certain to be
/// superseded before anybody reads it. The trailing edge, 120 ms after the
/// user stops, is the query worth running, and `maximum_wait_ms` bounds how
/// long a continuous typist waits for a first answer.
pub(crate) fn file_provider_policy() -> PluginPolicy {
    PluginPolicy {
        profile: SchedulingProfile::Modern,
        debounce: DebouncePolicy {
            debounce_ms: 120,
            maximum_wait_ms: Some(300),
            leading_edge: false,
            trailing_edge: true,
            minimum_query_length: 0,
        },
        activation: ActivationPolicy {
            // Nothing to search for: an empty query must not walk the user's
            // home directory. The prefix cache replaces a minimum length, so
            // this is the only query length the provider refuses.
            supports_empty_query: false,
            prefixes: Vec::new(),
            keywords: Vec::new(),
            patterns: Vec::new(),
        },
        max_concurrent_requests: 1,
        queue_policy: QueuePolicy::ReplaceOldest,
        queue_capacity: 1,
    }
}

/// Builds the file search the worker thread will own.
///
/// A factory rather than a value because the implementation is built on the
/// worker thread: a platform backend is not required to be `Send`, and nothing
/// about it needs to cross a thread boundary if it is never constructed on the
/// wrong side of one.
pub(crate) type FileSearchFactory = Box<dyn FnOnce() -> Box<dyn FileItemSearch> + Send>;

/// One query handed to the worker.
struct FileJob {
    /// Search generation this answer will be published under — the launcher's,
    /// not the pipeline's.
    generation: Generation,
    query: String,
    now: u64,
    cancel: CancelToken,
    builtin_rows: Vec<ResultRow>,
    builtin_pending: bool,
    selected: usize,
}

/// The single-slot mailbox between the UI thread and the worker.
struct FileMailbox {
    job: Option<FileJob>,
    stop: bool,
}

/// The hits of one prefix, kept so the rest of a typing burst costs no search.
struct PrefixCache {
    /// Normalized prefix these items answer, at most [`PREFIX_CHARS`] long.
    prefix: String,
    /// At most [`MAX_FILE_HITS`] items: a prefix answer that reached the limit
    /// was truncated and is therefore not a superset of anything, so it is
    /// never cached.
    items: Vec<Item>,
}

/// The file items behind the rows currently on screen.
///
/// Keyed by owner and item id because a stable id is only unique within its
/// owner. Cleared once per query rather than once per batch: a generation can
/// be answered across several batches, and clearing per batch would throw away
/// the items the previous batch just published.
#[derive(Default)]
struct PublishedFileItems {
    generation: Option<Generation>,
    items: BTreeMap<(PluginId, ItemId), Item>,
}

/// The launcher's file provider: a worker thread, one in-flight search, and the
/// items it published.
pub(crate) struct FileSearchDriver {
    owner: PluginId,
    mailbox: Arc<(Mutex<FileMailbox>, Condvar)>,
    outcome: Arc<Mutex<Option<ViewModel>>>,
    /// Newest search generation the UI submitted. The worker re-reads it before
    /// publishing and drops an answer that is no longer current.
    current: Arc<AtomicU64>,
    /// Cancel token of the search that is or is about to be in flight.
    cancel: Mutex<Option<CancelToken>>,
    published: Arc<Mutex<PublishedFileItems>>,
    forget_cache: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl FileSearchDriver {
    /// Moves `pipeline` and a freshly built file search onto a worker thread and
    /// returns a handle the UI thread drives without ever blocking.
    ///
    /// `pipeline` must already have `owner` registered with
    /// [`file_provider_policy`]. `publish` runs on the worker thread with each
    /// merged frame. A thread that fails to spawn degrades to a driver that
    /// accepts queries and answers nothing, rather than a panic in the
    /// composition root.
    pub(crate) fn spawn<P>(
        owner: PluginId,
        pipeline: QueryPipeline,
        search: FileSearchFactory,
        publish: P,
    ) -> Self
    where
        P: Fn(&ViewModel) + Send + 'static,
    {
        let mailbox = Arc::new((
            Mutex::new(FileMailbox {
                job: None,
                stop: false,
            }),
            Condvar::new(),
        ));
        let outcome = Arc::new(Mutex::new(None));
        let current = Arc::new(AtomicU64::new(0));
        let published = Arc::new(Mutex::new(PublishedFileItems::default()));
        let forget_cache = Arc::new(AtomicBool::new(false));

        let thread_mailbox = Arc::clone(&mailbox);
        let thread_outcome = Arc::clone(&outcome);
        let thread_current = Arc::clone(&current);
        let thread_owner = owner.clone();
        let thread_published = Arc::clone(&published);
        let thread_forget = Arc::clone(&forget_cache);
        let spawned = std::thread::Builder::new()
            .name("crikey-files".to_owned())
            .spawn(move || {
                // Assembled here rather than by the caller: the search is the
                // one part of this worker that may not be `Send`, so it is built
                // on the thread that will own it for the rest of the session.
                let mut worker = FileSearchWorker {
                    owner: thread_owner,
                    pipeline,
                    cache: None,
                    published: thread_published,
                    forget_cache: thread_forget,
                    search: search(),
                    last_error: None,
                };
                let (lock, cvar) = &*thread_mailbox;
                loop {
                    let job = {
                        let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
                        loop {
                            if slot.stop {
                                return;
                            }
                            if let Some(job) = slot.job.take() {
                                break job;
                            }
                            slot = cvar
                                .wait_timeout(slot, Duration::from_millis(10))
                                .unwrap_or_else(|error| error.into_inner())
                                .0;
                        }
                    };

                    // The search happens here, on this thread, never on the
                    // caller's.
                    let merged = worker.answer(&job);

                    // A late answer must never appear under a newer generation.
                    // The whole check-store-publish is held under the mailbox
                    // lock so the gate cannot race a `submit`: `submit` records
                    // the newer generation into `current` before it takes this
                    // lock, so either we observe the newer generation and drop
                    // this frame, or no supersession has happened yet. A job
                    // already queued is likewise a supersession in flight.
                    let slot = lock.lock().unwrap_or_else(|error| error.into_inner());
                    if slot.stop
                        || slot.job.is_some()
                        || thread_current.load(Ordering::Acquire) != job.generation.get()
                    {
                        continue;
                    }
                    *thread_outcome.lock().unwrap_or_else(|error| error.into_inner()) = Some(merged.clone());
                    publish(&merged);
                    drop(slot);
                }
            });

        Self {
            owner,
            mailbox,
            outcome,
            current,
            cancel: Mutex::new(None),
            published,
            forget_cache,
            worker: spawned.ok(),
        }
    }

    /// Submits a query for asynchronous file search and returns at once.
    ///
    /// `builtin_rows` are the built-in provider's rows for `generation`, which
    /// the merged frame keeps ahead of the file rows.
    pub(crate) fn submit(
        &self,
        generation: Generation,
        query: &str,
        now: u64,
        builtin_rows: Vec<ResultRow>,
        builtin_pending: bool,
        selected: usize,
    ) {
        // Intake is monotonic: a delayed caller must not rewind the live
        // generation and make an obsolete job eligible for publication again.
        let generation_value = generation.get();
        let mut observed = self.current.load(Ordering::Acquire);
        loop {
            if generation_value < observed {
                return;
            }
            match self.current.compare_exchange_weak(
                observed,
                generation_value,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(next) => observed = next,
            }
        }

        // Cancel before queueing: the point of the token is that a backend
        // already inside a walk stops as soon as it can, which is now, not
        // whenever the worker next looks at its mailbox.
        let cancel = CancelToken::new();
        if let Some(previous) = self
            .cancel
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .replace(cancel.clone())
        {
            previous.cancel();
        }

        // The rows about to be replaced are the rows whose items were retained,
        // so the store is emptied and re-stamped in the same breath.
        {
            let mut published = self.published.lock().unwrap_or_else(|error| error.into_inner());
            published.generation = Some(generation);
            published.items.clear();
        }

        let (lock, cvar) = &*self.mailbox;
        let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
        if slot.stop {
            return;
        }
        slot.job = Some(FileJob {
            generation,
            query: query.to_owned(),
            now,
            cancel,
            builtin_rows,
            builtin_pending,
            selected,
        });
        drop(slot);
        cvar.notify_one();
    }

    /// Takes the latest merged frame the worker produced, for the UI thread to
    /// fold into its retained rows. Single slot, replace-oldest: only the newest
    /// matters.
    pub(crate) fn take_outcome(&self) -> Option<ViewModel> {
        self.outcome
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    /// The item behind a published file row, if that row is still current.
    ///
    /// `generation` is the generation the presented rows belong to. A lookup
    /// against any other generation resolves nothing: the rows on screen and
    /// the items they can execute have to describe the same query, or selecting
    /// a row would open whatever the previous query happened to have found in
    /// the same position.
    pub(crate) fn resolve(&self, generation: Generation, item: &ItemId) -> Option<Item> {
        let published = self.published.lock().unwrap_or_else(|error| error.into_inner());
        if published.generation != Some(generation) {
            return None;
        }
        published.items.get(&(self.owner.clone(), item.clone())).cloned()
    }

    /// Drops the prefix cache, so the next query reaches the backend.
    ///
    /// Called when the launcher is activated: within one typing burst the
    /// filesystem is effectively still, but between two the user has had time
    /// to create the file they are now looking for, and a cache that outlived
    /// the session it was built in would keep insisting it does not exist.
    pub(crate) fn invalidate_cache(&self) {
        self.forget_cache.store(true, Ordering::Release);
    }

    /// Whether the worker thread exists to answer a query.
    ///
    /// A driver whose thread failed to spawn accepts submissions and answers
    /// nothing, so the caller must not mark file work outstanding: the frame
    /// would say "Providers are still responding" for the rest of the session.
    pub(crate) fn is_serving(&self) -> bool {
        self.worker.is_some()
    }
}

impl Drop for FileSearchDriver {
    fn drop(&mut self) {
        {
            let (lock, cvar) = &*self.mailbox;
            let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
            slot.stop = true;
            slot.job = None;
            drop(slot);
            cvar.notify_all();
        }
        // Whatever is in flight is worthless once the launcher is going away,
        // and the token is the only thing that shortens the wait below.
        if let Some(cancel) = self
            .cancel
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            cancel.cancel();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The worker thread's half: the pipeline, the search, and the prefix cache.
struct FileSearchWorker {
    owner: PluginId,
    pipeline: QueryPipeline,
    cache: Option<PrefixCache>,
    published: Arc<Mutex<PublishedFileItems>>,
    forget_cache: Arc<AtomicBool>,
    /// Built on the worker thread and never leaving it. See
    /// [`FileSearchFactory`].
    search: Box<dyn FileItemSearch>,
    /// Last search failure reported, so a backend that is broken for the whole
    /// session says so once rather than once per keystroke.
    last_error: Option<String>,
}

impl FileSearchWorker {
    /// The frame to publish for `job`: the built-in rows it carried, followed by
    /// this provider's own.
    fn answer(&mut self, job: &FileJob) -> ViewModel {
        let files = self.serve(job);
        let mut rows = job.builtin_rows.clone();
        let mut pending = job.builtin_pending;
        if let Some(frame) = files {
            rows.extend(frame.rows.iter().cloned());
            pending |= frame.pending_plugins;
        }
        ViewModel {
            generation: job.generation,
            query: job.query.clone(),
            rows: rows.into(),
            selected: job.selected,
            pending_plugins: pending,
            actions_open: false,
            settings_open: false,
            settings: Arc::default(),
            settings_focus: None,
        }
    }

    /// Drives the pipeline through one query and returns this provider's frame.
    ///
    /// `None` when the pipeline reported an error or the frame belongs to a
    /// superseded pipeline generation, so a partial or stale set of file rows is
    /// never published.
    fn serve(&mut self, job: &FileJob) -> Option<ViewModel> {
        let generation = self.pipeline.keystroke(&job.query, job.now);
        let mut at = job.now;
        let mut delivered = false;
        // Advance the pipeline until it has dispatched this generation. The
        // provider debounces and has no leading edge, so the dispatch lands on a
        // later scheduler wake-up rather than this tick; following the
        // scheduler's own wake-ups spends no wall-clock time waiting for them.
        for _ in 0..64 {
            let tick = self.pipeline.tick(at);
            for cancellation in tick.cancellations {
                let _ = self
                    .pipeline
                    .complete(&cancellation.plugin, cancellation.generation, at);
            }

            let mut requests = Vec::new();
            for request in tick.dispatches {
                if request.plugin != self.owner || request.generation != generation {
                    let _ = self.pipeline.complete(&request.plugin, request.generation, at);
                    continue;
                }
                requests.push(request);
            }

            if requests.is_empty() {
                match self.pipeline.next_wakeup() {
                    Some(next) if next > at => {
                        at = next;
                        continue;
                    }
                    _ => break,
                }
            }

            for request in requests {
                let items = self.items_for(job);
                self.retain(job.generation, &items);
                delivered = self.deliver(&request.plugin, request.generation, items, at);
                let _ = self.pipeline.complete(&request.plugin, request.generation, at);
            }

            match self.pipeline.next_wakeup() {
                Some(next) if next > at => at = next,
                _ => break,
            }
        }

        // Drained rather than merely presented: every batch delivered above is
        // already in hand, and leaving any of it queued leaves the request
        // unsettled and the frame permanently pending.
        let frame = self.pipeline.present_drained(at);
        let presented = self.pipeline.take_errors().is_empty();
        if !presented || !delivered {
            return None;
        }
        frame.filter(|frame| frame.generation == generation)
    }

    /// Delivers one answer as batches the aggregator will accept.
    ///
    /// Split to the aggregator's ceiling because an over-large batch is refused
    /// whole rather than truncated, and a refused batch leaves the request
    /// unsettled: the frame would never present and the launcher would go on
    /// saying "Providers are still responding". A file search can return
    /// hundreds of hits, so this is the ordinary case rather than the extreme
    /// one.
    fn deliver(&mut self, plugin: &PluginId, generation: Generation, items: Vec<Item>, at: u64) -> bool {
        let ceiling = self.pipeline.max_items_per_batch().max(1);
        // Two different ceilings, and both bite. `ceiling` above splits the
        // stream; this one ends it. The per-owner quota is not a truncation the
        // pipeline performs for us — it refuses the crossing batch whole, which
        // leaves the request unsettled and publishes nothing — so the truncation
        // has to happen here. `run` already asks the backend for no more than
        // this, and this is the belt to that braces: a backend under no
        // obligation to honour `limit` cannot turn a broad query into an empty
        // launcher.
        let quota = self.quota().max(1);
        let items = if items.len() > quota {
            let mut items = items;
            items.truncate(quota);
            items
        } else {
            items
        };
        // An empty answer still owes the pipeline a terminal batch, exactly as
        // an over-large one does.
        if items.is_empty() {
            return self
                .pipeline
                .deliver(
                    ResultBatch {
                        generation,
                        plugin: plugin.clone(),
                        state: BatchState::Final,
                        items: Vec::new(),
                    },
                    at,
                )
                .is_ok();
        }
        let batches = items.len().div_ceil(ceiling);
        let mut accepted = true;
        for (index, chunk) in items.chunks(ceiling).enumerate() {
            // Only the last chunk ends the stream: a `Final` in the middle
            // would terminate it and every later chunk would be refused as
            // arriving after the end.
            let state = if index + 1 == batches {
                BatchState::Final
            } else {
                BatchState::Partial
            };
            accepted &= self
                .pipeline
                .deliver(
                    ResultBatch {
                        generation,
                        plugin: plugin.clone(),
                        state,
                        items: chunk.to_vec(),
                    },
                    at,
                )
                .is_ok();
        }
        accepted
    }

    /// Retains the items of one batch so the rows they become can be executed.
    ///
    /// Refuses to write under a generation the UI has already moved past: the
    /// store is stamped and cleared by `submit`, so a stamp that no longer
    /// matches means this answer is stale and its items must not be resolvable.
    fn retain(&self, generation: Generation, items: &[Item]) {
        let mut published = self.published.lock().unwrap_or_else(|error| error.into_inner());
        if published.generation != Some(generation) {
            return;
        }
        for item in items {
            published
                .items
                .insert((item.plugin_id.clone(), item.stable_id.clone()), item.clone());
        }
    }

    /// The items answering `job`, from the prefix cache wherever it can answer.
    fn items_for(&mut self, job: &FileJob) -> Vec<Item> {
        if self.forget_cache.swap(false, Ordering::AcqRel) {
            self.cache = None;
        }
        let normalized = job.query.trim().to_lowercase();
        if normalized.is_empty() {
            return Vec::new();
        }
        if let Some(cache) = self
            .cache
            .as_ref()
            .filter(|cache| normalized.starts_with(&cache.prefix))
        {
            return matching(&cache.items, &normalized);
        }

        let prefix: String = normalized.chars().take(PREFIX_CHARS).collect();
        let Some((items, coverage)) = self.run(&prefix, &job.cancel) else {
            self.cache = None;
            return Vec::new();
        };
        // Cacheable only while it really is a superset, which is exactly
        // `Complete` and nothing else. Every other coverage says the answer is
        // a fragment of the prefix's hits: cancelled, out of time, stopped at
        // the hit ceiling, or — and this is the one that looks safe and is not
        // — `Partial`, which the backends report for an unreadable root, an
        // entry cap, and an index that only covers configured scopes. Filtering
        // a fragment locally would hide a file for the whole typing burst with
        // nothing to show the user why, so a fragment is answered by asking
        // again rather than by narrowing what we have.
        let bounded = items.len() < MAX_FILE_HITS;
        let settled = matches!(coverage, FileSearchCoverage::Complete);
        if bounded && settled {
            let answer = matching(&items, &normalized);
            self.cache = Some(PrefixCache { prefix, items });
            return answer;
        }

        self.cache = None;
        if prefix == normalized {
            return items;
        }
        // The prefix answer was a fragment, so it cannot be filtered down to
        // this query. Asking again for the whole query is narrower, and is what
        // the provider would have done all along without a cache.
        self.run(&normalized, &job.cancel)
            .map(|(items, _)| items)
            .unwrap_or_default()
    }

    /// The most items this owner may contribute to one query.
    ///
    /// The smaller of the two pipeline ceilings, because both are enforced by
    /// refusing the crossing batch whole rather than by truncating it, and the
    /// whole-query one is lowered independently by `launcher.max-results`.
    fn quota(&self) -> usize {
        self.pipeline
            .max_items_per_plugin_per_query()
            .min(self.pipeline.max_items_per_query())
    }

    /// One search through the backend.
    ///
    /// `None` when this session has no file search at all, or when the search
    /// could not be performed — neither of which is an empty answer, but both
    /// of which contribute no rows.
    fn run(&mut self, normalized: &str, cancel: &CancelToken) -> Option<(Vec<Item>, FileSearchCoverage)> {
        // Bounded by what the pipeline will actually accept, not by what a
        // backend is willing to find. Two ceilings apply and the smaller wins:
        // the per-owner quota (250 by default) and the whole-query ceiling,
        // which the launcher's `max-results` setting lowers on its own and can
        // therefore be well below it. Neither is a truncation the pipeline
        // performs for us — `deliver` refuses the batch that would cross either
        // one whole, which becomes a pipeline error and publishes NO frame at
        // all. Asking for more than the smaller of them turns a broad, entirely
        // legal query into an empty launcher.
        let limit = MAX_FILE_HITS.min(self.quota());
        let query = FileSearchQuery {
            normalized: normalized.to_owned(),
            limit,
            deadline: FILE_SEARCH_DEADLINE,
            cancel: cancel.clone(),
        };
        match self.search.search_file_items(&self.owner, &query)? {
            Ok((items, results)) => {
                self.last_error = None;
                Some((items, results.coverage))
            }
            Err(error) => {
                // Once per distinct reason: this is the query path, and a
                // backend that is broken for the session would otherwise print
                // a line per keystroke.
                let reason = error.to_string();
                if self.last_error.as_deref() != Some(reason.as_str()) {
                    eprintln!("crikey: file search failed: {reason}");
                    self.last_error = Some(reason);
                }
                None
            }
        }
    }
}

/// The items whose name matches `normalized`, by the same case-insensitive
/// substring test the backends apply to a basename. Matching any other way
/// would make a cached answer differ from a fresh one.
fn matching(items: &[Item], normalized: &str) -> Vec<Item> {
    items
        .iter()
        .filter(|item| item.label.to_lowercase().contains(normalized))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {

    use crikey_app::PipelineConfig;
    use crikey_core::{Category, PlatformPath};
    use crikey_platform::{file_items, FileHit, FileKind, FileSearchResults};

    use super::*;

    /// A file search that answers from a fixed set and counts what it was asked.
    struct RecordingSearch {
        hits: Vec<FileHit>,
        queries: Arc<Mutex<Vec<String>>>,
        coverage: FileSearchCoverage,
        /// Fires once per search, so a test can observe that the worker is
        /// inside one without guessing how long that takes.
        entered: Option<std::sync::mpsc::Sender<()>>,
        /// Blocks each search until the test releases it.
        release: Option<Arc<(Mutex<bool>, Condvar)>>,
    }

    impl RecordingSearch {
        fn new(names: &[&str], queries: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                hits: names
                    .iter()
                    .map(|name| FileHit {
                        name: (*name).to_owned(),
                        path: PlatformPath::from(std::path::PathBuf::from(format!("/tmp/{name}"))),
                        kind: FileKind::File,
                        modified_unix_seconds: None,
                    })
                    .collect(),
                queries,
                coverage: FileSearchCoverage::Complete,
                entered: None,
                release: None,
            }
        }
    }

    impl FileItemSearch for RecordingSearch {
        fn search_file_items(
            &self,
            plugin: &PluginId,
            query: &FileSearchQuery,
        ) -> Option<crikey_core::Result<(Vec<Item>, FileSearchResults)>> {
            self.queries
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(query.normalized.clone());
            if let Some(entered) = self.entered.as_ref() {
                let _ = entered.send(());
            }
            if let Some(release) = self.release.as_ref() {
                let (lock, cvar) = &**release;
                let mut released = lock.lock().unwrap_or_else(|error| error.into_inner());
                while !*released {
                    released = cvar.wait(released).unwrap_or_else(|error| error.into_inner());
                }
            }
            let hits: Vec<FileHit> = self
                .hits
                .iter()
                .filter(|hit| hit.name.to_lowercase().contains(&query.normalized))
                .cloned()
                .collect();
            let items = file_items(plugin, &hits);
            Some(Ok((
                items,
                FileSearchResults {
                    hits,
                    coverage: self.coverage,
                },
            )))
        }
    }

    fn owner() -> PluginId {
        PluginId(FILE_SEARCH_PLUGIN.to_owned())
    }

    fn pipeline() -> QueryPipeline {
        let mut pipeline = QueryPipeline::new(PipelineConfig::default());
        pipeline
            .register_plugin(owner(), file_provider_policy())
            .expect("the file provider registers once");
        pipeline
    }

    fn worker(search: Box<dyn FileItemSearch>) -> FileSearchWorker {
        FileSearchWorker {
            owner: owner(),
            pipeline: pipeline(),
            cache: None,
            published: Arc::new(Mutex::new(PublishedFileItems::default())),
            forget_cache: Arc::new(AtomicBool::new(false)),
            search,
            last_error: None,
        }
    }

    fn job(generation: u64, query: &str, now: u64) -> FileJob {
        FileJob {
            generation: Generation::from_raw(generation),
            query: query.to_owned(),
            now,
            cancel: CancelToken::new(),
            builtin_rows: Vec::new(),
            builtin_pending: false,
            selected: 0,
        }
    }

    /// Stamps the store the way `submit` does, since these tests drive the
    /// worker half directly.
    fn begin(worker: &FileSearchWorker, generation: u64) {
        let mut published = worker.published.lock().unwrap_or_else(|error| error.into_inner());
        published.generation = Some(Generation::from_raw(generation));
        published.items.clear();
    }

    #[test]
    fn a_query_produces_rows_owned_by_the_file_provider() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut worker = worker(Box::new(RecordingSearch::new(
            &["quarterly-report.txt", "budget.ods"],
            Arc::clone(&queries),
        )));
        begin(&worker, 1);

        let frame = worker
            .serve(&job(1, "quarterly", 1_000))
            .expect("the provider presents a frame for its own query");

        let labels: Vec<&str> = frame.rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(labels, ["quarterly-report.txt"]);
        for row in frame.rows.iter() {
            assert_eq!(
                row.plugin_name, FILE_SEARCH_PLUGIN,
                "a file row must be owned by the file provider"
            );
            assert_eq!(row.category, Category::File.as_str());
        }
    }

    /// A query broad enough to exceed the per-owner quota still presents.
    ///
    /// The defect this closes published NOTHING for a broad query, which is
    /// worse than publishing too little. `MAX_FILE_HITS` is 512 and the default
    /// per-owner quota is 250, and the quota is not a truncation the pipeline
    /// performs: it refuses the batch that would cross it whole, the refusal
    /// leaves the request unsettled, `serve` finds a pipeline error and returns
    /// no frame at all. So the launcher answered a two-hundred-and-fifty-first
    /// match with an empty list and "providers are still responding".
    #[test]
    fn a_query_with_more_matches_than_the_quota_still_presents_a_frame() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        // Deliberately more than the default quota and more than one batch, so
        // both ceilings are crossed by the same answer.
        let names: Vec<String> = (0..400).map(|index| format!("report-{index:03}.txt")).collect();
        let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut worker = worker(Box::new(RecordingSearch::new(&borrowed, Arc::clone(&queries))));
        let quota = worker.pipeline.max_items_per_plugin_per_query();
        assert!(
            names.len() > quota,
            "the fixture must exceed the quota or this test proves nothing"
        );
        begin(&worker, 1);

        let frame = worker
            .serve(&job(1, "report", 1_000))
            .expect("a broad query presents a frame rather than being refused whole");

        assert!(
            !frame.rows.is_empty(),
            "the quota truncates the answer, it does not erase it"
        );
        assert!(
            frame.rows.len() <= quota,
            "no more than the quota is published, got {} against {quota}",
            frame.rows.len()
        );
        assert!(
            frame.rows.iter().all(|row| row.label.starts_with("report-")),
            "the published rows are the real matches"
        );
    }

    /// A launcher configured for few results still gets file rows.
    ///
    /// `launcher.max-results` lowers `max_items_per_query` and the intake item
    /// capacity, and leaves the per-owner quota alone. A provider that respects
    /// only the per-owner number therefore still overshoots on a configured
    /// launcher — 250 items into a 100-item query — and the crossing batch is
    /// refused whole, publishing nothing. Both ceilings have to be obeyed, and
    /// the smaller one is the one that matters.
    #[test]
    fn a_lowered_whole_query_ceiling_is_obeyed_as_well_as_the_per_owner_quota() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let names: Vec<String> = (0..400).map(|index| format!("report-{index:03}.txt")).collect();
        let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();

        // A whole-query ceiling well below the per-owner quota, which is what
        // `launcher.max-results` produces.
        let mut config = PipelineConfig::default();
        config.limits.max_items_per_query = 100;
        config.intake_limits.capacity_items = 100;
        let mut pipeline = QueryPipeline::new(config);
        pipeline
            .register_plugin(owner(), file_provider_policy())
            .expect("the file provider registers once");

        let mut worker = FileSearchWorker {
            owner: owner(),
            pipeline,
            cache: None,
            published: Arc::new(Mutex::new(PublishedFileItems::default())),
            forget_cache: Arc::new(AtomicBool::new(false)),
            search: Box::new(RecordingSearch::new(&borrowed, Arc::clone(&queries))),
            last_error: None,
        };
        assert!(
            worker.quota() <= 100,
            "the configured ceiling must be the binding one, got {}",
            worker.quota()
        );
        begin(&worker, 1);

        let frame = worker
            .serve(&job(1, "report", 1_000))
            .expect("a configured launcher still presents file rows");

        assert!(!frame.rows.is_empty(), "the ceiling truncates, it does not erase");
        assert!(
            frame.rows.len() <= 100,
            "no more than the configured ceiling is published, got {}",
            frame.rows.len()
        );
    }

    #[test]
    fn a_published_row_resolves_to_its_item_only_under_its_own_generation() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut worker = worker(Box::new(RecordingSearch::new(
            &["quarterly-report.txt"],
            Arc::clone(&queries),
        )));
        let published = Arc::clone(&worker.published);
        begin(&worker, 7);

        let frame = worker
            .serve(&job(7, "quarterly", 1_000))
            .expect("the provider presents a frame");
        let row = frame.rows.first().expect("one file row").clone();

        // The same resolution the execution path performs, over the same store.
        let driver_lookup = |generation: u64| -> Option<Item> {
            let store = published.lock().unwrap_or_else(|error| error.into_inner());
            if store.generation != Some(Generation::from_raw(generation)) {
                return None;
            }
            store.items.get(&(owner(), row.item.clone())).cloned()
        };

        let item = driver_lookup(7).expect("a published file row resolves to its item");
        assert_eq!(item.label, "quarterly-report.txt");
        assert!(
            !item.target.is_empty(),
            "the resolved item must carry the target the open action needs"
        );
        assert!(
            driver_lookup(8).is_none(),
            "a lookup under a generation the store was not stamped with must resolve nothing"
        );
    }

    #[test]
    fn an_extending_query_is_served_from_the_prefix_cache() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut worker = worker(Box::new(RecordingSearch::new(
            &["quarterly-report.txt", "quota.txt", "budget.ods"],
            Arc::clone(&queries),
        )));

        begin(&worker, 1);
        worker.serve(&job(1, "qu", 1_000));
        begin(&worker, 2);
        let frame = worker
            .serve(&job(2, "quarterly", 2_000))
            .expect("the extending query presents a frame");

        assert_eq!(
            &*queries.lock().unwrap_or_else(|error| error.into_inner()),
            &["qu".to_owned()],
            "an extending query must be answered from the cached prefix hits"
        );
        let labels: Vec<&str> = frame.rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(
            labels,
            ["quarterly-report.txt"],
            "the cached hits must be filtered down to the longer query"
        );
    }

    #[test]
    fn a_query_that_is_not_an_extension_re_queries_the_backend() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut worker = worker(Box::new(RecordingSearch::new(
            &["quarterly-report.txt", "budget.ods"],
            Arc::clone(&queries),
        )));

        begin(&worker, 1);
        worker.serve(&job(1, "qu", 1_000));
        begin(&worker, 2);
        let frame = worker
            .serve(&job(2, "budget", 2_000))
            .expect("the new prefix presents a frame");

        assert_eq!(
            &*queries.lock().unwrap_or_else(|error| error.into_inner()),
            &["qu".to_owned(), "bu".to_owned()],
            "a query outside the cached prefix must reach the backend again"
        );
        let labels: Vec<&str> = frame.rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(labels, ["budget.ods"]);
    }

    #[test]
    fn invalidating_the_cache_sends_the_next_query_back_to_the_backend() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut worker = worker(Box::new(RecordingSearch::new(
            &["quarterly-report.txt"],
            Arc::clone(&queries),
        )));
        let forget = Arc::clone(&worker.forget_cache);

        begin(&worker, 1);
        worker.serve(&job(1, "qu", 1_000));
        forget.store(true, Ordering::Release);
        begin(&worker, 2);
        worker.serve(&job(2, "quarterly", 2_000));

        assert_eq!(
            &*queries.lock().unwrap_or_else(|error| error.into_inner()),
            &["qu".to_owned(), "qu".to_owned()],
            "an invalidated cache must be refilled from the backend"
        );
    }

    #[test]
    fn an_incomplete_prefix_answer_is_not_cached_and_the_query_is_asked_in_full() {
        // Every coverage other than `Complete` describes an answer that is a
        // fragment of the prefix's hits. `Partial` is in the list deliberately:
        // the backends report it for an unreadable root, an entry cap and an
        // index that only covers configured scopes, so filtering it locally
        // would hide a file for the whole burst.
        for coverage in [
            FileSearchCoverage::Partial,
            FileSearchCoverage::Cancelled,
            FileSearchCoverage::Deadline,
        ] {
            let queries = Arc::new(Mutex::new(Vec::new()));
            let mut search = RecordingSearch::new(&["quarterly-report.txt"], Arc::clone(&queries));
            search.coverage = coverage;
            let mut worker = worker(Box::new(search));

            begin(&worker, 1);
            worker.serve(&job(1, "quarterly", 1_000));
            begin(&worker, 2);
            worker.serve(&job(2, "quarterly", 2_000));

            assert_eq!(
                &*queries.lock().unwrap_or_else(|error| error.into_inner()),
                &[
                    "qu".to_owned(),
                    "quarterly".to_owned(),
                    "qu".to_owned(),
                    "quarterly".to_owned()
                ],
                "a {coverage:?} prefix answer must not be cached, and must not be filtered either"
            );
        }
    }

    #[test]
    fn an_empty_query_searches_nothing() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut worker = worker(Box::new(RecordingSearch::new(
            &["quarterly-report.txt"],
            Arc::clone(&queries),
        )));

        begin(&worker, 1);
        worker.serve(&job(1, "   ", 1_000));

        assert!(
            queries
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_empty(),
            "an empty query must not reach the backend"
        );
    }

    #[test]
    fn a_superseded_query_never_publishes_its_answer() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut search = RecordingSearch::new(&["quarterly-report.txt"], Arc::clone(&queries));
        let (entered, entries) = std::sync::mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        search.entered = Some(entered);
        search.release = Some(Arc::clone(&release));

        let published = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&published);
        let driver = FileSearchDriver::spawn(
            owner(),
            pipeline(),
            Box::new(move || Box::new(search)),
            move |frame: &ViewModel| {
                recorder
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(frame.generation);
            },
        );

        driver.submit(Generation::from_raw(1), "qu", 1_000, Vec::new(), false, 0);
        // Ordered by handshake, not by hoping: the first query is provably
        // inside the backend before the second is submitted, and provably
        // finishes after it.
        entries.recv().expect("the worker reaches the backend");
        // A query outside the first one's prefix, so the second answer is a
        // second backend call rather than a cache hit with nothing to observe.
        driver.submit(Generation::from_raw(2), "budget", 2_000, Vec::new(), false, 0);
        {
            let (lock, cvar) = &*release;
            *lock.lock().unwrap_or_else(|error| error.into_inner()) = true;
            cvar.notify_all();
        }
        // The second query's own search, so both answers have been produced.
        entries.recv().expect("the second query reaches the backend");
        drop(driver);

        let frames = published.lock().unwrap_or_else(|error| error.into_inner());
        assert!(
            !frames.contains(&Generation::from_raw(1)),
            "the superseded query's late answer must be dropped, saw {frames:?}"
        );
    }
}
