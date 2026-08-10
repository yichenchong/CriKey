//! Contract for remote catalog sources (spec 2.2 "distributed or remote
//! indexing"; ADR-0016).
//!
//! What is defended here is the seam, not the transport: a remote source is one
//! more catalog owner publishing one more slice, so a document that verifies
//! becomes searchable through the ordinary query path, and a document that does
//! not is refused without costing the launcher the slice it was already serving.
//!
//! Nothing here opens a socket. Two fetchers appear:
//!
//! * the production [`DefaultCatalogFetcher`], exercised over `file://` — which
//!   is not a test shim but the mounted-share half of the feature; and
//! * a recording fake, used where a test has to *count* fetches to say anything
//!   at all: coalescing a burst into one fetch, and proving the query path never
//!   fetches.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crikey_app::{
    fetch_source, remote_owner, App, CatalogFetcher, DefaultCatalogFetcher, RemoteCatalogError,
    RemoteCatalogService, RemoteReport, RemoteSource, SearchService, StartupStage,
};
use crikey_catalog::{encode_slice_document, CachedSlice};
use crikey_core::{ArgumentPolicy, Category, Generation, HitPolicy, Item, ItemId, PluginId};
use crikey_package_manager::TrustStore;

/// The owner a shared index stamps into its own documents. Deliberately not the
/// local owner id, so re-owning is observable.
const PUBLISHER: &str = "team.shared-index";

/// The source name every test declares, and therefore the local owner
/// `remote.team`.
const SOURCE: &str = "team";

const MANIFEST_NAME: &str = "index.txt";
const SLICE_NAME: &str = "catalog.slice";

