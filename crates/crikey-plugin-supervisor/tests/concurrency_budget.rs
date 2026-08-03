//! Enforcement of per-plugin concurrency budgets at the supervisor seam
//! (spec 13.5: "the supervisor shall enforce configured limits").
//!
//! The manifest half of the contract — what `[concurrency]` may declare — is
//! pinned in `crikey-plugin-model/tests/concurrency_manifest.rs`. Here the
//! declaration has to *bind*: the component that decides whether a plugin may
//! begin another unit of work must refuse once the declared budget is
//! exhausted, and must say so.
//!
//! The lifecycle seam in the current supervisor is
//! `MemorySupervisor::mark_busy(&mut self, plugin: &PluginId) -> Result<()>`:
//! it is the only call that moves a registered plugin from `Ready` into
//! `Busy`, i.e. the point at which work begins. That signature is binary — one
//! unit of work per plugin, no slot count — so it cannot itself express the
//! four budgets of spec 13.5. `ConcurrencyBudget` is the separate admission
//! gate that supplies those counts, and the seam tests below drive it *before*
//! the real `MemorySupervisor` so a refusal is shown to keep the plugin out of
//! `Busy` rather than being an isolated counter.
//!
//! Four properties drive the design:
//!
//! * A refused unit of work must be observable. `refusals(kind)` is the
//!   diagnostic; a supervisor that drops work silently leaves an operator with
//!   a plugin that mysteriously answers nothing.
//! * The four kinds are independent. Exhausting suggestions must not stall
//!   background, catalog or action work — an implementation that wires only
//!   the suggestion budget must fail here.
//! * Release is by ownership. A guard frees exactly one slot on drop, so a
//!   panicking unit of work cannot leak capacity.
//! * Declaration and enforcement are separate layers. `None` in the manifest
//!   means "the author declared nothing", and at the enforcement layer that
//!   resolves to a conservative host default (never unlimited), matching the
//!   scheduler's existing `.max(1)` normalisation of an absent request budget.

use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Barrier,
    },
    thread,
    time::Duration,
};

use crikey_core::PluginId;
use crikey_plugin_model::{ConcurrencySection, Manifest};
use crikey_plugin_supervisor::{
    BudgetGuard, BudgetKind, CircuitBreakerConfig, ConcurrencyBudget, MemorySupervisor, Supervisor,
    WorkerState, DEFAULT_ACTION_BUDGET, DEFAULT_BACKGROUND_BUDGET, DEFAULT_CATALOG_BUDGET,
    DEFAULT_SUGGESTION_BUDGET,
};

const ALL_KINDS: [BudgetKind; 4] = [
    BudgetKind::Suggestion,
    BudgetKind::Action,
    BudgetKind::Background,
    BudgetKind::Catalog,
];

/// A budget declaring the same limit for every kind, so a test that exhausts
/// one kind can prove the other three are untouched.
fn uniform_budget(limit: u32) -> ConcurrencyBudget {
    ConcurrencyBudget::from_section(&ConcurrencySection {
        max_suggestion_requests: Some(limit),
        max_action_requests: Some(limit),
        max_background_tasks: Some(limit),
        max_catalog_tasks: Some(limit),
    })
}

fn acquire_n(budget: &ConcurrencyBudget, kind: BudgetKind, n: u32) -> Vec<BudgetGuard<'_>> {
    (0..n)
        .map(|i| {
            budget
                .try_acquire(kind)
                .unwrap_or_else(|| panic!("acquisition {i} of {n} for {kind:?} is within budget"))
        })
        .collect()
}

fn plugin(name: &str) -> PluginId {
    PluginId(name.to_owned())
}

fn ready_supervisor(id: &PluginId) -> MemorySupervisor {
    let mut supervisor = MemorySupervisor::new(CircuitBreakerConfig {
        failure_threshold: 3,
        cooldown: Duration::from_secs(30),
    });
    supervisor.register(id).expect("a new plugin registers");
    supervisor.start(id).expect("a registered plugin starts");
    supervisor
        .mark_ready(id)
        .expect("a starting plugin becomes ready");
    supervisor
}

