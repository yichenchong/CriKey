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
//! * **The merge is not ours.** A file item is ranked against the application
//!   catalog rather than appended after it, and the ranker lives on the UI
//!   thread. So this driver's answer is a set of *items* under a generation,
//!   not a frame: the UI thread merges them into the ranked answer and
//!   republishes from it. What crosses the thread boundary is the search, not
//!   the presentation.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crikey_app::{BatchState, FileItemSearch, QueryPipeline, ResultBatch};
use crikey_core::{Generation, Item, PluginId};
use crikey_input_scheduler::{
    ActivationPolicy, DebouncePolicy, PluginPolicy, QueuePolicy, SchedulingProfile,
};
use crikey_platform::{CancelToken, FileSearchCoverage, FileSearchQuery, MAX_FILE_HITS};

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

/// How long the driver waits for the worker before settling a generation
/// itself.
///
/// [`FILE_SEARCH_DEADLINE`] is a promise the backend is asked to keep, not one
/// the launcher can enforce: nothing here can interrupt a call that is wedged
/// in a hung mount or an index socket that never replies. One job makes at most
/// two backend calls — the prefix, then the full query when the prefix answer
/// turned out to be a fragment — so two seconds is the longest wait a backend
/// that keeps its promise can impose, and the third second is slack. Past that
/// the answer is not late, it is not coming, and a source that goes on being
/// claimed as outstanding leaves the launcher saying "Providers are still
/// responding" for the rest of the session.
const FILE_SETTLE_DEADLINE_MS: u64 = 3 * FILE_SEARCH_DEADLINE.as_millis() as u64;

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
}

/// The single-slot mailbox between the UI thread and the worker.
struct FileMailbox {
    job: Option<FileJob>,
    stop: bool,
}

/// The generation the driver has accepted and not yet seen answered.
///
/// The launcher marks the file source pending the moment a query is handed
/// over, and only an outcome under that generation takes the mark off again.
/// This is the driver's own copy of the answer it owes, so a worker that
/// cannot produce one — wedged in a backend that ignores its deadline, or gone
/// — does not strand the mark.
struct Outstanding {
    /// The generation owed, which is what the settlement outcome carries: this
    /// provider contributes no items to it, exactly as the worker itself would
    /// report after a search that found nothing.
    generation: Generation,
    /// Clock reading, on the caller's monotonic query clock, at hand-over.
    submitted_at: u64,
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

/// One generation's worth of file items, for the UI thread to rank.
///
/// Items rather than rows: a file row is ranked against the catalog's rows in
/// one answer, and the ranker is the UI thread's. Producing rows here would be
/// producing the thing that has to be thrown away.
pub(crate) struct FileOutcome {
    /// The generation these items answer. Not necessarily the newest one the
    /// UI has: a late answer is refused by the merge rather than by silence.
    pub(crate) generation: Generation,
    pub(crate) items: Vec<Item>,
    /// Whether the provider still owes this generation anything. Always false
    /// in practice — the worker drains its own pipeline before answering — and
    /// carried rather than assumed so the caller's pending mark is taken off
    /// by what happened, not by what is expected to have happened.
    pub(crate) pending: bool,
}

/// The launcher's file provider: a worker thread and one in-flight search.
pub(crate) struct FileSearchDriver {
    mailbox: Arc<(Mutex<FileMailbox>, Condvar)>,
    outcome: Arc<Mutex<Option<FileOutcome>>>,
    /// Newest search generation the UI submitted. The worker re-reads it before
    /// parking an answer and drops one that is no longer current.
    current: Arc<AtomicU64>,
    /// Cancel token of the search that is or is about to be in flight.
    cancel: Mutex<Option<CancelToken>>,
    forget_cache: Arc<AtomicBool>,
    /// The generation this driver has accepted and owes an answer for. See
    /// [`Outstanding`].
    outstanding: Mutex<Option<Outstanding>>,
    worker: Option<JoinHandle<()>>,
}

impl FileSearchDriver {
    /// Moves `pipeline` and a freshly built file search onto a worker thread and
    /// returns a handle the UI thread drives without ever blocking.
    ///
    /// `pipeline` must already have `owner` registered with
    /// [`file_provider_policy`]. `wake` runs on the worker thread once an
    /// answer has been parked, and its only job is to make the UI thread turn
    /// and call [`Self::take_outcome`]; it carries no answer, because the
    /// answer is not presentable until the UI thread has ranked it. A thread
    /// that fails to spawn degrades to a driver that refuses every submission,
    /// so the launcher never marks file work outstanding it will not get,
    /// rather than a panic in the composition root.
    pub(crate) fn spawn<W>(
        owner: PluginId,
        pipeline: QueryPipeline,
        search: FileSearchFactory,
        wake: W,
    ) -> Self
    where
        W: Fn() + Send + 'static,
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
        let forget_cache = Arc::new(AtomicBool::new(false));

