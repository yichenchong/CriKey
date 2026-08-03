//! Public-API contract for the persistent per-plugin catalog cache.
//!
//! Scope: catalog items are a cached artifact (spec 22.1), explicit plugin
//! invalidation and schema-version changes discard cached state (spec 22.4),
//! and startup stage 2 loads the persisted core catalog before queries are
//! accepted (spec 25.6, 25.1). ADR-0008 fixes the shape defended here: one
//! slice per plugin so a rebuild or a damaged archive never invalidates the
//! whole catalog, an embedded schema version, and *discard and rebuild* rather
//! than migration whenever a slice cannot be trusted.
//!
//! The central rule: an unreadable slice is a cache miss, not a failure. Every
//! damage mode below must surface as `Ok(None)` for that plugin while every
//! other plugin's slice keeps loading. A startup path that returns `Err` on a
//! torn file would take the launcher down instead of rebuilding.
//!
//! Two layout requirements follow from fault isolation and are asserted here
//! rather than assumed:
//!
//! 1. Each plugin's slice occupies its own file(s) under the cache root, so
//!    damaging one plugin's bytes cannot reach another's.
//! 2. The archive records `SCHEMA_VERSION` in its leading header bytes, so a
//!    build reading a foreign layout detects it before interpreting payload.
//!
//! In-memory catalog semantics live in `catalog_behavior.rs`. Every test here
//! is filesystem-only, deterministic and free of sleeps, clocks and thresholds;
//! each owns a unique temp root that is removed when the test ends.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crikey_catalog::{
    CacheError, CachedSlice, CatalogCache, FileCatalogCache, MAX_ARCHIVE_BYTES, SCHEMA_VERSION,
};
use crikey_core::{
    Action, ActionId, ArgumentPolicy, Category, ExecutionPolicy, Generation, GenerationTracker, HitPolicy,
    Item, ItemId, PluginId,
};

/// Marker embedded in fixture payloads so a payload byte can be located and
/// flipped without knowing the archive layout.
const VICTIM_MARKER: &str = "CHKSUMMARKER";
const SURVIVOR_MARKER: &str = "SURVIVOR";

/// Header bytes searched for the recorded schema version.
const HEADER_WINDOW: usize = 64;

// ---------------------------------------------------------------------------
// temp roots
// ---------------------------------------------------------------------------

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

