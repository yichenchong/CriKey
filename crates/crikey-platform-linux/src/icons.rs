//! XDG icon theme lookup (spec 18.1, 18.6).
//!
//! A desktop entry's `Icon` key is one of two things, and the launcher cannot
//! tell which from the catalog: an absolute path to an image, or a *themed
//! name* -- `firefox`, `text-editor` -- which means nothing until it is resolved
//! against the installed icon themes. The freedesktop icon theme specification
//! is what defines that resolution, and this module implements it: an
//! inheritance chain of themes, each spanning several base directories, each
//! base directory holding size-annotated subdirectories, and one lookup that has
//! to pick the closest size out of all of them.
//!
//! # Why the theme table is built once
//!
//! Resolution is on the critical path: [`IconSource::locate`] is called for
//! every visible row of every frame that changes, before the decoded-icon cache
//! can help, because the cache is keyed on the file the lookup returns. Parsing
//! `Adwaita/index.theme` -- a 30 KiB file listing 34 directories -- per row per
//! frame is not viable, so the whole chain is flattened at construction into a
//! list of existing directories with their size rules, and a lookup is then a
//! handful of `stat` calls.
//!
//! # What this does not do
//!
//! Scaled directories (`Scale=2` and up) are skipped. Selecting them correctly
//! means knowing the output scale factor, which is a window property the backend
//! does not have; ignoring them costs sharpness on a HiDPI display and nothing
//! else, because every theme that ships scaled directories ships the unscaled
//! ones too.
//!
//! `.svgz` and `.xpm` files are not candidates, because nothing in this build
//! decodes them. Passing them over in favour of a decodable sibling is the point:
//! locating a file only to fail on it would lose an icon that a `.png` in the
//! next directory would have provided. It is also why
//! [`LinuxBackend::capability`] reports [`Capability::Partial`] for
//! [`Capability::Icons`] rather than `Available`.
//!
//! [`LinuxBackend::capability`]: crate::LinuxBackend::capability
//! [`Capability::Partial`]: crikey_platform::CapabilityState::Partial
//! [`Capability::Icons`]: crikey_platform::Capability::Icons

use std::{
    env, fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use crikey_platform::{IconFormat, IconSource, PathIconSource};

/// The theme every compliant theme inherits from and every application installs
/// its own icon into, so it is always the last link of the chain.
const FALLBACK_THEME: &str = "hicolor";

/// The theme used when nothing says which theme is configured.
///
/// [`FALLBACK_THEME`] rather than a guess at the desktop's default: an icon
/// theme that is not installed resolves nothing, and `hicolor` is where the
/// applications themselves put their icons.
const DEFAULT_THEME: &str = FALLBACK_THEME;

/// The largest `index.theme` this will read. Adwaita's is 30 KiB.
const MAX_INDEX_BYTES: u64 = 1024 * 1024;

/// How many themes an inheritance chain may span.
///
/// `Inherits` is author-supplied and can name a cycle or a hundred-deep chain;
/// the visited set already stops a cycle, and this stops the chain from turning
/// one lookup into an unbounded directory walk.
const MAX_CHAIN: usize = 16;

/// The section naming the theme itself rather than one of its directories.
const THEME_SECTION: &str = "Icon Theme";

/// Where an icon that belongs to no theme lives.
const PIXMAPS: &str = "/usr/share/pixmaps";

/// How a theme directory's name maps onto the sizes it serves.
///
/// Straight from the specification, because the three rules genuinely differ:
/// a `48x48` directory serves exactly 48, a `scalable` directory serves a
/// declared range, and a threshold directory serves a band around its nominal
/// size and is willing to be scaled within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sizing {
    Fixed(u32),
    Scalable { min: u32, max: u32 },
    Threshold { size: u32, threshold: u32 },
}

impl Sizing {
    /// Whether this directory serves `size` without scaling beyond what its
    /// own rule permits.
    fn matches(&self, size: u32) -> bool {
        match *self {
            Self::Fixed(fixed) => fixed == size,
            Self::Scalable { min, max } => (min..=max).contains(&size),
            Self::Threshold {
                size: nominal,
                threshold,
            } => nominal.saturating_sub(threshold) <= size && size <= nominal.saturating_add(threshold),
        }
    }

    /// How far this directory's nearest served size is from `size`.
    ///
    /// Zero whenever [`Sizing::matches`] holds, so the two agree and the
    /// closest-directory search cannot prefer a non-matching directory over a
    /// matching one.
    fn distance(&self, size: u32) -> u32 {
        // One expression per rule: the distance below the band plus the distance
        // above it, exactly one of which can be non-zero.
        let outside = |low: u32, high: u32| low.saturating_sub(size) + size.saturating_sub(high);
        match *self {
            Self::Fixed(fixed) => fixed.abs_diff(size),
            Self::Scalable { min, max } => outside(min, max),
            Self::Threshold {
                size: nominal,
                threshold,
            } => outside(
                nominal.saturating_sub(threshold),
                nominal.saturating_add(threshold),
            ),
        }
    }

