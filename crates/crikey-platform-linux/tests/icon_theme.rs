//! XDG icon theme lookup (spec 18.1, 18.6).
//!
//! Every case supplies its own theme roots, so nothing here depends on which
//! icon themes the host happens to have installed and nothing reads or mutates
//! the process environment. The fixtures are real `index.theme` files and real
//! PNG/SVG files in the directories they declare, because the lookup is defined
//! entirely in terms of those two things.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

use crikey_platform::{IconLoader, IconProvider, IconSource};
use crikey_platform_linux::XdgIconSource;

/// A unique scratch directory that deletes itself when the test ends.
#[derive(Debug)]
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);

        let path = std::env::temp_dir().join(format!(
            "crikey-icon-theme-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");
        Self { path }
    }

    /// A theme base directory: the thing that holds `<theme>/index.theme`.
    fn root(&self, name: &str) -> PathBuf {
        let root = self.path.join(name);
        fs::create_dir_all(&root).expect("theme root is creatable");
        root
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Writes `<root>/<theme>/index.theme`.
fn write_index(root: &Path, theme: &str, contents: &str) {
    let directory = root.join(theme);
    fs::create_dir_all(&directory).expect("theme directory is creatable");
    fs::write(directory.join("index.theme"), contents).expect("index is writable");
}

/// Writes an icon file into `<root>/<theme>/<subdirectory>/<name>` and returns
/// its path.
fn write_icon(root: &Path, theme: &str, subdirectory: &str, name: &str) -> PathBuf {
    let directory = root.join(theme).join(subdirectory);
    fs::create_dir_all(&directory).expect("icon directory is creatable");
    let path = directory.join(name);
    fs::write(&path, icon_bytes(name)).expect("icon is writable");
    path
}

/// A file whose bytes really are the format its name claims, so a located icon
/// can also be decoded.
fn icon_bytes(name: &str) -> Vec<u8> {
    if name.ends_with(".svg") {
        br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="green"/></svg>"#.to_vec()
    } else {
        // A real 1x1 PNG of one blue pixel, so a located icon is also a
        // decodable one.
        const PIXEL: [u8; 70] = [
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
            0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x60, 0x68, 0xf8, 0xff,
            0x1f, 0x00, 0x04, 0x82, 0x02, 0x7f, 0x38, 0x86, 0x48, 0x7c, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
            0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        PIXEL.to_vec()
    }
}

/// A theme declaring one fixed directory per size plus one scalable directory.
const SIZED_THEME: &str = "[Icon Theme]
Name=Sized
Directories=16x16/apps,32x32/apps,48x48/apps,scalable/apps

[16x16/apps]
Size=16
Type=Fixed

[32x32/apps]
Size=32
Type=Fixed

[48x48/apps]
Size=48
Type=Fixed

[scalable/apps]
Size=48
Type=Scalable
MinSize=8
MaxSize=512
";

fn sized_source(scratch: &Scratch, root: &Path) -> XdgIconSource {
    let _ = scratch;
    XdgIconSource::new(vec![root.to_path_buf()], Vec::new(), "Sized")
}

// ---------------------------------------------------------------------------
// Size selection
// ---------------------------------------------------------------------------

#[test]
fn the_directory_that_serves_the_requested_size_is_the_one_used() {
    let scratch = Scratch::new();
    let root = scratch.root("icons");
    write_index(&root, "Sized", SIZED_THEME);
    write_icon(&root, "Sized", "16x16/apps", "app.png");
    let expected = write_icon(&root, "Sized", "48x48/apps", "app.png");

    let located = sized_source(&scratch, &root).locate("app", 48);

    assert_eq!(located, Some(expected));
}

#[test]
fn a_size_no_directory_serves_falls_back_to_the_closest_one() {
    let scratch = Scratch::new();
    let root = scratch.root("icons");
    write_index(&root, "Sized", SIZED_THEME);
    write_icon(&root, "Sized", "16x16/apps", "app.png");
    let expected = write_icon(&root, "Sized", "32x32/apps", "app.png");

    // 48 is served by no directory that has the name, so the nearer of the two
    // that do wins. An approximate icon is what a launcher wants here; nothing
    // is what it does not.
    let located = sized_source(&scratch, &root).locate("app", 48);

    assert_eq!(located, Some(expected));
}

#[test]
fn a_scalable_directory_serves_any_size_inside_its_declared_range() {
    let scratch = Scratch::new();
    let root = scratch.root("icons");
    write_index(&root, "Sized", SIZED_THEME);
    let expected = write_icon(&root, "Sized", "scalable/apps", "app.svg");

    let located = sized_source(&scratch, &root).locate("app", 37);

    assert_eq!(located, Some(expected));
}

#[test]
fn a_directory_at_the_exact_size_prefers_its_raster_and_a_scalable_one_its_vector() {
    let scratch = Scratch::new();
    let root = scratch.root("icons");
    write_index(&root, "Sized", SIZED_THEME);
    let exact_png = write_icon(&root, "Sized", "48x48/apps", "app.png");
    write_icon(&root, "Sized", "48x48/apps", "app.svg");
    let scalable_svg = write_icon(&root, "Sized", "scalable/apps", "other.svg");
    write_icon(&root, "Sized", "scalable/apps", "other.png");
    let source = sized_source(&scratch, &root);

    // A hand-tuned 48x48 raster is sharper than any render of the same artwork.
    assert_eq!(source.locate("app", 48), Some(exact_png));
    // In a scalable directory the vector is the one drawn for every size.
    assert_eq!(source.locate("other", 48), Some(scalable_svg));
}

#[test]
fn a_scaled_directory_is_not_a_candidate() {
    let scratch = Scratch::new();
    let root = scratch.root("icons");
    write_index(
        &root,
        "Scaled",
        "[Icon Theme]
Name=Scaled
Directories=48x48@2/apps

[48x48@2/apps]
Size=48
Scale=2
Type=Fixed
",
    );
    write_icon(&root, "Scaled", "48x48@2/apps", "app.png");

    // Choosing a scaled directory correctly needs the output scale factor, which
    // the backend does not have, so the directory is skipped rather than served
    // at the wrong density.
    let source = XdgIconSource::new(vec![root.clone()], Vec::new(), "Scaled");

    assert_eq!(source.locate("app", 48), None);
}

// ---------------------------------------------------------------------------
// Theme order
// ---------------------------------------------------------------------------

#[test]
fn the_configured_theme_is_exhausted_before_the_theme_it_inherits_from() {
    let scratch = Scratch::new();
    let root = scratch.root("icons");
    write_index(
        &root,
        "Child",
        "[Icon Theme]
Name=Child
Inherits=Parent
Directories=16x16/apps

[16x16/apps]
Size=16
Type=Fixed
",
    );
    write_index(
        &root,
        "Parent",
        "[Icon Theme]
Name=Parent
Directories=48x48/apps

[48x48/apps]
Size=48
Type=Fixed
",
    );
    let child = write_icon(&root, "Child", "16x16/apps", "app.png");
    write_icon(&root, "Parent", "48x48/apps", "app.png");

    let located = XdgIconSource::new(vec![root.clone()], Vec::new(), "Child").locate("app", 48);

    // The parent's icon is the exact size and still loses: the specification
    // resolves theme by theme, so a configured theme that has the name at all
    // answers. Otherwise a user who chose a theme would see their parent theme's
    // icons whenever the sizes lined up better.
    assert_eq!(located, Some(child));
}

#[test]
fn hicolor_answers_a_name_no_theme_in_the_chain_carries() {
    let scratch = Scratch::new();
    let root = scratch.root("icons");
    write_index(&root, "Sized", SIZED_THEME);
    write_index(
        &root,
        "hicolor",
        "[Icon Theme]
Name=hicolor
Directories=48x48/apps

[48x48/apps]
Size=48
Type=Fixed
",
    );
    let fallback = write_icon(&root, "hicolor", "48x48/apps", "app.png");

    let located = sized_source(&scratch, &root).locate("app", 48);

    // Applications install their own icons into hicolor, so it is the last link
    // of every chain whether a theme names it or not.
    assert_eq!(located, Some(fallback));
}

#[test]
fn an_inheritance_cycle_still_terminates() {
    let scratch = Scratch::new();
    let root = scratch.root("icons");
    for (theme, parent) in [("Loop", "Knot"), ("Knot", "Loop")] {
        write_index(
            &root,
            theme,
            &format!(
                "[Icon Theme]
Name={theme}
Inherits={parent}
Directories=48x48/apps

[48x48/apps]
Size=48
Type=Fixed
"
            ),
        );
    }
    let expected = write_icon(&root, "Knot", "48x48/apps", "app.png");

    // `Inherits` is author supplied, so a cycle is an input rather than a bug,
    // and construction must not walk it forever.
    let located = XdgIconSource::new(vec![root.clone()], Vec::new(), "Loop").locate("app", 48);

    assert_eq!(located, Some(expected));
}

#[test]
fn an_earlier_theme_root_overrides_the_system_copy_of_the_same_theme() {
    let scratch = Scratch::new();
    let user = scratch.root("user-icons");
    let system = scratch.root("system-icons");
    write_index(&user, "Sized", SIZED_THEME);
    write_index(&system, "Sized", SIZED_THEME);
    let overriding = write_icon(&user, "Sized", "48x48/apps", "app.png");
    write_icon(&system, "Sized", "48x48/apps", "app.png");

    let source = XdgIconSource::new(vec![user.clone(), system.clone()], Vec::new(), "Sized");

    assert_eq!(source.locate("app", 48), Some(overriding));
}

#[test]
fn an_unthemed_directory_answers_only_after_every_theme_has() {
    let scratch = Scratch::new();
    let root = scratch.root("icons");
    let pixmaps = scratch.root("pixmaps");
    write_index(&root, "Sized", SIZED_THEME);
    let themed = write_icon(&root, "Sized", "48x48/apps", "themed.png");
    let unthemed = pixmaps.join("legacy.png");
    fs::write(&unthemed, icon_bytes("legacy.png")).expect("pixmap is writable");
    // The same name in both: the theme must win.
    fs::write(pixmaps.join("themed.png"), icon_bytes("themed.png")).expect("pixmap is writable");

    let source = XdgIconSource::new(vec![root.clone()], vec![pixmaps.clone()], "Sized");

    assert_eq!(source.locate("themed", 48), Some(themed));
    assert_eq!(source.locate("legacy", 48), Some(unthemed));
}

// ---------------------------------------------------------------------------
// What a reference is allowed to be
// ---------------------------------------------------------------------------

#[test]
fn an_absolute_path_reference_is_used_exactly_as_written() {
    let scratch = Scratch::new();
    let root = scratch.root("icons");
    write_index(&root, "Sized", SIZED_THEME);
    let outside = scratch.path.join("elsewhere.png");
    fs::write(&outside, icon_bytes("elsewhere.png")).expect("icon is writable");

    let located = sized_source(&scratch, &root).locate(&outside.to_string_lossy(), 48);

    assert_eq!(located, Some(outside));
}

#[test]
fn a_reference_whose_name_carries_an_extension_is_still_found_by_name() {
    let scratch = Scratch::new();
    let root = scratch.root("icons");
    write_index(&root, "Sized", SIZED_THEME);
    let expected = write_icon(&root, "Sized", "48x48/apps", "app.png");

    // Desktop entries in the wild write `Icon=app.png` even though the key is
    // specified as a bare name; without stripping, the search would look for
    // `app.png.png`.
    let located = sized_source(&scratch, &root).locate("app.png", 48);

    assert_eq!(located, Some(expected));
}

#[test]
fn a_reference_containing_a_separator_or_a_traversal_resolves_nothing() {
    let scratch = Scratch::new();
    let root = scratch.root("icons");
    write_index(&root, "Sized", SIZED_THEME);
    write_icon(&root, "Sized", "48x48/apps", "app.png");
    let source = sized_source(&scratch, &root);

    // A desktop entry is a file any user or package can write. The theme search
    // joins the reference onto directories it trusts, so this is the only place
    // that can refuse a traversal -- and it must, because an icon path would
    // otherwise be a way to make the launcher read an arbitrary file.
    for hostile in [
        "../../../../etc/shadow",
        "..",
        "48x48/apps/app",
        "/etc/shadow",
        ".hidden",
        "",
    ] {
        assert_eq!(
            source.locate(hostile, 48),
            None,
            "{hostile:?} must resolve to nothing"
        );
    }
}

#[test]
fn a_theme_root_with_no_index_contributes_no_directories() {
    let scratch = Scratch::new();
    let root = scratch.root("icons");
    // A directory that looks like a theme but declares nothing is not a theme:
    // without an index there is no `Directories` list and no size rules.
    fs::create_dir_all(root.join("Sized").join("48x48").join("apps")).expect("directory is creatable");
    fs::write(
        root.join("Sized").join("48x48").join("apps").join("app.png"),
        icon_bytes("app.png"),
    )
    .expect("icon is writable");

    let source = XdgIconSource::new(vec![root.clone()], Vec::new(), "Sized");

    assert_eq!(source.locate("app", 48), None);
}

// ---------------------------------------------------------------------------
// End to end
// ---------------------------------------------------------------------------

#[test]
fn a_themed_name_resolves_all_the_way_to_decoded_pixels() {
    let scratch = Scratch::new();
    let root = scratch.root("icons");
    write_index(&root, "Sized", SIZED_THEME);
    write_icon(&root, "Sized", "scalable/apps", "app.svg");
    let loader = IconLoader::new(XdgIconSource::new(vec![root.clone()], Vec::new(), "Sized"));

    let image = loader
        .load("app", 48)
        .expect("the located icon decodes")
        .expect("the themed name resolves");

    assert_eq!((image.width(), image.height()), (48, 48));
}

#[test]
fn a_name_no_theme_carries_is_absent_rather_than_an_error() {
    let scratch = Scratch::new();
    let root = scratch.root("icons");
    write_index(&root, "Sized", SIZED_THEME);
    let loader = IconLoader::new(XdgIconSource::new(vec![root.clone()], Vec::new(), "Sized"));

    let located = loader
        .load("nothing-installed", 48)
        .expect("an absent icon is not a failure");

    assert!(located.is_none());
}

// ---------------------------------------------------------------------------
// Hostile theme files
// ---------------------------------------------------------------------------

/// A FIFO where a file is expected. Opening one for reading blocks until a
/// writer shows up, so a reader that opens before it stats -- or that opens
/// without `O_NONBLOCK` -- waits forever on a file any user can drop into their
/// own `~/.icons`.
fn make_fifo(path: &Path) {
    fs::create_dir_all(path.parent().expect("the fixture has a parent")).expect("the directory is creatable");
    let status = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("mkfifo is available on a Linux host");
    assert!(status.success(), "mkfifo could not create {}", path.display());
}

/// Resolves on a worker thread so a regression that blocks fails this test by
/// name instead of hanging the run.
///
/// The bound is a liveness guard, never a timing assertion: a correct lookup over
/// a two-directory root returns in microseconds and the wait ends with it.
fn locate_without_blocking(theme_roots: Vec<PathBuf>, unthemed: Vec<PathBuf>, name: &str) -> Option<PathBuf> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let name = name.to_owned();
    std::thread::spawn(move || {
        let source = XdgIconSource::new(theme_roots, unthemed, "Sized");
        // A failed send only means this test already gave up.
        let _ = sender.send(source.locate(&name, 48));
    });

    receiver
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("icon lookup must return instead of blocking on a hostile theme file")
}

#[test]
fn a_fifo_planted_where_a_theme_index_belongs_does_not_block_lookup() {
    let scratch = Scratch::new();
    let hostile = scratch.root("hostile-icons");
    let real = scratch.root("icons");
    make_fifo(&hostile.join("Sized").join("index.theme"));
    write_index(&real, "Sized", SIZED_THEME);
    let expected = write_icon(&real, "Sized", "48x48/apps", "app.png");

    // The hostile root comes first, so its index is the first file the theme
    // table tries to read. It must be skipped, and the real root behind it must
    // still answer.
    let located = locate_without_blocking(vec![hostile, real], Vec::new(), "app");

    assert_eq!(located, Some(expected));
}

#[test]
fn a_fifo_planted_where_a_theme_icon_belongs_is_not_a_candidate() {
    let scratch = Scratch::new();
    let root = scratch.root("icons");
    write_index(&root, "Sized", SIZED_THEME);
    make_fifo(&root.join("Sized").join("48x48").join("apps").join("app.png"));
    let expected = write_icon(&root, "Sized", "32x32/apps", "app.png");

    // A FIFO is not an ordinary file, so it never becomes the located candidate
    // even though it sits in the directory that serves the requested size, and
    // the next-closest real icon answers instead.
    let located = locate_without_blocking(vec![root], Vec::new(), "app");

    assert_eq!(located, Some(expected));
}