/// A unique temp directory removed when the test ends.
///
/// The cache root is a *child* of the guard directory and is deliberately not
/// created up front: an absent root is a valid empty cache, and a slice written
/// by a plugin whose id looks like a relative path must still land inside the
/// guard so cleanup reaches it.
#[derive(Debug)]
struct TempRoot {
    dir: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let unique = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "crikey-persistent-cache-{pid}-{unique}-{label}",
            pid = std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp root must be creatable");
        Self { dir }
    }

    fn cache_dir(&self) -> PathBuf {
        self.dir.join("cache")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

// ---------------------------------------------------------------------------
// API helpers: these pin the exact signatures the cache must expose
// ---------------------------------------------------------------------------

fn plugin_id(id: &str) -> PluginId {
    PluginId(id.to_owned())
}

/// Builds a non-zero generation without reaching past the public core API.
fn generation(steps: u64) -> Generation {
    let tracker = GenerationTracker::new();
    let mut generation = Generation::ZERO;
    for _ in 0..steps {
        generation = tracker.advance();
    }
    generation
}

fn store(cache: &dyn CatalogCache, slice: &CachedSlice) {
    let stored: Result<(), CacheError> = cache.store_slice(slice);
    stored.unwrap_or_else(|error| {
        panic!(
            "store_slice({owner}) must succeed: {error:?}",
            owner = slice.plugin.0
        )
    });
}

fn load(cache: &dyn CatalogCache, plugin: &PluginId) -> Option<CachedSlice> {
    let loaded: Result<Option<CachedSlice>, CacheError> = cache.load_slice(plugin);
    loaded.unwrap_or_else(|error| {
        panic!(
            "load_slice({owner}) must report a miss as Ok(None), never an error: {error:?}",
            owner = plugin.0
        )
    })
}

fn invalidate(cache: &dyn CatalogCache, plugin: &PluginId) {
    let invalidated: Result<(), CacheError> = cache.invalidate(plugin);
    invalidated
        .unwrap_or_else(|error| panic!("invalidate({owner}) must succeed: {error:?}", owner = plugin.0));
}

/// Owners currently held by the cache, sorted so assertions never depend on
/// directory iteration order, and checked for duplicates.
fn owners(cache: &dyn CatalogCache) -> Vec<String> {
    let listed: Result<Vec<PluginId>, CacheError> = cache.plugins();
    let listed = listed.expect("plugins() must succeed on a readable cache root");

    let mut names: Vec<String> = listed.into_iter().map(|plugin| plugin.0).collect();
    names.sort();
    let mut deduped = names.clone();
    deduped.dedup();
    assert_eq!(names, deduped, "plugins() must not repeat an owner");
    names
}

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn rich_action(marker: &str) -> Action {
    Action {
        action_id: ActionId(format!("{marker}.open")),
        label: "Öffnen 🚀".to_owned(),
        description: String::new(),
        applicable_categories: vec![
            Category::Application,
            Category::PluginDefined("café.plugin-defined".to_owned()),
        ],
        icon_reference: Some("action-icon://öffnen".to_owned()),
        execution_policy: ExecutionPolicy::Plugin,
    }
}

fn bare_action() -> Action {
    Action {
        action_id: ActionId(String::new()),
        label: String::new(),
        description: "Action with no categories and no icon".to_owned(),
        applicable_categories: Vec::new(),
        icon_reference: None,
        execution_policy: ExecutionPolicy::HostMediated,
    }
}

/// Three items chosen to exercise every field, both extremes of `score_hint`,
/// every optional as `Some` and `None`, empty strings and empty collections,
/// and non-ASCII text in ids, labels, targets, search terms and metadata.
fn fixture_items(plugin: &PluginId, marker: &str) -> Vec<Item> {
    let rich = Item {
        stable_id: ItemId(format!("{owner}::{marker}::rich", owner = plugin.0)),
        plugin_id: plugin.clone(),
        category: Category::PluginDefined("café.plugin-defined".to_owned()),
        label: format!("{marker} Café 日本語 🚀"),
        description: "Descripción — with an em dash".to_owned(),
        target: "/tmp/ünïcode path/with spaces/ﬁle.txt".to_owned(),
        search_terms: vec![
            "café".to_owned(),
            String::new(),
            "日本語".to_owned(),
            "🚀".to_owned(),
        ],
        icon_reference: Some("icon://café/🚀".to_owned()),
        argument_policy: ArgumentPolicy::Required,
        hit_policy: HitPolicy::Ignored,
        score_hint: i32::MIN,
        metadata: BTreeMap::from([
            (String::new(), "value under an empty key".to_owned()),
            ("empty-value".to_owned(), String::new()),
            (
                "multiline".to_owned(),
                "line one\nline two\t\"quoted\"".to_owned(),
            ),
            ("ünïcode".to_owned(), "值 🚀".to_owned()),
        ]),
        actions: vec![rich_action(marker), bare_action()],
    };

    let minimal = Item {
        stable_id: ItemId(format!("{owner}::{marker}::minimal", owner = plugin.0)),
        plugin_id: plugin.clone(),
        category: Category::Application,
        label: String::new(),
        description: String::new(),
        target: String::new(),
        search_terms: Vec::new(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    };

    let derived_target = format!("/tmp/{marker}/derived target");
    let derived = Item {
        stable_id: ItemId::derived(plugin, &Category::File, &derived_target),
        plugin_id: plugin.clone(),
        category: Category::File,
        label: "Derived identity".to_owned(),
        description: "Host-derived stable id".to_owned(),
        target: derived_target,
        search_terms: vec!["derived".to_owned()],
        icon_reference: Some(String::new()),
        argument_policy: ArgumentPolicy::Optional,
        hit_policy: HitPolicy::Ignored,
        score_hint: i32::MAX,
        metadata: BTreeMap::from([("kind".to_owned(), "derived".to_owned())]),
        actions: vec![bare_action()],
    };

    vec![rich, minimal, derived]
}

fn fixture_slice(plugin: &PluginId, instance: u64, steps: u64, marker: &str) -> CachedSlice {
    CachedSlice {
        plugin: plugin.clone(),
        instance,
        generation: generation(steps),
        items: fixture_items(plugin, marker),
    }
}

fn empty_slice(plugin: &PluginId, instance: u64, steps: u64) -> CachedSlice {
    CachedSlice {
        plugin: plugin.clone(),
        instance,
        generation: generation(steps),
        items: Vec::new(),
    }
}

fn item_ids(items: &[Item]) -> Vec<String> {
    items.iter().map(|item| item.stable_id.0.clone()).collect()
}

// ---------------------------------------------------------------------------
// field-by-field comparison (core `Item`/`Action` deliberately have no `PartialEq`)
// ---------------------------------------------------------------------------

fn assert_actions_eq(loaded: &[Action], expected: &[Action], context: &str) {
    assert_eq!(
        loaded.len(),
        expected.len(),
        "{context}: action count must round trip"
    );

    for (index, (left, right)) in loaded.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            left.action_id, right.action_id,
            "{context}: action {index} id must round trip"
        );
        assert_eq!(
            left.label, right.label,
            "{context}: action {index} label must round trip"
        );
        assert_eq!(
            left.description, right.description,
            "{context}: action {index} description must round trip"
        );
        assert_eq!(
            left.applicable_categories, right.applicable_categories,
            "{context}: action {index} applicable categories must round trip in order"
        );
        assert_eq!(
            left.icon_reference, right.icon_reference,
            "{context}: action {index} icon reference must round trip, including None"
        );
        assert_eq!(
            left.execution_policy, right.execution_policy,
            "{context}: action {index} execution policy must round trip"
        );
    }
}