    /// The extensions to try in this directory, best first.
    ///
    /// A directory that serves the requested size exactly is asked for its
    /// raster first: a hand-tuned 48x48 PNG is sharper than any render of the
    /// same artwork. Anywhere else the vector wins, because rendering an SVG at
    /// the size the row draws beats scaling a raster that was drawn for another
    /// size.
    fn preferred_extensions(&self, size: u32) -> [&'static str; 2] {
        if matches!(self, Self::Fixed(fixed) if *fixed == size) {
            ["png", "svg"]
        } else {
            ["svg", "png"]
        }
    }
}

/// One existing directory of one theme, with the sizes it serves.
#[derive(Debug)]
struct ThemeDirectory {
    path: PathBuf,
    sizing: Sizing,
}

/// One link of the inheritance chain: every directory of one theme, across every
/// base directory that carries a copy of it.
///
/// Themes are a chain rather than one flat list because the specification
/// resolves a name theme by theme: the configured theme is asked for every size
/// it has before its parent is asked at all, so a parent's exact-size icon never
/// beats the configured theme's approximate one.
#[derive(Debug)]
struct Theme {
    directories: Vec<ThemeDirectory>,
}

impl Theme {
    /// The best file for `name` in this theme, or `None` when it has none.
    ///
    /// A directory that serves the requested size wins immediately. Otherwise
    /// the closest directory that has the name at all wins, which is what makes
    /// a 32-pixel icon show up in a 48-pixel row instead of nothing.
    fn lookup(&self, name: &str, size: u32) -> Option<PathBuf> {
        let mut closest: Option<(u32, PathBuf)> = None;
        for directory in &self.directories {
            for extension in directory.sizing.preferred_extensions(size) {
                let candidate = directory.path.join(format!("{name}.{extension}"));
                if !candidate.is_file() {
                    continue;
                }
                if directory.sizing.matches(size) {
                    return Some(candidate);
                }
                let distance = directory.sizing.distance(size);
                if closest.as_ref().is_none_or(|(best, _)| distance < *best) {
                    closest = Some((distance, candidate));
                }
                // The remaining extension in this directory can only be a worse
                // spelling of the same distance, so the search moves on.
                break;
            }
        }
        closest.map(|(_, path)| path)
    }
}

/// Resolves a desktop entry's `Icon` key against the installed icon themes.
#[derive(Debug)]
pub struct XdgIconSource {
    /// The inheritance chain, configured theme first, `hicolor` last.
    themes: Vec<Theme>,
    /// Directories holding icons that belong to no theme, searched only after
    /// every theme has been asked.
    unthemed: Vec<PathBuf>,
}

impl XdgIconSource {
    /// The themes and base directories of the running user's session.
    ///
    /// The theme name comes from the environment and the GTK settings files
    /// rather than from a live settings daemon: reading the configured name must
    /// not require a DBus round trip during startup, and a name that is wrong
    /// costs the `hicolor` fallback rather than an error.
    pub fn for_session() -> Self {
        Self::new(
            xdg_icon_roots(),
            vec![PathBuf::from(PIXMAPS)],
            &configured_theme(),
        )
    }

    /// The themes reachable from exactly these base directories.
    ///
    /// `theme_roots` are the directories that *contain* themes -- each holds
    /// `<theme>/index.theme` -- in precedence order. `unthemed` are directories
    /// searched flat, for icons no theme claims.
    pub fn new(theme_roots: Vec<PathBuf>, unthemed: Vec<PathBuf>, theme: &str) -> Self {
        let mut themes = Vec::new();
        let mut visited = Vec::new();
        let mut pending = vec![theme.to_owned()];
        while let Some(name) = pending.first().cloned() {
            pending.remove(0);
            if visited.iter().any(|seen| *seen == name) || visited.len() >= MAX_CHAIN {
                continue;
            }
            visited.push(name.clone());

            let mut directories = Vec::new();
            let mut inherits = Vec::new();
            for root in &theme_roots {
                let index = root.join(&name).join("index.theme");
                let Some(contents) = read_index(&index) else {
                    continue;
                };
                let parsed = parse_index(&contents);
                inherits.extend(parsed.inherits);
                for (subdirectory, sizing) in parsed.directories {
                    let path = root.join(&name).join(subdirectory);
                    if path.is_dir() {
                        directories.push(ThemeDirectory { path, sizing });
                    }
                }
            }
            if !directories.is_empty() {
                themes.push(Theme { directories });
            }
            // Breadth first, so a theme's own parents are asked before its
            // grandparents, which is the order `Inherits` declares.
            pending.extend(inherits);
            if pending.is_empty() && !visited.iter().any(|seen| seen == FALLBACK_THEME) {
                pending.push(FALLBACK_THEME.to_owned());
            }
        }

        Self { themes, unthemed }
    }
}

