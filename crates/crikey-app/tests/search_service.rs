//! Contract for the composed search service (spec 11.1, 11.3, 11.6, 25.6;
//! roadmap M1).
//!
//! [`SearchService`] is the seam where the catalog, the query engine, the
//! ranker and the aggregator become one user-visible behaviour. The pieces are
//! already defended individually; what is defended here is the composition:
//! startup gating decides *when* a query is legal, generations decide *which*
//! answer is current, and ranking decides *what order* the answer arrives in.
//!
//! Pinned API, implemented by the behavior wave in `crikey-app`:
//!
//! ```ignore
//! #[derive(Debug, Clone, Copy, PartialEq, Eq)]
//! pub enum SearchError {
//!     /// Startup has not acknowledged the AcceptQueries milestone yet.
//!     NotAcceptingQueries { pending: StartupStage },
//! }
//!
//! #[derive(Debug, Clone)]
//! pub struct SearchHit {
//!     pub item: Item,
//!     pub score: crikey_ranking::Score,
//!     pub method: crikey_query::MatchMethod,
//!     pub highlights: Vec<(usize, usize)>,
//! }
//!
//! #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
//! pub struct CatalogLoad {
//!     /// Items admitted into the live catalog.
//!     pub items: usize,
//!     /// Advertised owners this pass could not admit.
//!     pub skipped: usize,
//! }
//!
//! impl SearchService {
//!     pub fn new(app: App) -> Self;
//!     pub fn stage(&self) -> StartupStage;
//!     pub fn complete_stage(
//!         &mut self,
//!         expected: StartupStage,
//!     ) -> Result<Option<StartupStage>, StartupError>;
//!     pub fn load_persisted_catalog(
//!         &mut self,
//!         cache: &dyn CatalogCache,
//!     ) -> Result<CatalogLoad, CacheError>;
//!     pub fn submit_query(&mut self, raw: &str) -> Result<Generation, SearchError>;
//!     pub fn results(&self) -> &[SearchHit];
//! }
//! ```
//!
//! `stage` and `complete_stage` are straight pass-throughs to the owned [`App`]:
//! taking the `App` by value is what lets one object answer "may I query?" and
//! "here are the results", but it would otherwise make the staging surface of
//! spec 25.6 unreachable.
//!
//! Candidate pruning (spec 11.1) is a property of how the answer is *reached*,
//! never of the answer itself: `submit_query` must return exactly what a sweep
//! over every retained item would have returned. That is not checkable against
//! a hand-written expectation, which would only pin the fixture, so the
//! equivalence tests below recompute the ranked answer by brute force - every
//! fixture item, matched and scored directly through [`crikey_query`] and
//! [`crikey_ranking`], with no catalog in the path - and compare it item for
//! item, in order, score and highlights included. A pruning bug therefore
//! surfaces as a missing or extra result rather than as a speed difference.
//!
//! Nothing here sleeps, reads a clock, opens a socket or touches a display.
//! The only I/O is a per-test directory under [`std::env::temp_dir`], removed
//! when the test ends. A filesystem fault is not reproducible on demand, so
//! the faults below are scripted by a cache double that touches no file at
//! all.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

use crikey_app::{App, SearchError, SearchHit, SearchService, StartupStage};
use crikey_catalog::{CacheError, CachedSlice, CatalogCache, FileCatalogCache};
use crikey_core::{ArgumentPolicy, Category, Generation, HitPolicy, Item, ItemId, PluginId};
use crikey_query::{DefaultMatcher, DefaultNormalizer, MatchMethod, Matcher, Normalizer};
use crikey_ranking::{DefaultRanker, Ranker, Score};
use crikey_result_aggregator::ResultLimits;

const PLUGIN: &str = "dev.crikey.search-service";

/// Owners that sort either side of [`PLUGIN`], so surviving a scripted fault
/// cannot be an accident of the order the load visits owners in.
const EARLY_PLUGIN: &str = "dev.crikey.aa-early";
const LATE_PLUGIN: &str = "dev.crikey.zz-late";

/// Every fixture item, matching and non-matching alike.
const FIXTURE_LEN: usize = 6;

/// The fixture items that answer the query `fire`, strongest first.
///
/// `a-prefix` and `b-prefix` are byte-identical apart from their stable ids, so
/// they score identically and the order between them is decided purely by the
/// tie-break (spec 11.6).
const FIRE_ORDER: [&str; 5] = ["a-prefix", "b-prefix", "substring", "keyword", "fuzzy"];

/// The milestones that must be acknowledged before a query is legal, each
/// paired with the milestone acknowledging it makes pending (spec 25.6).
const PRE_QUERY_STAGES: [(StartupStage, StartupStage); 3] = [
    (StartupStage::WindowAndHotkey, StartupStage::PersistedCatalog),
    (StartupStage::PersistedCatalog, StartupStage::AcceptQueries),
    (StartupStage::AcceptQueries, StartupStage::RequiredWorkers),
];

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn candidate(id: &str, label: &str, description: &str, search_terms: &[&str]) -> Item {
    owned(PLUGIN, id, label, description, search_terms)
}