// ---------------------------------------------------------------------------
// The budget mechanism
// ---------------------------------------------------------------------------

/// The declared limit is exactly the number of units admitted: the last
/// in-budget acquisition succeeds and the first over-budget one is refused. An
/// off-by-one in either direction — refusing at the limit, or admitting one
/// past it — turns this red.
#[test]
fn acquisition_succeeds_up_to_the_declared_limit_and_is_refused_beyond_it() {
    let budget = uniform_budget(3);

    let held = acquire_n(&budget, BudgetKind::Suggestion, 3);
    assert_eq!(budget.in_flight(BudgetKind::Suggestion), 3);

    assert!(
        budget.try_acquire(BudgetKind::Suggestion).is_none(),
        "a fourth suggestion must be refused against a budget of three"
    );
    assert_eq!(
        budget.in_flight(BudgetKind::Suggestion),
        3,
        "a refusal must not consume a slot"
    );
    drop(held);
}

/// A refusal that is not counted is a diagnostic hole: the operator sees a
/// plugin that answers nothing and no evidence why. The counter must move on
/// every refusal and stay still on every success.
#[test]
fn every_refusal_increments_the_observable_refusal_counter() {
    let budget = uniform_budget(1);
    assert_eq!(budget.refusals(BudgetKind::Action), 0);

    let held = budget
        .try_acquire(BudgetKind::Action)
        .expect("first is in budget");
    assert_eq!(
        budget.refusals(BudgetKind::Action),
        0,
        "a successful acquisition is not a refusal"
    );

    for expected in 1..=3 {
        assert!(budget.try_acquire(BudgetKind::Action).is_none());
        assert_eq!(
            budget.refusals(BudgetKind::Action),
            expected,
            "refusal {expected} must be recorded"
        );
    }

    drop(held);
    assert_eq!(
        budget.refusals(BudgetKind::Action),
        3,
        "releasing a slot must not erase the refusal history"
    );
}

/// Refusals are attributed per kind. A single shared counter would blame
/// catalog builds for suggestion pressure and send an operator after the wrong
/// subsystem.
#[test]
fn refusal_counters_are_tracked_per_kind() {
    let budget = uniform_budget(0);

    for _ in 0..2 {
        assert!(budget.try_acquire(BudgetKind::Background).is_none());
    }
    assert!(budget.try_acquire(BudgetKind::Catalog).is_none());

    assert_eq!(budget.refusals(BudgetKind::Background), 2);
    assert_eq!(budget.refusals(BudgetKind::Catalog), 1);
    assert_eq!(budget.refusals(BudgetKind::Suggestion), 0);
    assert_eq!(budget.refusals(BudgetKind::Action), 0);
}

/// Capacity is returned by ownership: dropping one guard frees one slot, not
/// all of them and not none. A budget that released everything would uncap the
/// plugin; one that released nothing would wedge it after its first burst.
#[test]
fn dropping_one_guard_frees_exactly_one_slot() {
    let budget = uniform_budget(2);
    let first = budget.try_acquire(BudgetKind::Catalog).expect("in budget");
    let second = budget.try_acquire(BudgetKind::Catalog).expect("in budget");
    assert!(budget.try_acquire(BudgetKind::Catalog).is_none());

    drop(first);
    assert_eq!(budget.in_flight(BudgetKind::Catalog), 1);

    let replacement = budget
        .try_acquire(BudgetKind::Catalog)
        .expect("the freed slot is reusable");
    assert_eq!(budget.in_flight(BudgetKind::Catalog), 2);
    assert!(
        budget.try_acquire(BudgetKind::Catalog).is_none(),
        "only one slot was freed"
    );

    drop((second, replacement));
    assert_eq!(budget.in_flight(BudgetKind::Catalog), 0);
}

