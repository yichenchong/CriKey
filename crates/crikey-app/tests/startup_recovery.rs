//! Startup-recovery journal contract (spec 24.2; roadmap M6).
//!
//! Spec 24.2 asks for exactly two behaviours, and this file defends the first
//! half of each of them at the persistence layer:
//!
//! - CriKey shall record which plugins were active during an abnormal
//!   shutdown.
//! - On repeated startup failure, CriKey shall enter safe mode.
//!
//! The second half of the safe-mode requirement — that third-party plugins are
//! actually *not loaded* — is not provable here. A journal that computes
//! [`StartupMode::SafeMode`] while the provider still spawns every package is
//! worthless, so that guarantee is pinned against the real `NativeProvider` in
//! `safe_mode_suppression.rs`.
//!
//! # Threshold semantics
//!
//! [`StartupJournal::begin_startup`] reports the mode implied by the failures
//! *already recorded on disk*, and only then records the attempt it was called
//! for. That is the sole reading under which both spec-derived expectations
//! hold at once: `SAFE_MODE_AFTER_FAILURES - 1` recorded failures must still
//! start normally, and `SAFE_MODE_AFTER_FAILURES` recorded failures must enter
//! safe mode. A "failure" is a startup that called `begin_startup` and never
//! reached [`StartupJournal::mark_ready`].
//!
//! # Determinism
//!
//! Nothing here sleeps, spawns, or reads the clock. Every journal is written
//! into a uniquely named scratch directory removed when the test ends, and
//! every "reloaded" assertion constructs a brand new [`StartupJournal::load`]
//! from the same path — never the in-memory value that was just mutated, which
//! would pass even if `save` wrote nothing at all.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crikey_app::{admitted_plugin_roots, StartupJournal, StartupMode, SAFE_MODE_AFTER_FAILURES};
use crikey_core::PluginId;