fn assert_item_eq(loaded: &Item, expected: &Item, context: &str) {
    assert_eq!(
        loaded.stable_id, expected.stable_id,
        "{context}: stable id must round trip"
    );
    assert_eq!(
        loaded.plugin_id, expected.plugin_id,
        "{context}: owning plugin must round trip"
    );
    assert_eq!(
        loaded.category, expected.category,
        "{context}: category must round trip, including plugin-defined names"
    );
    assert_eq!(
        loaded.label, expected.label,
        "{context}: label must round trip, including empty and non-ASCII text"
    );
    assert_eq!(
        loaded.description, expected.description,
        "{context}: description must round trip"
    );
    assert_eq!(
        loaded.target, expected.target,
        "{context}: target must round trip"
    );
    assert_eq!(
        loaded.search_terms, expected.search_terms,
        "{context}: search terms must round trip in order, including empty terms"
    );
    assert_eq!(
        loaded.icon_reference, expected.icon_reference,
        "{context}: icon reference must round trip, distinguishing None from Some(\"\")"
    );
    assert_eq!(
        loaded.argument_policy, expected.argument_policy,
        "{context}: argument policy must round trip"
    );
    assert_eq!(
        loaded.hit_policy, expected.hit_policy,
        "{context}: hit policy must round trip"
    );
    assert_eq!(
        loaded.score_hint, expected.score_hint,
        "{context}: score hint must round trip, including i32 extremes"
    );
    assert_eq!(
        loaded.metadata, expected.metadata,
        "{context}: metadata must round trip entry for entry"
    );
    assert_actions_eq(&loaded.actions, &expected.actions, context);
}

fn assert_slice_eq(loaded: &CachedSlice, expected: &CachedSlice, context: &str) {
    assert_eq!(
        loaded.plugin, expected.plugin,
        "{context}: slice owner must round trip"
    );
    assert_eq!(
        loaded.instance, expected.instance,
        "{context}: publishing instance must round trip so a stale worker cannot reclaim the slice"
    );
    assert_eq!(
        loaded.generation, expected.generation,
        "{context}: generation must round trip"
    );
    assert_eq!(
        item_ids(&loaded.items),
        item_ids(&expected.items),
        "{context}: items must round trip in their stable catalog order"
    );

    for (index, (left, right)) in loaded.items.iter().zip(expected.items.iter()).enumerate() {
        assert_item_eq(left, right, &format!("{context}: item {index}"));
    }
}

// ---------------------------------------------------------------------------
// on-disk helpers
// ---------------------------------------------------------------------------

fn collect_files(dir: &Path, files: &mut BTreeSet<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries {
        let path = entry.expect("cache directory entry must be readable").path();
        if path.is_dir() {
            collect_files(&path, files);
        } else {
            files.insert(path);
        }
    }
}

fn list_files(dir: &Path) -> BTreeSet<PathBuf> {
    let mut files = BTreeSet::new();
    collect_files(dir, &mut files);
    files
}

/// The byte-bearing file of a slice: the largest of the files the store created
/// for that plugin. Deterministic because the candidate list is path-sorted.
fn payload_file(files: &[PathBuf]) -> PathBuf {
    files
        .iter()
        .max_by_key(|path| fs::metadata(path).map(|meta| meta.len()).unwrap_or(0))
        .expect("a stored slice must have at least one file")
        .clone()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&start| &haystack[start..start + needle.len()] == needle)
}

/// The last occurrence of `needle`, used where a fixture string is encoded
/// more than once and only the final copy is the field being aimed at.
fn find_last(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).rfind(|&start| &haystack[start..start + needle.len()] == needle)
}

fn read_bytes(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|error| {
        panic!(
            "slice file {path} must be readable: {error:?}",
            path = path.display()
        )
    })
}

fn write_bytes(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap_or_else(|error| {
        panic!(
            "slice file {path} must be writable: {error:?}",
            path = path.display()
        )
    });
}

