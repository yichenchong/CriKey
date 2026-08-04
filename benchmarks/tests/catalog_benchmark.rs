//! Contract for the 500k-item catalog benchmark harness (spec 25.1, roadmap M1).
//!
//! The harness *measures*; these tests defend only what must be true of a
//! measurement, never how large or how fast it was. Latency budgets — warm
//! activation below 30 ms p95, cached local results below 16 ms p95 — and the
//! idle-memory target of ADR-0008 are properties of the documented reference
//! machine (spec 25.1) and are deliberately absent here: a CI runner or a
//! loaded developer laptop would turn them into a coin flip. What *is*
//! deterministic is arithmetic: how many items went in, how many came back out
//! of the persistent cache, how many bytes that cost on disk, and that the
//! harness reported a figure for every phase and every axis it claims to
//! measure.
//!
//! Pinned API, implemented by the behavior wave in `crikey-benchmarks`:
//!
//! ```ignore
//! #[derive(Debug, Clone, Copy)]
//! pub struct BenchmarkConfig {
//!     pub items: usize,
//!     pub queries: usize,
//!     pub top_k: usize,
//! }
//!
//! #[derive(Debug, Clone)]
//! pub struct BenchmarkReport {
//!     pub format: &'static str,
//!     pub items: usize,
//!     pub unique_items: usize,
//!     pub build_nanos: u64,
//!     pub store_nanos: u64,
//!     pub load_nanos: u64,
//!     pub query_nanos_p50: u64,
//!     pub query_nanos_p95: u64,
//!     pub matched_total: usize,
//!     pub candidates_examined: u64,
//!     pub archive_bytes: u64,
//!     pub peak_rss_bytes_after_load: u64,
//!     pub peak_rss_bytes_after_query: u64,
//! }
//!
//! pub const ARCHIVE_FORMAT: &str;
//! pub fn run_catalog_benchmark(config: &BenchmarkConfig) -> BenchmarkReport;
//! ```
//!
//! The harness builds `config.items` synthetic items, persists them through the
//! catalog cache, reloads them, and runs `config.queries` deterministic queries
//! against the *reloaded* catalog keeping at most `config.top_k` results each.
//! `items` and `unique_items` therefore describe the round-tripped catalog, and
//! `matched_total` is the number of results retained across every query, which
//! is what makes `top_k` observable in the report.
//!
//! `candidates_examined` counts the (item, query) pairs bounded selection
//! considered after the catalog prefilters. The prefilter of spec 11.1 exists
//! to hold that number below a full scan, so the harness has to report it: a
//! run that considers every item for every query is still doing the linear
//! work the prefilter was added to remove, and in a report whose only other
//! query figures are latencies this file deliberately refuses to assert on,
//! that is otherwise invisible.
//!
//! `archive_bytes` and the two `peak_rss_bytes_*` figures are the memory axis
//! ADR-0008 turns on, and `format` names the archive they were measured over:
//! the shipped full-decode catalog archive, which is the only one this
//! workspace has an encoder for. They are defended here exactly as the
//! durations are — that the harness reported them, and that they relate to one
//! another correctly — never that they came in under a target.

use std::collections::HashSet;

use crikey_benchmarks::{
    run_catalog_benchmark, synthetic_catalog, BenchmarkConfig, BenchmarkReport, ARCHIVE_FORMAT,
    STRESS_CATALOG_SIZE,
};
use crikey_core::ItemId;
use crikey_query::{DefaultMatcher, DefaultNormalizer, Matcher, Normalizer};

/// Small enough for the ordinary test suite, large enough that the harness has
/// to build, persist, reload and query a real catalog rather than a toy one.
fn small_config() -> BenchmarkConfig {
    BenchmarkConfig {
        items: 2_048,
        queries: 16,
        top_k: 20,
    }
}

/// The quantities that must reproduce exactly. Elapsed nanoseconds and resident
/// bytes are excluded on purpose: they are the parts of the report allowed to
/// differ between two identical runs. The archive size is not one of them —
/// the same catalog encodes to the same bytes — and neither is the candidate
/// count: the catalog, the queries and the index built over them carry no seed,
fn counts(report: &BenchmarkReport) -> (usize, usize, usize, u64, usize, u64, &'static str) {
    (
        report.items,
        report.unique_items,
        report.matched_total,
        report.candidates_examined,
        report.prefix_samples,
        report.archive_bytes,
        report.format,
    )
}

