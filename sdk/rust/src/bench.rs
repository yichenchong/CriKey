//! Small benchmark helper for plugin authors (spec 24.3).

use std::time::Instant;

use crate::{harness::TestHarness, SdkError};

/// Summary of repeated in-process query measurements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BenchReport {
    pub iterations: usize,
    pub p50_us: u64,
    pub p95_us: u64,
    pub items: usize,
}

/// Drives every query once per iteration and reports aggregate latency
/// percentiles and item count.  No wall-clock sleeps are introduced.
pub fn measure(
    harness: &mut TestHarness,
    queries: &[&str],
    iterations: usize,
) -> Result<BenchReport, SdkError> {
    if iterations == 0 {
        return Ok(BenchReport {
            iterations: 0,
            p50_us: 0,
            p95_us: 0,
            items: 0,
        });
    }
    let mut durations = Vec::with_capacity(iterations);
    let mut items = 0usize;
    for _ in 0..iterations {
        let started = Instant::now();
        for query in queries {
            items = items.saturating_add(harness.suggest(query)?.items.len());
        }
        durations.push(started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
    }
    durations.sort_unstable();
    let p50_us = percentile(&durations, 50);
    let p95_us = percentile(&durations, 95);
    Ok(BenchReport {
        iterations,
        p50_us,
        p95_us,
        items,
    })
}

fn percentile(values: &[u64], percentile: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let rank = (values.len() * percentile).saturating_add(99) / 100;
    values[rank.saturating_sub(1).min(values.len() - 1)]
}