/// A catalog item belonging to a named owner.
fn owned(plugin: &str, id: &str, label: &str, description: &str, search_terms: &[&str]) -> Item {
    Item {
        stable_id: ItemId(id.to_owned()),
        plugin_id: PluginId(plugin.to_owned()),
        category: Category::Application,
        label: label.to_owned(),
        description: description.to_owned(),
        target: format!("app://{id}"),
        search_terms: search_terms.iter().map(|term| (*term).to_owned()).collect(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    }
}

/// Items that between them exercise prefix, substring, keyword and fuzzy
/// matching against the query `fire`, an exact score tie across two of them,
/// and one item that answers only the disjoint query `water` so exclusion is
/// observable rather than vacuous.
fn fixture_items() -> Vec<Item> {
    vec![
        candidate("a-prefix", "Fire Atlas", "Launch the map", &[]),
        candidate("b-prefix", "Fire Atlas", "Launch the map", &[]),
        candidate("substring", "Campfire Notes", "Outdoor notebook", &[]),
        candidate("keyword", "Blaze Guide", "Wildland fire safety", &[]),
        candidate("fuzzy", "File Reader", "Open documents", &[]),
        candidate("nonmatch", "Water Clock", "Track the tides", &["rain"]),
    ]
}

/// A private directory under the system temp dir, removed when the test ends.
///
/// The process id keeps concurrent `cargo test` invocations apart and the
/// counter keeps the threads of one invocation apart, so no two caches ever
/// share a root.
#[derive(Debug)]
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "crikey-search-service-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create the test cache root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The fixture items as one owner's cached slice.
fn fixture_slice() -> CachedSlice {
    CachedSlice {
        plugin: PluginId(PLUGIN.to_owned()),
        instance: 1,
        generation: Generation::ZERO,
        items: fixture_items(),
    }
}

/// A file-backed cache already holding the fixture slice.
fn seeded_cache(root: &Path) -> FileCatalogCache {
    let cache = FileCatalogCache::new(root.to_path_buf());
    cache
        .store_slice(&fixture_slice())
        .expect("persist the fixture catalog slice");
    cache
}

/// The root a scripted cache blames, so a propagated fault is checkable
/// without naming a path any of these tests could really own.
const SCRIPTED_ROOT: &str = "/scripted-cache-root";

/// The fault kind every scripted failure reports.
const FAULT_KIND: io::ErrorKind = io::ErrorKind::PermissionDenied;

/// What a [`ScriptedCache`] answers for one owner.
#[derive(Debug, Clone)]
enum Answer {
    /// Readable, trustworthy bytes.
    Slice(CachedSlice),
    /// A filesystem fault confined to this owner's archive.
    Fault,
}

/// A cache whose per-owner answers are scripted.
///
/// A filesystem fault on a single slice cannot be provoked on demand from a
/// test, and what it drives - one bad slice must not cost the launcher the
/// owners around it - is exactly what needs pinning, so it is scripted here.
#[derive(Debug)]
struct ScriptedCache {
    /// Owners the cache advertises, with the answer each one gets.
    answers: Vec<(PluginId, Answer)>,
    /// Whether enumerating the cache root itself faults.
    root_faults: bool,
}

impl ScriptedCache {
    fn new(answers: Vec<(&str, Answer)>) -> Self {
        Self {
            answers: answers
                .into_iter()
                .map(|(owner, answer)| (PluginId(owner.to_owned()), answer))
                .collect(),
            root_faults: false,
        }
    }

    /// A cache whose root cannot be enumerated, so it names no owner at all.
    fn unreadable_root() -> Self {
        Self {
            answers: Vec::new(),
            root_faults: true,
        }
    }

    fn fault(path: &str) -> CacheError {
        CacheError::Io {
            path: PathBuf::from(path),
            kind: FAULT_KIND,
        }
    }
}

impl CatalogCache for ScriptedCache {
    fn load_slice(&self, plugin: &PluginId) -> Result<Option<CachedSlice>, CacheError> {
        let Some((_, answer)) = self.answers.iter().find(|(owner, _)| owner == plugin) else {
            return Ok(None);
        };
        match answer {
            Answer::Slice(slice) => Ok(Some(slice.clone())),
            Answer::Fault => Err(Self::fault(&format!("{SCRIPTED_ROOT}/{}", plugin.0))),
        }
    }

    fn store_slice(&self, _slice: &CachedSlice) -> Result<(), CacheError> {
        Ok(())
    }

    fn invalidate(&self, _plugin: &PluginId) -> Result<(), CacheError> {
        Ok(())
    }

    fn plugins(&self) -> Result<Vec<PluginId>, CacheError> {
        if self.root_faults {
            return Err(Self::fault(SCRIPTED_ROOT));
        }

        let mut owners: Vec<PluginId> = self.answers.iter().map(|(owner, _)| owner.clone()).collect();
        owners.sort();
        Ok(owners)
    }
}

/// A service whose startup has been acknowledged far enough to accept queries.
fn accepting_service() -> SearchService {
    accepting_service_with_limits(ResultLimits::default())
}

/// A query-ready service configured before its aggregator is built.
fn accepting_service_with_limits(limits: ResultLimits) -> SearchService {
    let mut service = SearchService::new(App::with_limits(limits));

    for (stage, next) in PRE_QUERY_STAGES {
        assert_eq!(
            service.complete_stage(stage),
            Ok(Some(next)),
            "acknowledging {stage:?} must make {next:?} pending"
        );
    }

    assert_eq!(service.stage(), StartupStage::RequiredWorkers);
    service
}

/// Result ids in presentation order, owned so the borrow of the service ends.
fn ids(hits: &[SearchHit]) -> Vec<String> {
    hits.iter().map(|hit| hit.item.stable_id.0.clone()).collect()
}

// ---------------------------------------------------------------------------
// Startup gating (spec 25.6)
// ---------------------------------------------------------------------------

#[test]
fn queries_are_rejected_until_the_accept_queries_milestone_is_acknowledged() {
    let mut service = SearchService::new(App::new());

    for (pending, next) in PRE_QUERY_STAGES {
        assert_eq!(service.stage(), pending);
        assert_eq!(
            service.submit_query("fire"),
            Err(SearchError::NotAcceptingQueries { pending }),
            "a query submitted while {pending:?} is pending must name the blocker"
        );
        assert!(
            service.results().is_empty(),
            "a rejected query must publish nothing while {pending:?} is pending"
        );

        assert_eq!(
            service.complete_stage(pending),
            Ok(Some(next)),
            "acknowledging {pending:?} must make {next:?} pending"
        );
    }

    assert_eq!(service.stage(), StartupStage::RequiredWorkers);

    let first = service
        .submit_query("fire")
        .expect("queries are legal once AcceptQueries is acknowledged");
    assert_eq!(first.get(), 1, "a rejected query must not consume a generation");
}

// ---------------------------------------------------------------------------
// Generations (spec 8.1, 11.6)
// ---------------------------------------------------------------------------

#[test]
fn each_accepted_query_allocates_a_fresh_increasing_generation() {
    let mut service = accepting_service();

    let first = service.submit_query("fire").expect("first query");
    let second = service.submit_query("fi").expect("narrowed query");
    let third = service.submit_query("fire").expect("retyped first query");

    assert!(
        first > Generation::ZERO,
        "a submitted query is a new launcher state, never the initial one"
    );
    assert!(second > first, "generations increase monotonically");
    assert!(
        third > second,
        "repeating the query text is still a new query state, not a replay of {first}"
    );
}

// ---------------------------------------------------------------------------
// Ranking and exclusion (spec 11.1, 11.3, 11.6)
// ---------------------------------------------------------------------------

#[test]
fn results_are_ranked_descending_and_tie_broken_by_item_id() {
    let root = TempRoot::new("ranked");
    let cache = seeded_cache(root.path());

    let mut service = accepting_service();
    let loaded = service
        .load_persisted_catalog(&cache)
        .expect("load the persisted catalog");
    assert_eq!(loaded.items, FIXTURE_LEN);

    service.submit_query("fire").expect("query accepted");

    let hits = service.results();
    assert_eq!(ids(hits), FIRE_ORDER);

    for pair in hits.windows(2) {
        assert!(
            pair[0].score >= pair[1].score,
            "results must be ordered by descending score: {:?} then {:?}",
            pair[0].item.stable_id,
            pair[1].item.stable_id
        );
        if pair[0].score == pair[1].score {
            assert!(
                pair[0].item.stable_id < pair[1].item.stable_id,
                "equal scores must be broken by ascending ItemId: {:?} then {:?}",
                pair[0].item.stable_id,
                pair[1].item.stable_id
            );
        }
    }

    assert_eq!(
        hits[0].score, hits[1].score,
        "two items differing only by stable id must score identically"
    );
    assert_eq!(hits[0].method, MatchMethod::Prefix);
    assert_eq!(hits[1].method, MatchMethod::Prefix);
    assert_eq!(hits[2].method, MatchMethod::Substring);
    assert_eq!(hits[3].method, MatchMethod::Keyword);
    assert_eq!(hits[4].method, MatchMethod::Fuzzy);

    assert_eq!(
        hits[0].highlights.first().map(|&(start, _)| start),
        Some(0),
        "a prefix hit must carry the matcher's highlight over the head of its label"
    );
}

#[test]
fn a_bounded_answer_retains_the_best_hits_not_the_first_arrivals() {
    let owner = "dev.crikey.top-k";
    let items = vec![
        owned(owner, "z-weak", "Campfire Notes", "", &[]),
        owned(owner, "y-fuzzy", "File Reader", "", &[]),
        owned(owner, "d-tie", "Fire Atlas", "", &[]),
        owned(owner, "c-tie", "Fire Atlas", "", &[]),
        owned(owner, "b-tie", "Fire Atlas", "", &[]),
        owned(owner, "a-tie", "Fire Atlas", "", &[]),
    ];
    let cache = ScriptedCache::new(vec![(owner, Answer::Slice(mixed_slice(owner, items.clone())))]);
    let limits = ResultLimits {
        max_items_per_batch: 50,
        max_items_per_plugin_per_query: 50,
        max_items_per_query: 3,
        ..ResultLimits::default()
    };
    let mut service = accepting_service_with_limits(limits);
    service
        .load_persisted_catalog(&cache)
        .expect("load top-k fixture");

    let mut expected = brute_force(&items, "fire");
    expected.truncate(limits.max_items_per_query);
    service.submit_query("fire").expect("query accepted");

    assert_eq!(
        observed(service.results()),
        expected,
        "bounded selection must equal a complete rank, sort, and truncate"
    );
    assert_eq!(
        ids(service.results()),
        ["a-tie", "b-tie", "c-tie"],
        "equal scores straddling the boundary are retained by ascending identity"
    );
}

#[test]
fn bounded_selection_uses_the_same_match_position_as_materialized_ranking() {
    let owner = "dev.crikey.match-position";
    let items = vec![
        owned(owner, "a-leading-space", " Fire", "", &[]),
        owned(owner, "z-at-start", "Fire", "", &[]),
    ];
    let cache = ScriptedCache::new(vec![(owner, Answer::Slice(mixed_slice(owner, items.clone())))]);
    let limits = ResultLimits {
        max_items_per_batch: 1,
        max_items_per_plugin_per_query: 1,
        max_items_per_query: 1,
        ..ResultLimits::default()
    };
    let mut service = accepting_service_with_limits(limits);
    service
        .load_persisted_catalog(&cache)
        .expect("load match-position fixture");

    let mut expected = brute_force(&items, "fire");
    expected.truncate(1);
    service.submit_query("fire").expect("query accepted");

    assert_eq!(
        observed(service.results()),
        expected,
        "the selection summary and the materialized outcome must rank the same item"
    );
    assert_eq!(ids(service.results()), ["z-at-start"]);
}

#[test]
fn a_result_limit_wider_than_the_match_set_drops_nothing() {
    let items = fixture_items();
    let cache = ScriptedCache::new(vec![(PLUGIN, Answer::Slice(mixed_slice(PLUGIN, items.clone())))]);
    let limits = ResultLimits {
        max_items_per_batch: 50,
        max_items_per_plugin_per_query: 50,
        max_items_per_query: 64,
        ..ResultLimits::default()
    };
    let mut service = accepting_service_with_limits(limits);
    service.load_persisted_catalog(&cache).expect("load fixture");

    let expected = brute_force(&items, "fire");
    service.submit_query("fire").expect("query accepted");

    assert_eq!(observed(service.results()), expected);
    assert!(service.results().len() < limits.max_items_per_query);
}

#[test]
fn items_that_match_nothing_are_excluded_from_the_results() {
    let root = TempRoot::new("excluded");
    let cache = seeded_cache(root.path());

    let mut service = accepting_service();
    service
        .load_persisted_catalog(&cache)
        .expect("load the persisted catalog");

    service.submit_query("fire").expect("query accepted");
    let matched = ids(service.results());
    assert_eq!(
        matched.len(),
        FIXTURE_LEN - 1,
        "one fixture item answers no interpretation of `fire`"
    );
    assert!(
        !matched.iter().any(|id| id == "nonmatch"),
        "an item the matcher rejected must never reach the result list: {matched:?}"
    );

    let generation = service
        .submit_query("xylophone")
        .expect("a query that matches nothing is still a legal query");
    assert!(
        service.results().is_empty(),
        "no fixture item supports any interpretation of `xylophone`"
    );
    assert!(
        generation > Generation::ZERO,
        "an empty answer is still an answer and still carries its own generation"
    );
}

// ---------------------------------------------------------------------------
// Persistence (spec 22.1, 25.6)
// ---------------------------------------------------------------------------

#[test]
fn a_cold_cache_loads_nothing_without_failing_startup() {
    let root = TempRoot::new("cold");
    // Never seeded, and the root itself was never written: this is the very
    // first launch. Stage 2 of spec 25.6 has to survive a cache miss, because
    // the alternative is a launcher that cannot start until it has already run.
    let cache = FileCatalogCache::new(root.path().join("never-written"));

    let mut service = accepting_service();
    let loaded = service
        .load_persisted_catalog(&cache)
        .expect("a cold cache is a miss, not a startup failure");
    assert_eq!(loaded.items, 0, "a cold cache contributes no items");
    assert_eq!(
        loaded.skipped, 0,
        "a cold cache advertises no owner, so there is nothing to skip"
    );

    let generation = service
        .submit_query("fire")
        .expect("a cold start still accepts queries");
    assert!(
        service.results().is_empty(),
        "nothing was persisted, so nothing can be found"
    );
    assert!(
        generation > Generation::ZERO,
        "an empty catalog still answers with its own generation"
    );
}

#[test]
fn a_catalog_loaded_through_the_cache_becomes_searchable() {
    let root = TempRoot::new("persisted");
    let cache = seeded_cache(root.path());

    let mut service = accepting_service();

    service.submit_query("fire").expect("query accepted");
    assert!(
        service.results().is_empty(),
        "nothing is searchable before the persisted catalog is loaded"
    );

    let loaded = service
        .load_persisted_catalog(&cache)
        .expect("load the persisted catalog");
    assert_eq!(
        loaded.items, FIXTURE_LEN,
        "every persisted item must be admitted to the live catalog"
    );
    assert_eq!(
        loaded.skipped, 0,
        "the one owner the cache advertised was admitted, so nothing was skipped"
    );

    service.submit_query("fire").expect("query accepted");
    let hits = service.results();
    assert_eq!(ids(hits), FIRE_ORDER);

    // Item identity, not just item count, has to survive the round trip: the
    // fields below are what execution and ranking read back out.
    let atlas = hits
        .iter()
        .find(|hit| hit.item.stable_id.0 == "a-prefix")
        .expect("the prefix match survives persistence");
    assert_eq!(atlas.item.plugin_id.0, PLUGIN);
    assert_eq!(atlas.item.label, "Fire Atlas");
    assert_eq!(atlas.item.description, "Launch the map");
    assert_eq!(atlas.item.target, "app://a-prefix");
    assert_eq!(atlas.item.category, Category::Application);
    assert_eq!(atlas.item.argument_policy, ArgumentPolicy::Forbidden);
    assert_eq!(atlas.item.hit_policy, HitPolicy::Recorded);
}

#[test]
fn a_new_query_supersedes_the_previous_results() {
    let root = TempRoot::new("supersede");
    let cache = seeded_cache(root.path());

    let mut service = accepting_service();
    service
        .load_persisted_catalog(&cache)
        .expect("load the persisted catalog");

    let first = service.submit_query("fire").expect("first query");
    assert_eq!(ids(service.results()), FIRE_ORDER);

    let second = service.submit_query("water").expect("second query");
    assert!(second > first, "the newer query owns the newer generation");

    assert_eq!(
        ids(service.results()),
        ["nonmatch"],
        "the visible list must belong to the newest query alone, with no \
         survivors carried over from {first}"
    );
}

// ---------------------------------------------------------------------------
// Cache fault isolation (spec 22.1, 22.4, 25.6; ADR-0008)
// ---------------------------------------------------------------------------

#[test]
fn a_faulted_slice_is_a_miss_and_the_other_owners_still_load() {
    // A fault on either side of the healthy owner: whichever order the load
    // walks its owners in, it walks through a fault to reach a good slice.
    let cache = ScriptedCache::new(vec![
        (EARLY_PLUGIN, Answer::Fault),
        (PLUGIN, Answer::Slice(fixture_slice())),
        (LATE_PLUGIN, Answer::Fault),
    ]);

    let mut service = accepting_service();
    let loaded = service
        .load_persisted_catalog(&cache)
        .expect("a fault on one slice is that slice's miss, not a startup failure");

    assert_eq!(
        loaded.items, FIXTURE_LEN,
        "the readable owner must load in full despite the faults around it"
    );
    assert_eq!(
        loaded.skipped, 2,
        "every faulted owner must be counted, so a half-loaded catalog is diagnosable"
    );

    service.submit_query("fire").expect("query accepted");
    assert_eq!(
        ids(service.results()),
        FIRE_ORDER,
        "the surviving owner must be searchable, exactly as if the faults had not happened"
    );
}

#[test]
fn a_fault_enumerating_the_cache_root_is_reported_to_the_caller() {
    let mut service = accepting_service();

    assert_eq!(
        service.load_persisted_catalog(&ScriptedCache::unreadable_root()),
        Err(CacheError::Io {
            path: PathBuf::from(SCRIPTED_ROOT),
            kind: FAULT_KIND,
        }),
        "a root that cannot be enumerated names no owner, so there is no slice to isolate"
    );

    service
        .submit_query("fire")
        .expect("a reported cache fault still leaves the launcher answering queries");
    assert!(
        service.results().is_empty(),
        "nothing was loaded, so nothing can be found"
    );
}

#[test]
fn a_slice_the_catalog_refuses_costs_only_its_own_owner() {
    // Readable bytes the live catalog will refuse: the slice claims
    // EARLY_PLUGIN while every item in it names another owner. The refusal
    // lands after the slice's instance has already been authorized.
    let impostor = CachedSlice {
        plugin: PluginId(EARLY_PLUGIN.to_owned()),
        instance: 9,
        generation: Generation::ZERO,
        items: fixture_items(),
    };
    let cache = ScriptedCache::new(vec![
        (EARLY_PLUGIN, Answer::Slice(impostor)),
        (PLUGIN, Answer::Slice(fixture_slice())),
    ]);

    let mut service = accepting_service();
    let loaded = service
        .load_persisted_catalog(&cache)
        .expect("a refused slice is a miss, not a startup failure");

    assert_eq!(
        loaded.items, FIXTURE_LEN,
        "the owner whose slice the catalog accepted still loads in full"
    );
    assert_eq!(
        loaded.skipped, 1,
        "a refused slice is a skipped owner, not a silent zero"
    );

    service.submit_query("fire").expect("query accepted");
    assert_eq!(
        ids(service.results()),
        FIRE_ORDER,
        "a refused slice must contribute nothing and cost the other owners nothing"
    );
}

// ---------------------------------------------------------------------------
// Pruned search is result-identical (spec 11.1; roadmap M1)
// ---------------------------------------------------------------------------

/// The owners of the mixed catalog, ascending, so "one owner out of several"
/// is a claim about a catalog that really holds several.
const MIXED_OWNERS: [&str; 3] = [EARLY_PLUGIN, PLUGIN, LATE_PLUGIN];

/// Every item of the mixed catalog, matching and non-matching alike.
const MIXED_LEN: usize = 12;

/// A query answered by nothing in the mixed catalog.
const UNANSWERABLE_QUERY: &str = "xylophone";

/// A query whose only answer is carried by an item's description.
const DESCRIPTION_ONLY_QUERY: &str = "wildland";

/// A query only one of the three owners can answer.
const SINGLE_OWNER_QUERY: &str = "vector";

/// A query whose only answer is carried by an item's search terms, and whose
/// characters are absent from that item's label entirely.
const SEARCH_TERM_ONLY_QUERY: &str = "kumquat";

/// A query whose only answer is carried by an item's description, and whose
/// characters are absent from that item's label entirely.
const LABEL_DISJOINT_DESCRIPTION_QUERY: &str = "jigsaw";

/// The mixed catalog's answer to `fire`, sorted by id: a prefix hit, an
/// acronym hit, a substring hit, a keyword hit and a fuzzy hit, spread over
/// every owner.
const FIRE_MATCHES: [&str; 6] = [
    "early-atlas",
    "early-engine",
    "late-vault",
    "main-blaze",
    "main-campfire",
    "main-finder",
];

/// Queries the pruned path must answer exactly as an unpruned sweep would.
const EQUIVALENCE_QUERIES: [&str; 11] = [
    // Single- and two-character tokens: nearly every item admits them, so a
    // prefilter that is even slightly too eager answers with less than every
    // possible match, and that is the failure this catches.
    "f",
    "fi",
    // Prefix, substring, acronym, keyword and fuzzy hits at once, across all
    // three owners.
    "fire",
    // Two tokens, one of which reproduces a whole label.
    "fire atlas",
    // Carried by a description alone.
    DESCRIPTION_ONLY_QUERY,
    // Answerable by one owner out of three.
    SINGLE_OWNER_QUERY,
    "water",
    // A substring hit that starts in the middle of its label.
    "notes",
    // Answered by a search term, over an item whose label shares not one
    // character with the query.
    SEARCH_TERM_ONLY_QUERY,
    // The same, answered by a description.
    LABEL_DISJOINT_DESCRIPTION_QUERY,
    // Answered by nothing at all.
    UNANSWERABLE_QUERY,
];

fn early_items() -> Vec<Item> {
    vec![
        // Prefix on `fire`, and the whole label on `fire atlas`.
        owned(
            EARLY_PLUGIN,
            "early-atlas",
            "Fire Atlas",
            "Launch the wall map",
            &[],
        ),
        // Acronym on `fire`: the leading initials of all four words.
        owned(
            EARLY_PLUGIN,
            "early-engine",
            "Fast Image Rendering Engine",
            "Graphics toolkit",
            &[],
        ),
        // Answers `water` and nothing else the other queries ask for.
        owned(
            EARLY_PLUGIN,
            "early-clock",
            "Water Clock",
            "Track the tides",
            &["rain"],
        ),
    ]
}

fn main_items() -> Vec<Item> {
    vec![
        // Substring on `fire` and on `notes`.
        owned(PLUGIN, "main-campfire", "Campfire Notes", "Outdoor notebook", &[]),
        // Keyword on `fire` through a search term, with a label that supports
        // no interpretation of it at all.
        owned(
            PLUGIN,
            "main-blaze",
            "Blaze Guide",
            "Safety handbook",
            &["fire", "drill"],
        ),
        // The description-only case: neither the label nor a search term
        // carries `wildland`.
        owned(
            PLUGIN,
            "main-beacon",
            "Beacon Tower",
            "Signal relay for wildland crews",
            &[],
        ),
        // Fuzzy on `fire`: f-i-r-e in order, none of them adjacent, and no
        // searchable field containing the token.
        owned(
            PLUGIN,
            "main-finder",
            "Finder Escape",
            "Browse the filesystem",
            &[],
        ),
        // A search-term-only answer whose label shares no character at all
        // with `kumquat`: anything derived from the label alone - a mask, a
        // token set - cannot admit this item, and pruning it loses a true
        // match.
        owned(
            PLUGIN,
            "main-ledger",
            "Ledger Rows",
            "Spreadsheet views",
            &["kumquat"],
        ),
        // The same shape, answered by the description instead: no character of
        // `jigsaw` occurs in `Petrol Pump`.
        owned(
            PLUGIN,
            "main-petrol",
            "Petrol Pump",
            "Cutting guide for the jigsaw bench",
            &[],
        ),
    ]
}

fn late_items() -> Vec<Item> {
    vec![
        // Prefix on `fire`, from the owner that sorts last.
        owned(
            LATE_PLUGIN,
            "late-vault",
            "Fireproof Vault",
            "Document storage",
            &[],
        ),
        // The sole answer to `vector`, anywhere in the catalog.
        owned(
            LATE_PLUGIN,
            "late-vector",
            "Vector Studio",
            "Draw shapes",
            &["svg"],
        ),
        owned(LATE_PLUGIN, "late-tide", "Tide Table", "Coastal timings", &[]),
    ]
}

/// Every mixed-catalog item, in the order its owners are loaded.
fn mixed_items() -> Vec<Item> {
    let mut items = early_items();
    items.extend(main_items());
    items.extend(late_items());
    items
}

fn mixed_slice(plugin: &str, items: Vec<Item>) -> CachedSlice {
    CachedSlice {
        plugin: PluginId(plugin.to_owned()),
        instance: 1,
        generation: Generation::ZERO,
        items,
    }
}

/// The mixed catalog as three owners' cached slices.
///
/// Scripted rather than file-backed: the equivalence tests care about which
/// items are reachable, and nothing here needs a real archive.
fn mixed_cache() -> ScriptedCache {
    ScriptedCache::new(vec![
        (
            EARLY_PLUGIN,
            Answer::Slice(mixed_slice(EARLY_PLUGIN, early_items())),
        ),
        (PLUGIN, Answer::Slice(mixed_slice(PLUGIN, main_items()))),
        (LATE_PLUGIN, Answer::Slice(mixed_slice(LATE_PLUGIN, late_items()))),
    ])
}

/// A query-ready service holding the whole mixed catalog.
fn mixed_service() -> SearchService {
    let cache = mixed_cache();
    let mut service = accepting_service();
    let loaded = service
        .load_persisted_catalog(&cache)
        .expect("load the mixed catalog");

    assert_eq!(
        loaded.items, MIXED_LEN,
        "every mixed fixture item must be searchable, or an equal answer proves nothing"
    );
    assert_eq!(loaded.skipped, 0, "no owner of the mixed catalog may be skipped");
    service
}

/// One ranked answer, reduced to everything the caller can observe about it.
#[derive(Debug, PartialEq, Eq)]
struct Ranked {
    plugin: PluginId,
    id: ItemId,
    score: Score,
    method: MatchMethod,
    highlights: Vec<(usize, usize)>,
}

/// The ranked answer to `raw`, computed by sweeping every item.
///
/// This is the definition the pruned path has to meet: the same normalizer,
/// matcher and ranker the service composes, applied to the fixture directly,
/// with no catalog and therefore no candidate index anywhere in the path. The
/// final ordering repeats the service's own tie-break - descending score, then
/// ascending item id, then ascending owner - because equivalence is a claim
/// about the order too, not only about the set.
fn brute_force(items: &[Item], raw: &str) -> Vec<Ranked> {
    let normalizer = DefaultNormalizer::default();
    let matcher = DefaultMatcher::default();
    let ranker = DefaultRanker::default();
    let query = normalizer.normalize(raw);

    let mut ranked: Vec<Ranked> = items
        .iter()
        .filter_map(|item| {
            let outcome = matcher.match_item(&query, item)?;
            let score = ranker.score(&query, item, &outcome);
            Some(Ranked {
                plugin: item.plugin_id.clone(),
                id: item.stable_id.clone(),
                score,
                method: outcome.method,
                highlights: outcome.highlights,
            })
        })
        .collect();

    ranked.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.id.cmp(&right.id))
            .then_with(|| left.plugin.cmp(&right.plugin))
    });
    ranked
}

