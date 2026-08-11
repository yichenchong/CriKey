//! Resolution of the icon references plugins put on their own items (spec 11.7).
//!
//! A catalog item's reference is a *platform* reference and the platform's icon
//! provider resolves it in `SearchService`. A plugin's reference is not: it
//! names a file inside the plugin's own package, or -- for a native plugin -- a
//! resource only the plugin process can produce. Resolving either against the
//! desktop's icon themes would find nothing or, worse, an unrelated icon of the
//! same name.
//!
//! Both plugin paths end at the same [`decode_icon`] seam and hand back the
//! same [`IconImage`] the catalog path does, so the renderer never learns where
//! the pixels came from and a row never carries two icon representations.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use crikey_core::PluginId;
use crikey_platform::{decode_icon, IconImage, DEFAULT_ICON_SIZE};
use crikey_plugin_model::{FilesystemScope, Permissions};

/// The most an icon a plugin supplies may weigh, encoded.
///
/// Deliberately far below the platform loader's four mebibytes. A package icon
/// is shipped art, not a photograph, and on the native path this is also the
/// number of bytes a plugin can make the host hold for one reference -- a
/// ceiling that has to be small enough that a hostile plugin cannot spend the
/// launcher's memory one icon at a time.
pub const MAX_PLUGIN_ICON_BYTES: usize = 256 * 1024;

/// How long a native plugin has to serve one icon before the host gives up.
///
/// The host abandons the answer rather than the plugin: a resource that misses
/// this bound costs the item its picture and nothing else.
pub const PLUGIN_ICON_DEADLINE: Duration = Duration::from_millis(250);

/// Resolved plugin icons retained before the memo is dropped whole.
///
/// Mirrors the catalog memo in `SearchService`: a decoded 48x48 icon is 9 KiB,
/// so this is a few megabytes at worst, and both hits and misses are recorded
/// because a reference that resolves to nothing is exactly the one that would
/// otherwise be re-read on every keystroke.
const MAX_RESOLVED_ICONS: usize = 512;

/// Native icon fetches allowed to be outstanding at once.
///
/// A fetch occupies one thread and one plugin's worker for up to
/// [`PLUGIN_ICON_DEADLINE`], so an unbounded fan-out would let a screenful of
/// rows from a slow plugin spawn a screenful of blocked threads.
const MAX_CONCURRENT_FETCHES: usize = 4;

/// Serves the bytes behind one reference for one plugin.
///
/// Implemented by the native provider, which asks the owning worker over the
/// protocol. Modern plugins need no implementation: their packages are on disk
/// and the host reads them directly.
pub trait PluginResourceSource: fmt::Debug + Send + Sync {
    /// `None` for every way a plugin can decline -- it has no such resource, it
    /// refused, it served more than the host accepts, or it stayed silent past
    /// the deadline.
    fn fetch(&self, reference: &str) -> Option<Vec<u8>>;
}

/// Where the bytes behind one plugin's icon references come from.
#[derive(Debug)]
enum IconOrigin {
    /// A file inside the plugin's own package directory.
    Package(PathBuf),
    /// A package read the manifest refused.
    ///
    /// Kept as an origin rather than dropped so the refusal is attributable:
    /// a plugin with no origin at all is one that never loaded, which is a
    /// different diagnosis from one whose author declared it needs no
    /// filesystem access and is being held to it.
    PackageRefused,
    /// Bytes only the plugin process itself can produce.
    Served(Arc<dyn PluginResourceSource>),
}

type ResolvedIcon = Option<Arc<IconImage>>;
type ResolvedIcons = HashMap<String, HashMap<String, ResolvedIcon>>;

/// Per-plugin icon resolution with a session memo.
///
/// Built once per provider, after discovery, and shared immutably: a plugin
/// that failed to load has no origin here and its references resolve to
/// nothing, which is the correct answer rather than a special case.
#[derive(Debug, Default)]
pub struct PluginIconResolver {
    origins: HashMap<String, IconOrigin>,
    /// Keyed plugin-first so a lookup costs no allocation on the hot path,
    /// where both halves of the key are already borrowed from a row.
    resolved: Mutex<ResolvedIcons>,
    inflight: Mutex<HashSet<(String, String)>>,
}

