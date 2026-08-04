//! Synthetic workloads for CriKey performance work (spec 25, 27.3).
//!
//! [`run_catalog_benchmark`] is the 500,000-item catalog harness of spec 25.1.
//! It builds a synthetic catalog, persists it through the file-backed catalog
//! cache, reloads it, and then runs a fixed set of queries against the
//! *reloaded* catalog through the real query and ranking engines. The point is
//! to measure the production path end to end: nothing here reimplements
//! normalization, matching or ranking, and nothing keeps the built catalog in
//! memory to answer the queries from.
//!
//! # What this harness measures, and what it does not
//!
//! It measures exactly one archive: the one `crikey-catalog` actually ships, a
//! full-decode owning codec in which [`CatalogCache::load_slice`] reads the
//! whole file and materializes owned [`Item`]s out of it. Every figure in a
//! [`BenchmarkReport`] therefore describes that archive and nothing else, which
//! is why each report carries [`ARCHIVE_FORMAT`]: a saved measurement that does
//! not name what it measured cannot be compared against anything later.
//!
//! It is not, by itself, a comparison of serialization formats. Running it
//! yields no figure for `rkyv`, for `bincode`, or for a memory-mapped zero-copy
//! layout, because no such encoder exists in this workspace to run it against.
//! Weighing the shipped archive against an alternative means implementing that
//! alternative and running this same harness over both; nothing here does that
//! on its own, and no report it emits should be read as having done it.
//!
//! The harness only *reports*. It contains no latency budget, no memory budget
//! and no assertion on either: the budgets of spec 25.1 (warm activation under
//! 30 ms p95, cached local results under 16 ms p95) and the idle-memory target
//! of ADR-0008 are properties of the documented reference machine, so comparing
//! against them belongs to whoever runs the harness on it. What the harness
//! does guarantee is that every count it reports is reproducible from one run
//! to the next, since the workload carries no seed, no clock and no randomness.
//!
//! # The query workload
//!
//! Every query goes through the catalog's candidate prefilter (spec 11.1)
//! before anything is scored, because that is the path a keystroke takes in
//! the launcher: a harness that scanned the whole slice itself would measure a
//! search the product no longer performs. What the prefilter removed is
//! reported rather than inferred. [`BenchmarkReport::candidates_examined`]
//! counts every (item, query) pair the phase handed to the matcher, so a run
//! that still scores the whole catalog is visible as a number instead of only
//! as a latency this crate refuses to judge.
//!
//! Each configured query is typed one character at a time. The report carries
//! both an overall percentile across those keystrokes and one bucket per
//! prefix length, so a fast fully typed query cannot hide slow early input.
//! Synthetic labels draw from deterministic varied vocabularies: alphabetic
//! prefixes select graded portions of the catalog rather than every item.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use crikey_app::{App, SearchService, StartupStage};
use crikey_catalog::{CachedSlice, CatalogCache, FileCatalogCache, SCHEMA_VERSION};
use crikey_core::{
    ArgumentPolicy, Category, Generation, GenerationTracker, HitPolicy, Item, ItemId, PluginId,
};
use crikey_result_aggregator::ResultLimits;

/// Stress-test target from the specification: 500,000 indexed catalog items.
pub const STRESS_CATALOG_SIZE: usize = 500_000;

/// The archive every [`BenchmarkReport`] describes.
///
/// The launcher ships exactly one catalog archive, so this is a constant rather
/// than a choice: [`run_catalog_benchmark`] has one format to measure and names
/// it in every report, because a saved measurement that does not say what it
/// measured cannot be compared against anything later.
pub const ARCHIVE_FORMAT: &str = "crikey-catalog-archive-v1";

// The label spells out the catalog's schema version, which makes it a claim
// about `crikey-catalog` rather than a string this crate is free to invent.
// Bumping that version must rename the format here in the same commit, or every
// report stored afterwards attributes new numbers to the old archive.
const _: () = assert!(
    SCHEMA_VERSION == 1,
    "ARCHIVE_FORMAT names catalog schema version 1; rename it when the schema moves"
);

