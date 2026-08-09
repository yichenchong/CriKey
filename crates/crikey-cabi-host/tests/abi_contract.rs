//! The restricted C-ABI contract, exercised against real `extern "C"` symbols.
//!
//! These fixtures are Rust functions with the C ABI rather than a compiled
//! `.so`, which is deliberate: it makes the *contract* — the version gate, the
//! by-name refusals, the ownership discipline, the bounds on a returned batch —
//! testable on every platform with no compiler in the loop, while still going
//! through the same `extern "C"` marshalling and the same refusal code a real
//! library goes through. `out_of_tree_cabi.rs` covers what only a genuine
//! shared library can prove: `dlopen`, and a fault taking the process down.
//!
//! Fixture state is global because a C ABI has nowhere else to put it, so every
//! test serialises on one lock for its duration.

#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crikey_cabi_host::abi::{
    self, AbiAction, AbiHost, AbiItem, AbiItems, AbiQuery, AbiStr, SYMBOL_ABI_VERSION, SYMBOL_EXECUTE,
    SYMBOL_FREE_ITEMS, SYMBOL_INIT, SYMBOL_SHUTDOWN, SYMBOL_SUGGEST,
};
use crikey_cabi_host::library::{LoadError, SymbolSource};
use crikey_cabi_host::plugin::{CabiPlugin, HostOptions, MAX_ITEMS};
use crikey_cabi_host::watchdog::{Armed, Verdict};
use crikey_core::{ActionId, Item, ItemId, PluginId, Result};
use crikey_plugin_sdk::protocol::RequestId;
use crikey_plugin_sdk::{
    CancellationToken, ExecuteRequest, LogLevel, Plugin, PluginContext, Query, SuggestionSink,
};

// -- fixture state ----------------------------------------------------------

const MODE_ECHO: usize = 0;
const MODE_FAIL_SUGGEST: usize = 1;
const MODE_CANCELLED: usize = 2;
const MODE_TOO_MANY_ROWS: usize = 3;
const MODE_NULL_ROWS: usize = 4;
const MODE_BAD_UTF8: usize = 5;
const MODE_EMPTY_ID: usize = 6;
const MODE_FAIL_INIT: usize = 7;
const MODE_FAIL_EXECUTE: usize = 8;

static MODE: AtomicUsize = AtomicUsize::new(MODE_ECHO);
static INITS: AtomicUsize = AtomicUsize::new(0);
static SUGGESTS: AtomicUsize = AtomicUsize::new(0);
static FREES: AtomicUsize = AtomicUsize::new(0);
static SHUTDOWNS: AtomicUsize = AtomicUsize::new(0);
static EXECUTES: AtomicUsize = AtomicUsize::new(0);

static ABI_VERSION_CURRENT: u32 = abi::ABI_VERSION;
static ABI_VERSION_FUTURE: u32 = abi::ABI_VERSION + 7;

const LAST_ERROR_DETAIL: &str = "fixture refused on purpose";

fn fixture(mode: usize) -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    MODE.store(mode, Ordering::SeqCst);
    for counter in [&INITS, &SUGGESTS, &FREES, &SHUTDOWNS, &EXECUTES] {
        counter.store(0, Ordering::SeqCst);
    }
    guard
}

/// What one fixture batch owns. Released only by `fx_free_items`, exactly as
/// the header requires of a real plugin.
struct FixtureBatch {
    items: Vec<AbiItem>,
    strings: Vec<Box<[u8]>>,
}

/// Copies `value` into the batch and returns a slice pointing at the copy.
/// Pushing more boxes may move the `Vec`, but never the boxed bytes.
fn owned(batch: &mut FixtureBatch, value: &[u8]) -> AbiStr {
    batch.strings.push(value.to_vec().into_boxed_slice());
    let stored = batch.strings.last().expect("just pushed");
    AbiStr {
        ptr: stored.as_ptr().cast::<std::ffi::c_char>(),
        len: stored.len(),
    }
}

unsafe extern "C" fn fx_init(host: *const AbiHost, plugin_out: *mut *mut c_void) -> i32 {
    INITS.fetch_add(1, Ordering::SeqCst);
    assert!(!host.is_null(), "the host must supply its identity");
    assert!(!plugin_out.is_null(), "the host must supply an out-parameter");
    let host = unsafe { &*host };
    assert_eq!(host.abi_version, abi::ABI_VERSION);
    assert_eq!(host.max_items, MAX_ITEMS);
    if MODE.load(Ordering::SeqCst) == MODE_FAIL_INIT {
        return abi::STATUS_ERROR;
    }
    let handle = Box::into_raw(Box::new(0xC0FFEEu64));
    unsafe { *plugin_out = handle.cast::<c_void>() };
    abi::STATUS_OK
}