/// A guard released by unwinding must return its slot too; otherwise a single
/// panicking suggestion permanently shrinks the plugin's capacity.
#[test]
fn a_guard_dropped_while_unwinding_returns_its_slot() {
    let budget = uniform_budget(1);

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = budget.try_acquire(BudgetKind::Suggestion).expect("in budget");
        panic!("unit of work failed");
    }));

    assert!(outcome.is_err(), "the work panicked");
    assert_eq!(budget.in_flight(BudgetKind::Suggestion), 0);
    assert!(
        budget.try_acquire(BudgetKind::Suggestion).is_some(),
        "capacity must survive a panicking unit of work"
    );
}

/// `in_flight` is the live occupancy of one kind and must follow acquire and
/// release step by step, independently for each kind.
#[test]
fn in_flight_tracks_acquisition_and_release_for_each_kind() {
    let budget = uniform_budget(2);

    for kind in ALL_KINDS {
        assert_eq!(budget.in_flight(kind), 0, "{kind:?} starts idle");
        let first = budget.try_acquire(kind).expect("in budget");
        assert_eq!(budget.in_flight(kind), 1);
        let second = budget.try_acquire(kind).expect("in budget");
        assert_eq!(budget.in_flight(kind), 2);
        drop(second);
        assert_eq!(budget.in_flight(kind), 1);
        drop(first);
        assert_eq!(budget.in_flight(kind), 0);
    }
}

/// Spec 13.5 names four budgets. Exhausting any one of them must leave the
/// other three at full capacity — an implementation that wires only the
/// suggestion budget, or that shares one counter across kinds, fails for every
/// kind here.
#[test]
fn exhausting_one_kind_leaves_the_other_three_fully_available() {
    for exhausted in ALL_KINDS {
        let budget = uniform_budget(1);
        let held = budget.try_acquire(exhausted).expect("first is in budget");
        assert!(
            budget.try_acquire(exhausted).is_none(),
            "{exhausted:?} is exhausted"
        );

        for other in ALL_KINDS.into_iter().filter(|kind| *kind != exhausted) {
            let guard = budget
                .try_acquire(other)
                .unwrap_or_else(|| panic!("{other:?} must be unaffected by {exhausted:?} pressure"));
            assert_eq!(budget.in_flight(other), 1);
            assert_eq!(
                budget.refusals(other),
                0,
                "{other:?} recorded a refusal it never suffered"
            );
            drop(guard);
        }

        drop(held);
    }
}

/// An undeclared budget is NOT unlimited: it resolves to the conservative host
/// default, matching the scheduler's existing `.max(1)` normalisation of an
/// absent `max-concurrent-requests` (spec 8.12 fairness). Treating absence as
/// unbounded would silently uncap every manifest that never mentions
/// concurrency, so the second concurrent unit of an undeclared kind is refused.
#[test]
fn an_absent_budget_resolves_to_the_conservative_host_default() {
    let budget = ConcurrencyBudget::from_section(&ConcurrencySection::default());

    let defaults = [
        (BudgetKind::Suggestion, DEFAULT_SUGGESTION_BUDGET),
        (BudgetKind::Action, DEFAULT_ACTION_BUDGET),
        (BudgetKind::Background, DEFAULT_BACKGROUND_BUDGET),
        (BudgetKind::Catalog, DEFAULT_CATALOG_BUDGET),
    ];

    for (kind, default) in defaults {
        assert_eq!(
            budget.limit(kind),
            default,
            "{kind:?} must fall back to its documented default"
        );

        let held = acquire_n(&budget, kind, default);
        assert!(
            budget.try_acquire(kind).is_none(),
            "{kind:?} must refuse the unit past its defaulted limit"
        );
        assert_eq!(budget.refusals(kind), 1);
        drop(held);
        assert_eq!(budget.in_flight(kind), 0);
    }
}