/// The service's answer in the shape the brute-force reference is stated in.
fn observed(hits: &[SearchHit]) -> Vec<Ranked> {
    hits.iter()
        .map(|hit| Ranked {
            plugin: hit.item.plugin_id.clone(),
            id: hit.item.stable_id.clone(),
            score: hit.score,
            method: hit.method,
            highlights: hit.highlights.clone(),
        })
        .collect()
}

#[test]
fn the_pruned_answer_is_the_unpruned_answer_query_for_query() {
    let items = mixed_items();
    let mut service = mixed_service();
    let mut exercised: Vec<MatchMethod> = Vec::new();

    for raw in EQUIVALENCE_QUERIES {
        let expected = brute_force(&items, raw);
        service.submit_query(raw).expect("query accepted");

        assert_eq!(
            observed(service.results()),
            expected,
            "`{raw}` must be answered exactly as a sweep over every item answers it"
        );

        for hit in &expected {
            if !exercised.contains(&hit.method) {
                exercised.push(hit.method);
            }
        }
    }

    // Equivalence over queries nothing answers is free. These are the
    // interpretations that make the comparison above worth making.
    for method in [
        MatchMethod::ExactPrefix,
        MatchMethod::Prefix,
        MatchMethod::Substring,
        MatchMethod::Acronym,
        MatchMethod::Keyword,
        MatchMethod::Fuzzy,
    ] {
        assert!(
            exercised.contains(&method),
            "the queries above must between them produce a {method:?} hit: {exercised:?}"
        );
    }
}