unsafe extern "C" fn fx_suggest(handle: *mut c_void, query: *const AbiQuery, out: *mut AbiItems) -> i32 {
    SUGGESTS.fetch_add(1, Ordering::SeqCst);
    assert!(!handle.is_null(), "the handle init produced comes back unchanged");
    assert!(!query.is_null());
    assert!(!out.is_null());
    let query = unsafe { &*query };
    assert!(!query.cancelled.is_null(), "the cancellation flag is never null");

    let mode = MODE.load(Ordering::SeqCst);
    if mode == MODE_FAIL_SUGGEST {
        return abi::STATUS_ERROR;
    }
    if mode == MODE_CANCELLED {
        return abi::STATUS_CANCELLED;
    }

    let mut batch = Box::new(FixtureBatch {
        items: Vec::new(),
        strings: Vec::new(),
    });
    match mode {
        MODE_TOO_MANY_ROWS => {
            // A count above the ceiling is refused before the pointer is read,
            // so the fixture never allocates the rows it claims to have.
            unsafe {
                *out = AbiItems {
                    items: std::ptr::NonNull::<AbiItem>::dangling().as_ptr(),
                    count: MAX_ITEMS as usize + 1,
                    cookie: Box::into_raw(batch).cast::<c_void>(),
                    reserved: [0; 2],
                };
            }
            return abi::STATUS_OK;
        }
        MODE_NULL_ROWS => {
            unsafe {
                *out = AbiItems {
                    items: std::ptr::null_mut(),
                    count: 3,
                    cookie: Box::into_raw(batch).cast::<c_void>(),
                    reserved: [0; 2],
                };
            }
            return abi::STATUS_OK;
        }
        MODE_BAD_UTF8 => {
            let id = owned(&mut batch, &[0xff, 0xfe]);
            let label = owned(&mut batch, b"broken");
            batch.items.push(AbiItem {
                id,
                label,
                ..AbiItem::default()
            });
        }
        MODE_EMPTY_ID => {
            let label = owned(&mut batch, b"nameless");
            batch.items.push(AbiItem {
                label,
                ..AbiItem::default()
            });
        }
        _ => {
            let id = owned(&mut batch, b"cabi.fixture");
            let label = owned(&mut batch, b"Fixture row");
            let description = owned(&mut batch, b"served across the C boundary");
            let target = owned(&mut batch, query_text(query).as_bytes());
            batch.items.push(AbiItem {
                id,
                label,
                description,
                target,
                score_hint: 42,
                ..AbiItem::default()
            });
        }
    }

    let count = batch.items.len();
    let items = batch.items.as_mut_ptr();
    unsafe {
        *out = AbiItems {
            items,
            count,
            cookie: Box::into_raw(batch).cast::<c_void>(),
            reserved: [0; 2],
        };
    }
    abi::STATUS_OK
}

fn query_text(query: &AbiQuery) -> String {
    if query.text.len == 0 {
        return String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(query.text.ptr.cast::<u8>(), query.text.len) };
    String::from_utf8_lossy(bytes).into_owned()
}

unsafe extern "C" fn fx_free_items(_handle: *mut c_void, items: *mut AbiItems) {
    FREES.fetch_add(1, Ordering::SeqCst);
    assert!(!items.is_null());
    let items = unsafe { &mut *items };
    assert!(!items.cookie.is_null(), "the cookie comes back untouched");
    drop(unsafe { Box::from_raw(items.cookie.cast::<FixtureBatch>()) });
    *items = AbiItems::default();
}

unsafe extern "C" fn fx_execute(handle: *mut c_void, action: *const AbiAction) -> i32 {
    EXECUTES.fetch_add(1, Ordering::SeqCst);
    assert!(!handle.is_null());
    assert!(!action.is_null());
    if MODE.load(Ordering::SeqCst) == MODE_FAIL_EXECUTE {
        return abi::STATUS_ERROR;
    }
    abi::STATUS_OK
}

