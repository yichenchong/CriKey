//! Black-box tests for `crikey catalog` (spec 2.2, 28; ADR-0016).
//!
//! Every invocation runs inside a private CriKey config root, so the sources
//! under test are the ones the test wrote and never whatever the developer's
//! machine happens to declare. No test reaches the network: the one source that
//! is actually fetched is served from a `file://` URL inside the same scratch
//! directory, which is the mounted-share half of the feature rather than a shim.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// The binary under test, as built by cargo for this integration target.
const CRIKEY: &str = env!("CARGO_BIN_EXE_crikey");

/// A completed operation that found nothing wrong.
const EX_OK: i32 = 0;
/// A completed operation that refused a source.
const EX_INVALID: i32 = 1;
/// A usage error.
const EX_USAGE: i32 = 64;
/// The Rust runtime's status for an unwound panic.
const PANIC_STATUS: i32 = 101;

/// One completed invocation, retained so assertion failures show all output.
#[derive(Debug)]
struct Run {
    args: Vec<String>,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl fmt::Display for Run {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "\n  command: crikey {args}\n  exit:    {code:?}\n  stdout:\n{stdout}\n  stderr:\n{stderr}",
            args = self.args.join(" "),
            code = self.code,
            stdout = indent(&self.stdout),
            stderr = indent(&self.stderr),
        )
    }
}