/// Rewrites the recorded schema version to a value this build does not accept.
///
/// The encoding is not dictated: the header window is searched for `u32`, `u16`,
/// single-byte and ASCII-decimal spellings before the whole file is searched.
/// A slice that records the version nowhere cannot honour ADR-0008's
/// "version mismatch discards the cache", so the search failing is a contract
/// failure, not a test bug.
fn patch_schema_version(path: &Path) {
    let bytes = read_bytes(path);
    let current = SCHEMA_VERSION;
    let next = SCHEMA_VERSION.wrapping_add(1);
    assert_ne!(
        current, next,
        "SCHEMA_VERSION must leave room for a distinguishable neighbouring version"
    );

    let mut encodings: Vec<(Vec<u8>, Vec<u8>)> = vec![
        (current.to_le_bytes().to_vec(), next.to_le_bytes().to_vec()),
        (current.to_be_bytes().to_vec(), next.to_be_bytes().to_vec()),
        (current.to_string().into_bytes(), next.to_string().into_bytes()),
    ];
    if let (Ok(current16), Ok(next16)) = (u16::try_from(current), u16::try_from(next)) {
        encodings.push((current16.to_le_bytes().to_vec(), next16.to_le_bytes().to_vec()));
        encodings.push((current16.to_be_bytes().to_vec(), next16.to_be_bytes().to_vec()));
    }
    if let (Ok(current8), Ok(next8)) = (u8::try_from(current), u8::try_from(next)) {
        encodings.push((vec![current8], vec![next8]));
    }

    for window in [bytes.len().min(HEADER_WINDOW), bytes.len()] {
        for (needle, replacement) in &encodings {
            let Some(offset) = find(&bytes[..window], needle) else {
                continue;
            };

            let mut patched = Vec::with_capacity(bytes.len() + replacement.len());
            patched.extend_from_slice(&bytes[..offset]);
            patched.extend_from_slice(replacement);
            patched.extend_from_slice(&bytes[offset + needle.len()..]);
            write_bytes(path, &patched);
            return;
        }
    }

    panic!(
        "a persisted slice must record SCHEMA_VERSION ({SCHEMA_VERSION}) so a foreign layout is \
         detected before its payload is interpreted; no encoding of it was found in {path}",
        path = path.display()
    );
}

// ---------------------------------------------------------------------------
// damage driver
// ---------------------------------------------------------------------------

/// Stores an intact slice, then a victim slice, hands the victim's freshly
/// created files to `damage`, and asserts the damage is contained: the victim
/// reads back as a miss, the neighbour is untouched, and a rebuild heals the
/// cache without any manual cleanup.
fn assert_damage_is_contained(label: &str, damage: impl FnOnce(&[PathBuf])) {
    let root = TempRoot::new(label);
    let cache = FileCatalogCache::new(root.cache_dir());
    let survivor = plugin_id("survivor.plugin");
    let victim = plugin_id("victim.plugin");

    store(&cache, &fixture_slice(&survivor, 3, 5, SURVIVOR_MARKER));
    let before = list_files(&root.cache_dir());
    store(&cache, &fixture_slice(&victim, 9, 11, VICTIM_MARKER));
    let after = list_files(&root.cache_dir());

    let victim_files: Vec<PathBuf> = after.difference(&before).cloned().collect();
    assert!(
        !victim_files.is_empty(),
        "{label}: each plugin's slice must occupy its own file(s) so damaged bytes cannot reach \
         another plugin's slice (ADR-0008)"
    );

    damage(victim_files.as_slice());

    assert!(
        load(&cache, &victim).is_none(),
        "{label}: an untrustworthy slice must be discarded as a cache miss, never returned and \
         never raised as an error during startup stage 2"
    );

    let intact = load(&cache, &survivor)
        .unwrap_or_else(|| panic!("{label}: an untouched slice must survive a damaged sibling"));
    assert_slice_eq(&intact, &fixture_slice(&survivor, 3, 5, SURVIVOR_MARKER), label);
    assert!(
        owners(&cache).contains(&survivor.0),
        "{label}: plugins() must still list the intact owner"
    );

    let rebuilt = fixture_slice(&victim, 12, 13, "REBUILT");
    store(&cache, &rebuilt);
    let reloaded = load(&cache, &victim).unwrap_or_else(|| {
        panic!("{label}: a discarded slice must be rebuildable straight over the damaged bytes")
    });
    assert_slice_eq(&reloaded, &rebuilt, &format!("{label}: rebuild"));
}

// ---------------------------------------------------------------------------
// round trip
// ---------------------------------------------------------------------------

#[test]
fn round_trip_preserves_every_field_of_a_stored_slice() {
    let root = TempRoot::new("round-trip");
    let cache = FileCatalogCache::new(root.cache_dir());
    let plugin = plugin_id("com.example.round-trip");
    let slice = fixture_slice(&plugin, 17, 42, "ROUNDTRIP");

    store(&cache, &slice);
    let loaded = load(&cache, &plugin).expect("a stored slice must load back");

    assert_slice_eq(&loaded, &slice, "round trip");
    assert_eq!(
        loaded.generation.get(),
        42,
        "round trip: the generation value itself must survive persistence"
    );
}