impl IconSource for XdgIconSource {
    fn locate(&self, reference: &str, size: u32) -> Option<PathBuf> {
        if let Some(path) = PathIconSource.locate(reference, size) {
            return Some(path);
        }
        let name = themed_name(reference)?;
        for theme in &self.themes {
            if let Some(path) = theme.lookup(name, size) {
                return Some(path);
            }
        }
        self.unthemed.iter().find_map(|directory| {
            ["png", "svg"].into_iter().find_map(|extension| {
                let candidate = directory.join(format!("{name}.{extension}"));
                candidate.is_file().then_some(candidate)
            })
        })
    }
}

/// The themed name a reference carries, or `None` when it is not one.
///
/// Two things happen here, and both are load-bearing.
///
/// A name is refused if it contains a path separator or names a directory
/// traversal. A desktop entry is a file any user or package can write, and
/// `Icon=../../../../etc/shadow` must not become a file the launcher reads:
/// the theme search joins this onto directories it trusts, so this is the only
/// place that can refuse it.
///
/// A trailing decodable extension is stripped. The specification says the `Icon`
/// key should carry a bare name, and shipped entries carry `Icon=firefox.png`
/// anyway; without stripping, the search would look for `firefox.png.png`.
fn themed_name(reference: &str) -> Option<&str> {
    if reference.is_empty() || reference.contains('/') || reference.starts_with('.') {
        return None;
    }
    let path = Path::new(reference);
    if IconFormat::from_extension(path).is_some() {
        return path.file_stem()?.to_str().filter(|stem| !stem.is_empty());
    }
    Some(reference)
}

/// The `[Icon Theme]` keys and the per-directory size rules of one index file.
#[derive(Debug, Default)]
struct Index {
    inherits: Vec<String>,
    directories: Vec<(String, Sizing)>,
}

/// Parses one `index.theme`.
///
/// The file is a desktop-entry style INI: an `[Icon Theme]` section naming the
/// theme's `Directories` and `Inherits`, then one section per directory carrying
/// its size rule. Only the directories `Directories` lists are returned, and in
/// that order, because the order is the theme author's preference.
///
/// A malformed line is skipped rather than failing the theme: one bad line in a
/// distribution's index file must not delete every icon on the system.
fn parse_index(contents: &str) -> Index {
    let sections = split_sections(contents);
    let theme = sections
        .iter()
        .find(|(name, _)| name == THEME_SECTION)
        .map(|(_, keys)| keys);
    let inherits = theme
        .and_then(|keys| value(keys, "Inherits"))
        .map(|value| comma_separated(value).collect())
        .unwrap_or_default();
    let listed: Vec<String> = theme
        .and_then(|keys| value(keys, "Directories"))
        .map(|value| comma_separated(value).collect())
        .unwrap_or_default();

    let directories = listed
        .into_iter()
        .filter_map(|name| {
            let keys = sections
                .iter()
                .find(|(section, _)| *section == name)
                .map(|(_, keys)| keys)?;
            Some((name, sizing(keys)?))
        })
        .collect();
    Index {
        inherits,
        directories,
    }
}

/// The size rule one directory section declares, or `None` when it declares
/// none this build can match against.
///
/// A section without a `Size` describes no size, and a scaled directory serves
/// the same nominal size at a higher pixel density -- choosing between those
/// needs the output scale factor, which this backend does not have.
fn sizing(keys: &[(&str, &str)]) -> Option<Sizing> {
    let number = |key: &str| value(keys, key).and_then(|value| value.parse::<u32>().ok());
    if number("Scale").unwrap_or(1) != 1 {
        return None;
    }
    let nominal = number("Size")?;
    let kind = value(keys, "Type").unwrap_or("");
    if kind.eq_ignore_ascii_case("Fixed") {
        return Some(Sizing::Fixed(nominal));
    }
    if kind.eq_ignore_ascii_case("Scalable") {
        return Some(Sizing::Scalable {
            min: number("MinSize").unwrap_or(nominal),
            max: number("MaxSize").unwrap_or(nominal),
        });
    }
    // Threshold is the specification's default type, and 2 its default band.
    Some(Sizing::Threshold {
        size: nominal,
        threshold: number("Threshold").unwrap_or(2),
    })
}