fn indent(text: &str) -> String {
    if text.trim().is_empty() {
        return "    <empty>".to_owned();
    }
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A private CriKey tree, removed when the test ends.
#[derive(Debug)]
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "crikey-catalog-commands-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("config")).expect("create the scratch config root");
        fs::create_dir_all(root.join("index")).expect("create the scratch index root");
        Self { root }
    }

    fn config(&self, text: &str) {
        fs::write(self.root.join("config").join("config.toml"), text).expect("write config.toml");
    }

    fn index(&self) -> PathBuf {
        self.root.join("index")
    }

    /// Runs `crikey` with every CriKey directory pinned inside this tree.
    fn run(&self, args: &[&str]) -> Run {
        let owned: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let output = Command::new(CRIKEY)
            .args(&owned)
            .env("HOME", &self.root)
            .env("CRIKEY_CONFIG_DIR", self.root.join("config"))
            .env("CRIKEY_DATA_DIR", self.root.join("data"))
            .env("CRIKEY_CACHE_DIR", self.root.join("cache"))
            .env("CRIKEY_STATE_DIR", self.root.join("state"))
            .env("CRIKEY_CATALOG_CACHE_ROOT", self.root.join("catalog-cache"))
            .output()
            .unwrap_or_else(|error| panic!("could not execute `{CRIKEY}`: {error}"));
        Run {
            args: owned.clone(),
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The `file://` URL naming `path`, in the one spelling the fetcher accepts.
///
/// A Windows path is not a URL path: it has backslashes and starts with a drive
/// letter rather than a slash, so the empty-authority form has to be spelled
/// `file:///C:/dir/file`. Building the URL by pasting `Path::display()` after
/// `file://` produces `file://C:\dir\file`, which names host `C:` and is
/// rightly refused.
fn file_url(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{text}")
    }
}

/// Publishes a slice document and a truthful manifest into `scratch`'s index.
///
/// The document is produced by the same encoder the launcher's own cache uses,
/// which is the whole point of the format being one format.
fn publish(scratch: &Scratch) -> PathBuf {
    use crikey_catalog::{encode_slice_document, CachedSlice};
    use crikey_core::{ArgumentPolicy, Category, Generation, HitPolicy, Item, ItemId, PluginId};
    use sha2::{Digest, Sha256};

    let owner = PluginId("team.shared-index".to_owned());
    let bytes = encode_slice_document(&CachedSlice {
        plugin: owner.clone(),
        instance: 1,
        generation: Generation::ZERO,
        items: vec![Item {
            stable_id: ItemId("atlas".to_owned()),
            plugin_id: owner,
            category: Category::Application,
            label: "Fire Atlas".to_owned(),
            description: String::new(),
            target: "app://atlas".to_owned(),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: ArgumentPolicy::Forbidden,
            hit_policy: HitPolicy::Recorded,
            score_hint: 0,
            metadata: std::collections::BTreeMap::new(),
            actions: Vec::new(),
        }],
    })
    .expect("the fixture slice encodes");

    fs::write(scratch.index().join("catalog.slice"), &bytes).expect("write the slice document");
    let digest = Sha256::digest(&bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    let manifest = scratch.index().join("index.txt");
    fs::write(
        &manifest,
        format!(
            "crikey-remote-catalog 1\nslice catalog.slice\nbytes {}\nsha256 {hex}\n",
            bytes.len()
        ),
    )
    .expect("write the manifest");
    manifest
}

#[test]
fn catalog_without_a_subcommand_is_a_usage_error() {
    let scratch = Scratch::new("no-subcommand");
    let run = scratch.run(&["catalog"]);
    assert_eq!(run.code, Some(EX_USAGE), "{run}");
    assert!(
        run.stderr.contains("sources") && run.stderr.contains("refresh"),
        "the refusal lists the subcommands{run}"
    );
}

#[test]
fn catalog_help_lists_both_subcommands_and_succeeds() {
    let scratch = Scratch::new("help");
    let run = scratch.run(&["catalog", "--help"]);
    assert_eq!(run.code, Some(EX_OK), "{run}");
    assert!(run.stdout.contains("sources"), "{run}");
    assert!(run.stdout.contains("refresh"), "{run}");
}

#[test]
fn an_unknown_catalog_subcommand_is_a_usage_error_and_never_a_panic() {
    let scratch = Scratch::new("unknown");
    let run = scratch.run(&["catalog", "publish"]);
    assert_eq!(run.code, Some(EX_USAGE), "{run}");
    assert_ne!(run.code, Some(PANIC_STATUS), "{run}");
}

#[test]
fn a_tree_with_no_configured_source_reports_none_and_succeeds() {
    let scratch = Scratch::new("empty");
    let sources = scratch.run(&["catalog", "sources"]);
    assert_eq!(sources.code, Some(EX_OK), "{sources}");
    assert!(
        sources.stdout.contains("sources=0"),
        "no source is the default state{sources}"
    );

    let refresh = scratch.run(&["catalog", "refresh"]);
    assert_eq!(
        refresh.code,
        Some(EX_OK),
        "nothing configured is not a failure{refresh}"
    );
    assert!(refresh.stdout.contains("refused=0"), "{refresh}");
}

#[test]
fn a_configured_source_is_listed_with_the_owner_it_publishes_as() {
    let scratch = Scratch::new("list");
    let manifest = scratch.index().join("index.txt");
    scratch.config(&format!(
        "[catalog.remote.team]\nurl = \"{}\"\ninterval-ms = 900000\n",
        file_url(&manifest)
    ));

    let run = scratch.run(&["catalog", "sources"]);
    assert_eq!(run.code, Some(EX_OK), "{run}");
    assert!(run.stdout.contains("source=team"), "{run}");
    assert!(
        run.stdout.contains("owner=remote.team"),
        "the owner id is derived from the source name{run}"
    );
    assert!(run.stdout.contains("interval-ms=900000"), "{run}");
    assert!(run.stdout.contains("sources=1"), "{run}");
}

#[test]
fn refreshing_a_valid_source_caches_it_and_reports_the_publisher() {
    let scratch = Scratch::new("refresh-ok");
    let manifest = publish(&scratch);
    scratch.config(&format!(
        "[catalog.remote.team]\nurl = \"{}\"\n",
        file_url(&manifest)
    ));

    let run = scratch.run(&["catalog", "refresh"]);
    assert_eq!(run.code, Some(EX_OK), "{run}");
    assert!(run.stdout.contains("refreshed=1 refused=0"), "{run}");
    assert!(
        run.stdout.contains("owner=remote.team"),
        "the slice is filed under the local owner{run}"
    );
    assert!(
        run.stdout.contains("published-by=team.shared-index"),
        "the publisher's own id is reported{run}"
    );
    assert!(
        run.stdout.contains("note=cached-for-next-start"),
        "the command says plainly that a running launcher is unaffected{run}"
    );

    let cached = scratch.root.join("catalog-cache").join("remote.team.slice");
    assert!(
        cached.is_file(),
        "the verified slice was written into the catalog cache: {}",
        cached.display()
    );
}

#[test]
fn a_digest_mismatch_is_refused_by_name_with_a_non_zero_exit() {
    let scratch = Scratch::new("refresh-digest");
    let manifest = publish(&scratch);
    let bytes = fs::read(scratch.index().join("catalog.slice")).expect("read the fixture document");
    fs::write(
        &manifest,
        format!(
            "crikey-remote-catalog 1\nslice catalog.slice\nbytes {}\nsha256 {}\n",
            bytes.len(),
            "0".repeat(64)
        ),
    )
    .expect("rewrite the manifest with a wrong digest");
    scratch.config(&format!(
        "[catalog.remote.team]\nurl = \"{}\"\n",
        file_url(&manifest)
    ));

    let run = scratch.run(&["catalog", "refresh"]);
    assert_eq!(run.code, Some(EX_INVALID), "{run}");
    assert!(run.stdout.contains("refreshed=0 refused=1"), "{run}");
    assert!(
        run.stderr.contains("catalog.slice") && run.stderr.contains("sha256"),
        "the refusal names the artefact and the digest{run}"
    );
    assert!(
        !scratch
            .root
            .join("catalog-cache")
            .join("remote.team.slice")
            .exists(),
        "a refused document is never cached"
    );
}

#[test]
fn an_unreachable_source_is_refused_rather_than_reported_as_refreshed() {
    let scratch = Scratch::new("refresh-missing");
    scratch.config(&format!(
        "[catalog.remote.team]\nurl = \"{}\"\n",
        file_url(&scratch.index().join("absent.txt"))
    ));

    let run = scratch.run(&["catalog", "refresh"]);
    assert_eq!(run.code, Some(EX_INVALID), "{run}");
    assert!(run.stdout.contains("refreshed=0 refused=1"), "{run}");
    assert!(run.stderr.contains("absent.txt"), "{run}");
}

#[test]
fn refreshing_a_source_that_is_not_declared_names_it() {
    let scratch = Scratch::new("refresh-unknown");
    let run = scratch.run(&["catalog", "refresh", "nowhere"]);
    assert_eq!(run.code, Some(EX_INVALID), "{run}");
    assert!(run.stderr.contains("nowhere"), "{run}");
}

#[test]
fn a_malformed_declaration_is_reported_by_key() {
    let scratch = Scratch::new("bad-config");
    scratch.config("[catalog.remote.team]\ninterval-ms = 1000\n");

    let run = scratch.run(&["catalog", "sources"]);
    assert_eq!(run.code, Some(EX_INVALID), "{run}");
    assert!(
        run.stderr.contains("catalog.remote.team.url"),
        "the message names the key to fix{run}"
    );
}