        let thread_mailbox = Arc::clone(&mailbox);
        let thread_outcome = Arc::clone(&outcome);
        let thread_current = Arc::clone(&current);
        let thread_forget = Arc::clone(&forget_cache);
        let spawned = std::thread::Builder::new()
            .name("crikey-files".to_owned())
            .spawn(move || {
                // Assembled here rather than by the caller: the search is the
                // one part of this worker that may not be `Send`, so it is built
                // on the thread that will own it for the rest of the session.
                let mut worker = FileSearchWorker {
                    owner,
                    pipeline,
                    cache: None,
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
                    let answered = worker.answer(&job);

                    // A late answer must never be offered under a newer
                    // generation. The whole check-store-wake is held under the
                    // mailbox lock so the gate cannot race a `submit`: `submit`
                    // records the newer generation into `current` before it
                    // takes this lock, so either we observe the newer
                    // generation and drop this answer, or no supersession has
                    // happened yet. A job already queued is likewise a
                    // supersession in flight.
                    let slot = lock.lock().unwrap_or_else(|error| error.into_inner());
                    if slot.stop
                        || slot.job.is_some()
                        || thread_current.load(Ordering::Acquire) != job.generation.get()
                    {
                        continue;
                    }
                    *thread_outcome.lock().unwrap_or_else(|error| error.into_inner()) = Some(answered);
                    wake();
                    drop(slot);
                }
            });

        Self {
            mailbox,
            outcome,
            current,
            cancel: Mutex::new(None),
            forget_cache,
            outstanding: Mutex::new(None),
            worker: spawned.ok(),
        }
    }