#[test]
fn a_small_run_reports_exact_counts_for_the_configured_catalog() {
    let config = small_config();
    let report = run_catalog_benchmark(&config);

    let distinct: HashSet<ItemId> = synthetic_catalog(config.items)
        .into_iter()
        .map(|item| item.stable_id)
        .collect();

    assert_eq!(
        report.items, config.items,
        "the report must describe the configured catalog: {report:?}"
    );
    assert_eq!(
        report.unique_items,
        distinct.len(),
        "unique ids must agree with the source catalog: {report:?}"
    );
    assert_eq!(
        report.unique_items, config.items,
        "the synthetic catalog collides on no stable id: {report:?}"
    );
}

#[test]
fn the_synthetic_catalog_is_deterministic_and_heterogeneous() {
    let first = synthetic_catalog(2_048);
    let second = synthetic_catalog(2_048);
    assert_eq!(
        first
            .iter()
            .map(|item| (&item.stable_id, &item.label))
            .collect::<Vec<_>>(),
        second
            .iter()
            .map(|item| (&item.stable_id, &item.label))
            .collect::<Vec<_>>(),
        "the synthetic workload must carry no seed or clock"
    );

    let normalizer = DefaultNormalizer::default();
    let matcher = DefaultMatcher::default();
    let query = normalizer.normalize("aurora");
    let matching = first
        .iter()
        .filter(|item| matcher.match_item(&query, item).is_some())
        .count();
    assert!(
        matching > 0 && matching < first.len(),
        "a realistic alphabetic query must select a graded fraction, got {matching} of {}",
        first.len()
    );
}

#[test]
fn the_reported_query_percentiles_are_ordered() {
    let report = run_catalog_benchmark(&small_config());

    // An invariant of percentile selection over one sample set, not a latency
    // claim: p95 selects at or above the position p50 selects.
    assert!(
        report.query_nanos_p95 >= report.query_nanos_p50,
        "p95 must not fall below p50: {report:?}"
    );
}

#[test]
fn the_query_report_covers_the_keystroke_prefix_sequence() {
    let config = small_config();
    let report = run_catalog_benchmark(&config);

    assert!(
        report.prefix_samples > config.queries,
        "each configured query must contribute more than its fully typed sample"
    );
    assert_eq!(
        report
            .prefix_latencies
            .iter()
            .map(|latency| latency.samples)
            .sum::<usize>(),
        report.prefix_samples,
        "every measured keystroke must belong to one prefix-length bucket"
    );
    assert!(
        report
            .prefix_latencies
            .windows(2)
            .all(|pair| pair[0].prefix_chars < pair[1].prefix_chars),
        "prefix buckets must be published in increasing length order: {:?}",
        report.prefix_latencies
    );
    assert!(
        report
            .prefix_latencies
            .iter()
            .all(|latency| latency.samples > 0 && latency.nanos_p95 >= latency.nanos_p50),
        "every prefix bucket must contain ordered measured samples: {:?}",
        report.prefix_latencies
    );

    let short = &report.prefix_latencies[0];
    let longer = &report.prefix_latencies[1];
    assert_eq!(
        short.samples, longer.samples,
        "every generated query has at least two characters"
    );
    assert!(
        short.candidates_examined >= longer.candidates_examined,
        "adding a character cannot enlarge the presence-mask candidate set"
    );
}

#[test]
fn two_identical_runs_agree_on_every_counted_quantity() {
    let config = small_config();

    let first = run_catalog_benchmark(&config);
    let second = run_catalog_benchmark(&config);

    assert_eq!(
        counts(&first),
        counts(&second),
        "the workload is synthetic and seedless, so counts must reproduce: \
         {first:?} vs {second:?}"
    );
}

#[test]
fn the_persisted_round_trip_returns_a_searchable_catalog() {
    let config = small_config();
    let report = run_catalog_benchmark(&config);

    // The item counts are taken from the catalog that came back *out* of the
    // cache, so a lossy store or a discarded slice shows up as a shortfall.
    assert_eq!(
        report.unique_items, config.items,
        "the reloaded catalog must hold every persisted item: {report:?}"
    );

    // Counting items proves the bytes survived; matching proves the searchable
    // payload did. Zero matches would mean the harness timed an empty index.
    assert!(
        report.matched_total > 0,
        "the benchmark queries must match the reloaded catalog: {report:?}"
    );
    assert!(
        report.matched_total <= report.prefix_samples * config.top_k,
        "no measured prefix may retain more than top_k results: {report:?}"
    );
}