#[test]
fn every_owner_answers_the_mixed_query() {
    let items = mixed_items();
    let mut service = mixed_service();

    let expected = brute_force(&items, "fire");
    service.submit_query("fire").expect("query accepted");
    assert_eq!(observed(service.results()), expected);

    let mut matched = ids(service.results());
    matched.sort();
    assert_eq!(
        matched, FIRE_MATCHES,
        "no true match may be pruned away, whichever interpretation earned it"
    );

    let mut owners: Vec<String> = service
        .results()
        .iter()
        .map(|hit| hit.item.plugin_id.0.clone())
        .collect();
    owners.sort();
    owners.dedup();
    assert_eq!(
        owners, MIXED_OWNERS,
        "pruning is per owner, so every owner holding a match must contribute one"
    );
}

#[test]
fn a_match_carried_only_by_a_description_survives_pruning() {
    let items = mixed_items();
    let mut service = mixed_service();

    let expected = brute_force(&items, DESCRIPTION_ONLY_QUERY);
    service
        .submit_query(DESCRIPTION_ONLY_QUERY)
        .expect("query accepted");
    assert_eq!(
        observed(service.results()),
        expected,
        "a candidate index built from labels alone loses this answer"
    );

    let hits = service.results();
    assert_eq!(
        ids(hits),
        ["main-beacon"],
        "the description-only item is the whole answer to `{DESCRIPTION_ONLY_QUERY}`"
    );

    let hit = &hits[0];
    assert_eq!(hit.method, MatchMethod::Keyword);
    assert!(
        !hit.item.label.to_lowercase().contains(DESCRIPTION_ONLY_QUERY),
        "the label must not carry the token, or the case is not description-only: {:?}",
        hit.item.label
    );
    assert!(
        hit.item.search_terms.is_empty(),
        "no search term may carry the token either: {:?}",
        hit.item.search_terms
    );
    assert!(
        hit.item
            .description
            .to_lowercase()
            .contains(DESCRIPTION_ONLY_QUERY),
        "the description is what the match rests on: {:?}",
        hit.item.description
    );
}