unsafe extern "C" fn fx_shutdown(handle: *mut c_void) {
    SHUTDOWNS.fetch_add(1, Ordering::SeqCst);
    assert!(!handle.is_null());
    drop(unsafe { Box::from_raw(handle.cast::<u64>()) });
}

unsafe extern "C" fn fx_last_error(_handle: *mut c_void) -> AbiStr {
    AbiStr::borrowed(LAST_ERROR_DETAIL)
}

/// Wired in wherever a passing test must never reach plugin code.
unsafe extern "C" fn fx_forbidden_init(_host: *const AbiHost, _out: *mut *mut c_void) -> i32 {
    panic!("the load gate must refuse before any plugin function runs");
}

// -- a symbol source over the fixture table ---------------------------------

#[derive(Debug)]
struct TableSource {
    origin: String,
    symbols: BTreeMap<&'static str, *mut c_void>,
}

impl SymbolSource for TableSource {
    fn origin(&self) -> &str {
        &self.origin
    }

    unsafe fn symbol(&self, name: &str) -> Option<std::ptr::NonNull<c_void>> {
        self.symbols.get(name).copied().and_then(std::ptr::NonNull::new)
    }
}

/// Every required symbol plus the optional detail accessor.
fn complete_table() -> TableSource {
    let mut symbols: BTreeMap<&'static str, *mut c_void> = BTreeMap::new();
    let version: *const u32 = &ABI_VERSION_CURRENT;
    symbols.insert(SYMBOL_ABI_VERSION, version.cast::<c_void>().cast_mut());
    symbols.insert(SYMBOL_INIT, fx_init as *mut c_void);
    symbols.insert(SYMBOL_SUGGEST, fx_suggest as *mut c_void);
    symbols.insert(SYMBOL_FREE_ITEMS, fx_free_items as *mut c_void);
    symbols.insert(SYMBOL_EXECUTE, fx_execute as *mut c_void);
    symbols.insert(SYMBOL_SHUTDOWN, fx_shutdown as *mut c_void);
    symbols.insert(abi::SYMBOL_LAST_ERROR, fx_last_error as *mut c_void);
    TableSource {
        origin: "/opt/crikey/plugins/example/bin/libexample.so".to_owned(),
        symbols,
    }
}

fn options() -> HostOptions {
    HostOptions {
        plugin_id: "dev.example.cabi".to_owned(),
        package_dir: PathBuf::from("/opt/crikey/plugins/example"),
        suggest_soft: Duration::from_millis(200),
        suggest_hard: Duration::from_millis(1_000),
        action_hard: Duration::from_millis(1_000),
    }
}

fn load(table: TableSource) -> std::result::Result<CabiPlugin, LoadError> {
    unsafe { CabiPlugin::load(Box::new(table), options()) }
}

// -- host-side test doubles -------------------------------------------------

#[derive(Debug, Default)]
struct NeverCancelled;

impl CancellationToken for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

#[derive(Debug)]
struct TestContext {
    plugin: PluginId,
    cancellation: NeverCancelled,
}

impl Default for TestContext {
    fn default() -> Self {
        Self {
            plugin: PluginId("native.dev.example.cabi".to_owned()),
            cancellation: NeverCancelled,
        }
    }
}

impl PluginContext for TestContext {
    fn plugin_id(&self) -> &PluginId {
        &self.plugin
    }

    fn cancellation(&self) -> &dyn CancellationToken {
        &self.cancellation
    }

    fn log(&self, _level: LogLevel, _message: &str) {}
}

#[derive(Debug, Default)]
struct RecordingSink {
    batches: Vec<Vec<Item>>,
    finished: usize,
}

impl SuggestionSink for RecordingSink {
    fn emit_batch(&mut self, items: Vec<Item>) -> Result<()> {
        self.batches.push(items);
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.finished += 1;
        Ok(())
    }

    fn is_cancelled(&self) -> bool {
        false
    }
}

fn query(text: &str) -> Query {
    Query {
        request: RequestId(7),
        text: text.to_owned(),
        normalized: text.to_lowercase(),
        deadline_ms: Some(400),
        generation: 3,
        selected_item_id: None,
    }
}

fn suggest(plugin: &mut CabiPlugin) -> (Result<()>, RecordingSink) {
    let context = TestContext::default();
    let mut sink = RecordingSink::default();
    let outcome = plugin.suggest(query("fixture"), &context, &mut sink);
    (outcome, sink)
}

// -- refusals ---------------------------------------------------------------