/// Splits an INI document into its sections, each with its `Key=Value` pairs in
/// file order.
///
/// Lines outside any section, comments, and lines without a separator are
/// dropped: they carry nothing this reads, and a hand-edited file has all three.
fn split_sections(contents: &str) -> Vec<(String, Vec<(&str, &str)>)> {
    let mut sections: Vec<(String, Vec<(&str, &str)>)> = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|line| line.strip_suffix(']')) {
            sections.push((name.to_owned(), Vec::new()));
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if let Some((_, keys)) = sections.last_mut() {
            keys.push((key.trim(), value.trim()));
        }
    }
    sections
}

/// The first spelling of `key`, so a duplicated key does not depend on how far
/// the parser read.
fn value<'a>(keys: &[(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    keys.iter()
        .find(|(candidate, _)| *candidate == key)
        .map(|(_, value)| *value)
}

fn comma_separated(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
}

/// Reads an `index.theme`, refusing anything that is not an ordinary file of
/// plausible size.
///
/// The same treatment desktop entries get, and for the same reasons -- see
/// [`read_entry`](crate::DesktopEntryScanner) for the long form. A theme lives
/// under any user's data home and under `~/.icons`, so the candidate is
/// attacker-controlled: `O_NONBLOCK` makes the open of a FIFO return instead of
/// waiting for a writer that never comes, which would otherwise hang icon lookup
/// for the rest of the session; `O_CLOEXEC` keeps the descriptor out of any
/// plugin child; and the metadata is taken from the *open descriptor* rather
/// than from the path, so a candidate swapped between the check and the read is
/// still refused.
///
/// The size is capped twice. The stat rejects a file that is already too big,
/// and the read still runs through a reader limited to one byte past the cap, so
/// a file that grows between the two calls is dropped rather than followed.
///
/// A non-UTF-8 file is ignored rather than converted: the format requires UTF-8,
/// and a lossy conversion would invent directory names.
fn read_index(path: &Path) -> Option<String> {
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_INDEX_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_INDEX_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_INDEX_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// The base directories that hold icon themes, in specification precedence
/// order: the user's own first, then the system data directories in their listed
/// order.
fn xdg_icon_roots() -> Vec<PathBuf> {
    const ICONS: &str = "icons";
    const DEFAULT_DATA_DIRS: [&str; 2] = ["/usr/local/share", "/usr/share"];

    let mut roots = Vec::new();
    if let Some(data_home) = absolute_from_env("XDG_DATA_HOME") {
        roots.push(data_home.join(ICONS));
    } else if let Some(home) = absolute_from_env("HOME") {
        roots.push(home.join(".local").join("share").join(ICONS));
    }
    // The historical per-user location, which the specification still lists
    // ahead of the system directories.
    if let Some(home) = absolute_from_env("HOME") {
        roots.push(home.join(".icons"));
    }
    match env::var_os("XDG_DATA_DIRS").filter(|dirs| !dirs.is_empty()) {
        Some(dirs) => roots.extend(
            env::split_paths(&dirs)
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join(ICONS)),
        ),
        None => roots.extend(DEFAULT_DATA_DIRS.map(|dir| Path::new(dir).join(ICONS))),
    }
    roots
}

fn absolute_from_env(key: &str) -> Option<PathBuf> {
    let value = PathBuf::from(env::var_os(key)?);
    value.is_absolute().then_some(value)
}

/// The configured icon theme name.
///
/// There is no standard environment variable for this, so the order is what
/// actually carries the setting on a Linux desktop: `XDG_ICON_THEME` where a
/// session sets it, then the GTK settings files, newest schema first. A missing
/// or unparsable setting falls back to [`DEFAULT_THEME`] rather than to a guess
/// at the desktop's own default, because a theme that is not installed resolves
/// nothing at all.
fn configured_theme() -> String {
    if let Some(theme) = env::var("XDG_ICON_THEME")
        .ok()
        .map(|theme| theme.trim().to_owned())
        .filter(|theme| !theme.is_empty())
    {
        return theme;
    }
    let config_home = absolute_from_env("XDG_CONFIG_HOME")
        .or_else(|| absolute_from_env("HOME").map(|home| home.join(".config")));
    if let Some(config_home) = config_home {
        for version in ["gtk-4.0", "gtk-3.0"] {
            let settings = config_home.join(version).join("settings.ini");
            if let Some(theme) = read_index(&settings).and_then(|contents| gtk_icon_theme(&contents)) {
                return theme;
            }
        }
    }
    DEFAULT_THEME.to_owned()
}

/// The `gtk-icon-theme-name` value out of a GTK `settings.ini`.
fn gtk_icon_theme(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let (key, value) = line.trim().split_once('=')?;
        (key.trim() == "gtk-icon-theme-name")
            .then(|| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    })
}