impl PluginIconResolver {
    /// Records that `plugin` keeps its icons in `directory`.
    ///
    /// This read is host-mediated: the launcher opens a file inside another
    /// program's package on that program's behalf, so it is subject to the
    /// plugin's own filesystem declaration and not only to the bound and
    /// escape check below. The package is the implicit
    /// [`FilesystemScope::Package`] grant, so a manifest that declares nothing
    /// keeps its icons; a manifest that declares it needs no filesystem access
    /// is honoured and gets none.
    pub fn insert_package(&mut self, plugin: &PluginId, directory: PathBuf, permissions: &Permissions) {
        let origin = if permissions.allows_filesystem_read(FilesystemScope::Package) {
            IconOrigin::Package(directory)
        } else {
            IconOrigin::PackageRefused
        };
        self.origins.insert(plugin.0.clone(), origin);
    }

    /// Whether `plugin`'s manifest refused the host-mediated package read.
    ///
    /// Icon resolution answers in [`Option`] and has no error channel, so this
    /// is how a caller tells "the author declared no filesystem access" apart
    /// from "the file is missing" without inferring it from a blank row.
    pub fn package_reads_refused(&self, plugin: &PluginId) -> bool {
        matches!(self.origins.get(&plugin.0), Some(IconOrigin::PackageRefused))
    }

    /// Records that `plugin` must be asked for its icons.
    pub fn insert_served(&mut self, plugin: &PluginId, source: Arc<dyn PluginResourceSource>) {
        self.origins.insert(plugin.0.clone(), IconOrigin::Served(source));
    }

    /// The pixels behind one plugin's icon reference, when they are in hand.
    ///
    /// `None` is not final for a served origin: it means the answer has been
    /// asked for and has not arrived. The caller publishes the row without an
    /// icon and asks again on the next frame, because a frame that is otherwise
    /// ready must not wait on a picture.
    pub fn resolve(self: &Arc<Self>, plugin: &str, reference: &str) -> Option<Arc<IconImage>> {
        if let Some(memo) = lock(&self.resolved)
            .get(plugin)
            .and_then(|references| references.get(reference))
        {
            return memo.clone();
        }
        match self.origins.get(plugin)? {
            IconOrigin::Package(directory) => {
                let image = package_icon(directory, reference);
                self.store(plugin, reference, image.clone());
                image
            }
            // Memoized so a refusal costs one lookup per reference rather than
            // a branch on every frame, exactly like a missing file.
            IconOrigin::PackageRefused => {
                self.store(plugin, reference, None);
                None
            }
            IconOrigin::Served(source) => {
                self.request(plugin, reference, Arc::clone(source));
                None
            }
        }
    }

    /// Starts at most one background fetch per reference.
    ///
    /// The answer travels back through the memo rather than to this caller. A
    /// plugin has a whole [`PLUGIN_ICON_DEADLINE`] to answer and resolution
    /// runs on the thread that assembles frames, so waiting here would hold a
    /// finished frame hostage to decoration.
    fn request(self: &Arc<Self>, plugin: &str, reference: &str, source: Arc<dyn PluginResourceSource>) {
        let key = (plugin.to_owned(), reference.to_owned());
        {
            let mut inflight = lock(&self.inflight);
            // Re-read the memo under this lock. `resolve` looked before it got
            // here, and a fetch that finished in between has already stored its
            // answer and given its slot back — so without this check the same
            // reference is fetched twice: once by the worker that just
            // finished, once by a caller holding a stale miss. The worker
            // writes the memo before it clears the slot, so holding the slot
            // lock while reading the memo cannot see a gap between them, and
            // the two locks are never held in the other order.
            if lock(&self.resolved)
                .get(plugin)
                .is_some_and(|references| references.contains_key(reference))
            {
                return;
            }
            if inflight.len() >= MAX_CONCURRENT_FETCHES || inflight.contains(&key) {
                return;
            }
            inflight.insert(key.clone());
        }
        let resolver = Arc::clone(self);
        let spawned = thread::Builder::new()
            .name("crikey-plugin-icon".to_owned())
            .spawn(move || {
                let image = source.fetch(&key.1).and_then(|bytes| decode(&key.1, &bytes));
                resolver.store(&key.0, &key.1, image);
                lock(&resolver.inflight).remove(&key);
            });
        if let Err(error) = spawned {
            // Nothing will clear the slot if the thread never existed.
            let _ = error;
            lock(&self.inflight).remove(&(plugin.to_owned(), reference.to_owned()));
        }
    }