#[test]
fn top_k_bounds_the_results_retained_by_each_query() {
    let wide = small_config();
    let narrow = BenchmarkConfig { top_k: 1, ..wide };

    let wide_report = run_catalog_benchmark(&wide);
    let narrow_report = run_catalog_benchmark(&narrow);

    assert_eq!(
        narrow_report.items, wide_report.items,
        "top_k changes what is retained, never what is indexed"
    );
    assert_eq!(
        narrow_report.unique_items, wide_report.unique_items,
        "top_k changes what is retained, never what is indexed"
    );
    assert!(
        narrow_report.matched_total <= narrow_report.prefix_samples,
        "with top_k = 1 each measured prefix may retain at most one result: {narrow_report:?}"
    );
    assert!(
        narrow_report.matched_total <= wide_report.matched_total,
        "narrowing top_k cannot retain more results: {narrow_report:?} vs {wide_report:?}"
    );
}

#[test]
fn every_report_names_the_archive_it_measured() {
    let wide = small_config();
    let narrow = BenchmarkConfig { items: 64, ..wide };

    // A saved measurement that does not say what it measured cannot be compared
    // against anything later, and ADR-0008 is a comparison. The label is the
    // crate's exported name for the shipped archive rather than a per-run
    // string, because the harness has exactly one format to measure.
    for report in [run_catalog_benchmark(&wide), run_catalog_benchmark(&narrow)] {
        assert!(
            !report.format.is_empty(),
            "the archive label must name something: {report:?}"
        );
        assert_eq!(
            report.format, ARCHIVE_FORMAT,
            "the report must name the archive it measured: {report:?}"
        );
    }
}

#[test]
fn the_persisted_archive_is_sized_and_the_size_tracks_the_catalog() {
    let base = small_config();
    let small = BenchmarkConfig { items: 256, ..base };
    let large = BenchmarkConfig { items: 2_048, ..base };

    let small_report = run_catalog_benchmark(&small);
    let large_report = run_catalog_benchmark(&large);

    // Not a size budget: ADR-0008 needs the number, and all this checks is that
    // the harness produced one and that it describes the catalog it persisted.
    assert!(
        small_report.archive_bytes > 0,
        "a persisted catalog occupies bytes on disk: {small_report:?}"
    );
    assert!(
        large_report.archive_bytes > small_report.archive_bytes,
        "eight times the items cannot fit in the same archive: \
         {large_report:?} vs {small_report:?}"
    );
}

#[test]
fn the_peak_resident_samples_never_fall_between_the_two_sampling_points() {
    let report = run_catalog_benchmark(&small_config());

    // A high-water mark is monotonic, so the later sample can only be at or
    // above the earlier one. That invariant and nothing else: no floor here, no
    // ceiling and no budget, and a platform that reports no peak answers zero
    // to both, which satisfies it too.
    assert!(
        report.peak_rss_bytes_after_query >= report.peak_rss_bytes_after_load,
        "the peak resident mark must not fall: {report:?}"
    );
}