/// How long a test waits for a refresh thread before calling it a failure.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// A private directory under the system temp dir, removed when the test ends.
#[derive(Debug)]
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "crikey-remote-catalog-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create the test source root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// The `file://` URL of a document in this directory.
    ///
    /// A Windows path is not a URL path: it has backslashes and starts with a
    /// drive letter rather than a slash, so the empty-authority form has to be
    /// spelled `file:///C:/dir/file`. Pasting `Path::display()` after `file://`
    /// would name host `C:`, which the fetcher rightly refuses.
    fn url(&self, name: &str) -> String {
        let text = self.path.to_string_lossy().replace('\\', "/");
        if text.starts_with('/') {
            format!("file://{text}/{name}")
        } else {
            format!("file:///{text}/{name}")
        }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn item(id: &str, label: &str) -> Item {
    Item {
        stable_id: ItemId(id.to_owned()),
        plugin_id: PluginId(PUBLISHER.to_owned()),
        category: Category::Application,
        label: label.to_owned(),
        description: "published by the shared index".to_owned(),
        target: format!("app://{id}"),
        search_terms: Vec::new(),
        icon_reference: None,
        argument_policy: ArgumentPolicy::Forbidden,
        hit_policy: HitPolicy::Recorded,
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
    }
}

/// The document a well-behaved shared index publishes.
fn document(instance: u64, items: Vec<Item>) -> Vec<u8> {
    encode_slice_document(&CachedSlice {
        plugin: PluginId(PUBLISHER.to_owned()),
        instance,
        generation: Generation::ZERO,
        items,
    })
    .expect("the fixture slice encodes")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Writes a slice document and a manifest that describes it truthfully.
fn publish(root: &TempRoot, instance: u64, items: Vec<Item>) -> Vec<u8> {
    let bytes = document(instance, items);
    fs::write(root.path().join(SLICE_NAME), &bytes).expect("write the slice document");
    write_manifest(root, bytes.len() as u64, &sha256_hex(&bytes), None);
    bytes
}

fn write_manifest(root: &TempRoot, bytes: u64, sha256: &str, signature: Option<&str>) {
    let mut text = format!("crikey-remote-catalog 1\nslice {SLICE_NAME}\nbytes {bytes}\nsha256 {sha256}\n");
    if let Some(signature) = signature {
        text.push_str(&format!("signature {signature}\n"));
    }
    fs::write(root.path().join(MANIFEST_NAME), text).expect("write the manifest");
}

fn source(root: &TempRoot) -> RemoteSource {
    RemoteSource::new(SOURCE, &root.url(MANIFEST_NAME))
}

/// A search service that accepts queries and holds nothing but what a test
/// publishes into it.
fn ready_search() -> SearchService {
    let mut search = SearchService::new(App::new());
    search
        .complete_stage(StartupStage::WindowAndHotkey)
        .expect("the window milestone is pending first");
    search
        .complete_stage(StartupStage::PersistedCatalog)
        .expect("the persisted-catalog milestone follows");
    search
        .complete_stage(StartupStage::AcceptQueries)
        .expect("queries become legal");
    search
}

/// Drives one refresh to completion and returns what it reported.
///
/// Polls once — the fetch is one thread, not a loop — then waits for the
/// outcome to arrive. A timeout is a failure rather than an empty result, so a
/// refresh that never finishes cannot pass as a refresh that produced nothing.
fn settle(service: &mut RemoteCatalogService, search: &mut SearchService) -> Vec<RemoteReport> {
    service.poll(0);
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        let reports = service.apply(search, 0);
        if !reports.is_empty() {
            return reports;
        }
        assert!(
            Instant::now() < deadline,
            "no refresh outcome arrived within {SETTLE_TIMEOUT:?}"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

/// The labels a query answers with.
fn answer(search: &mut SearchService, query: &str) -> Vec<String> {
    search.submit_query(query).expect("queries are accepted");
    search
        .results()
        .iter()
        .map(|hit| hit.item.label.clone())
        .collect()
}

/// A fetcher that records every request and reads from a directory.
#[derive(Debug, Default)]
struct RecordingFetcher {
    calls: Mutex<Vec<(String, u64)>>,
    inner: DefaultCatalogFetcher,
}

impl RecordingFetcher {
    fn calls(&self) -> Vec<(String, u64)> {
        self.calls.lock().expect("the call log is not poisoned").clone()
    }
}

impl CatalogFetcher for RecordingFetcher {
    fn fetch(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, RemoteCatalogError> {
        self.calls
            .lock()
            .expect("the call log is not poisoned")
            .push((url.to_owned(), max_bytes));
        self.inner.fetch(url, max_bytes)
    }
}

// ---------------------------------------------------------------------------
// admission
// ---------------------------------------------------------------------------

#[test]
fn a_verified_document_becomes_searchable_through_the_ordinary_query_path() {
    let root = TempRoot::new("searchable");
    publish(&root, 1, vec![item("atlas", "Fire Atlas")]);
    let mut search = ready_search();
    let mut service = RemoteCatalogService::new(
        vec![source(&root)],
        Arc::new(DefaultCatalogFetcher),
        Arc::new(TrustStore::empty()),
    );

    assert!(
        answer(&mut search, "fire").is_empty(),
        "the fixture must not be searchable before it is fetched"
    );

    let reports = settle(&mut service, &mut search);
    assert!(
        matches!(reports.as_slice(), [RemoteReport::Published { items: 1, .. }]),
        "one document published one item: {reports:?}"
    );
    assert_eq!(answer(&mut search, "fire"), vec!["Fire Atlas".to_owned()]);
}

#[test]
fn a_published_document_is_re_owned_to_the_local_source() {
    let root = TempRoot::new("reowned");
    publish(&root, 1, vec![item("atlas", "Fire Atlas")]);
    let remote = fetch_source(&source(&root), &DefaultCatalogFetcher, &TrustStore::empty())
        .expect("the fixture verifies");

    assert_eq!(
        remote.published_by,
        PluginId(PUBLISHER.to_owned()),
        "the publisher's own owner id is retained for diagnostics"
    );
    assert_eq!(remote.slice.plugin, remote_owner(SOURCE));
    for item in &remote.slice.items {
        assert_eq!(
            item.plugin_id,
            remote_owner(SOURCE),
            "every item is owned by the local source, or the catalog would refuse the slice"
        );
    }
}

#[test]
fn an_unsigned_document_is_refused_where_the_operator_required_a_signature() {
    let root = TempRoot::new("unsigned");
    publish(&root, 1, vec![item("atlas", "Fire Atlas")]);
    let mut source = source(&root);
    source.require_signature = true;

    let error = fetch_source(&source, &DefaultCatalogFetcher, &TrustStore::empty())
        .expect_err("an unsigned document cannot satisfy a signature requirement");
    assert!(
        matches!(error, RemoteCatalogError::Unsigned { .. }),
        "the refusal says the document is unsigned: {error}"
    );
    assert!(
        error.to_string().contains(SLICE_NAME),
        "the refusal names the artefact: {error}"
    );
}

#[test]
fn a_signature_document_that_is_not_a_signature_is_refused() {
    let root = TempRoot::new("bad-signature");
    let bytes = document(1, vec![item("atlas", "Fire Atlas")]);
    fs::write(root.path().join(SLICE_NAME), &bytes).expect("write the slice document");
    write_manifest(
        &root,
        bytes.len() as u64,
        &sha256_hex(&bytes),
        Some("catalog.slice.sig"),
    );
    fs::write(root.path().join("catalog.slice.sig"), "not a signature at all\n")
        .expect("write the signature document");

    let error = fetch_source(&source(&root), &DefaultCatalogFetcher, &TrustStore::empty())
        .expect_err("an unparseable signature is not a verified one");
    assert!(
        matches!(error, RemoteCatalogError::Signature { .. }),
        "the refusal is about the signature: {error}"
    );
}

// ---------------------------------------------------------------------------
// offline first
// ---------------------------------------------------------------------------

#[test]
fn a_digest_mismatch_is_refused_and_the_previous_document_keeps_serving() {
    let root = TempRoot::new("digest");
    publish(&root, 1, vec![item("atlas", "Fire Atlas")]);
    let mut search = ready_search();
    let mut service = RemoteCatalogService::new(
        vec![source(&root)],
        Arc::new(DefaultCatalogFetcher),
        Arc::new(TrustStore::empty()),
    );
    settle(&mut service, &mut search);
    assert_eq!(answer(&mut search, "fire"), vec!["Fire Atlas".to_owned()]);

    // A second revision whose manifest disagrees with its own bytes: whether the
    // document or the manifest was tampered with is not knowable, and neither is
    // admitted.
    let replacement = document(2, vec![item("flood", "Water Clock")]);
    fs::write(root.path().join(SLICE_NAME), &replacement).expect("write the second revision");
    write_manifest(&root, replacement.len() as u64, &"0".repeat(64), None);

    service.request_refresh();
    let reports = settle(&mut service, &mut search);
    assert!(
        matches!(
            reports.as_slice(),
            [RemoteReport::Refused {
                error: RemoteCatalogError::DigestMismatch { .. },
                ..
            }]
        ),
        "the refusal names the digest: {reports:?}"
    );
    assert_eq!(
        answer(&mut search, "fire"),
        vec!["Fire Atlas".to_owned()],
        "the last good document keeps serving"
    );
    assert!(
        answer(&mut search, "water").is_empty(),
        "the refused revision contributed nothing"
    );
    let status = service.status().remove(0);
    assert!(
        status.last_error.is_some_and(|error| error.contains("sha256")),
        "the failure is retained for diagnostics rather than swallowed"
    );
}

#[test]
fn an_unreachable_endpoint_leaves_the_catalog_intact_and_reports_the_failure() {
    let root = TempRoot::new("unreachable");
    publish(&root, 1, vec![item("atlas", "Fire Atlas")]);
    let mut search = ready_search();
    let mut service = RemoteCatalogService::new(
        vec![source(&root)],
        Arc::new(DefaultCatalogFetcher),
        Arc::new(TrustStore::empty()),
    );
    settle(&mut service, &mut search);

    fs::remove_file(root.path().join(MANIFEST_NAME)).expect("remove the manifest");
    service.request_refresh();
    let reports = settle(&mut service, &mut search);
    assert!(
        matches!(
            reports.as_slice(),
            [RemoteReport::Refused {
                error: RemoteCatalogError::Unreachable { .. },
                ..
            }]
        ),
        "an endpoint that is not there is reported, not swallowed: {reports:?}"
    );
    assert_eq!(
        answer(&mut search, "fire"),
        vec!["Fire Atlas".to_owned()],
        "an unreachable source costs a refresh, never the retained slice"
    );
}

#[test]
fn an_item_with_no_visible_label_is_refused_and_the_previous_document_keeps_serving() {
    let root = TempRoot::new("refused-item");
    publish(&root, 1, vec![item("atlas", "Fire Atlas")]);
    let mut search = ready_search();
    let mut service = RemoteCatalogService::new(
        vec![source(&root)],
        Arc::new(DefaultCatalogFetcher),
        Arc::new(TrustStore::empty()),
    );
    settle(&mut service, &mut search);

    // An item with no label is a row a user could not read, so it is refused
    // before the catalog is touched.
    let mut unreadable = item("blank", "Fire Atlas");
    unreadable.label = "   ".to_owned();
    publish(&root, 2, vec![unreadable]);
    service.request_refresh();
    let reports = settle(&mut service, &mut search);
    assert!(
        matches!(
            reports.as_slice(),
            [RemoteReport::Refused {
                error: RemoteCatalogError::Item { .. },
                ..
            }]
        ),
        "the refusal names the item: {reports:?}"
    );
    assert_eq!(answer(&mut search, "fire"), vec!["Fire Atlas".to_owned()]);
}

// ---------------------------------------------------------------------------
// bounds
// ---------------------------------------------------------------------------

#[test]
fn an_oversized_document_is_refused_on_the_manifests_word_before_it_is_read() {
    let root = TempRoot::new("oversized");
    publish(&root, 1, vec![item("atlas", "Fire Atlas")]);
    let declared = 64 * 1024 * 1024;
    write_manifest(&root, declared, &"a".repeat(64), None);
    let mut source = source(&root);
    source.max_bytes = 4096;
    let fetcher = RecordingFetcher::default();

    let error = fetch_source(&source, &fetcher, &TrustStore::empty())
        .expect_err("a document larger than the ceiling is not fetched");
    assert!(
        matches!(
            error,
            RemoteCatalogError::Oversized {
                declared: 67_108_864,
                limit: 4096,
                ..
            }
        ),
        "the refusal names both numbers: {error}"
    );
    let calls = fetcher.calls();
    assert_eq!(
        calls.len(),
        1,
        "only the manifest was read; the document was never requested: {calls:?}"
    );
    assert_eq!(
        calls[0].1,
        crikey_app::MAX_MANIFEST_BYTES,
        "even the manifest read is capped"
    );
}

#[test]
fn a_document_longer_than_its_manifest_declares_is_refused_before_it_is_decoded() {
    let root = TempRoot::new("too-long");
    let bytes = document(1, vec![item("atlas", "Fire Atlas")]);
    let mut padded = bytes.clone();
    padded.extend_from_slice(&[0u8; 4096]);
    fs::write(root.path().join(SLICE_NAME), &padded).expect("write an over-long document");
    write_manifest(&root, bytes.len() as u64, &sha256_hex(&bytes), None);

    let error = fetch_source(&source(&root), &DefaultCatalogFetcher, &TrustStore::empty())
        .expect_err("a document that is not the length its manifest declares is not that document");
    let RemoteCatalogError::Oversized { declared, limit, .. } = error else {
        panic!("the bounded fetcher must refuse before decoding: {error:?}");
    };
    assert_eq!(declared, padded.len() as u64, "the file size is reported");
    assert_eq!(
        limit,
        bytes.len() as u64,
        "the read is bounded to the manifest's declared document length"
    );
}

#[test]
fn a_manifest_larger_than_the_manifest_ceiling_is_refused() {
    let root = TempRoot::new("fat-manifest");
    let filler = "#".repeat(usize::try_from(crikey_app::MAX_MANIFEST_BYTES).expect("the cap fits") + 1);
    fs::write(root.path().join(MANIFEST_NAME), filler).expect("write an oversized manifest");

    let error = fetch_source(&source(&root), &DefaultCatalogFetcher, &TrustStore::empty())
        .expect_err("a manifest is five short lines");
    assert!(
        matches!(error, RemoteCatalogError::Oversized { .. }),
        "the manifest ceiling applies to the manifest too: {error}"
    );
}

#[test]
fn bytes_that_are_not_a_slice_document_are_refused_rather_than_partly_admitted() {
    let root = TempRoot::new("garbage");
    let garbage = vec![0x5au8; 512];
    fs::write(root.path().join(SLICE_NAME), &garbage).expect("write a hostile document");
    write_manifest(&root, garbage.len() as u64, &sha256_hex(&garbage), None);

    let error = fetch_source(&source(&root), &DefaultCatalogFetcher, &TrustStore::empty())
        .expect_err("a digest proves integrity, never format");
    assert!(
        matches!(error, RemoteCatalogError::Document { .. }),
        "the bounded decoder refuses it: {error}"
    );
}

// ---------------------------------------------------------------------------
// scheduling
// ---------------------------------------------------------------------------

#[test]
fn a_burst_of_refresh_triggers_causes_one_fetch() {
    let root = TempRoot::new("burst");
    publish(&root, 1, vec![item("atlas", "Fire Atlas")]);
    let fetcher = Arc::new(RecordingFetcher::default());
    let mut search = ready_search();
    let mut service = RemoteCatalogService::new(
        vec![source(&root)],
        Arc::clone(&fetcher) as Arc<dyn CatalogFetcher>,
        Arc::new(TrustStore::empty()),
    );

    for _ in 0..16 {
        service.request_refresh();
    }
    settle(&mut service, &mut search);

    let manifest_url = root.url(MANIFEST_NAME);
    let manifest_reads = fetcher
        .calls()
        .into_iter()
        .filter(|(url, _)| url == &manifest_url)
        .count();
    assert_eq!(
        manifest_reads, 1,
        "sixteen triggers and one construction coalesced into a single fetch"
    );

    // And a second burst is one more fetch, not sixteen: coalescing collapses a
    // burst, it does not disable refreshing.
    for _ in 0..16 {
        service.request_refresh();
    }
    settle(&mut service, &mut search);
    let manifest_reads = fetcher
        .calls()
        .into_iter()
        .filter(|(url, _)| url == &manifest_url)
        .count();
    assert_eq!(manifest_reads, 2);
}

#[test]
fn a_source_with_no_interval_never_refreshes_on_its_own() {
    let root = TempRoot::new("manual-only");
    publish(&root, 1, vec![item("atlas", "Fire Atlas")]);
    let fetcher = Arc::new(RecordingFetcher::default());
    let mut search = ready_search();
    let mut service = RemoteCatalogService::new(
        vec![source(&root)],
        Arc::clone(&fetcher) as Arc<dyn CatalogFetcher>,
        Arc::new(TrustStore::empty()),
    );
    settle(&mut service, &mut search);
    let after_first = fetcher.calls().len();

    // A day of polling with no trigger and no interval.
    for tick in 0..1_000 {
        assert_eq!(
            service.poll(tick * 86_400),
            0,
            "an interval of zero means refresh only when asked"
        );
    }
    assert_eq!(fetcher.calls().len(), after_first);
}

#[test]
fn an_interval_comes_due_and_a_source_mid_refresh_is_not_started_twice() {
    let root = TempRoot::new("interval");
    publish(&root, 1, vec![item("atlas", "Fire Atlas")]);
    let fetcher = Arc::new(RecordingFetcher::default());
    let mut search = ready_search();
    let mut source = source(&root);
    source.interval_ms = 500;
    let mut service = RemoteCatalogService::new(
        vec![source],
        Arc::clone(&fetcher) as Arc<dyn CatalogFetcher>,
        Arc::new(TrustStore::empty()),
    );

    // The first refresh is the one every source is owed at construction; polling
    // repeatedly while it is in flight must not start a second.
    assert_eq!(service.poll(0), 1);
    assert_eq!(service.poll(0), 0, "a refresh in flight is not started again");
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    while service.apply(&mut search, 0).is_empty() {
        assert!(Instant::now() < deadline, "the first refresh never finished");
        thread::sleep(Duration::from_millis(2));
    }

    assert_eq!(service.poll(499), 0, "the interval has not elapsed");
    assert_eq!(service.poll(500), 1, "the interval came due");
}

#[test]
fn no_fetch_happens_on_the_query_path() {
    let root = TempRoot::new("query-path");
    publish(&root, 1, vec![item("atlas", "Fire Atlas")]);
    let fetcher = Arc::new(RecordingFetcher::default());
    let mut search = ready_search();
    let mut service = RemoteCatalogService::new(
        vec![source(&root)],
        Arc::clone(&fetcher) as Arc<dyn CatalogFetcher>,
        Arc::new(TrustStore::empty()),
    );
    settle(&mut service, &mut search);
    let after_refresh = fetcher.calls().len();
    assert!(after_refresh > 0, "the fixture was fetched at least once");

    for query in ["f", "fi", "fir", "fire", "fire a", "atlas", "water", ""] {
        for _ in 0..8 {
            let _ = answer(&mut search, query);
        }
    }
    assert_eq!(
        fetcher.calls().len(),
        after_refresh,
        "a query must never reach the network (README invariant 2)"
    );
}

#[test]
fn a_launcher_with_no_configured_source_does_nothing_at_all() {
    let fetcher = Arc::new(RecordingFetcher::default());
    let mut search = ready_search();
    let mut service = RemoteCatalogService::new(
        Vec::new(),
        Arc::clone(&fetcher) as Arc<dyn CatalogFetcher>,
        Arc::new(TrustStore::empty()),
    );

    assert!(service.is_idle(), "no source is the default state");
    service.request_refresh();
    assert_eq!(service.poll(0), 0);
    assert!(service.apply(&mut search, 0).is_empty());
    assert!(service.status().is_empty());
    assert!(fetcher.calls().is_empty());
}

#[test]
fn a_later_revision_replaces_an_earlier_one_and_an_earlier_one_is_refused() {
    let root = TempRoot::new("revisions");
    publish(&root, 4, vec![item("atlas", "Fire Atlas")]);
    let mut search = ready_search();
    let mut service = RemoteCatalogService::new(
        vec![source(&root)],
        Arc::new(DefaultCatalogFetcher),
        Arc::new(TrustStore::empty()),
    );
    settle(&mut service, &mut search);
    assert_eq!(answer(&mut search, "fire"), vec!["Fire Atlas".to_owned()]);

    publish(&root, 5, vec![item("clock", "Water Clock")]);
    service.request_refresh();
    settle(&mut service, &mut search);
    assert_eq!(answer(&mut search, "water"), vec!["Water Clock".to_owned()]);
    assert!(
        answer(&mut search, "fire").is_empty(),
        "a replacement replaces the whole slice"
    );

    // Rolling the index back to a revision the launcher has already superseded
    // is refused: the catalog's instance high-water mark never falls.
    publish(&root, 3, vec![item("atlas", "Fire Atlas")]);
    service.request_refresh();
    let reports = settle(&mut service, &mut search);
    assert!(
        matches!(
            reports.as_slice(),
            [RemoteReport::Refused {
                error: RemoteCatalogError::Refused { .. },
                ..
            }]
        ),
        "an older revision is a stale publisher: {reports:?}"
    );
    assert_eq!(answer(&mut search, "water"), vec!["Water Clock".to_owned()]);
}