    /// Submits a query for asynchronous file search and returns at once.
    ///
    /// Returns whether this driver accepted `generation` and will therefore
    /// settle it. This is the *only* honest answer to "may the caller mark the
    /// file source pending", and it is returned rather than asked separately so
    /// the two cannot drift apart: every reason a submission is refused —
    /// no worker thread, a rewound generation, a driver being dropped — is a
    /// reason nothing will ever come back.
    #[must_use]
    pub(crate) fn submit(&self, generation: Generation, query: &str, now: u64) -> bool {
        if !self.has_worker_thread() {
            return false;
        }

        // Intake is monotonic: a delayed caller must not rewind the live
        // generation and make an obsolete job eligible for publication again.
        let generation_value = generation.get();
        let mut observed = self.current.load(Ordering::Acquire);
        loop {
            if generation_value < observed {
                return false;
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

        let (lock, cvar) = &*self.mailbox;
        let mut slot = lock.lock().unwrap_or_else(|error| error.into_inner());
        if slot.stop {
            return false;
        }
        *self.outstanding.lock().unwrap_or_else(|error| error.into_inner()) = Some(Outstanding {
            generation,
            submitted_at: now,
        });
        slot.job = Some(FileJob {
            generation,
            query: query.to_owned(),
            now,
            cancel,
        });
        drop(slot);
        cvar.notify_one();
        true
    }

    /// Takes the items the worker found, for the UI thread to rank into the
    /// answer. Single slot, replace-oldest: only the newest matters.
    ///
    /// `now` is the caller's monotonic query clock, the same one `submit` was
    /// given. Past [`FILE_SETTLE_DEADLINE_MS`] the driver answers for a worker
    /// that has not: the outcome it hands back settles the generation the
    /// caller marked pending and contributes no items. A worker that answers
    /// after that is not discarded — its items merge under the same generation
    /// on a later turn, exactly as an early answer would.
    pub(crate) fn take_outcome(&self, now: u64) -> Option<FileOutcome> {
        let taken = self
            .outcome
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let mut outstanding = self.outstanding.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(outcome) = taken {
            // The worker parked this under the live generation, but a `submit`
            // may have overtaken it since. The merge would refuse a stale
            // generation anyway; refusing it here as well is what keeps the
            // driver's own settlement mark honest, because this driver knows
            // which generation it is serving and the merge only knows which one
            // it was asked about.
            if outcome.generation.get() != self.current.load(Ordering::Acquire) {
                return None;
            }
            // Only what this outcome actually answers. A newer generation may
            // have been submitted since the worker parked its answer, and
            // discharging that one on the strength of an older answer is the
            // whole bug this guards against.
            if outstanding
                .as_ref()
                .is_some_and(|owed| owed.generation <= outcome.generation)
            {
                *outstanding = None;
            }
            return Some(outcome);
        }
        if outstanding
            .as_ref()
            .is_some_and(|owed| now.saturating_sub(owed.submitted_at) >= FILE_SETTLE_DEADLINE_MS)
        {
            return outstanding.take().map(|owed| FileOutcome {
                generation: owed.generation,
                items: Vec::new(),
                pending: false,
            });
        }
        None
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

    /// Whether `spawn` got a thread — nothing more.
    ///
    /// Deliberately not called `is_serving`: it says nothing about whether a
    /// backend exists, whether the worker is still answering, or whether this
    /// session can search files at all. It is the one precondition
    /// [`Self::submit`] checks before accepting a generation, and callers ask
    /// about readiness by reading `submit`'s answer instead.
    fn has_worker_thread(&self) -> bool {
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
    forget_cache: Arc<AtomicBool>,
    /// Built on the worker thread and never leaving it. See
    /// [`FileSearchFactory`].
    search: Box<dyn FileItemSearch>,
    /// Last search failure reported, so a backend that is broken for the whole
    /// session says so once rather than once per keystroke.
    last_error: Option<String>,
}

/// What one query found, before anything has been ranked or rendered.
struct FileAnswer {
    items: Vec<Item>,
    pending: bool,
}

impl FileSearchWorker {
    /// The outcome to park for `job`: the items this provider found under the
    /// generation that asked for them.
    ///
    /// Always an outcome, never an `Option`. Every way a search can come to
    /// nothing — no backend in this session, a backend that failed, a pipeline
    /// that refused the batch — is answered here with no items at all, because
    /// the caller marked this source pending when it handed the job over and
    /// only an outcome under this generation takes that mark off again.
    fn answer(&mut self, job: &FileJob) -> FileOutcome {
        let (items, pending) = match self.serve(job) {
            Some(found) => (found.items, found.pending),
            None => (Vec::new(), false),
        };
        FileOutcome {
            generation: job.generation,
            items,
            pending,
        }
    }

    /// Drives the pipeline through one query and returns what it accepted.
    ///
    /// `None` when this session has no file search, when the pipeline reported
    /// an error, or when the presented frame belongs to a superseded pipeline
    /// generation, so a partial or stale set of file items is never offered to
    /// the ranker. It is not a refusal to settle: [`Self::answer`] parks an
    /// outcome regardless.
    fn serve(&mut self, job: &FileJob) -> Option<FileAnswer> {
        let generation = self.pipeline.keystroke(&job.query, job.now);
        let mut at = job.now;
        let mut accepted: Option<Vec<Item>> = None;
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
                accepted = self.deliver(&request.plugin, request.generation, items, at);
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
        let (Some(items), true) = (accepted, presented) else {
            return None;
        };
        // The frame is built and then thrown away, which is the price of asking
        // the aggregator whether it would present these items at all: the rows
        // it composes answer the previous query if this generation was
        // superseded inside the pipeline, and the items would then be a stale
        // answer with a current generation stamped on it. Building rows nobody
        // renders costs the worker thread, not the frame.
        let frame = frame.filter(|frame| frame.generation == generation)?;
        Some(FileAnswer {
            items,
            pending: frame.pending_plugins,
        })
    }

    /// Delivers one answer as batches the aggregator will accept, and returns
    /// the items it actually took.
    ///
    /// Split to the aggregator's ceiling because an over-large batch is refused
    /// whole rather than truncated, and a refused batch leaves the request
    /// unsettled: the frame would never present and the launcher would go on
    /// saying "Providers are still responding". A file search can return
    /// hundreds of hits, so this is the ordinary case rather than the extreme
    /// one.
    ///
    /// `None` is a refusal. The items come back rather than the caller keeping
    /// its own copy because the quota below truncates them here, and ranking a
    /// set the pipeline never accepted would put rows on screen that the
    /// launcher's own limits say are not there.
    fn deliver(
        &mut self,
        plugin: &PluginId,
        generation: Generation,
        items: Vec<Item>,
        at: u64,
    ) -> Option<Vec<Item>> {
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
                .is_ok()
                .then(Vec::new);
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
        accepted.then_some(items)
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
        }
    }

    /// The labels of what one query found, in the order the pipeline accepted
    /// them.
    fn labels(answer: &FileAnswer) -> Vec<&str> {
        answer.items.iter().map(|item| item.label.as_str()).collect()
    }

    #[test]
    fn a_query_produces_items_owned_by_the_file_provider() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut worker = worker(Box::new(RecordingSearch::new(
            &["quarterly-report.txt", "budget.ods"],
            Arc::clone(&queries),
        )));

        let answer = worker
            .serve(&job(1, "quarterly", 1_000))
            .expect("the provider answers its own query");

        assert_eq!(labels(&answer), ["quarterly-report.txt"]);
        for item in &answer.items {
            assert_eq!(
                item.plugin_id,
                owner(),
                "a file item must be owned by the file provider"
            );
            assert_eq!(item.category, Category::File);
            assert!(
                !item.target.is_empty(),
                "the item handed to the ranker must carry the target the open action needs"
            );
        }
    }

    /// A query broad enough to exceed the per-owner quota still presents.
    ///
    /// The defect this closes published NOTHING for a broad query, which is
    /// worse than publishing too little. `MAX_FILE_HITS` is 512 and the default
    /// per-owner quota is 250, and the quota is not a truncation the pipeline
    /// performs: it refuses the batch that would cross it whole, the refusal
    /// leaves the request unsettled, `serve` finds a pipeline error and returns
    /// nothing at all. So the launcher answered a two-hundred-and-fifty-first
    /// match with an empty list and "providers are still responding".
    #[test]
    fn a_query_with_more_matches_than_the_quota_still_answers() {
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

        let answer = worker
            .serve(&job(1, "report", 1_000))
            .expect("a broad query answers rather than being refused whole");

        assert!(
            !answer.items.is_empty(),
            "the quota truncates the answer, it does not erase it"
        );
        assert!(
            answer.items.len() <= quota,
            "no more than the quota is offered, got {} against {quota}",
            answer.items.len()
        );
        assert!(
            answer.items.iter().all(|item| item.label.starts_with("report-")),
            "the offered items are the real matches"
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
            forget_cache: Arc::new(AtomicBool::new(false)),
            search: Box::new(RecordingSearch::new(&borrowed, Arc::clone(&queries))),
            last_error: None,
        };
        assert!(
            worker.quota() <= 100,
            "the configured ceiling must be the binding one, got {}",
            worker.quota()
        );

        let answer = worker
            .serve(&job(1, "report", 1_000))
            .expect("a configured launcher still gets file items");

        assert!(
            !answer.items.is_empty(),
            "the ceiling truncates, it does not erase"
        );
        assert!(
            answer.items.len() <= 100,
            "no more than the configured ceiling is offered, got {}",
            answer.items.len()
        );
    }

    #[test]
    fn an_extending_query_is_served_from_the_prefix_cache() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut worker = worker(Box::new(RecordingSearch::new(
            &["quarterly-report.txt", "quota.txt", "budget.ods"],
            Arc::clone(&queries),
        )));

        worker.serve(&job(1, "qu", 1_000));
        let answer = worker
            .serve(&job(2, "quarterly", 2_000))
            .expect("the extending query answers");

        assert_eq!(
            &*queries.lock().unwrap_or_else(|error| error.into_inner()),
            &["qu".to_owned()],
            "an extending query must be answered from the cached prefix hits"
        );
        assert_eq!(
            labels(&answer),
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

        worker.serve(&job(1, "qu", 1_000));
        let answer = worker
            .serve(&job(2, "budget", 2_000))
            .expect("the new prefix answers");

        assert_eq!(
            &*queries.lock().unwrap_or_else(|error| error.into_inner()),
            &["qu".to_owned(), "bu".to_owned()],
            "a query outside the cached prefix must reach the backend again"
        );
        assert_eq!(labels(&answer), ["budget.ods"]);
    }

    #[test]
    fn invalidating_the_cache_sends_the_next_query_back_to_the_backend() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut worker = worker(Box::new(RecordingSearch::new(
            &["quarterly-report.txt"],
            Arc::clone(&queries),
        )));
        let forget = Arc::clone(&worker.forget_cache);

        worker.serve(&job(1, "qu", 1_000));
        forget.store(true, Ordering::Release);
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

            worker.serve(&job(1, "quarterly", 1_000));
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

        worker.serve(&job(1, "   ", 1_000));

        assert!(
            queries
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_empty(),
            "an empty query must not reach the backend"
        );
    }

    /// A superseded query's answer must never reach the ranker.
    ///
    /// Two gates guard this and both are exercised here: the worker refuses to
    /// park an answer once a newer generation has been submitted, and
    /// `take_outcome` refuses to hand out one that was parked just before the
    /// supersession. What the caller must never see is generation 1.
    #[test]
    fn a_superseded_query_never_offers_its_answer() {
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut search = RecordingSearch::new(&["quarterly-report.txt"], Arc::clone(&queries));
        let (entered, entries) = std::sync::mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        search.entered = Some(entered);
        search.release = Some(Arc::clone(&release));

        let wakes = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&wakes);
        let driver = FileSearchDriver::spawn(
            owner(),
            pipeline(),
            Box::new(move || Box::new(search)),
            move || {
                counter.fetch_add(1, Ordering::Release);
            },
        );

        assert!(driver.submit(Generation::from_raw(1), "qu", 1_000));
        // Ordered by handshake, not by hoping: the first query is provably
        // inside the backend before the second is submitted, and provably
        // finishes after it.
        entries.recv().expect("the worker reaches the backend");
        // A query outside the first one's prefix, so the second answer is a
        // second backend call rather than a cache hit with nothing to observe.
        assert!(driver.submit(Generation::from_raw(2), "budget", 2_000));
        {
            let (lock, cvar) = &*release;
            *lock.lock().unwrap_or_else(|error| error.into_inner()) = true;
            cvar.notify_all();
        }
        // The second query's own search, so both answers have been produced.
        entries.recv().expect("the second query reaches the backend");

        // Everything the caller is ever offered, drained until the worker is
        // provably finished with both queries.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut offered = Vec::new();
        while std::time::Instant::now() < deadline {
            if let Some(outcome) = driver.take_outcome(2_000) {
                offered.push(outcome.generation);
                if outcome.generation == Generation::from_raw(2) {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(driver);

        assert!(
            !offered.contains(&Generation::from_raw(1)),
            "the superseded query's late answer must be dropped, saw {offered:?}"
        );
        assert_eq!(
            offered.last(),
            Some(&Generation::from_raw(2)),
            "the live query's answer must still arrive, saw {offered:?}"
        );
        assert!(
            wakes.load(Ordering::Acquire) > 0,
            "an answer the UI thread has to merge is worthless unless the UI thread is woken for it"
        );
    }

    /// A session with no file backend at all.
    ///
    /// `search_file_items` returning `None` is the platform contract for "this
    /// machine has nothing to search" — on Linux, an empty `FilesystemSearch`
    /// root set. It is not an error and not an empty answer.
    struct NoBackend;

    impl FileItemSearch for NoBackend {
        fn search_file_items(
            &self,
            _plugin: &PluginId,
            _query: &FileSearchQuery,
        ) -> Option<crikey_core::Result<(Vec<Item>, FileSearchResults)>> {
            None
        }
    }

    /// A generation the driver settles itself, or one the worker answers with
    /// nothing found, contributes no items — and that is what takes the
    /// caller's pending mark off.
    #[test]
    fn a_session_without_a_file_backend_still_settles_the_generation() {
        let driver = FileSearchDriver::spawn(owner(), pipeline(), Box::new(|| Box::new(NoBackend)), || {});

        assert!(driver.submit(Generation::from_raw(1), "quarterly", 1_000));

        // Always the submission instant, so the driver's own settlement
        // deadline can never be what answers here: this is the worker's outcome
        // or nothing.
        let wait = std::time::Instant::now() + Duration::from_secs(5);
        let outcome = loop {
            if let Some(outcome) = driver.take_outcome(1_000) {
                break Some(outcome);
            }
            if std::time::Instant::now() >= wait {
                break None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };

        let outcome = outcome.expect("a source that marked itself pending must settle its generation");
        assert_eq!(outcome.generation, Generation::from_raw(1));
        assert!(
            !outcome.pending,
            "the settling outcome must not still claim work outstanding"
        );
        assert!(
            outcome.items.is_empty(),
            "a session with no file backend contributes no items"
        );
    }

    /// A backend that takes the query and does not come back.
    ///
    /// `FILE_SEARCH_DEADLINE` is a promise, not a mechanism: nothing in the
    /// launcher can interrupt a call wedged in a hung mount or an index socket
    /// that never replies. It returns on cancellation only so that dropping the
    /// driver, which is the last thing every test does, can join the thread —
    /// the call is still outstanding for the whole of the test proper.
    struct WedgedSearch {
        entered: std::sync::mpsc::Sender<()>,
    }

    impl FileItemSearch for WedgedSearch {
        fn search_file_items(
            &self,
            _plugin: &PluginId,
            query: &FileSearchQuery,
        ) -> Option<crikey_core::Result<(Vec<Item>, FileSearchResults)>> {
            let _ = self.entered.send(());
            while !query.cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            None
        }
    }

    /// Before the driver settled generations itself, a wedged backend left the
    /// file source marked pending with no answer owed by anyone, and the
    /// launcher said "Providers are still responding" until it was restarted.
    #[test]
    fn a_backend_that_never_answers_stops_owing_the_generation() {
        let (entered, entries) = std::sync::mpsc::channel();
        let search = WedgedSearch { entered };

        let driver = FileSearchDriver::spawn(owner(), pipeline(), Box::new(move || Box::new(search)), || {});

        assert!(driver.submit(Generation::from_raw(1), "quarterly", 1_000));
        // Provably inside the backend, so what follows is a worker that is
        // stuck rather than one that has not started.
        entries.recv().expect("the worker reaches the backend");

        assert!(
            driver.take_outcome(1_000 + FILE_SETTLE_DEADLINE_MS - 1).is_none(),
            "a backend still inside the time it was promised is still owed an answer"
        );

        let settled = driver
            .take_outcome(1_000 + FILE_SETTLE_DEADLINE_MS)
            .expect("a generation the driver accepted must settle even if the worker never answers");
        assert_eq!(settled.generation, Generation::from_raw(1));
        assert!(
            !settled.pending,
            "the settling outcome is what takes the pending mark off, so it cannot carry one"
        );
        assert!(
            settled.items.is_empty(),
            "settling for a wedged worker contributes nothing to the ranked answer"
        );
    }

    /// The second gate, on the taking side.
    ///
    /// The worker refuses to park an answer once a newer generation exists, but
    /// it can only refuse what it has not parked yet: an answer parked an
    /// instant before the next keystroke is already in the slot when `submit`
    /// runs, and `submit` does not clear it. Handing that out would rank the
    /// previous query's files into this query's answer. The merge would refuse
    /// the stale generation too, but only this side knows *which* generation is
    /// live, and only this side is holding the settlement mark that must stay
    /// owed.
    ///
    /// The outcome is planted rather than raced into place, because the window
    /// it lives in is a few instructions wide and a test that had to hit it
    /// would prove nothing on the runs where it missed.
    #[test]
    fn an_answer_parked_just_before_a_new_query_is_never_handed_out() {
        let (entered, entries) = std::sync::mpsc::channel();
        let search = WedgedSearch { entered };
        let driver = FileSearchDriver::spawn(owner(), pipeline(), Box::new(move || Box::new(search)), || {});

        assert!(driver.submit(Generation::from_raw(1), "quarterly", 1_000));
        entries.recv().expect("the worker reaches the backend");
        assert!(driver.submit(Generation::from_raw(2), "budget", 2_000));

        // The first query's answer, in the slot, under the generation the user
        // has already typed past.
        *driver.outcome.lock().unwrap_or_else(|error| error.into_inner()) = Some(FileOutcome {
            generation: Generation::from_raw(1),
            items: Vec::new(),
            pending: false,
        });

        assert!(
            driver.take_outcome(2_000).is_none(),
            "an answer to a query the user has typed past must never be offered"
        );

        // And the newer generation is still owed, so its pending mark is still
        // the driver's to take off.
        let settled = driver
            .take_outcome(2_000 + FILE_SETTLE_DEADLINE_MS)
            .expect("the live generation is still owed an answer");
        assert_eq!(
            settled.generation,
            Generation::from_raw(2),
            "the stale answer must not have discharged the live generation"
        );
    }
}
