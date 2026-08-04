//! Windows icon locations (spec 18.1, 18.4).
//!
//! An icon location is a string, and what it means is table work: strip the
//! resource index, expand the environment, refuse what is not a file. None of
//! that needs a Windows kernel, so all of it runs here, on whatever host the
//! suite runs on -- the same reason the rest of this crate's pure logic is not
//! gated on its target.
//!
//! The fixture paths are therefore this host's, not `C:\`-shaped: the final
//! check is `Path::is_absolute` plus a real `stat`, and a Windows-shaped literal
//! is neither absolute nor present on a Unix host. What the cases pin down is the
//! part that is platform independent -- which locations become a path at all.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

use crikey_platform::{IconLoader, IconProvider, IconSource};
use crikey_platform_windows::ShortcutIconSource;

/// A unique scratch directory that deletes itself when the test ends.
#[derive(Debug)]
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);

        let path = std::env::temp_dir().join(format!(
            "crikey-windows-icons-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("scratch directory is creatable");
        Self { path }
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.path.join(name);
        fs::write(&path, bytes).expect("fixture is writable");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A real 1x1 PNG of one blue pixel.
const PIXEL_PNG: [u8; 70] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00,
    0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00,
    0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x60, 0x68, 0xf8, 0xff, 0x1f, 0x00, 0x04,
    0x82, 0x02, 0x7f, 0x38, 0x86, 0x48, 0x7c, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42,
    0x60, 0x82,
];

/// A single-frame `.ico` whose frame is [`PIXEL_PNG`].
fn ico() -> Vec<u8> {
    let mut out = vec![0, 0, 1, 0, 1, 0];
    out.extend_from_slice(&[1, 1, 0, 0, 1, 0, 32, 0]);
    out.extend_from_slice(&(PIXEL_PNG.len() as u32).to_le_bytes());
    out.extend_from_slice(&22_u32.to_le_bytes());
    out.extend_from_slice(&PIXEL_PNG);
    out
}

/// A source whose environment holds exactly these variables.
fn source(variables: &[(&'static str, PathBuf)]) -> ShortcutIconSource {
    let variables: Vec<(&'static str, OsString)> = variables
        .iter()
        .map(|(name, value)| (*name, value.clone().into_os_string()))
        .collect();
    ShortcutIconSource::with_environment(move |name| {
        variables
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, value)| value.clone())
    })
}

fn locate(source: &ShortcutIconSource, reference: &str) -> Option<PathBuf> {
    source.locate(reference, 32)
}

#[test]
fn a_location_naming_a_real_image_file_resolves_to_it() {
    let scratch = Scratch::new();
    let icon = scratch.write("tool.ico", &ico());

    let located = locate(&source(&[]), &icon.to_string_lossy());

    assert_eq!(located, Some(icon));
}

#[test]
fn a_resource_index_is_stripped_before_the_path_is_used() {
    let scratch = Scratch::new();
    let icon = scratch.write("tool.ico", &ico());
    // `GetIconLocation` hands back the resource index separately and discovery
    // appends it; it selects a resource inside an image and is not part of any
    // path. Both spellings occur, because a resource id may be negative.
    for suffix in [",0", ",3", ",-16801"] {
        let located = locate(&source(&[]), &format!("{}{suffix}", icon.to_string_lossy()));
        assert_eq!(
            located,
            Some(icon.clone()),
            "the {suffix:?} index must be stripped"
        );
    }
}

#[test]
fn a_comma_that_is_not_a_resource_index_stays_part_of_the_path() {
    let scratch = Scratch::new();
    // A file really called `logo,v2.ico` is a file, and a location ending in a
    // non-numeric tail is not an index.
    let icon = scratch.write("logo,v2.ico", &ico());

    let located = locate(&source(&[]), &icon.to_string_lossy());

    assert_eq!(located, Some(icon));
}

#[test]
fn an_environment_variable_in_a_location_is_expanded() {
    let scratch = Scratch::new();
    let icon = scratch.write("tool.ico", &ico());
    let source = source(&[("ToolHome", scratch.path.clone())]);

    // Shell links routinely store `%SystemRoot%\...` rather than a resolved
    // path, so a location that is not expanded names nothing.
    let located = locate(
        &source,
        &format!("%ToolHome%{}tool.ico", std::path::MAIN_SEPARATOR),
    );

    assert_eq!(located, Some(icon));
}

#[test]
fn an_unset_variable_leaves_the_location_naming_nothing() {
    let scratch = Scratch::new();
    scratch.write("tool.ico", &ico());

    // Left exactly as written, as Windows does. Dropping the `%NAME%` instead
    // would shorten the path into one that might name a different file.
    let located = locate(&source(&[]), "%NotSet%/tool.ico");

    assert_eq!(located, None);
}

#[test]
fn a_packaged_application_reference_resolves_nothing() {
    // A packaged application's icon is a property of a shell item, not a file.
    // Reporting no icon is the honest answer until the shell call exists.
    for reference in [
        r"shell:AppsFolder\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App",
        r"SHELL:AppsFolder\Whatever",
    ] {
        assert_eq!(locate(&source(&[]), reference), None, "{reference:?}");
    }
}

#[test]
fn a_location_naming_a_portable_executable_resolves_nothing() {
    let scratch = Scratch::new();
    // Real files, so the refusal is about the format rather than about absence:
    // extracting an icon from a PE image needs `FindResource` or
    // `SHDefExtractIcon`, and neither is implemented.
    for name in ["shell32.dll", "tool.exe", "desk.cpl"] {
        let path = scratch.write(name, &ico());
        assert_eq!(
            locate(&source(&[]), &format!("{},-1", path.to_string_lossy())),
            None,
            "{name:?} is a PE image, not an image file"
        );
    }
}

#[test]
fn a_relative_location_resolves_nothing() {
    let scratch = Scratch::new();
    scratch.write("tool.ico", &ico());

    // A catalog entry is not a shell: a relative location names a different file
    // depending on where the launcher happened to be started.
    assert_eq!(locate(&source(&[]), "tool.ico"), None);
}

#[test]
fn an_empty_location_resolves_nothing() {
    assert_eq!(locate(&source(&[]), ""), None);
    assert_eq!(locate(&source(&[]), ",0"), None);
}

#[test]
fn a_location_whose_expansion_exceeds_the_path_limit_resolves_nothing() {
    let scratch = Scratch::new();
    let source = source(&[("Huge", PathBuf::from("x".repeat(40_000)))]);
    let _ = &scratch;

    // A hostile or merely broken environment variable must not turn one icon
    // lookup into an unbounded allocation.
    assert_eq!(locate(&source, "%Huge%/tool.ico"), None);
}

#[test]
fn a_resolved_location_decodes_all_the_way_to_pixels() {
    let scratch = Scratch::new();
    let icon = scratch.write("tool.ico", &ico());
    let loader = IconLoader::new(source(&[]));

    let image = loader
        .load(&format!("{},0", icon.to_string_lossy()), 32)
        .expect("the located icon decodes")
        .expect("the location resolves");

    assert_eq!((image.width(), image.height()), (1, 1));
    assert_eq!(image.rgba(), &[0x00, 0x80, 0xff, 0xff]);
}

#[test]
fn a_location_pointing_at_a_file_that_is_gone_is_absent_rather_than_an_error() {
    let scratch = Scratch::new();
    let missing = scratch.path.join("never-written.ico");
    let loader = IconLoader::new(source(&[]));

    let located = loader
        .load(&missing.to_string_lossy(), 32)
        .expect("a missing icon is not a failure");

    assert!(located.is_none());
    assert!(!Path::new(&missing).exists());
}