/// A private directory removed when the test that made it ends.
#[derive(Debug)]
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);

        let path = std::env::temp_dir().join(format!(
            "crikey-startup-recovery-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // A crashed earlier run must never leak a journal into this one.
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");

        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn plugin(id: &str) -> PluginId {
    PluginId(id.to_string())
}

/// The two third-party plugins used as the recorded active set. Two distinct
/// ids, so an implementation that records only the first — or none at all —
/// cannot pass the abnormal-shutdown assertion.
fn active_set() -> Vec<PluginId> {
    vec![plugin("third.party.alpha"), plugin("third.party.beta")]
}

/// One startup that began and never became ready: the process died mid-boot.
/// Returns the mode that startup was admitted under.
fn failed_startup(path: &Path, plugins: &[PluginId]) -> StartupMode {
    let mut journal = StartupJournal::load(path);
    let mode = journal.begin_startup(plugins);
    journal.save().expect("journal is persistable");
    mode
}

/// One startup that came up and shut down cleanly.
fn clean_startup(path: &Path, plugins: &[PluginId]) -> StartupMode {
    let mut journal = StartupJournal::load(path);
    let mode = journal.begin_startup(plugins);
    journal.mark_ready();
    journal.mark_clean_shutdown();
    journal.save().expect("journal is persistable");
    mode
}

fn sorted(plugins: &[PluginId]) -> Vec<String> {
    let mut ids = plugins.iter().map(|id| id.0.clone()).collect::<Vec<_>>();
    ids.sort();
    ids
}

// ---------------------------------------------------------------------------
// Loading a journal must never be the thing that stops CriKey starting
// ---------------------------------------------------------------------------

/// A first-ever launch has no journal file. Loading one must yield a usable
/// fresh journal and admit the startup normally.
///
/// Kills the bug where `load` unwraps a missing file, turning "no journal yet"
/// into a boot crash that no recovery path can catch.
#[test]
fn a_missing_journal_file_loads_as_a_fresh_journal_and_admits_the_first_startup_normally() {
    let scratch = Scratch::new("missing");
    let path = scratch.join("startup.json");
    assert!(!path.exists(), "the test fixture must start without a journal");

    let mut journal = StartupJournal::load(&path);
    assert!(
        journal.active_during_abnormal_shutdown().is_empty(),
        "a journal that never existed cannot name plugins from a previous crash",
    );
    assert_eq!(
        journal.begin_startup(&active_set()),
        StartupMode::Normal,
        "the first launch on a clean machine must start normally",
    );
    journal.save().expect("a fresh journal is persistable");
    assert!(path.exists(), "saving must create the journal file");
}

/// A journal truncated or scribbled on by a crash must be treated as fresh.
///
/// Kills the bug where a damaged recovery file becomes permanently fatal:
/// the mechanism meant to survive crashes must not itself be a crash source,
/// and it must be repairable by the next save.
#[test]
fn a_corrupt_journal_file_loads_as_a_fresh_journal_instead_of_preventing_startup() {
    let scratch = Scratch::new("corrupt");
    let path = scratch.join("startup.json");
    fs::write(&path, b"\x00\x01not json at all {{{ \xfftruncated").expect("garbage is writable");

    let mut journal = StartupJournal::load(&path);
    assert!(
        journal.active_during_abnormal_shutdown().is_empty(),
        "an unreadable journal names no plugins rather than inventing some",
    );
    assert_eq!(
        journal.begin_startup(&active_set()),
        StartupMode::Normal,
        "a damaged journal must not put a healthy install into safe mode",
    );
    journal
        .save()
        .expect("a corrupt journal is repaired by the next save");

    let recovered = StartupJournal::load(&path);
    assert_eq!(
        sorted(recovered.active_during_abnormal_shutdown()),
        sorted(&active_set()),
        "the rewritten journal must be readable again, carrying the live run's plugin set",
    );
}

#[test]
fn a_non_regular_journal_path_is_ignored_without_opening_it() {
    let scratch = Scratch::new("non-regular");
    let path = scratch.join("startup.json");
    fs::create_dir(&path).expect("the directory fixture is creatable");

    let journal = StartupJournal::load(&path);
    assert!(
        journal.active_during_abnormal_shutdown().is_empty(),
        "a directory is not a journal and must not be treated as a prior crash"
    );
}

#[test]
fn an_unwritable_state_parent_reports_save_failure_without_stopping_load() {
    let scratch = Scratch::new("unwritable-parent");
    let blocker = scratch.join("not-a-directory");
    fs::write(&blocker, b"state directory blocker").expect("the blocker is writable");
    let path = blocker.join("startup.json");

    let mut journal = StartupJournal::load(&path);
    assert_eq!(journal.begin_startup(&active_set()), StartupMode::Normal);
    assert!(
        journal.save().is_err(),
        "an unwritable state parent must be reported to the caller"
    );
}

/// A well-formed record built to exceed a byte budget.
///
/// Deliberately *valid*: the size ceiling is only worth having if it stops the
/// journal being read in full before its contents are judged, so the file this
/// builds would parse perfectly if the reader ever got that far.
fn record_of_at_least(bytes: u64, failures: u32) -> String {
    let target = usize::try_from(bytes).unwrap_or(usize::MAX);
    let mut text = format!("{{\"consecutive_failures\":{failures},\"active\":[");
    let mut index = 0_u32;
    while text.len() < target {
        if index > 0 {
            text.push(',');
        }
        text.push_str(&format!("\"third.party.bulk.{index:08}\""));
        index += 1;
    }
    text.push_str("]}");
    text
}

/// A journal larger than the accepted ceiling is corruption, not input.
///
/// The oversized file is valid JSON recording a safe-mode-deep failure count,
/// so nothing about its *contents* makes it unreadable: only its size does.
/// The small control below is the same shape and is honoured in full, so this
/// cannot pass by rejecting every record.
///
/// Kills the bug where `load` reads an attacker-controlled file whole before
/// deciding it is corrupt — an allocation `.ok()` cannot recover from, in the
/// one code path that must never be able to stop startup.
#[test]
fn a_journal_larger_than_the_accepted_ceiling_loads_as_a_fresh_journal() {
    let scratch = Scratch::new("oversized");
    let oversized = scratch.join("oversized.json");
    let control = scratch.join("control.json");
    let deep = SAFE_MODE_AFTER_FAILURES + 1;
    fs::write(
        &oversized,
        record_of_at_least(StartupJournal::MAX_BYTES + 1, deep),
    )
    .expect("the oversized journal is writable");
    fs::write(&control, record_of_at_least(64, deep)).expect("the control journal is writable");

    let mut refused = StartupJournal::load(&oversized);
    assert!(
        refused.active_during_abnormal_shutdown().is_empty(),
        "an over-limit journal names no plugins",
    );
    assert_eq!(
        refused.begin_startup(&active_set()),
        StartupMode::Normal,
        "an over-limit journal must not decide this launch's mode",
    );

    let mut honoured = StartupJournal::load(&control);
    assert_eq!(
        honoured.begin_startup(&active_set()),
        StartupMode::SafeMode {
            consecutive_failures: deep
        },
        "the same record within the ceiling is still read and obeyed",
    );
}

/// A journal of exactly the ceiling is still read: the bound is a limit, not a
/// margin, and an off-by-one that rejects the largest legal record would
/// silently discard real recovery state.
#[test]
fn a_journal_of_exactly_the_accepted_ceiling_is_still_read() {
    let scratch = Scratch::new("at-ceiling");
    let path = scratch.join("startup.json");
    let ceiling = usize::try_from(StartupJournal::MAX_BYTES).expect("the ceiling fits in memory");
    let mut text = record_of_at_least(StartupJournal::MAX_BYTES - 1_024, SAFE_MODE_AFTER_FAILURES);
    // Pad to exactly the ceiling with whitespace, which the parser skips.
    assert!(text.len() <= ceiling, "the padded record must fit");
    while text.len() < ceiling {
        text.push(' ');
    }
    fs::write(&path, &text).expect("the journal is writable");

    assert_eq!(
        StartupJournal::load(&path).begin_startup(&active_set()),
        StartupMode::SafeMode {
            consecutive_failures: SAFE_MODE_AFTER_FAILURES
        },
        "a record of exactly the ceiling is legal input",
    );
}

// ---------------------------------------------------------------------------
// "Which plugins were active during an abnormal shutdown" (spec 24.2)
// ---------------------------------------------------------------------------

/// A startup that never reached ready leaves its plugin set on disk for the
/// next launch to read.
///
/// Asserts the exact ids of two distinct plugins, so an implementation that
/// persists an empty vector, drops all but one entry, or records the plugin
/// count without the identities fails.
#[test]
fn a_startup_that_never_became_ready_records_its_exact_plugin_set_for_the_next_launch() {
    let scratch = Scratch::new("abnormal-set");
    let path = scratch.join("startup.json");
    let expected = active_set();

    failed_startup(&path, &expected);

    let reloaded = StartupJournal::load(&path);
    let recorded = reloaded.active_during_abnormal_shutdown();
    assert_eq!(
        recorded.len(),
        2,
        "both plugins active at the crash must be named, found: {:?}",
        sorted(recorded),
    );
    assert_eq!(
        sorted(recorded),
        sorted(&expected),
        "the recorded set must be the plugins that were active, not a placeholder",
    );
}

/// A clean shutdown clears the abnormal-shutdown record.
///
/// The journal is deliberately dirtied by a crashed run first, so this cannot
/// pass by never recording anything: the assertion is that the clean cycle
/// *cleared* a set that was demonstrably present.
#[test]
fn a_clean_shutdown_clears_the_plugins_recorded_by_an_earlier_crashed_run() {
    let scratch = Scratch::new("clean");
    let path = scratch.join("startup.json");
    let plugins = active_set();

    failed_startup(&path, &plugins);
    assert!(
        !StartupJournal::load(&path)
            .active_during_abnormal_shutdown()
            .is_empty(),
        "precondition: the crashed run must have left a record to clear",
    );

    clean_startup(&path, &plugins);

    let reloaded = StartupJournal::load(&path);
    assert!(
        reloaded.active_during_abnormal_shutdown().is_empty(),
        "a clean shutdown leaves no plugin blamed for a crash, found: {:?}",
        sorted(reloaded.active_during_abnormal_shutdown()),
    );
    let mut next = StartupJournal::load(&path);
    assert_eq!(
        next.begin_startup(&plugins),
        StartupMode::Normal,
        "a launch after a clean shutdown starts normally",
    );
}

// ---------------------------------------------------------------------------
// "On repeated startup failure ... safe mode" (spec 24.2)
// ---------------------------------------------------------------------------

/// Fewer recorded failures than the threshold must still start normally.
///
/// Kills an off-by-one that trips safe mode a launch early and disables every
/// third-party plugin on an install that is merely unlucky.
#[test]
fn fewer_recorded_failures_than_the_safe_mode_threshold_still_start_normally() {
    let scratch = Scratch::new("below-threshold");
    let path = scratch.join("startup.json");
    let plugins = active_set();

    for _ in 0..(SAFE_MODE_AFTER_FAILURES - 1) {
        assert_eq!(
            failed_startup(&path, &plugins),
            StartupMode::Normal,
            "each launch below the threshold is itself admitted normally",
        );
    }

    let mut reloaded = StartupJournal::load(&path);
    assert_eq!(
        reloaded.begin_startup(&plugins),
        StartupMode::Normal,
        "with {} recorded failures and a threshold of {SAFE_MODE_AFTER_FAILURES}, startup is normal",
        SAFE_MODE_AFTER_FAILURES - 1,
    );
}

/// Reaching the threshold enters safe mode and reports the true failure count.
///
/// The count is asserted exactly: a mode that hardcodes the threshold, or that
/// keeps counting stale failures across a successful boot, disagrees with the
/// number of failures this test actually caused.
#[test]
fn reaching_the_safe_mode_threshold_enters_safe_mode_carrying_the_consecutive_failure_count() {
    let scratch = Scratch::new("threshold");
    let path = scratch.join("startup.json");
    let plugins = active_set();

    for _ in 0..SAFE_MODE_AFTER_FAILURES {
        failed_startup(&path, &plugins);
    }

    let mut reloaded = StartupJournal::load(&path);
    assert_eq!(
        reloaded.begin_startup(&plugins),
        StartupMode::SafeMode {
            consecutive_failures: SAFE_MODE_AFTER_FAILURES,
        },
        "{SAFE_MODE_AFTER_FAILURES} consecutive failed startups must enter safe mode with the real count",
    );
}

/// A successful startup resets the failure count instead of merely capping it.
///
/// Sequence: fail, fail, succeed, fail. Four launches, only one of them recent
/// and failing. An implementation that saturates or never resets sees three or
/// more failures and drops into safe mode; the reset makes this Normal.
#[test]
fn marking_ready_resets_the_consecutive_failure_count_rather_than_saturating_it() {
    let scratch = Scratch::new("reset");
    let path = scratch.join("startup.json");
    let plugins = active_set();

    failed_startup(&path, &plugins);
    failed_startup(&path, &plugins);
    clean_startup(&path, &plugins);
    failed_startup(&path, &plugins);

    let mut reloaded = StartupJournal::load(&path);
    assert_eq!(
        reloaded.begin_startup(&plugins),
        StartupMode::Normal,
        "one failure since the last ready startup is not repeated failure",
    );
}

/// Once a journal has reached ready, a later startup in the same process must
/// decide its mode from the reset failure count rather than replaying the
/// previous boot's verdict.
#[test]
fn reusing_a_ready_journal_does_not_replay_safe_mode_for_the_next_startup() {
    let scratch = Scratch::new("reuse-after-ready");
    let path = scratch.join("startup.json");
    let plugins = active_set();

    for _ in 0..SAFE_MODE_AFTER_FAILURES {
        failed_startup(&path, &plugins);
    }

    let mut journal = StartupJournal::load(&path);
    assert!(matches!(
        journal.begin_startup(&plugins),
        StartupMode::SafeMode { .. }
    ));
    journal.mark_ready();
    assert_eq!(
        journal.begin_startup(&plugins),
        StartupMode::Normal,
        "a new startup after readiness must not reuse the prior safe-mode verdict",
    );
}

/// Safe mode must persist across launches until a startup actually succeeds:
/// reloading a journal that already reached the threshold stays in safe mode,
/// and one successful boot returns it to normal.
///
/// Kills the bug where safe mode is a per-process flag that a restart clears,
/// which would let the crash loop resume immediately.
#[test]
fn safe_mode_survives_a_reload_and_is_left_only_by_a_successful_startup() {
    let scratch = Scratch::new("persisted-safe-mode");
    let path = scratch.join("startup.json");
    let plugins = active_set();

    for _ in 0..SAFE_MODE_AFTER_FAILURES {
        failed_startup(&path, &plugins);
    }

    let mut still_broken = StartupJournal::load(&path);
    assert!(
        matches!(still_broken.begin_startup(&plugins), StartupMode::SafeMode { .. }),
        "the threshold state is on disk, so the next process starts in safe mode",
    );
    still_broken.mark_ready();
    still_broken.mark_clean_shutdown();
    still_broken.save().expect("journal is persistable");

    let mut recovered = StartupJournal::load(&path);
    assert_eq!(
        recovered.begin_startup(&plugins),
        StartupMode::Normal,
        "one startup that reached ready must take the install back out of safe mode",
    );
}

// ---------------------------------------------------------------------------
// The mode decides which third-party roots are even offered to a provider
// ---------------------------------------------------------------------------

/// Safe mode drops every third-party plugin root; normal mode passes them all
/// through unchanged.
///
/// Both directions are asserted against the same non-empty input, so a
/// function that always returns the input, and one that always returns empty,
/// each fail.
#[test]
fn safe_mode_admits_no_third_party_plugin_roots_while_normal_mode_admits_every_root() {
    let roots = vec![
        PathBuf::from("/opt/crikey/plugins/alpha"),
        PathBuf::from("/opt/crikey/plugins/beta"),
    ];

    assert_eq!(
        admitted_plugin_roots(&StartupMode::Normal, &roots),
        roots,
        "a normal startup offers every configured plugin root",
    );
    assert!(
        admitted_plugin_roots(
            &StartupMode::SafeMode {
                consecutive_failures: SAFE_MODE_AFTER_FAILURES,
            },
            &roots,
        )
        .is_empty(),
        "safe mode offers no third-party plugin root at all (spec 24.2)",
    );
}

// ---------------------------------------------------------------------------
// The record has to be true at every instant a crash could happen
// ---------------------------------------------------------------------------

/// Plugins discovered after the attempt was opened are committed as they
/// become active, and doing so never charges a second attempt.
///
/// A composition root learns plugin ids one provider at a time, and any of
/// those loads can be the thing that kills the process. The record must name
/// what was active at that instant, so each refresh has to be persistable on
/// its own — and must not inflate the failure count it is interleaved with,
/// which would drive an install into safe mode one provider at a time.
#[test]
fn the_active_plugin_set_can_be_refreshed_between_providers_without_charging_an_attempt() {
    let scratch = Scratch::new("incremental");
    let path = scratch.join("startup.json");
    let builtin = plugin("builtin.crikey.applications");
    let legacy = plugin("legacy.alpha");
    let modern = plugin("modern.beta");

    let mut journal = StartupJournal::load(&path);
    journal.begin_startup(std::slice::from_ref(&builtin));
    journal.save().expect("journal is persistable");
    journal.record_active_plugins(&[builtin.clone(), legacy.clone()]);
    journal.save().expect("journal is persistable");

    let after_legacy = StartupJournal::load(&path);
    assert_eq!(
        sorted(after_legacy.active_during_abnormal_shutdown()),
        sorted(&[builtin.clone(), legacy.clone()]),
        "a crash after the legacy provider must name the legacy plugin, not only the built-in",
    );

    journal.record_active_plugins(&[builtin.clone(), legacy.clone(), modern.clone()]);
    journal.save().expect("journal is persistable");

    let mut next_launch = StartupJournal::load(&path);
    assert_eq!(
        sorted(next_launch.active_during_abnormal_shutdown()),
        sorted(&[builtin, legacy, modern]),
        "the last refresh before the crash is the set the next launch reads",
    );
    assert_eq!(
        next_launch.begin_startup(&[]),
        StartupMode::Normal,
        "three refreshes are one unfinished attempt, not three failures",
    );
}

/// A save must not depend on one fixed `<journal>.tmp` name.
///
/// That name is shared by every process and thread that ever saves this
/// journal: two concurrent saves stage through one inode, and the rename that
/// publishes it can then publish a mixture of both records — losing the
/// crash-loop count safe mode is decided from. There is no lock to serialize
/// them, so uniqueness of the staging name is the whole guarantee.
///
/// Occupying the shared name with a directory makes that concrete and
/// deterministic: a save that still insists on it cannot write, cannot rename,
/// and loses the record entirely.
#[test]
fn a_save_does_not_stage_through_a_name_shared_with_every_other_save() {
    let scratch = Scratch::new("staging-name");
    let path = scratch.join("startup.json");
    fs::create_dir_all(scratch.join("startup.json.tmp")).expect("the shared name is occupiable");

    let mut journal = StartupJournal::load(&path);
    journal.begin_startup(&active_set());
    journal
        .save()
        .expect("a staging name another save could also pick is not usable");

    assert_eq!(
        sorted(StartupJournal::load(&path).active_during_abnormal_shutdown()),
        sorted(&active_set()),
        "the record must survive a name collision on the staging file",
    );
}

/// Concurrent saves of the same journal always leave one whole record behind.
///
/// Every writer here saves a set that is distinguishable from the others, so a
/// mixture of two staged writes, a truncation, or a lost rename all show up as
/// a load that is neither of them. The last save wins; which one that is, is
/// deliberately not asserted.
#[test]
fn concurrent_saves_of_one_journal_never_publish_a_partial_record() {
    let scratch = Scratch::new("concurrent");
    let path = scratch.join("startup.json");
    // Big enough that a partial write is a plausible outcome rather than one
    // atomic syscall: a torn record is what the shared staging name risked.
    let writers: Vec<Vec<PluginId>> = (0..4)
        .map(|writer: u32| {
            (0..256)
                .map(|index| plugin(&format!("third.party.w{writer}.plugin.{index:04}")))
                .collect()
        })
        .collect();

    std::thread::scope(|scope| {
        for plugins in &writers {
            let path = path.clone();
            scope.spawn(move || {
                for _ in 0..8 {
                    let mut journal = StartupJournal::load(&path);
                    journal.record_active_plugins(plugins);
                    journal.save().expect("journal is persistable");
                }
            });
        }
    });

    let published = sorted(StartupJournal::load(&path).active_during_abnormal_shutdown());
    assert!(
        writers.iter().any(|plugins| sorted(plugins) == published),
        "the journal must hold exactly one writer's record, found {} ids",
        published.len(),
    );
}