/// The default is a fallback, not a ceiling. An explicit declaration wins in
/// both directions — above the default and at it — or a plugin that asked for
/// eight concurrent background tasks would silently run one at a time.
#[test]
fn an_explicit_declaration_overrides_the_host_default() {
    let budget = ConcurrencyBudget::from_section(&ConcurrencySection {
        max_background_tasks: Some(DEFAULT_BACKGROUND_BUDGET + 7),
        ..ConcurrencySection::default()
    });

    let raised = DEFAULT_BACKGROUND_BUDGET + 7;
    assert_eq!(budget.limit(BudgetKind::Background), raised);

    let held = acquire_n(&budget, BudgetKind::Background, raised);
    assert_eq!(budget.in_flight(BudgetKind::Background), raised);
    assert!(
        budget.try_acquire(BudgetKind::Background).is_none(),
        "the raised limit is still a limit"
    );
    assert_eq!(
        budget.limit(BudgetKind::Catalog),
        DEFAULT_CATALOG_BUDGET,
        "raising one kind must not disturb the others' defaults"
    );
    drop(held);
}

/// The two layers must stay distinguishable. An undeclared budget and one
/// declared at exactly the default value enforce identically, but the manifest
/// still records which of the two the author wrote — so a future change of the
/// host default cannot be mistaken for the author's intent, and "undeclared"
/// can never collapse into "declared 1" (nor into unlimited).
#[test]
fn an_undeclared_budget_and_one_declared_at_the_default_differ_only_at_the_declaration_layer() {
    let undeclared = ConcurrencySection::default();
    let declared = ConcurrencySection {
        max_suggestion_requests: Some(DEFAULT_SUGGESTION_BUDGET),
        ..ConcurrencySection::default()
    };

    assert_eq!(undeclared.max_suggestion_requests, None);
    assert_eq!(
        declared.max_suggestion_requests,
        Some(DEFAULT_SUGGESTION_BUDGET),
        "the declaration layer keeps the author's words"
    );
    assert_ne!(
        undeclared, declared,
        "silence and an explicit default are different declarations"
    );

    let undeclared = ConcurrencyBudget::from_section(&undeclared);
    let declared = ConcurrencyBudget::from_section(&declared);
    assert_eq!(
        undeclared.limit(BudgetKind::Suggestion),
        declared.limit(BudgetKind::Suggestion),
        "both enforce the same effective limit"
    );

    let held = acquire_n(&undeclared, BudgetKind::Suggestion, DEFAULT_SUGGESTION_BUDGET);
    assert!(undeclared.try_acquire(BudgetKind::Suggestion).is_none());
    drop(held);
}

/// An explicit `0` disables that kind of work outright, and must do so from the
/// very first request. Absence defaults to a permitted unit, so folding
/// `Some(0)` into "unset" would keep running a surface the author deliberately
/// switched off — the exact confusion the manifest keeps apart.
#[test]
fn a_zero_budget_refuses_the_first_acquisition() {
    let budget = ConcurrencyBudget::from_section(&ConcurrencySection {
        max_suggestion_requests: Some(0),
        max_action_requests: None,
        max_background_tasks: None,
        max_catalog_tasks: None,
    });

    assert!(
        budget.try_acquire(BudgetKind::Suggestion).is_none(),
        "a zero budget admits nothing"
    );
    assert_eq!(budget.in_flight(BudgetKind::Suggestion), 0);
    assert_eq!(budget.refusals(BudgetKind::Suggestion), 1);
    assert_eq!(
        budget.limit(BudgetKind::Suggestion),
        0,
        "an explicit zero is an effective limit of zero, not the host default"
    );

    let other = budget
        .try_acquire(BudgetKind::Action)
        .expect("an undeclared action budget still admits its defaulted unit");
    drop(other);
}