#[test]
fn an_abi_version_mismatch_is_refused_by_library_and_version_before_any_plugin_code_runs() {
    let _fixture = fixture(MODE_ECHO);
    let mut table = complete_table();
    let future: *const u32 = &ABI_VERSION_FUTURE;
    table
        .symbols
        .insert(SYMBOL_ABI_VERSION, future.cast::<c_void>().cast_mut());
    // Any call at all would be a contract violation, so `init` is replaced by a
    // function that panics if it is ever reached.
    table
        .symbols
        .insert(SYMBOL_INIT, fx_forbidden_init as *mut c_void);

    let error = load(table).expect_err("a future ABI version must be refused");
    match &error {
        LoadError::AbiVersionMismatch {
            library,
            found,
            expected,
        } => {
            assert!(
                library.ends_with("libexample.so"),
                "the refusal names the library: {library}"
            );
            assert_eq!(*found, ABI_VERSION_FUTURE);
            assert_eq!(*expected, abi::ABI_VERSION);
        }
        other => panic!("expected a version mismatch, got {other:?}"),
    }
    let message = error.to_string();
    assert!(
        message.contains("libexample.so"),
        "the message names the library: {message}"
    );
    assert!(
        message.contains(&ABI_VERSION_FUTURE.to_string()),
        "the message names the version found: {message}"
    );
    assert_eq!(INITS.load(Ordering::SeqCst), 0, "no plugin function may run");
}

#[test]
fn a_missing_version_symbol_is_refused_by_name() {
    let _fixture = fixture(MODE_ECHO);
    let mut table = complete_table();
    table.symbols.remove(SYMBOL_ABI_VERSION);
    table
        .symbols
        .insert(SYMBOL_INIT, fx_forbidden_init as *mut c_void);

    let error = load(table).expect_err("a library with no version symbol must be refused");
    assert!(
        matches!(&error, LoadError::MissingSymbol { symbol, .. } if *symbol == SYMBOL_ABI_VERSION),
        "expected a missing-symbol refusal for the version symbol, got {error:?}"
    );
    assert!(error.to_string().contains(SYMBOL_ABI_VERSION), "{error}");
    assert_eq!(INITS.load(Ordering::SeqCst), 0);
}

#[test]
fn every_missing_required_function_is_refused_by_its_own_name_before_init() {
    for absent in [
        SYMBOL_INIT,
        SYMBOL_SUGGEST,
        SYMBOL_FREE_ITEMS,
        SYMBOL_EXECUTE,
        SYMBOL_SHUTDOWN,
    ] {
        let _fixture = fixture(MODE_ECHO);
        let mut table = complete_table();
        table.symbols.remove(absent);

        let error = load(table).unwrap_err();
        match &error {
            LoadError::MissingSymbol { library, symbol } => {
                assert_eq!(*symbol, absent, "the refusal names the symbol that is missing");
                assert!(library.ends_with("libexample.so"));
            }
            other => panic!("expected a missing-symbol refusal for {absent}, got {other:?}"),
        }
        assert!(
            error.to_string().contains(absent),
            "the message names {absent}: {error}"
        );
        assert_eq!(
            INITS.load(Ordering::SeqCst),
            0,
            "{absent}: a half-exported library must not be initialised"
        );
    }
}

#[test]
fn the_optional_detail_symbol_is_not_required() {
    let _fixture = fixture(MODE_ECHO);
    let mut table = complete_table();
    table.symbols.remove(abi::SYMBOL_LAST_ERROR);

    let plugin = load(table).expect("a plugin without the optional detail accessor still loads");
    drop(plugin);
    assert_eq!(SHUTDOWNS.load(Ordering::SeqCst), 1);
}

#[test]
fn an_init_failure_is_reported_with_its_detail_and_no_shutdown_follows() {
    let _fixture = fixture(MODE_FAIL_INIT);

    let error = load(complete_table()).expect_err("a failing init must refuse the library");
    match &error {
        LoadError::Init {
            library,
            status,
            detail,
        } => {
            assert!(library.ends_with("libexample.so"));
            assert_eq!(*status, abi::STATUS_ERROR);
            assert!(
                detail.contains(LAST_ERROR_DETAIL),
                "the plugin's own detail is reported: {detail}"
            );
        }
        other => panic!("expected an init failure, got {other:?}"),
    }
    assert_eq!(INITS.load(Ordering::SeqCst), 1);
    assert_eq!(
        SHUTDOWNS.load(Ordering::SeqCst),
        0,
        "a plugin that never initialised is never shut down"
    );
}