#[test]
fn round_trip_survives_a_fresh_cache_handle_over_the_same_root() {
    let root = TempRoot::new("reopen");
    let plugin = plugin_id("com.example.reopen");
    let slice = fixture_slice(&plugin, 4, 8, "REOPEN");

    let writer = FileCatalogCache::new(root.cache_dir());
    store(&writer, &slice);
    drop(writer);

    // Startup stage 2 always reads through a brand new handle, never the one
    // that wrote the slice: nothing may live only in process memory.
    let reader = FileCatalogCache::new(root.cache_dir());
    let loaded = load(&reader, &plugin).expect("a persisted slice must load through a new handle");
    assert_slice_eq(&loaded, &slice, "reopen");
    assert_eq!(owners(&reader), vec![plugin.0.clone()]);
}

#[test]
fn empty_item_list_round_trips_as_a_stored_slice() {
    let root = TempRoot::new("empty-items");
    let cache = FileCatalogCache::new(root.cache_dir());
    let plugin = plugin_id("com.example.empty");
    let slice = empty_slice(&plugin, 6, 9);

    store(&cache, &slice);
    let loaded = load(&cache, &plugin)
        .expect("a plugin that legitimately contributes nothing must still be a stored slice");

    assert_slice_eq(&loaded, &slice, "empty slice");
    assert!(
        loaded.items.is_empty(),
        "empty slice: no items may be invented on load"
    );
    assert_eq!(
        owners(&cache),
        vec![plugin.0.clone()],
        "empty slice: an emptied owner is cached, not absent, so it is not rebuilt every launch"
    );
    assert!(
        load(&cache, &plugin_id("com.example.never-stored")).is_none(),
        "empty slice: a stored-but-empty slice must stay distinguishable from an unknown plugin"
    );
}

// ---------------------------------------------------------------------------
// isolation
// ---------------------------------------------------------------------------

#[test]
fn slices_are_isolated_per_plugin() {
    let root = TempRoot::new("isolation");
    let cache = FileCatalogCache::new(root.cache_dir());
    let first = plugin_id("alpha.plugin");
    let second = plugin_id("beta.plugin");
    let third = plugin_id("gamma.plugin");

    let slices = [
        fixture_slice(&first, 1, 2, "ALPHA"),
        fixture_slice(&second, 20, 30, "BETA"),
        empty_slice(&third, 300, 400),
    ];
    for slice in &slices {
        store(&cache, slice);
    }

    for slice in &slices {
        let loaded = load(&cache, &slice.plugin).unwrap_or_else(|| {
            panic!(
                "every stored owner must load its own slice: {owner}",
                owner = slice.plugin.0
            )
        });
        assert_slice_eq(&loaded, slice, &slice.plugin.0);
    }

    let alpha = load(&cache, &first).expect("alpha slice");
    let beta = load(&cache, &second).expect("beta slice");
    for item in &alpha.items {
        assert_eq!(
            item.plugin_id, first,
            "a slice must only hand back items owned by the plugin it was loaded for"
        );
    }

    let beta_ids = item_ids(&beta.items);
    for id in item_ids(&alpha.items) {
        assert!(
            !beta_ids.contains(&id),
            "slices must not bleed into each other: {id} is in both alpha's and beta's slice"
        );
    }
}