#[test]
fn a_kernel_that_publishes_the_peak_mark_yields_a_measurement() {
    // Conditioned on the source the harness reads actually being there. A host
    // without `/proc`, or a kernel that does not track the mark, legitimately
    // reports no peak, and that must not read as a fault in the harness.
    let published =
        std::fs::read_to_string("/proc/self/status").is_ok_and(|status| status.contains("VmHWM:"));
    if !published {
        return;
    }

    let report = run_catalog_benchmark(&small_config());

    // Recorded-ness, not size. A zero where the kernel does publish the mark
    // means the harness never sampled it, not that the process used no memory.
    for (sample, bytes) in [
        ("after load", report.peak_rss_bytes_after_load),
        ("after the query phase", report.peak_rss_bytes_after_query),
    ] {
        assert!(
            bytes > 0,
            "the peak resident mark {sample} was published but not recorded: {report:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Candidate pruning (spec 11.1)
// ---------------------------------------------------------------------------

/// Every (item, prefix) pair a harness with no candidate index would score.
///
/// The item count and prefix count come from the report because the query
/// phase runs against the catalog that came back out of the cache and measures
/// every keystroke it actually submitted.
fn unpruned_pairs(report: &BenchmarkReport) -> u64 {
    let items = u64::try_from(report.items).expect("an item count fits in u64");
    let samples = u64::try_from(report.prefix_samples).expect("a prefix-sample count fits in u64");
    items.saturating_mul(samples)
}

#[test]
fn the_query_phase_counts_the_candidates_it_examined() {
    let report = run_catalog_benchmark(&small_config());

    // Recorded-ness, as everywhere else in this file: no ceiling here beyond
    // arithmetic, and no comparison against a budget. A zero would mean the
    // harness searched a catalog it never counted, which is exactly what makes
    // pruning invisible in a report that asserts nothing about speed.
    assert!(
        report.candidates_examined > 0,
        "the query phase searched a catalog but counted no candidates: {report:?}"
    );

    // Every result the phase retained had to pass through bounded selection,
    // so it had to be a candidate. A count below the retained set is
    // arithmetically impossible and means the harness counts the wrong work.
    let retained = u64::try_from(report.matched_total).expect("a result count fits in u64");
    assert!(
        report.candidates_examined >= retained,
        "fewer candidates were counted than results were retained: {report:?}"
    );
}

#[test]
fn the_candidates_examined_never_exceed_a_full_scan() {
    let config = small_config();
    let report = run_catalog_benchmark(&config);

    assert_eq!(
        report.items, config.items,
        "the full-scan ceiling describes the reloaded catalog, so the round \
         trip has to have returned one: {report:?}"
    );

    // Scoring every item for every measured prefix is the ceiling a candidate
    // index lowers. Exceeding it means one item was handed to the matcher more
    // than once for the same query state.
    assert!(
        report.candidates_examined <= unpruned_pairs(&report),
        "no run may score more candidates than every item for every prefix: \
         {report:?}"
    );
}

#[test]
fn two_identical_runs_agree_on_the_candidates_examined() {
    let config = small_config();

    let first = run_catalog_benchmark(&config);
    let second = run_catalog_benchmark(&config);

    // The catalog is synthetic, each query derives from its index alone, and
    // the candidate index is built from those two. Nothing in that path carries
    // a seed or a clock, so a count that wobbles is a count taken from
    // something other than the workload.
    assert_eq!(
        first.candidates_examined, second.candidates_examined,
        "the candidate count must reproduce between identical runs: \
         {first:?} vs {second:?}"
    );
}

#[test]
fn pruning_skips_catalog_work_across_the_keystroke_sequence() {
    // Retaining as many results as the catalog holds uncaps every prefix, which
    // turns `matched_total` into the exact number of accepted (item, prefix)
    // pairs.
    let base = small_config();
    let config = BenchmarkConfig {
        top_k: base.items,
        ..base
    };
    let report = run_catalog_benchmark(&config);

    assert_eq!(
        report.items, config.items,
        "the full-scan ceiling describes the reloaded catalog, so the round \
         trip has to have returned one: {report:?}"
    );

    let ceiling = unpruned_pairs(&report);
    let matching = u64::try_from(report.matched_total).expect("a result count fits in u64");

    assert!(
        report.candidates_examined >= matching,
        "the prefilter dropped pairs the matcher accepts: {report:?}"
    );
    assert!(
        report.candidates_examined < ceiling,
        "the query phase scored every item for every prefix, so nothing was \
         pruned: {report:?}"
    );
}

#[test]
#[ignore = "500,000-item stress scale (spec 25.1); run with `cargo test -p crikey-benchmarks -- --ignored`"]
fn the_stress_scale_catalog_round_trips_every_item() {
    let config = BenchmarkConfig {
        items: STRESS_CATALOG_SIZE,
        queries: 64,
        top_k: 20,
    };

    let report = run_catalog_benchmark(&config);

    assert_eq!(
        report.items, STRESS_CATALOG_SIZE,
        "spec 25.1 requires at least 500,000 indexed items: {report:?}"
    );
    assert_eq!(
        report.unique_items, STRESS_CATALOG_SIZE,
        "every stress-scale item must survive the persisted round trip: {report:?}"
    );
    assert!(
        report.matched_total > 0,
        "the reloaded stress catalog must still be searchable: {report:?}"
    );

    // The figures ADR-0008 actually turns on, recorded at the scale that makes
    // them worth anything and asserted only for having been recorded at all.
    assert!(
        report.archive_bytes > 0,
        "the stress-scale run must size the archive it persisted: {report:?}"
    );
    assert!(
        report.peak_rss_bytes_after_query >= report.peak_rss_bytes_after_load,
        "the peak resident mark must not fall: {report:?}"
    );

    // No latency or memory assertion, at any scale. The 25.1 budgets and the
    // ADR-0008 memory target are measured on the documented reference machine
    // and tracked over time; asserting them here would only report on whatever
    // else the host happened to be running.
}
