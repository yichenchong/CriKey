//! Windows icon locations (spec 18.1, 18.4).
//!
//! What discovery records as an item's `icon_reference` on Windows is a shell
//! *icon location*, not a path: `IShellLinkW::GetIconLocation` returns whatever
//! the shortcut's author stored, with the resource index appended when there is
//! one. Three shapes turn up in practice, and only one of
//! them is a file this build can read:
//!
//! * `C:\Program Files\Tool\tool.ico` -- a real image file. Resolved.
//! * `%SystemRoot%\system32\shell32.dll,-16801` -- a resource inside a PE
//!   image. Not resolved: reading it means `LoadLibraryEx` plus
//!   `FindResource`/`LookupIconIdFromDirectoryEx`, or `SHDefExtractIcon` and a
//!   GDI round trip to get the bits back out. Neither is implemented, so the
//!   reference reports no icon rather than a wrong one.
//! * `shell:AppsFolder\<AppUserModelID>` -- a packaged application, whose icon
//!   is a property of a shell item rather than a file at all. Also not resolved.
//!
//! That is why [`WindowsBackend::capability`] reports [`CapabilityState::Partial`]
//! for [`Capability::Icons`] on Windows: the interface works and answers for a
//! real subset of shortcuts, and says nothing for the rest.
//!
//! # Why this file is not behind `cfg(target_os = "windows")`
//!
//! For the reason the rest of this crate is not: an icon location is a string,
//! and deciding what one means is table work that is easy to get wrong and needs
//! no Windows kernel. Splitting the location, refusing a traversal, and choosing
//! not to feed a `.dll` to a PNG decoder are exercised by the suite on every
//! host. Only the `is_file` check at the end touches a filesystem, and it
//! behaves the same on all of them.
//!
//! [`WindowsBackend::capability`]: crate::WindowsBackend::capability
//! [`CapabilityState::Partial`]: crikey_platform::CapabilityState::Partial
//! [`Capability::Icons`]: crikey_platform::Capability::Icons

use std::{ffi::OsString, path::PathBuf};

use crikey_platform::{IconSource, PathIconSource};

/// The prefix of a packaged application's shell reference.
const APPS_FOLDER: &str = "shell:";

/// The longest location this will expand, in units of its encoded form.
///
/// Matches the extended-path limit the `.lnk` reader already uses. A hostile
/// environment variable must not turn one icon lookup into an unbounded
/// allocation.
const MAX_LOCATION_UNITS: usize = 32_767;

/// Resolves a shell icon location to a file, where it names one.
///
/// The environment is injected rather than read from the process so that
/// expansion is a pure function of its inputs: a test states the environment it
/// means, and no case here can be perturbed by -- or perturb -- the ambient
/// environment, which no parallel test could do safely.
pub struct ShortcutIconSource {
    environment: Box<EnvironmentLookup>,
}

/// Reads one environment variable, or reports that it is unset.
type EnvironmentLookup = dyn Fn(&str) -> Option<OsString> + Send + Sync;

impl std::fmt::Debug for ShortcutIconSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShortcutIconSource")
            .finish_non_exhaustive()
    }
}

impl ShortcutIconSource {
    /// Expands against the running process's environment.
    pub fn new() -> Self {
        Self::with_environment(|name| std::env::var_os(name))
    }

    /// Expands against exactly the environment `get` describes.
    pub fn with_environment(get: impl Fn(&str) -> Option<OsString> + Send + Sync + 'static) -> Self {
        Self {
            environment: Box::new(get),
        }
    }

    /// The path part of an icon location, with its resource index removed and
    /// its environment variables expanded.
    ///
    /// `None` for a location that names no file at all: a packaged application's
    /// shell reference, or one whose expansion would exceed the path limit.
    fn path_of(&self, reference: &str) -> Option<OsString> {
        if reference.is_empty() || reference.to_ascii_lowercase().starts_with(APPS_FOLDER) {
            return None;
        }
        let location = strip_resource_index(reference);
        let expanded = self.expand(location)?;
        (!expanded.is_empty()).then_some(expanded)
    }

    /// Replaces every `%NAME%` for which the environment has a value.
    ///
    /// One pass, because that is what Windows does: a variable whose value
    /// itself contains `%NAME%` is not expanded again, so the substituted text
    /// is never rescanned and the work is bounded by the input.
    ///
    /// A `%NAME%` with no value is left exactly as written, also as Windows
    /// does. The result then names no file and reports no icon, rather than
    /// collapsing to a shorter path that might name a different one.
    fn expand(&self, location: &str) -> Option<OsString> {
        if !location.contains('%') {
            return Some(OsString::from(location));
        }
        let mut expanded = OsString::new();
        let mut rest = location;
        while let Some(open) = rest.find('%') {
            expanded.push(&rest[..open]);
            let after = &rest[open + 1..];
            // `%%` is not a variable, and neither is a name with a separator in
            // it: both are literal text in a path.
            let variable = after.find('%').and_then(|close| {
                let name = &after[..close];
                (!name.is_empty() && !name.contains(['\\', '/'])).then(|| (name, &after[close + 1..]))
            });
            match variable.and_then(|(name, tail)| (self.environment)(name).map(|value| (value, tail))) {
                Some((value, tail)) => {
                    expanded.push(&value);
                    rest = tail;
                }
                None => {
                    expanded.push("%");
                    rest = after;
                }
            }
            if expanded.len() > MAX_LOCATION_UNITS {
                return None;
            }
        }
        expanded.push(rest);
        Some(expanded)
    }
}

impl Default for ShortcutIconSource {
    fn default() -> Self {
        Self::new()
    }
}

impl IconSource for ShortcutIconSource {
    fn locate(&self, reference: &str, size: u32) -> Option<PathBuf> {
        let path = self.path_of(reference)?;
        // The shared path source is what enforces "absolute, decodable, and
        // really a file". Reaching it is the whole job here: a `.dll` or `.exe`
        // location is refused there, by extension, which is the honest answer
        // for a PE resource this build cannot read.
        PathIconSource.locate(&path.to_string_lossy(), size)
    }
}

/// Removes the `,<index>` suffix a shell icon location may carry.
///
/// The index selects a resource inside a PE image and is not part of any path.
/// It is only stripped when the tail really is an integer: a file called
/// `logo,2.ico` is a file, and `C:\a,b\icon.ico` is a directory with a comma in
/// its name.
fn strip_resource_index(reference: &str) -> &str {
    let Some((head, tail)) = reference.rsplit_once(',') else {
        return reference;
    };
    let index = tail.strip_prefix('-').unwrap_or(tail);
    if !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()) {
        head
    } else {
        reference
    }
}