#[test]
fn a_query_only_one_owner_can_answer_still_reaches_that_owner() {
    let items = mixed_items();
    let mut service = mixed_service();

    let expected = brute_force(&items, SINGLE_OWNER_QUERY);
    assert_eq!(
        expected.len(),
        1,
        "the fixture must give `{SINGLE_OWNER_QUERY}` exactly one true match"
    );

    service.submit_query(SINGLE_OWNER_QUERY).expect("query accepted");
    assert_eq!(
        observed(service.results()),
        expected,
        "the two owners that cannot answer must not be able to hide the one that can"
    );
    assert_eq!(ids(service.results()), ["late-vector"]);
    assert_eq!(
        service.results()[0].item.plugin_id,
        PluginId(LATE_PLUGIN.to_owned()),
        "the answer belongs to the owner that sorts last, so it is reached last"
    );
}

#[test]
fn a_query_nothing_answers_prunes_to_an_empty_answer() {
    let items = mixed_items();
    let mut service = mixed_service();

    assert!(
        brute_force(&items, UNANSWERABLE_QUERY).is_empty(),
        "no fixture item supports any interpretation of `{UNANSWERABLE_QUERY}`"
    );

    let generation = service
        .submit_query(UNANSWERABLE_QUERY)
        .expect("a query that matches nothing is still a legal query");
    assert!(
        service.results().is_empty(),
        "pruning to nothing is correct here, and an empty answer is still an answer"
    );
    assert!(generation > Generation::ZERO);

    // An empty answer must leave nothing behind that could hide the next one.
    let expected = brute_force(&items, "fire");
    service.submit_query("fire").expect("query accepted");
    assert_eq!(
        observed(service.results()),
        expected,
        "the query after an empty one must still find every match"
    );
}