// -- the round trip ---------------------------------------------------------

#[test]
fn a_successful_round_trip_returns_the_plugins_rows_and_frees_the_batch_once() {
    let _fixture = fixture(MODE_ECHO);
    let mut plugin = load(complete_table()).expect("the fixture library loads");

    let (outcome, sink) = suggest(&mut plugin);
    outcome.expect("the fixture answers the query");

    assert_eq!(sink.batches.len(), 1, "one batch crossed the boundary");
    assert_eq!(sink.finished, 1, "the stream was terminated exactly once");
    let items = &sink.batches[0];
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].stable_id, ItemId("cabi.fixture".to_owned()));
    assert_eq!(items[0].label, "Fixture row");
    assert_eq!(items[0].description, "served across the C boundary");
    assert_eq!(
        items[0].target, "fixture",
        "the query text reached the plugin and came back"
    );
    assert_eq!(items[0].score_hint, 42);

    assert_eq!(SUGGESTS.load(Ordering::SeqCst), 1);
    assert_eq!(
        FREES.load(Ordering::SeqCst),
        1,
        "every successful batch is returned to the plugin exactly once"
    );

    drop(plugin);
    assert_eq!(SHUTDOWNS.load(Ordering::SeqCst), 1);
}

#[test]
fn an_action_crosses_the_boundary_and_a_refusal_carries_the_plugins_detail() {
    let _fixture = fixture(MODE_ECHO);
    let mut plugin = load(complete_table()).expect("the fixture library loads");
    let context = TestContext::default();

    plugin
        .execute(
            ExecuteRequest {
                request: RequestId(11),
                item: ItemId("cabi.fixture".to_owned()),
                action: Some(ActionId("open".to_owned())),
                argument: None,
            },
            &context,
        )
        .expect("the fixture performs the action");
    assert_eq!(EXECUTES.load(Ordering::SeqCst), 1);

    MODE.store(MODE_FAIL_EXECUTE, Ordering::SeqCst);
    let error = plugin
        .execute(
            ExecuteRequest {
                request: RequestId(12),
                item: ItemId("cabi.fixture".to_owned()),
                action: None,
                argument: None,
            },
            &context,
        )
        .expect_err("a non-OK status is a failed action");
    let message = error.to_string();
    assert!(message.contains("crikey_plugin_execute"), "{message}");
    assert!(message.contains(LAST_ERROR_DETAIL), "{message}");
}

#[test]
fn a_plugin_reported_failure_names_the_entry_point_and_the_plugins_detail() {
    let _fixture = fixture(MODE_FAIL_SUGGEST);
    let mut plugin = load(complete_table()).expect("the fixture library loads");

    let (outcome, sink) = suggest(&mut plugin);
    let error = outcome.expect_err("a non-OK status is a failed request");
    let message = error.to_string();
    assert!(message.contains("libexample.so"), "{message}");
    assert!(message.contains("crikey_plugin_suggest"), "{message}");
    assert!(message.contains(LAST_ERROR_DETAIL), "{message}");
    assert!(sink.batches.is_empty(), "a failed request publishes no rows");
    assert_eq!(
        FREES.load(Ordering::SeqCst),
        0,
        "a failing suggest hands the host nothing to free"
    );
}

#[test]
fn an_observed_cancellation_terminates_the_stream_without_reporting_a_failure() {
    let _fixture = fixture(MODE_CANCELLED);
    let mut plugin = load(complete_table()).expect("the fixture library loads");

    let (outcome, sink) = suggest(&mut plugin);
    outcome.expect("a plugin that stopped when asked did not fail");
    assert!(sink.batches.is_empty());
    assert_eq!(sink.finished, 1, "the stream is still terminated");
    assert_eq!(FREES.load(Ordering::SeqCst), 0);
}

// -- hostile batches --------------------------------------------------------