/// Plugin the synthetic catalog is attributed to.
const BENCHMARK_PLUGIN: &str = "crikey.benchmarks";

/// Owner of every synthetic item, and therefore of the persisted cache slice.
fn benchmark_plugin() -> PluginId {
    PluginId(BENCHMARK_PLUGIN.into())
}

/// Instance the synthetic slice is published under.
///
/// One instance for the whole run: the slice is stored under it and the
/// reloaded slice is republished under it, so the query phase reads back the
/// catalog the store wrote rather than a second one that merely resembles it.
const BENCHMARK_INSTANCE: u64 = 1;

const ADJECTIVES: [&str; 25] = [
    "Aurora", "Binary", "Cedar", "Delta", "Ember", "Frost", "Golden", "Harbor", "Indigo", "Juniper",
    "Kinetic", "Lunar", "Mosaic", "Nimbus", "Opal", "Quartz", "River", "Solar", "Tidal", "Umber", "Velvet",
    "Willow", "Xenon", "Yellow", "Zephyr",
];

const NOUNS: [&str; 16] = [
    "Archive",
    "Browser",
    "Calculator",
    "Calendar",
    "Compass",
    "Editor",
    "Gallery",
    "Messenger",
    "Monitor",
    "Notebook",
    "Planner",
    "Player",
    "Settings",
    "Studio",
    "Terminal",
    "Vault",
];

fn synthetic_label(index: usize) -> String {
    let adjective = ADJECTIVES[index % ADJECTIVES.len()];
    let noun = NOUNS[(index / ADJECTIVES.len()) % NOUNS.len()];
    format!("{adjective} {noun} {index:06}")
}