#[test]
fn plugin_ids_are_never_used_as_raw_paths() {
    let root = TempRoot::new("hostile-ids");
    let cache = FileCatalogCache::new(root.cache_dir());

    // Plugin ids are arbitrary strings from manifests, not filenames.
    let hostile = [
        plugin_id("nested/child"),
        plugin_id("../escape"),
        plugin_id("space and 🚀"),
        plugin_id("Case.Sensitive"),
        plugin_id("case.sensitive"),
    ];

    for (index, plugin) in hostile.iter().enumerate() {
        store(&cache, &fixture_slice(plugin, index as u64, 3, "HOSTILE"));
    }

    for (index, plugin) in hostile.iter().enumerate() {
        let loaded = load(&cache, plugin).unwrap_or_else(|| {
            panic!(
                "a slice stored under id {owner:?} must load back under the same id",
                owner = plugin.0
            )
        });
        assert_slice_eq(
            &loaded,
            &fixture_slice(plugin, index as u64, 3, "HOSTILE"),
            &plugin.0,
        );
    }

    let mut expected: Vec<String> = hostile.iter().map(|plugin| plugin.0.clone()).collect();
    expected.sort();
    assert_eq!(
        owners(&cache),
        expected,
        "plugins() must report the owner ids that were stored, not filesystem-mangled names"
    );

    // Scan the whole guard directory, not just the cache root: an id spelled
    // like a relative path must not be able to write outside the root at all.
    let cache_dir = root.cache_dir();
    for path in list_files(&root.dir) {
        assert!(
            path.starts_with(&cache_dir),
            "no plugin id may place cache bytes outside the cache root: {path}",
            path = path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// replace and invalidate
// ---------------------------------------------------------------------------

#[test]
fn windows_device_names_are_encoded_as_safe_file_components() {
    let root = TempRoot::new("windows-device-names");
    let cache = FileCatalogCache::new(root.cache_dir());
    let names = ["con", "con.", "prn", "aux", "nul", "com1", "lpt9"];

    for (index, name) in names.iter().enumerate() {
        let plugin = plugin_id(name);
        store(&cache, &fixture_slice(&plugin, index as u64, 1, "DEVICE"));
        assert_slice_eq(
            &load(&cache, &plugin).expect("a reserved-name plugin slice must round trip"),
            &fixture_slice(&plugin, index as u64, 1, "DEVICE"),
            name,
        );
    }

    for path in list_files(cache.root()) {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        assert!(
            !names
                .iter()
                .any(|name| file_name.eq_ignore_ascii_case(&format!("{name}.slice"))),
            "a Windows device name must never be used as a cache filename: {file_name}"
        );
    }
    let mut expected: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
    expected.sort();
    assert_eq!(owners(&cache), expected);
}

#[test]
fn storing_a_slice_again_replaces_the_previous_one_without_residue() {
    let root = TempRoot::new("replace");
    let cache_dir = root.cache_dir();
    let cache = FileCatalogCache::new(cache_dir.clone());
    let plugin = plugin_id("com.example.replace");

    let old = fixture_slice(&plugin, 4, 10, "OLD");
    store(&cache, &old);
    let after_first = list_files(&cache_dir).len();

    let new = fixture_slice(&plugin, 9, 30, "NEW");
    store(&cache, &new);
    let after_second = list_files(&cache_dir).len();

    let loaded = load(&cache, &plugin).expect("the replaced slice must load");
    assert_slice_eq(&loaded, &new, "replace");

    let stale_ids = item_ids(&old.items);
    for id in item_ids(&loaded.items) {
        assert!(
            !stale_ids.contains(&id),
            "a replace must be all-or-nothing: item {id} survived from the previous slice"
        );
    }

    assert_eq!(
        after_second, after_first,
        "an atomic replace leaves no temporary or superseded file behind: file count grew from \
         {after_first} to {after_second}"
    );
    assert_eq!(
        owners(&cache),
        vec![plugin.0.clone()],
        "replacing a slice must not register the owner twice"
    );
}

#[test]
fn invalidate_removes_only_the_named_slice() {
    let root = TempRoot::new("invalidate");
    let cache = FileCatalogCache::new(root.cache_dir());
    let first = plugin_id("alpha.plugin");
    let second = plugin_id("beta.plugin");
    let third = plugin_id("gamma.plugin");

    let alpha = fixture_slice(&first, 1, 2, "ALPHA");
    let beta = fixture_slice(&second, 3, 4, "BETA");
    let gamma = fixture_slice(&third, 5, 6, "GAMMA");
    store(&cache, &alpha);
    store(&cache, &beta);
    store(&cache, &gamma);

    invalidate(&cache, &second);

    assert!(
        load(&cache, &second).is_none(),
        "an invalidated slice must read back as a miss (spec 22.4)"
    );
    assert_slice_eq(
        &load(&cache, &first).expect("alpha must survive beta's invalidation"),
        &alpha,
        "alpha after invalidate",
    );
    assert_slice_eq(
        &load(&cache, &third).expect("gamma must survive beta's invalidation"),
        &gamma,
        "gamma after invalidate",
    );
    assert_eq!(
        owners(&cache),
        vec![first.0.clone(), third.0.clone()],
        "plugins() must drop only the invalidated owner"
    );

    // Invalidation is idempotent, and unknown owners are not an error: the
    // rebuild path fires invalidate() without first proving a slice exists.
    invalidate(&cache, &second);
    invalidate(&cache, &plugin_id("never.stored"));
    assert_eq!(owners(&cache), vec![first.0.clone(), third.0.clone()]);

    let rebuilt = fixture_slice(&second, 7, 8, "BETA-REBUILT");
    store(&cache, &rebuilt);
    assert_slice_eq(
        &load(&cache, &second).expect("an invalidated owner must be storable again"),
        &rebuilt,
        "beta rebuild",
    );
}

// ---------------------------------------------------------------------------
// listing and misses
// ---------------------------------------------------------------------------

#[test]
fn plugins_lists_every_stored_owner_exactly_once() {
    let root = TempRoot::new("owners");
    let cache = FileCatalogCache::new(root.cache_dir());
    let first = plugin_id("alpha.plugin");
    let second = plugin_id("beta.plugin");

    assert!(
        owners(&cache).is_empty(),
        "a cache root that was never written must list no owners"
    );

    store(&cache, &fixture_slice(&first, 1, 1, "ALPHA"));
    assert_eq!(owners(&cache), vec![first.0.clone()]);

    store(&cache, &fixture_slice(&first, 2, 2, "ALPHA-AGAIN"));
    assert_eq!(
        owners(&cache),
        vec![first.0.clone()],
        "re-storing an owner must not list it twice"
    );

    store(&cache, &empty_slice(&second, 1, 1));
    assert_eq!(owners(&cache), vec![first.0.clone(), second.0.clone()]);

    invalidate(&cache, &first);
    assert_eq!(owners(&cache), vec![second.0.clone()]);
}

#[test]
fn unknown_plugins_and_a_missing_root_read_as_misses() {
    let root = TempRoot::new("misses");
    let cache = FileCatalogCache::new(root.cache_dir());
    let unknown = plugin_id("never.stored");

    // A first launch has no cache root at all; that is a cold cache, not a fault.
    assert!(
        !root.cache_dir().exists(),
        "the fixture must start without a cache root"
    );
    assert!(
        load(&cache, &unknown).is_none(),
        "loading from a missing cache root must be a miss, not an error"
    );
    assert!(owners(&cache).is_empty());
    invalidate(&cache, &unknown);

    let known = plugin_id("alpha.plugin");
    let slice = fixture_slice(&known, 2, 3, "ALPHA");
    store(&cache, &slice);

    assert!(
        load(&cache, &unknown).is_none(),
        "an unknown owner must stay a miss once the cache is populated"
    );
    assert!(
        load(&cache, &plugin_id("alpha.plugin.extra")).is_none(),
        "owner lookup must match the whole id, not a prefix"
    );
    assert!(
        load(&cache, &plugin_id("ALPHA.PLUGIN")).is_none(),
        "owner lookup must not fold case: plugin ids are exact"
    );
    assert_slice_eq(
        &load(&cache, &known).expect("the stored owner still loads"),
        &slice,
        "after misses",
    );
}

#[test]
fn noncanonical_percent_escapes_are_not_listed_as_cached_owners() {
    let root = TempRoot::new("canonical-name");
    let cache_dir = root.cache_dir();
    let cache = FileCatalogCache::new(cache_dir.clone());
    let plugin = plugin_id("a");

    store(&cache, &fixture_slice(&plugin, 1, 1, "CANONICAL"));
    let canonical = cache_dir.join("a.slice");
    let alias = cache_dir.join("%61.slice");
    fs::copy(&canonical, &alias).expect("the alias fixture must be copyable");
    fs::remove_file(&canonical).expect("the canonical fixture must be removable");

    assert!(
        owners(&cache).is_empty(),
        "a noncanonical escaped filename must not masquerade as a cache owner"
    );
    assert!(
        load(&cache, &plugin).is_none(),
        "load_slice must use the canonical filename rather than accepting an alias"
    );
}

#[test]
fn an_archive_larger_than_the_read_bound_is_a_cache_miss() {
    let root = TempRoot::new("archive-bound");
    let cache_dir = root.cache_dir();
    let cache = FileCatalogCache::new(cache_dir.clone());
    let plugin = plugin_id("bounded.plugin");

    store(&cache, &fixture_slice(&plugin, 1, 1, "BOUND"));
    let path = cache_dir.join("bounded.plugin.slice");
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("the stored archive must be writable");
    file.set_len(MAX_ARCHIVE_BYTES + 1)
        .expect("the filesystem must support extending the sparse fixture");

    assert!(
        load(&cache, &plugin).is_none(),
        "a file larger than the bounded reader must be discarded before decoding"
    );
}

#[test]
fn an_unreadable_archive_reports_a_filesystem_error_without_panicking() {
    let root = TempRoot::new("unreadable");
    let cache_dir = root.cache_dir();
    let cache = FileCatalogCache::new(cache_dir.clone());
    let plugin = plugin_id("unreadable.plugin");
    fs::create_dir_all(&cache_dir).expect("the cache root must be creatable");
    fs::create_dir(cache_dir.join("unreadable.plugin.slice"))
        .expect("the unreadable fixture directory must be creatable");

    let result = cache.load_slice(&plugin);
    assert!(
        matches!(result, Err(CacheError::Io { .. })),
        "a filesystem read failure must be reported as an I/O error, got {result:?}"
    );
}

#[test]
fn the_cache_is_usable_behind_a_trait_object() {
    let root = TempRoot::new("dyn-object");
    let plugin = plugin_id("com.example.dynamic");
    let slice = fixture_slice(&plugin, 5, 7, "DYNAMIC");

    // Startup hands the cache to the search service as `&dyn CatalogCache`;
    // the trait must stay object safe.
    let boxed: Box<dyn CatalogCache> = Box::new(FileCatalogCache::new(root.cache_dir()));
    let cache: &dyn CatalogCache = &*boxed;

    store(cache, &slice);
    assert_slice_eq(
        &load(cache, &plugin).expect("a slice stored through a trait object must load back"),
        &slice,
        "trait object",
    );
    assert_eq!(owners(cache), vec![plugin.0.clone()]);
    invalidate(cache, &plugin);
    assert!(load(cache, &plugin).is_none());
}

// ---------------------------------------------------------------------------
// damaged slices: every mode is a miss, never an error, never a neighbour's problem
// ---------------------------------------------------------------------------

#[test]
fn corrupt_bytes_discard_only_that_slice() {
    assert_damage_is_contained("corrupt-bytes", |files: &[PathBuf]| {
        for path in files {
            write_bytes(path, b"this is not a catalog slice archive");
        }
    });
}

#[test]
fn a_truncated_slice_file_is_discarded() {
    type Truncation = (&'static str, fn(usize) -> usize);
    let truncations: [Truncation; 4] = [
        ("truncated-to-empty", |_| 0),
        ("truncated-to-header", |len| len.min(3)),
        ("truncated-to-half", |len| len / 2),
        ("truncated-by-one-byte", |len| len.saturating_sub(1)),
    ];

    for (label, keep) in truncations {
        assert_damage_is_contained(label, |files: &[PathBuf]| {
            let path = payload_file(files);
            let bytes = read_bytes(&path);
            assert!(
                bytes.len() > 1,
                "the fixture slice must exceed one byte for truncation to mean anything"
            );
            write_bytes(&path, &bytes[..keep(bytes.len())]);
        });
    }
}

#[test]
fn a_slice_with_the_wrong_header_bytes_is_discarded() {
    assert_damage_is_contained("wrong-magic", |files: &[PathBuf]| {
        let path = payload_file(files);
        let mut bytes = read_bytes(&path);
        assert!(
            bytes.len() >= 4,
            "a slice archive must carry at least a four byte header to identify itself"
        );
        for byte in &mut bytes[..4] {
            *byte ^= 0xFF;
        }
        write_bytes(&path, &bytes);
    });
}

#[test]
fn trailing_bytes_are_not_accepted_as_a_complete_archive() {
    assert_damage_is_contained("trailing-bytes", |files: &[PathBuf]| {
        let path = payload_file(files);
        let mut bytes = read_bytes(&path);
        bytes.push(0);
        write_bytes(&path, &bytes);
    });
}

#[test]
fn a_foreign_schema_version_discards_the_slice() {
    const {
        assert!(
            SCHEMA_VERSION >= 1,
            "SCHEMA_VERSION must start at 1 so an all-zero header is never mistaken for a valid archive"
        );
    }

    assert_damage_is_contained("schema-version", |files: &[PathBuf]| {
        patch_schema_version(&payload_file(files));
    });
}

#[test]
fn a_payload_edit_that_still_parses_is_caught_by_the_checksum() {
    assert_damage_is_contained("checksum-mismatch", |files: &[PathBuf]| {
        let path = payload_file(files);
        let mut bytes = read_bytes(&path);
        assert!(
            bytes.len() >= 4,
            "the fixture slice must be large enough to damage a payload byte"
        );

        // Flip one bit inside a payload byte, keeping the header, every length
        // and UTF-8 validity intact: 'C' becomes 'A'. Nothing but a checksum
        // over the payload can notice, so a cache without one hands the
        // launcher silently wrong catalog text.
        let offset = find(&bytes, VICTIM_MARKER.as_bytes()).unwrap_or(bytes.len() * 3 / 4);
        bytes[offset] ^= 0x02;
        write_bytes(&path, &bytes);
    });
}

#[test]
fn a_payload_element_count_the_file_cannot_honour_is_discarded() {
    assert_damage_is_contained("hostile-element-count", |files: &[PathBuf]| {
        let path = payload_file(files);
        let mut bytes = read_bytes(&path);

        // The derived item's target is encoded twice: inside its host-derived
        // stable id, and again as the target field. The count that follows the
        // last copy is that item's search-term count, which the fixture
        // publishes as one. Raising it to `u32::MAX` asks the reader for four
        // billion elements out of a file of a few hundred bytes.
        //
        // Which guard turns it away is deliberately not asserted — today the
        // checksum notices the edit before the count is read. What is pinned
        // is the outcome: a miss, no error, no panic, no allocation the file
        // cannot account for, and a neighbouring slice that still loads.
        let target = format!("/tmp/{VICTIM_MARKER}/derived target");
        let at = find_last(&bytes, target.as_bytes())
            .expect("the fixture's derived target must reach the payload verbatim")
            + target.len();
        assert_eq!(
            bytes.get(at..at + 4),
            Some(1u32.to_le_bytes().as_slice()),
            "a repeated field must record how many elements follow it; with no count to inflate, \
             this test damages an unrelated field and proves nothing"
        );

        bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        write_bytes(&path, &bytes);
    });
}