/// The budget is the shared admission gate for a plugin's worker pool, so the
/// limit has to hold under genuine contention rather than only in sequence.
/// The test has two phases because they discriminate against different bugs.
///
/// **Phase 1 — aligned peak.** Contenders are held at a barrier placed
/// immediately before `try_acquire`, and every winner keeps its guard until a
/// second barrier. The number of successes is therefore the true peak
/// occupancy of one budget, and it must equal the declared limit exactly.
/// Barriers placed only *after* the acquisition would order nothing at all:
/// thread creation alone can serialise every check.
///
/// **Phase 2 — hammer.** Alignment can only ever be approximate; a host with
/// fewer cores than contenders physically cannot run them all at once, so
/// phase 1 alone catches a check-then-increment race only intermittently
/// (measured: 2 runs in 6 against a naive load/check/store). The hammer
/// instead runs a quarter of a million uncontended-by-limit acquisitions per
/// contender and asserts the occupancy counter is *exact*: equal acquisitions
/// and releases must leave it at zero. Every overlap in the whole run can
/// leave a permanent residue, so detection accumulates instead of depending
/// on one race landing inside the window an assertion happens to be watching
/// (measured: 10 runs in 10).
///
/// No sleeps are involved: the barriers order phase 1, and phase 2 is bounded
/// by its iteration count.
#[test]
fn concurrent_contenders_never_exceed_the_limit() {
    const LIMIT: u32 = 3;
    const CONTENDERS: u32 = 16;
    const ROUNDS: usize = 100;

    // One fresh budget per round. A budget has no reset — reusing one would
    // measure release ordering instead of admission.
    let budgets: Vec<Arc<ConcurrencyBudget>> = (0..ROUNDS).map(|_| Arc::new(uniform_budget(LIMIT))).collect();
    let budgets = Arc::new(budgets);
    let admitted = Arc::new(AtomicU32::new(0));
    // Contenders that have passed `start` and are spinning on the acquisition.
    // A `Barrier` only guarantees every thread has been *released*; the futex
    // wakeups themselves are staggered, and on a host with fewer cores than
    // contenders the first thread can finish its whole acquisition before the
    // last is scheduled. This gate holds every contender on-CPU until all of
    // them are past the barrier, so the acquisitions genuinely overlap.
    let runnable = Arc::new(AtomicU32::new(0));
    // Reached immediately before every contender attempts its acquisition.
    let start = Arc::new(Barrier::new(CONTENDERS as usize + 1));
    // Reached once every acquisition has been attempted and every winner is
    // still holding its guard.
    let peak = Arc::new(Barrier::new(CONTENDERS as usize + 1));
    // Reached once this thread has observed the peak; releases the guards.
    let release = Arc::new(Barrier::new(CONTENDERS as usize + 1));

    let workers: Vec<_> = (0..CONTENDERS)
        .map(|_| {
            let budgets = Arc::clone(&budgets);
            let admitted = Arc::clone(&admitted);
            let start = Arc::clone(&start);
            let peak = Arc::clone(&peak);
            let release = Arc::clone(&release);
            let runnable = Arc::clone(&runnable);
            thread::spawn(move || {
                for budget in budgets.iter() {
                    start.wait();
                    runnable.fetch_add(1, Ordering::SeqCst);
                    while runnable.load(Ordering::SeqCst) < CONTENDERS {
                        thread::yield_now();
                    }
                    let guard = budget.try_acquire(BudgetKind::Suggestion);
                    if guard.is_some() {
                        admitted.fetch_add(1, Ordering::SeqCst);
                    }
                    peak.wait();
                    release.wait();
                    // Dropped before this thread can reach the next round's
                    // `start`, so the next budget is never contended by a
                    // guard from the previous one.
                    drop(guard);
                }
            })
        })
        .collect();

    for (round, budget) in budgets.iter().enumerate() {
        start.wait();
        peak.wait();
        assert_eq!(
            admitted.load(Ordering::SeqCst),
            LIMIT,
            "round {round}: exactly the declared number of contenders may hold a slot at once"
        );
        assert_eq!(
            budget.in_flight(BudgetKind::Suggestion),
            LIMIT,
            "round {round}: occupancy must equal the number of admitted contenders"
        );
        assert_eq!(
            budget.refusals(BudgetKind::Suggestion),
            u64::from(CONTENDERS - LIMIT),
            "round {round}: every loser must be counted exactly once"
        );
        // Safe here: no contender touches the counter between `peak` and the
        // next round's `start`.
        admitted.store(0, Ordering::SeqCst);
        runnable.store(0, Ordering::SeqCst);
        release.wait();
    }

    for worker in workers {
        worker.join().expect("contender thread must not panic");
    }
    for (round, budget) in budgets.iter().enumerate() {
        assert_eq!(
            budget.in_flight(BudgetKind::Suggestion),
            0,
            "round {round}: every guard must have released its slot"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 2: hammer
    // -----------------------------------------------------------------------

    // Every contender can hold a slot at once, so nothing is ever refused and
    // every single attempt exercises the admission write. What is asserted is
    // the counter's *exactness*: after equal numbers of acquisitions and
    // releases the occupancy must be zero. A check-then-increment loses one
    // side of a concurrent pair — the increment clobbers a release, or two
    // increments collapse into one — and the residue is permanent. That is the
    // same defect that lets a bounded budget over-admit, but it accumulates
    // over every overlap in the run instead of needing one to land inside a
    // nanosecond-wide window while the assertion is looking.
    const HAMMER_LIMIT: u32 = CONTENDERS;
    const ITERATIONS: usize = 250_000;

    let budget = Arc::new(uniform_budget(HAMMER_LIMIT));
    // Occupancy accounted for entirely outside the budget, so a budget that
    // miscounts its own slots cannot hide the excess.
    let live = Arc::new(AtomicU32::new(0));
    let observed_peak = Arc::new(AtomicU32::new(0));
    let hammer_start = Arc::new(Barrier::new(CONTENDERS as usize));

    let hammers: Vec<_> = (0..CONTENDERS)
        .map(|_| {
            let budget = Arc::clone(&budget);
            let live = Arc::clone(&live);
            let observed_peak = Arc::clone(&observed_peak);
            let hammer_start = Arc::clone(&hammer_start);
            thread::spawn(move || {
                hammer_start.wait();
                for _ in 0..ITERATIONS {
                    let Some(guard) = budget.try_acquire(BudgetKind::Suggestion) else {
                        continue;
                    };
                    let held = live.fetch_add(1, Ordering::SeqCst) + 1;
                    observed_peak.fetch_max(held, Ordering::SeqCst);
                    live.fetch_sub(1, Ordering::SeqCst);
                    drop(guard);
                }
            })
        })
        .collect();
    for hammer in hammers {
        hammer.join().expect("hammering thread must not panic");
    }

    let observed_peak = observed_peak.load(Ordering::SeqCst);
    assert!(
        observed_peak <= HAMMER_LIMIT,
        "{observed_peak} contenders held a slot at once against a declared limit of {HAMMER_LIMIT}"
    );
    assert_eq!(
        budget.in_flight(BudgetKind::Suggestion),
        0,
        "occupancy must return to zero: every acquisition was released, so a \
         non-zero residue is an acquisition or a release that was lost to a race"
    );
    assert_eq!(
        budget.refusals(BudgetKind::Suggestion),
        0,
        "no attempt can legally be refused when every contender has its own slot"
    );
    if thread::available_parallelism().is_ok_and(|cores| cores.get() > 1) {
        assert!(
            observed_peak > 1,
            "the hammer never overlapped, so it proved nothing about contention"
        );
    }
}

// ---------------------------------------------------------------------------
// The supervisor admission seam
// ---------------------------------------------------------------------------

/// The declared manifest budget must be the one enforced. Building the budget
/// from a parsed `[concurrency]` section closes the gap between what the author
/// wrote and what the host counts; a wiring bug that reads the wrong key, or
/// ignores the section entirely, shows up as the wrong admission count.
#[test]
fn the_enforced_budget_comes_from_the_parsed_manifest_section() {
    let manifest = Manifest::parse(
        "manifest-version = 1\n\
         \n\
         [plugin]\n\
         id = \"dev.example.concurrency\"\n\
         name = \"Concurrency Fixture\"\n\
         version = \"1.0.0\"\n\
         runtime = \"native\"\n\
         entrypoint = \"bin/plugin\"\n\
         \n\
         [concurrency]\n\
         max-suggestion-requests = 2\n\
         max-catalog-tasks = 0\n",
    )
    .expect("manifest must parse");

    let budget = ConcurrencyBudget::from_section(&manifest.concurrency);

    let held = acquire_n(&budget, BudgetKind::Suggestion, 2);
    assert!(
        budget.try_acquire(BudgetKind::Suggestion).is_none(),
        "the declared suggestion budget of two must bind"
    );
    assert!(
        budget.try_acquire(BudgetKind::Catalog).is_none(),
        "a declared catalog budget of zero must bind"
    );
    assert_eq!(
        budget.limit(BudgetKind::Background),
        DEFAULT_BACKGROUND_BUDGET,
        "an undeclared background budget takes the host default"
    );
    assert!(
        budget.try_acquire(BudgetKind::Background).is_some(),
        "the defaulted background unit is admitted"
    );
    drop(held);
}

/// `MemorySupervisor::mark_busy` is the seam at which a plugin begins a unit of
/// work. When the budget refuses, that transition must never happen: the plugin
/// stays `Ready` and available for the work it is still allowed to do. A host
/// that admits the work anyway has a budget in name only.
#[test]
fn a_refused_budget_keeps_the_plugin_out_of_the_busy_transition() {
    let id = plugin("dev.example.concurrency");
    let mut supervisor = ready_supervisor(&id);
    let budget = ConcurrencyBudget::from_section(&ConcurrencySection {
        max_suggestion_requests: Some(1),
        ..ConcurrencySection::default()
    });

    let admitted = budget
        .try_acquire(BudgetKind::Suggestion)
        .expect("first is in budget");
    supervisor.mark_busy(&id).expect("admitted work begins");
    assert_eq!(supervisor.state(&id), WorkerState::Busy);

    // Second request against an exhausted budget: refused before the seam.
    let refused = budget.try_acquire(BudgetKind::Suggestion);
    assert!(refused.is_none(), "the second suggestion is over budget");
    assert_eq!(budget.refusals(BudgetKind::Suggestion), 1);

    drop(admitted);
    supervisor
        .record_success(&id)
        .expect("the admitted work completed");
    supervisor.mark_ready(&id).expect("the worker returns to ready");
    assert_eq!(supervisor.state(&id), WorkerState::Ready);
    assert!(
        budget.try_acquire(BudgetKind::Suggestion).is_some(),
        "the freed slot admits the next suggestion"
    );
}

/// Budgets are per plugin, not per host: one plugin saturating its suggestion
/// budget must not deny another plugin work it is entitled to. Sharing a single
/// budget across the registry would let one misbehaving plugin mute the rest.
#[test]
fn budgets_are_scoped_to_a_single_plugin() {
    let noisy = plugin("dev.example.noisy");
    let quiet = plugin("dev.example.quiet");
    let mut supervisor = ready_supervisor(&noisy);
    supervisor.register(&quiet).expect("a second plugin registers");
    supervisor.start(&quiet).expect("it starts");
    supervisor.mark_ready(&quiet).expect("it becomes ready");

    let noisy_budget = uniform_budget(1);
    let quiet_budget = uniform_budget(1);

    let held = noisy_budget
        .try_acquire(BudgetKind::Suggestion)
        .expect("first is in budget");
    assert!(noisy_budget.try_acquire(BudgetKind::Suggestion).is_none());

    let quiet_guard = quiet_budget
        .try_acquire(BudgetKind::Suggestion)
        .expect("a second plugin has its own budget");
    supervisor
        .mark_busy(&quiet)
        .expect("the quiet plugin still works");
    assert_eq!(supervisor.state(&quiet), WorkerState::Busy);
    assert_eq!(quiet_budget.refusals(BudgetKind::Suggestion), 0);

    drop((held, quiet_guard));
}