/// Builds a deterministic synthetic catalog of `count` items.
pub fn synthetic_catalog(count: usize) -> Vec<Item> {
    let plugin = benchmark_plugin();
    (0..count)
        .map(|index| {
            let target = format!("/synthetic/app-{index:06}");
            let label = synthetic_label(index);
            let description = format!(
                "{} utility from the {} collection",
                NOUNS[(index + 7) % NOUNS.len()],
                ADJECTIVES[(index + 11) % ADJECTIVES.len()]
            );
            Item {
                stable_id: ItemId::derived(&plugin, &Category::Application, &target),
                plugin_id: plugin.clone(),
                category: Category::Application,
                label: label.clone(),
                description,
                target,
                search_terms: vec![label],
                icon_reference: None,
                argument_policy: ArgumentPolicy::Forbidden,
                hit_policy: HitPolicy::Recorded,
                score_hint: 0,
                metadata: BTreeMap::new(),
                actions: Vec::new(),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Configuration and report
// ---------------------------------------------------------------------------

/// One benchmark workload.
#[derive(Debug, Clone, Copy)]
pub struct BenchmarkConfig {
    /// Synthetic items to build, persist and reload.
    pub items: usize,
    /// Queries to run against the reloaded catalog.
    pub queries: usize,
    /// Results each query retains, as the launcher's result list would.
    pub top_k: usize,
}

/// Aggregate latency and work for one typed prefix length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixLatency {
    /// Number of Unicode scalar values typed.
    pub prefix_chars: usize,
    /// Configured queries long enough to contribute this prefix.
    pub samples: usize,
    /// Median latency among those samples.
    pub nanos_p50: u64,
    /// 95th-percentile latency among those samples.
    pub nanos_p95: u64,
    /// Candidates examined by bounded selection after catalog prefilters.
    pub candidates_examined: u64,
}

/// What one benchmark run observed.
///
/// The counts describe the catalog that came back *out* of the cache, so a
/// lossy store or a discarded slice shows up as a shortfall rather than being
/// hidden by the still-live source catalog. Those counts, the archive size and
/// the format label reproduce exactly between two identical runs; the
/// nanosecond and resident-byte fields are raw measurements of the host and are
/// the part of the report allowed to differ, and nothing in this crate compares
/// any of them to a budget.
///
/// The resident figures describe the *process*, not the harness's own
/// footprint. The two `peak_rss_bytes_*` fields are high-water marks, and a
/// high-water mark never falls, so it still carries whatever the process
/// peaked at before the run — the build phase's source catalog, and anything
/// else sharing the process. `resident_bytes_after_load` is the opposite
/// measurement, sampled rather than accumulated after the source catalog has
/// been dropped. It reports the process's loaded steady state, including pages
/// the allocator retained from the build phase; it is not an incremental
/// measurement of catalog allocations alone. Running `crikey dev benchmark`
/// in its own process excludes unrelated test workloads, but not allocator or
/// runtime overhead.
#[derive(Debug, Clone)]
pub struct BenchmarkReport {
    /// The archive every figure here describes; always [`ARCHIVE_FORMAT`].
    pub format: &'static str,
    /// Items in the reloaded catalog.
    pub items: usize,
    /// Distinct stable ids in the reloaded catalog.
    pub unique_items: usize,
    /// Time to build the synthetic catalog in memory.
    pub build_nanos: u64,
    /// Time to persist the catalog as one cache slice.
    pub store_nanos: u64,
    /// Time to read that slice back.
    pub load_nanos: u64,
    /// Median keystroke latency across every typed query prefix.
    pub query_nanos_p50: u64,
    /// 95th-percentile keystroke latency across every typed query prefix.
    pub query_nanos_p95: u64,
    /// Results retained across every query, each bounded by
    /// [`BenchmarkConfig::top_k`].
    pub matched_total: usize,
    /// (item, query) pairs examined by bounded selection after the catalog
    /// prefilters of spec 11.1. A run reporting `items * queries` here pruned
    /// nothing at the catalog boundary and is still paying for a full scan on
    /// every keystroke.
    pub candidates_examined: u64,
    /// Number of individual query prefixes measured.
    pub prefix_samples: usize,
    /// Latency and candidate work grouped by typed prefix length.
    pub prefix_latencies: Vec<PrefixLatency>,
    /// Bytes the persisted archive occupies on disk: what keeping this catalog
    /// between launches costs, as opposed to what holding it costs.
    pub archive_bytes: u64,
    /// Process peak resident bytes once the catalog was reloaded, or zero where
    /// the platform reports no peak.
    pub peak_rss_bytes_after_load: u64,
    /// Process peak resident bytes once the query phase finished, or zero where
    /// the platform reports no peak.
    pub peak_rss_bytes_after_query: u64,
    /// Process resident bytes once the catalog was reloaded and indexed, or
    /// zero where the platform reports no figure. Unlike the high-water marks
    /// above this one falls when memory is released, so it separates the
    /// loaded catalog's footprint from the build phase's peak.
    pub resident_bytes_after_load: u64,
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Builds, persists, reloads and queries a synthetic catalog, reporting what it
/// measured.
///
/// The cache slice is written into a temp directory unique to this run and
/// removed before returning, so concurrent runs never share state and a run
/// leaves nothing behind. A cache fault is not treated as a measurement
/// failure: the reload then observes an empty catalog and every count in the
/// report reflects that honestly rather than panicking on a machine whose temp
/// directory is unwritable.
pub fn run_catalog_benchmark(config: &BenchmarkConfig) -> BenchmarkReport {
    let plugin = benchmark_plugin();

    let started = Instant::now();
    let source = synthetic_catalog(config.items);
    let build_nanos = elapsed_nanos(started);

    let root = TempRoot::new();
    let cache = FileCatalogCache::new(root.cache_dir());

    let slice = CachedSlice {
        plugin: plugin.clone(),
        instance: BENCHMARK_INSTANCE,
        generation: first_generation(),
        items: source,
    };
    let started = Instant::now();
    let stored = cache.store_slice(&slice);
    let store_nanos = elapsed_nanos(started);

    // The source catalog is dead once persisted, and at stress scale it is the
    // single largest allocation in the process: dropping it here keeps peak
    // memory at one catalog rather than two.
    drop(slice);
    let _ = stored;

    // Sized outside the timed region: stat-ing the archive is measurement, not
    // storage. A store that failed left no file, which reports as zero bytes.
    let archive_bytes = stored_bytes(&root.cache_dir());

    let limits = ResultLimits {
        max_items_per_plugin_per_query: config.top_k,
        max_items_per_query: config.top_k,
        ..ResultLimits::default()
    };
    let mut service = SearchService::new(App::with_limits(limits));
    let _ = service.complete_stage(StartupStage::WindowAndHotkey);

    let started = Instant::now();
    let loaded = service.load_persisted_catalog(&cache);
    let load_nanos = elapsed_nanos(started);
    let item_count = loaded.as_ref().map_or(0, |load| load.items);
    // MemoryCatalog admits at most one row per composite identity, so its
    // retained count is also the round-tripped unique-identity count.
    let unique_items = item_count;

    let _ = service.complete_stage(StartupStage::PersistedCatalog);
    let _ = service.complete_stage(StartupStage::AcceptQueries);

    // Sampled before the query bookkeeping: this is the steady loaded state.
    let loaded_memory = memory_sample();
    let queries = query_phase(&mut service, config);
    let queried_memory = memory_sample();

    BenchmarkReport {
        format: ARCHIVE_FORMAT,
        items: item_count,
        unique_items,
        build_nanos,
        store_nanos,
        load_nanos,
        query_nanos_p50: queries.p50,
        query_nanos_p95: queries.p95,
        matched_total: queries.matched_total,
        candidates_examined: queries.candidates_examined,
        prefix_samples: queries.prefix_samples,
        prefix_latencies: queries.prefix_latencies,
        archive_bytes,
        peak_rss_bytes_after_load: loaded_memory.peak_bytes,
        peak_rss_bytes_after_query: queried_memory.peak_bytes.max(loaded_memory.peak_bytes),
        resident_bytes_after_load: loaded_memory.resident_bytes,
    }
}

#[derive(Debug)]
struct QueryPhase {
    p50: u64,
    p95: u64,
    matched_total: usize,
    candidates_examined: u64,
    prefix_samples: usize,
    prefix_latencies: Vec<PrefixLatency>,
}

#[derive(Debug, Default)]
struct PrefixAccumulator {
    nanos: Vec<u64>,
    candidates_examined: u64,
}

/// Types every configured query through the shipped [`SearchService`].
///
/// The timed region is exactly [`SearchService::submit_query`], including
/// generation replacement, candidate pruning, matching, bounded selection,
/// aggregation and final ordering.
fn query_phase(service: &mut SearchService, config: &BenchmarkConfig) -> QueryPhase {
    let mut samples = Vec::new();
    let mut by_length: BTreeMap<usize, PrefixAccumulator> = BTreeMap::new();
    let mut matched_total = 0usize;
    let mut candidates_examined = 0u64;

    for index in 0..config.queries {
        let text = benchmark_query(index, config);
        for (prefix_index, (offset, character)) in text.char_indices().enumerate() {
            let prefix_chars = prefix_index + 1;
            let end = offset + character.len_utf8();
            let started = Instant::now();
            if service.submit_query(&text[..end]).is_err() {
                continue;
            }
            let nanos = elapsed_nanos(started);
            let stats = service.last_query_stats();

            samples.push(nanos);
            matched_total = matched_total.saturating_add(service.results().len());
            candidates_examined = candidates_examined.saturating_add(stats.candidates_examined);
            let bucket = by_length.entry(prefix_chars).or_default();
            bucket.nanos.push(nanos);
            bucket.candidates_examined = bucket
                .candidates_examined
                .saturating_add(stats.candidates_examined);
        }
    }

    let prefix_samples = samples.len();
    samples.sort_unstable();
    let prefix_latencies = by_length
        .into_iter()
        .map(|(prefix_chars, mut bucket)| {
            bucket.nanos.sort_unstable();
            PrefixLatency {
                prefix_chars,
                samples: bucket.nanos.len(),
                nanos_p50: percentile(&bucket.nanos, 50),
                nanos_p95: percentile(&bucket.nanos, 95),
                candidates_examined: bucket.candidates_examined,
            }
        })
        .collect();

    QueryPhase {
        p50: percentile(&samples, 50),
        p95: percentile(&samples, 95),
        matched_total,
        candidates_examined,
        prefix_samples,
        prefix_latencies,
    }
}

/// The complete text of the `index`-th benchmark query.
///
/// It exactly names one synthetic label. The query phase types every prefix,
/// so the early broad alphabetic states and the final selective numeric state
/// are both represented in the reported percentile curve.
fn benchmark_query(index: usize, config: &BenchmarkConfig) -> String {
    synthetic_label(pinned_index(index, config))
}

/// The catalog index the `index`-th query pins.
///
/// The queries walk the catalog in even steps, so successive queries name
/// items far apart in it instead of re-asking one corner. Two properties make
/// the walk land on indices worth measuring.
///
/// It is as wide as the catalog allows — four digits in a 2,048-item catalog,
/// six in a 500,000-item one — because a short index is a substring of the
/// longer ones that contain it and is answered by all of them.
///
/// And its digits are distinct. That is not cosmetic. The prefilter admits an
/// item whose text carries every *character* of the query, so `100000` asks
/// for two characters that a fifth of a numbered catalog carries, and a query
/// like it measures the fixture's alphabet rather than the prefilter. A
/// repeated digit is an artifact of numbering items instead of naming them —
/// the word a real query pins spreads over as many characters as it is long —
/// so the walk steps over those indices. Where the catalog holds none, the
/// step is kept as it fell: an honest measurement of a degenerate catalog
/// beats no measurement at all.
fn pinned_index(index: usize, config: &BenchmarkConfig) -> usize {
    let decade = index_decade(config.items);
    let span = config.items.saturating_sub(decade);
    if span == 0 {
        // A catalog too small to hold an index of the full width; its last
        // item is the most specific thing there is to pin.
        return config.items.saturating_sub(1);
    }

    // `queries` bounds the walk, and a zero-query configuration runs no queries
    // at all; treating it as one keeps this total for any index it is asked.
    let queries = config.queries.max(1);
    let start = decade + ((span.saturating_mul(index) / queries) % span);

    let mut pinned = start;
    for _ in 0..span {
        if spells_distinct_digits(pinned) {
            return pinned;
        }
        pinned += 1;
        if pinned >= config.items {
            pinned = decade;
        }
    }
    start
}

/// The place value of the catalog's highest index: 1,000 for 2,048 items.
fn index_decade(items: usize) -> usize {
    let highest = items.saturating_sub(1);
    let mut decade = 1usize;
    while decade <= highest / 10 {
        decade *= 10;
    }
    decade
}

/// Whether `value`'s decimal spelling repeats no digit.
fn spells_distinct_digits(value: usize) -> bool {
    let mut seen = 0u16;
    let mut rest = value;
    loop {
        let bit = 1u16 << (rest % 10);
        if seen & bit != 0 {
            return false;
        }
        seen |= bit;
        rest /= 10;
        if rest == 0 {
            return true;
        }
    }
}

/// The generation a freshly built catalog is published under.
fn first_generation() -> Generation {
    GenerationTracker::new().advance()
}

/// Elapsed nanoseconds since `started`, saturating rather than wrapping.
fn elapsed_nanos(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Nearest-rank percentile of an ascending sample set.
///
/// Nearest rank rather than interpolation: every reported figure is then a
/// sample the harness actually measured. The rank rises monotonically with
/// `percent`, so a higher percentile can never select below a lower one.
fn percentile(ascending: &[u64], percent: u32) -> u64 {
    let last = match ascending.len().checked_sub(1) {
        Some(last) => last,
        // No samples is not a measurement; reporting zero says so.
        None => return 0,
    };
    let rank = (ascending.len() as u128 * u128::from(percent)).div_ceil(100);
    let index = usize::try_from(rank.saturating_sub(1)).unwrap_or(last);
    ascending[index.min(last)]
}

// ---------------------------------------------------------------------------
// Size and memory sampling
// ---------------------------------------------------------------------------

/// Bytes the archives under `root` occupy on disk.
///
/// A benchmark run owns its cache root and stores exactly one slice into it, so
/// summing the directory measures that slice. Summing is also the only way to
/// do it from out here: the file name a slice lands under is spelled by
/// `crikey-catalog` alone, and reproducing that escaping would be a second copy
/// of it. A run whose store failed left no file behind and is reported as zero.
fn stored_bytes(root: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(root) else {
        // An absent or unreadable root is a cold cache, not a fault.
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(fs::Metadata::is_file)
        .fold(0, |total, metadata| total.saturating_add(metadata.len()))
}

/// One reading of the process's memory, peak and current together.
///
/// Both come from a single sample so the pair is internally consistent: a
/// current figure above its own peak would be an artifact of reading twice.
#[derive(Debug, Clone, Copy, Default)]
struct MemorySample {
    /// Highest resident bytes the process has ever held, or zero where the
    /// platform publishes no mark.
    peak_bytes: u64,
    /// Resident bytes the process holds at the instant of the sample, or zero
    /// where the platform publishes no figure.
    resident_bytes: u64,
}

/// Samples the process's peak and current resident size.
///
/// The kernel maintains the high-water mark, so one small read cannot miss a
/// peak that happened between two samples the way polling current usage would.
/// The current size is the opposite measurement, and taking both is the whole
/// point: the mark never falls, so only the current size can tell a loaded
/// catalog apart from a peak the build phase left behind. It is a ceiling on
/// the catalog's own footprint rather than an exact figure, since it also
/// counts whatever pages the allocator has not returned to the kernel.
#[cfg(target_os = "linux")]
fn memory_sample() -> MemorySample {
    fs::read_to_string("/proc/self/status").map_or_else(
        |_| MemorySample::default(),
        |status| MemorySample {
            peak_bytes: parse_status_kibibytes(&status, "VmHWM:"),
            resident_bytes: parse_status_kibibytes(&status, "VmRSS:"),
        },
    )
}

/// Zero: no portable resident-memory source exists, and the memory target of
/// ADR-0008 is tracked on the Linux reference machine.
#[cfg(not(target_os = "linux"))]
fn memory_sample() -> MemorySample {
    MemorySample::default()
}

/// The `field` line of `/proc/self/status`, in bytes.
///
/// A field is absent on kernels that do not track it, and its unit is part of
/// the contract, so anything this function does not recognise — no such line,
/// an unparseable count, a unit other than kibibytes — answers zero. An
/// invented memory figure is worse than a missing one, because the missing one
/// is visible.
#[cfg(any(target_os = "linux", test))]
fn parse_status_kibibytes(status: &str, field: &str) -> u64 {
    let Some(line) = status.lines().find_map(|line| line.strip_prefix(field)) else {
        return 0;
    };
    let mut fields = line.split_ascii_whitespace();
    let Some(Ok(kibibytes)) = fields.next().map(str::parse::<u64>) else {
        return 0;
    };
    if fields.next() != Some("kB") {
        return 0;
    }
    // Saturating rather than wrapping: a nonsense count must not come back as a
    // small plausible number of bytes.
    kibibytes.saturating_mul(1024)
}

// ---------------------------------------------------------------------------
// Temp root
// ---------------------------------------------------------------------------

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

/// A temp directory unique to one benchmark run, removed when the run ends.
///
/// The guard is created with an exclusive operation. A pre-existing path is
/// never removed or reused, so a stale run or another process cannot lose data
/// when this benchmark starts. The cache root is a child of the guard and is
/// deliberately not created until the cache stores its first slice.
struct TempRoot {
    dir: PathBuf,
    owned: bool,
}

impl TempRoot {
    fn new() -> Self {
        let mut owned = false;
        let dir = loop {
            let unique = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
            let candidate = std::env::temp_dir().join(format!(
                "crikey-catalog-benchmark-{pid}-{unique}",
                pid = std::process::id()
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => {
                    owned = true;
                    break candidate;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => break candidate,
            }
        };
        Self { dir, owned }
    }

    fn cache_dir(&self) -> PathBuf {
        self.dir.join("cache")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        if self.owned {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use crikey_query::{DefaultMatcher, DefaultNormalizer, Matcher, Normalizer};

    #[test]
    fn synthetic_items_have_unique_stable_ids() {
        let items = synthetic_catalog(1_000);
        let unique: HashSet<_> = items.iter().map(|item| item.stable_id.clone()).collect();
        assert_eq!(unique.len(), items.len());
    }

    #[test]
    fn percentiles_select_measured_samples_in_rank_order() {
        let samples: Vec<u64> = (1..=20).collect();
        assert_eq!(percentile(&samples, 50), 10);
        assert_eq!(percentile(&samples, 95), 19);
        assert_eq!(percentile(&samples, 100), 20);
        // A single sample answers every percentile, and no samples answers none.
        assert_eq!(percentile(&[7], 50), 7);
        assert_eq!(percentile(&[7], 95), 7);
        assert_eq!(percentile(&[], 95), 0);
    }

    /// The configuration the selectivity of the workload is reasoned about at,
    /// and the one the integration suite runs.
    fn query_config() -> BenchmarkConfig {
        BenchmarkConfig {
            items: 2_048,
            queries: 16,
            top_k: 20,
        }
    }

    #[test]
    fn a_benchmark_query_pins_a_full_width_index_of_the_configured_catalog() {
        let config = query_config();

        for index in 0..config.queries {
            let pinned = pinned_index(index, &config);
            assert!(
                (1_000..config.items).contains(&pinned),
                "query {index} pinned {pinned}, which is not a four-digit index of the catalog"
            );
            assert!(
                spells_distinct_digits(pinned),
                "query {index} pinned {pinned}, whose repeated digit spells a query \
                 over too small an alphabet to be selective"
            );
            assert_eq!(benchmark_query(index, &config), synthetic_label(pinned));
        }

        // Derivable from the configuration alone, and not the same query twice
        // over: the walk covers the catalog rather than one corner of it.
        assert_eq!(benchmark_query(3, &config), benchmark_query(3, &config));
        assert_ne!(benchmark_query(0, &config), benchmark_query(8, &config));
    }

    #[test]
    fn a_benchmark_query_is_answered_by_the_one_item_it_pins() {
        let config = query_config();
        let items = synthetic_catalog(config.items);
        let normalizer = DefaultNormalizer::default();
        let matcher = DefaultMatcher::default();

        // The property the whole pruning measurement rests on, checked against
        // the real matcher rather than assumed from the shape of the label: a
        // query the catalog answers in bulk would make a prefilter look useless
        // however well it works.
        for index in 0..config.queries {
            let query = normalizer.normalize(&benchmark_query(index, &config));
            let answered: Vec<usize> = items
                .iter()
                .enumerate()
                .filter(|(_, item)| matcher.match_item(&query, item).is_some())
                .map(|(position, _)| position)
                .collect();
            assert_eq!(
                answered,
                vec![pinned_index(index, &config)],
                "{:?} must be answered by the one item it names",
                query.raw
            );
        }
    }

    #[test]
    fn the_pinned_index_is_as_wide_as_the_catalog_allows() {
        assert_eq!(index_decade(0), 1);
        assert_eq!(index_decade(1), 1);
        assert_eq!(index_decade(10), 1);
        assert_eq!(index_decade(11), 10);
        assert_eq!(index_decade(64), 10);
        assert_eq!(index_decade(256), 100);
        assert_eq!(index_decade(2_048), 1_000);
        assert_eq!(index_decade(STRESS_CATALOG_SIZE), 100_000);

        // A catalog with no index of the full width still has to answer, and
        // its last item is the most specific thing there is to pin.
        let tiny = BenchmarkConfig {
            items: 1,
            queries: 4,
            top_k: 1,
        };
        assert_eq!(pinned_index(2, &tiny), 0);
    }

    #[test]
    fn a_repeated_digit_does_not_spell_a_distinctive_index() {
        assert!(spells_distinct_digits(0));
        assert!(spells_distinct_digits(1_023));
        assert!(spells_distinct_digits(102_345));
        assert!(!spells_distinct_digits(11));
        assert!(!spells_distinct_digits(1_223));
        assert!(!spells_distinct_digits(100_000));
    }

    #[test]
    fn the_temp_root_is_unique_per_run_and_removed_with_the_guard() {
        let first = TempRoot::new();
        let second = TempRoot::new();
        assert_ne!(first.cache_dir(), second.cache_dir());

        let dir = second.dir.clone();
        assert!(dir.is_dir(), "the guard directory must exist while held");
        drop(second);
        assert!(!dir.exists(), "the guard directory must not outlive the run");
    }

    #[test]
    fn the_archive_size_reports_what_the_cache_actually_wrote() {
        let root = TempRoot::new();
        // The cache root is a child the cache creates on first store, and an
        // absent root is a cold cache holding no bytes.
        assert_eq!(stored_bytes(&root.cache_dir()), 0);

        let cache = FileCatalogCache::new(root.cache_dir());
        let store = |items| {
            let slice = CachedSlice {
                plugin: benchmark_plugin(),
                instance: 1,
                generation: first_generation(),
                items: synthetic_catalog(items),
            };
            cache
                .store_slice(&slice)
                .expect("the temp cache root must be writable");
            stored_bytes(&root.cache_dir())
        };

        let small = store(8);
        assert!(small > 0, "a persisted slice occupies bytes on disk");
        // Re-storing replaces the archive instead of adding to it, so the figure
        // describes one archive rather than a directory that accumulated the
        // scratch file of every write.
        assert_eq!(store(8), small);
        assert!(
            store(64) > small,
            "a larger catalog cannot fit in the same archive"
        );
    }

    #[test]
    fn the_resident_marks_are_read_out_of_the_kernel_fields_in_bytes() {
        let status = "Name:\tcrikey\nVmRSS:\t    1024 kB\nVmHWM:\t    4096 kB\nThreads:\t1\n";
        assert_eq!(parse_status_kibibytes(status, "VmHWM:"), 4096 * 1024);
        assert_eq!(parse_status_kibibytes(status, "VmRSS:"), 1024 * 1024);
    }

    #[test]
    fn an_unreadable_resident_mark_reports_no_measurement() {
        for (case, text) in [
            ("no status at all", ""),
            ("the field absent", "VmRSS:\t 1024 kB\n"),
            ("no value", "VmHWM:\n"),
            ("an unparseable value", "VmHWM:\t not-a-number kB\n"),
            ("a negative value", "VmHWM:\t -1 kB\n"),
            ("a unit this parser does not know", "VmHWM:\t 4096 MB\n"),
            ("no unit", "VmHWM:\t 4096\n"),
        ] {
            assert_eq!(
                parse_status_kibibytes(text, "VmHWM:"),
                0,
                "{case} is not a peak worth inventing"
            );
        }

        // Saturating rather than wrapping: a nonsense count must not come back
        // as a small plausible number of bytes.
        assert_eq!(
            parse_status_kibibytes("VmHWM:\t 18446744073709551615 kB\n", "VmHWM:"),
            u64::MAX
        );
    }
}