    fn store(&self, plugin: &str, reference: &str, image: Option<Arc<IconImage>>) {
        let mut resolved = lock(&self.resolved);
        if resolved.values().map(HashMap::len).sum::<usize>() >= MAX_RESOLVED_ICONS {
            resolved.clear();
        }
        resolved
            .entry(plugin.to_owned())
            .or_default()
            .insert(reference.to_owned(), image);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn decode(reference: &str, bytes: &[u8]) -> Option<Arc<IconImage>> {
    decode_icon(reference, bytes, DEFAULT_ICON_SIZE)
        .ok()
        .map(Arc::new)
}

/// Reads and decodes one icon shipped inside a plugin package.
fn package_icon(directory: &Path, reference: &str) -> Option<Arc<IconImage>> {
    let relative = Path::new(reference);
    if reference.is_empty() || escapes_package(relative) {
        return None;
    }
    let bytes = read_capped(&directory.join(relative), MAX_PLUGIN_ICON_BYTES)?;
    decode(reference, &bytes)
}

/// Whether `path` is anything other than a plain descent into the package.
///
/// Stricter than the SDK packaging validator's `has_parent_component`, and on
/// purpose: that one checks a path an author wrote in their own manifest, this
/// one checks a string an installed plugin emits at runtime. `ParentDir` covers
/// the `..` escapes; `RootDir` and `Prefix` cover every absolute form,
/// including the Windows `C:relative` shape that `Path::is_absolute` reports as
/// relative. Without both, a plugin could name any file the user can read and
/// have the launcher open it.
fn escapes_package(path: &Path) -> bool {
    path.components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
}

/// Reads at most `cap` bytes, refusing rather than truncating.
fn read_capped(path: &Path, cap: usize) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > cap as u64 {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    // One byte past the cap: the metadata length is a hint taken before the
    // read, and a file that grew in between must still be refused.
    file.by_ref().take(cap as u64 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= cap).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const SVG: &[u8] =
        br##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="#112233"/></svg>"##;

    fn scratch() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "crikey-plugin-icons-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("scratch package is creatable");
        path
    }

    #[test]
    fn package_icons_are_relative_bounded_and_memoized() {
        let directory = scratch();
        let icon_path = directory.join("icons/item.svg");
        std::fs::create_dir_all(icon_path.parent().expect("icon parent")).expect("icon directory");
        std::fs::write(&icon_path, SVG).expect("icon is writable");

        let plugin = PluginId("modern.example".to_owned());
        let resolver = Arc::new({
            let mut resolver = PluginIconResolver::default();
            resolver.insert_package(&plugin, directory.clone(), &Permissions::default());
            resolver
        });

        let first = resolver
            .resolve(&plugin.0, "icons/item.svg")
            .expect("package-relative SVG resolves");
        let second = resolver
            .resolve(&plugin.0, "icons/item.svg")
            .expect("the memoized package-relative SVG resolves");
        assert!(
            Arc::ptr_eq(&first, &second),
            "repeated references share memoized pixels"
        );
        assert!(
            resolver.resolve(&plugin.0, "./icons/item.svg").is_some(),
            "curdir-relative package paths remain within the package"
        );
        assert_eq!((first.width(), first.height()), (48, 48));
        assert_eq!(&first.rgba()[..4], &[0x11, 0x22, 0x33, 0xff]);

        let outside = directory.parent().expect("scratch parent").join("outside.svg");
        std::fs::write(&outside, SVG).expect("outside fixture is writable");
        assert!(
            resolver.resolve(&plugin.0, "../outside.svg").is_none(),
            "a plugin result cannot escape its package"
        );
        assert!(
            resolver.resolve(&plugin.0, &outside.to_string_lossy()).is_none(),
            "absolute references are not package-relative"
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    /// The package read is host-mediated, so an author who declares that the
    /// plugin needs no filesystem access must be held to it — and must be
    /// distinguishable from a plugin that never loaded, which also has no
    /// icons. A regression here is silent: the rows still render, just without
    /// pictures, so nothing but a test notices which way this went.
    #[test]
    fn a_plugin_that_declared_no_filesystem_scope_is_refused_its_package_icons() {
        use crikey_plugin_model::{FilesystemAccess, FilesystemPermission};

        let directory = scratch();
        let icon_path = directory.join("item.svg");
        std::fs::write(&icon_path, SVG).expect("icon is writable");

        let refused = PluginId("modern.renouncing".to_owned());
        let permitted = PluginId("modern.declaring".to_owned());
        let never_loaded = PluginId("modern.absent".to_owned());
        let resolver = Arc::new({
            let mut resolver = PluginIconResolver::default();
            resolver.insert_package(
                &refused,
                directory.clone(),
                &Permissions {
                    filesystem: vec![FilesystemPermission {
                        scope: FilesystemScope::None,
                        access: FilesystemAccess::Read,
                    }],
                    ..Permissions::default()
                },
            );
            resolver.insert_package(
                &permitted,
                directory.clone(),
                &Permissions {
                    filesystem: vec![FilesystemPermission {
                        scope: FilesystemScope::Package,
                        access: FilesystemAccess::Read,
                    }],
                    ..Permissions::default()
                },
            );
            resolver
        });

        assert!(
            resolver.resolve(&refused.0, "item.svg").is_none(),
            "a declared `none` filesystem scope must refuse the host-mediated package read"
        );
        assert!(
            resolver.resolve(&permitted.0, "item.svg").is_some(),
            "an explicit package scope keeps the icon"
        );
        // The refusal is attributable, and only to the owner that earned it.
        assert!(resolver.package_reads_refused(&refused));
        assert!(!resolver.package_reads_refused(&permitted));
        assert!(!resolver.package_reads_refused(&never_loaded));

        let _ = std::fs::remove_dir_all(directory);
    }

    /// A served origin that answers nothing, the way a plugin whose resource
    /// deadline the host abandoned looks from this side of the seam.
    #[derive(Debug, Default)]
    struct SilentSource {
        fetches: AtomicU64,
    }

    impl PluginResourceSource for SilentSource {
        fn fetch(&self, _reference: &str) -> Option<Vec<u8>> {
            self.fetches.fetch_add(1, Ordering::Relaxed);
            // Long enough that the concurrency bound is genuinely reached
            // while several references are outstanding at once, which is the
            // only state in which a leaked slot is distinguishable from a
            // released one.
            std::thread::sleep(Duration::from_millis(20));
            None
        }
    }

    /// A fetch that answers nothing must give its concurrency slot back.
    ///
    /// The failure this defends against is silent and permanent: keep the slot
    /// and, after [`MAX_CONCURRENT_FETCHES`] silent references, no plugin ever
    /// gets another icon for the rest of the session. Asking for more
    /// references than there are slots is what makes that observable.
    #[test]
    fn a_served_origin_that_answers_nothing_releases_its_fetch_slot() {
        let plugin = PluginId("native.silent".to_owned());
        let source = Arc::new(SilentSource::default());
        let resolver = Arc::new({
            let mut resolver = PluginIconResolver::default();
            resolver.insert_served(&plugin, Arc::clone(&source) as Arc<dyn PluginResourceSource>);
            resolver
        });

        let references: Vec<String> = (0..MAX_CONCURRENT_FETCHES + 3)
            .map(|index| format!("silent-{index}.svg"))
            .collect();
        // `resolve` reports "not in hand yet" the same way it reports "there is
        // no icon", so settlement is read from the memo, not the answer.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            for reference in &references {
                let _ = resolver.resolve(&plugin.0, reference);
            }
            let settled = lock(&resolver.resolved).get(&plugin.0).map_or(0, HashMap::len);
            if settled == references.len() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        let settled = lock(&resolver.resolved).get(&plugin.0).map_or(0, HashMap::len);
        assert_eq!(
            settled,
            references.len(),
            "every silent reference settles, so no fetch slot was kept"
        );
        assert!(
            lock(&resolver.inflight).is_empty(),
            "no outstanding request survives its own fetch"
        );
        assert_eq!(
            source.fetches.load(Ordering::Relaxed) as usize,
            references.len(),
            "a settled reference is asked for exactly once"
        );
    }
}