#[test]
fn a_match_whose_label_shares_no_character_with_the_query_survives_pruning() {
    let items = mixed_items();
    let mut service = mixed_service();

    // Keyword matching reads the description and the search terms as well as
    // the label, so any prefilter derived from labels alone drops these two
    // answers outright. Neither label shares a single character with the query
    // that finds it, which is what makes the omission impossible to mask.
    for (raw, expected_id) in [
        (SEARCH_TERM_ONLY_QUERY, "main-ledger"),
        (LABEL_DISJOINT_DESCRIPTION_QUERY, "main-petrol"),
    ] {
        let item = items
            .iter()
            .find(|item| item.stable_id.0 == expected_id)
            .expect("the fixture must hold the item this query answers to");
        let label = item.label.to_lowercase();
        for character in raw.chars() {
            assert!(
                !label.contains(character),
                "`{expected_id}` must share no character with `{raw}`, and its label holds \
                 {character:?}: {label:?}"
            );
        }

        let expected = brute_force(&items, raw);
        assert_eq!(
            expected.len(),
            1,
            "`{raw}` must have exactly one true match for this regression to be sharp"
        );

        service.submit_query(raw).expect("query accepted");
        assert_eq!(
            observed(service.results()),
            expected,
            "`{raw}` is answered off the label, so pruning must consider every searchable field"
        );
        assert_eq!(ids(service.results()), [expected_id]);
        assert_eq!(service.results()[0].method, MatchMethod::Keyword);
    }
}