/// Each of these is a plugin breaking the ABI contract. The batch is refused
/// whole, the plugin is poisoned, and — because the plugin reported success —
/// the batch is still handed back to be freed.
#[test]
fn a_batch_that_breaks_the_contract_is_refused_whole_freed_and_poisons_the_plugin() {
    for (mode, expected) in [
        (MODE_TOO_MANY_ROWS, "row ceiling"),
        (MODE_NULL_ROWS, "null item pointer"),
        (MODE_BAD_UTF8, "not valid UTF-8"),
        (MODE_EMPTY_ID, "is empty"),
    ] {
        let _fixture = fixture(mode);
        let mut plugin = load(complete_table()).expect("the fixture library loads");

        let (outcome, sink) = suggest(&mut plugin);
        let error = outcome.expect_err("a malformed batch is refused");
        assert!(
            error.to_string().contains(expected),
            "mode {mode}: the refusal names the violation ({expected}): {error}"
        );
        assert!(
            sink.batches.is_empty(),
            "mode {mode}: nothing partial is published"
        );
        assert_eq!(
            FREES.load(Ordering::SeqCst),
            1,
            "mode {mode}: the host still owes the plugin its matching free"
        );

        let (second, _) = suggest(&mut plugin);
        let second = second.expect_err("a poisoned plugin refuses every later call");
        assert!(
            second.to_string().contains("previously broke the ABI contract"),
            "mode {mode}: {second}"
        );
        assert_eq!(
            SUGGESTS.load(Ordering::SeqCst),
            1,
            "mode {mode}: the poisoned plugin was not entered again"
        );

        drop(plugin);
        assert_eq!(
            SHUTDOWNS.load(Ordering::SeqCst),
            0,
            "mode {mode}: a plugin whose state is untrustworthy is not asked to tear itself down"
        );
    }
}

// -- deadline enforcement ---------------------------------------------------

#[test]
fn the_watchdog_cancels_at_the_soft_deadline_and_aborts_at_the_hard_one() {
    let start = Instant::now();
    let armed = Armed {
        soft: start + Duration::from_millis(100),
        hard: start + Duration::from_millis(500),
        cancelled: false,
    };

    assert_eq!(
        armed.verdict(start),
        Verdict::Wait(Duration::from_millis(100)),
        "before the soft deadline the watchdog only sleeps"
    );
    assert_eq!(
        armed.verdict(start + Duration::from_millis(100)),
        Verdict::Cancel,
        "the soft deadline raises the flag"
    );

    let raised = Armed {
        cancelled: true,
        ..armed
    };
    assert_eq!(
        raised.verdict(start + Duration::from_millis(100)),
        Verdict::Wait(Duration::from_millis(400)),
        "once raised, the watchdog waits out the remaining grace"
    );
    assert_eq!(
        raised.verdict(start + Duration::from_millis(500)),
        Verdict::Abort,
        "a plugin still inside the call at the hard deadline costs its process"
    );
    assert_eq!(
        armed.verdict(start + Duration::from_millis(900)),
        Verdict::Abort,
        "abort wins over cancel once both deadlines have passed"
    );
}

#[test]
fn a_host_deadline_shorter_than_the_manifests_is_the_one_that_applies() {
    let _fixture = fixture(MODE_ECHO);
    let mut plugin = load(complete_table()).expect("the fixture library loads");
    let context = TestContext::default();
    let mut sink = RecordingSink::default();

    // What is under test is that the request completes under a host deadline
    // far below the manifest's 1000 ms, which it could not do if the host took
    // the larger of the two.
    let mut tight = query("fixture");
    tight.deadline_ms = Some(50);
    plugin
        .suggest(tight, &context, &mut sink)
        .expect("a prompt plugin answers inside a tight deadline");
    assert_eq!(sink.batches.len(), 1);
}

// -- the type mirror --------------------------------------------------------

/// `crikey_plugin.h` and `abi.rs` are two hand-written copies of one layout.
/// Nothing makes them agree except review, so pin what a mismatch would change.
#[test]
fn the_abi_structs_have_the_layout_the_header_declares() {
    use std::mem::size_of;
    let word = size_of::<usize>();
    assert_eq!(size_of::<AbiStr>(), word * 2, "pointer plus length");
    assert_eq!(
        size_of::<AbiItems>(),
        word * 3 + 16,
        "two pointers, a count, two reserved"
    );
    assert_eq!(
        size_of::<AbiItem>(),
        word * 8 + 8 + 16,
        "four slices, a score, a reserved word, two reserved"
    );
    assert_eq!(abi::ABI_VERSION, 1, "version 1 is what the header declares");
    assert_eq!(abi::REQUIRED_SYMBOLS.len(), 6);
    assert_eq!(
        abi::REQUIRED_SYMBOLS[0],
        SYMBOL_ABI_VERSION,
        "the version symbol is resolved first"
    );
}
