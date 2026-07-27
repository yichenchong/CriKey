//! Start Menu resolution and packaged-application enumeration.
//!
//! Both halves go through the shell's own object model rather than a helper
//! process. Resolving a `.lnk` is `IShellLinkW` plus `IPersistFile`; listing
//! packaged applications is the Applications known folder bound to its item
//! enumerator. Neither spawns anything, which matters: a Start Menu holds
//! hundreds of entries, and a launcher that paid for a process per entry would
//! have no startup budget left (spec 18.4).

#![allow(unsafe_code)]

use std::ffi::{c_void, OsString};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows::core::{Interface, GUID, PCWSTR, PWSTR};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ,
};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_AppsFolder, FOLDERID_CommonStartMenu, FOLDERID_StartMenu, IEnumShellItems,
    IShellItem, IShellLinkW, SHGetKnownFolderItem, SHGetKnownFolderPath, ShellLink, KF_FLAG_DEFAULT,
    SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING,
};

use crikey_core::{PlatformPath, Result};
use crikey_platform::DiscoveredApplication;

use crate::win32::{refused, wide, Apartment};

use super::{split_arguments, ApplicationSet, Shortcut, StartMenuDiscovery};

/// How much room one shell string gets.
///
/// `IShellLinkW` writes into a caller buffer and truncates rather than asking
/// for more, so the size is a real limit. `MAX_PATH` is the documented floor
/// and `INFOTIPSIZE` (1024) bounds an argument string; this is comfortably past
/// both, which is what it costs to keep long-path targets intact.
const SHELL_TEXT_CAPACITY: usize = 4096;

/// The moniker that names a packaged application to the shell.
///
/// `shell:AppsFolder\<AppUserModelID>` is what `ShellExecute` and the shell's
/// own parsing accept for an application that has no executable path, so it is
/// both the launch target and a usable icon reference.
const APPS_FOLDER_PREFIX: &str = "shell:AppsFolder\\";

/// The separator between a package family name and an application id.
///
/// A packaged application's AppUserModelID is always
/// `<PackageFamilyName>!<ApplicationId>`; the Applications folder also lists
/// desktop programs, whose parsing names carry no `!` and whose real
/// executables the Start Menu walk already found.
const PACKAGE_SEPARATOR: char = '!';

/// The Start Menu known folders, per-user first.
pub(super) fn start_menu_roots() -> Vec<PathBuf> {
    [FOLDERID_StartMenu, FOLDERID_CommonStartMenu]
        .iter()
        .filter_map(known_folder)
        .collect()
}

/// One known folder's path, or `None` when this machine does not have it.
fn known_folder(folder: &GUID) -> Option<PathBuf> {
    // SAFETY: `folder` is one of the constants above and outlives the call;
    // `SHGetKnownFolderPath` needs no apartment.
    let path = unsafe { SHGetKnownFolderPath(folder, KF_FLAG_DEFAULT, None) }.ok()?;
    // SAFETY: the shell allocated `path` and handed over ownership.
    let path = unsafe { take_shell_string(path) };
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Resolves the Start Menu and, when asked, the packaged applications.
pub(super) fn discover(scanner: &StartMenuDiscovery) -> Result<Vec<DiscoveredApplication>> {
    let _apartment = Apartment::enter("application discovery")?;

    let mut applications = ApplicationSet::new();
    let mut text = vec![0u16; SHELL_TEXT_CAPACITY];
    for shortcut in scanner.shortcuts() {
        if let Some(application) = resolve(&shortcut, &mut text) {
            applications.insert(application);
        }
    }

    if scanner.packaged {
        for application in packaged_applications()? {
            applications.insert(application);
        }
    }

    Ok(applications.into_applications())
}

/// Reads one `.lnk` into the application it launches.
///
/// `None` for anything a launcher cannot run: a shortcut the shell will not
/// load, or one that points at a shell object with no path at all, which is
/// what a link to Control Panel or a printer looks like from here.
///
/// A shortcut that declares no icon of its own reports none, rather than
/// reporting its target again. Extracting the executable's default icon is the
/// icon layer's decision to make and its cost to pay; discovery only passes on
/// what the shortcut actually says.
fn resolve(shortcut: &Shortcut, text: &mut [u16]) -> Option<DiscoveredApplication> {
    // A fresh link object per shortcut. Reusing one across loads would make
    // every field of a shortcut that fails halfway through read back as the
    // previous shortcut's, which is exactly the kind of silent mix-up that puts
    // the wrong program behind the right name.
    // SAFETY: the class and interface match; the apartment is live.
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }.ok()?;
    let persist: IPersistFile = link.cast().ok()?;

    let path = wide(shortcut.path.as_os_str());
    // SAFETY: `path` is NUL terminated and outlives the call.
    unsafe { persist.Load(PCWSTR(path.as_ptr()), STGM_READ) }.ok()?;

    // Deliberately not `IShellLinkW::Resolve`: it is allowed to search the
    // filesystem, hit the network and trigger installer repair, none of which a
    // launcher may do while building a catalog. A stale shortcut is reported as
    // it is stored and fails at launch, where the user can see why.
    // SAFETY: the slice is a live buffer the call writes into; a null
    // `WIN32_FIND_DATAW` pointer is documented as "no extra information".
    let target = read_shell_text(text, |slot| unsafe {
        link.GetPath(slot, std::ptr::null_mut(), 0)
    })?;
    if target.is_empty() {
        return None;
    }

    // SAFETY: as above.
    let arguments = read_shell_text(text, |slot| unsafe { link.GetArguments(slot) })
        .map(|arguments| split_arguments(&arguments.to_string_lossy()))
        .unwrap_or_default();

    let mut icon_index = 0i32;
    // SAFETY: as above; `icon_index` is a live `i32` for the duration.
    let icon_reference = read_shell_text(text, |slot| unsafe {
        link.GetIconLocation(slot, &mut icon_index)
    })
    .filter(|location| !location.is_empty())
    .map(|location| icon_reference(&location.to_string_lossy(), icon_index));

    Some(DiscoveredApplication {
        name: shortcut.name.clone(),
        target: PlatformPath::new(target),
        arguments,
        icon_reference,
        // A Start Menu shortcut is identified by where it points, not by an
        // AppUserModelID: only packaged applications below carry one.
        platform_id: None,
    })
}

/// The `path,index` form Windows uses to name one icon inside a file.
///
/// Index zero is written bare, which is the same convention the registry's
/// `DefaultIcon` values use and keeps the common case readable.
fn icon_reference(location: &str, index: i32) -> String {
    if index == 0 {
        location.to_owned()
    } else {
        format!("{location},{index}")
    }
}

/// Every packaged application the shell publishes, ordered by identifier.
fn packaged_applications() -> Result<Vec<DiscoveredApplication>> {
    // SAFETY: the constant outlives the call and the apartment is live.
    let folder: IShellItem = unsafe { SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None) }
        .map_err(|error| refused("open the Windows Applications folder", &error))?;
    // SAFETY: `BHID_EnumItems` is the documented handler for enumerating a
    // folder's items and yields exactly this interface.
    let items: IEnumShellItems = unsafe { folder.BindToHandler(None, &BHID_EnumItems) }
        .map_err(|error| refused("enumerate the Windows Applications folder", &error))?;

    let mut applications = Vec::new();
    loop {
        let mut slot: [Option<IShellItem>; 1] = [None];
        let mut fetched = 0u32;
        // `Next` reports `S_FALSE` at the end of the folder; the Windows
        // binding preserves that as success and sets the fetched count to zero.
        // Any failing HRESULT is a discovery failure, not a plausible end.
        // SAFETY: both out parameters are live for the duration.
        unsafe { items.Next(&mut slot, Some(&mut fetched)) }
            .map_err(|error| refused("enumerate the Windows Applications folder", &error))?;
        if fetched == 0 {
            break;
        }
        let Some(item) = slot[0].take() else {
            break;
        };

        if let Some(application) = packaged_application(&item) {
            applications.push(application);
        }
    }

    // The shell's enumeration order is its own business; sorting by identifier
    // makes two scans of an unchanged machine produce the same catalog.
    applications.sort_by(|left, right| left.platform_id.cmp(&right.platform_id));
    Ok(applications)
}

/// One Applications-folder item, if it is a packaged application.
fn packaged_application(item: &IShellItem) -> Option<DiscoveredApplication> {
    // The parsing name of an item in the Applications folder is its
    // AppUserModelID: that is what makes this folder the documented way to
    // enumerate packaged applications without a shell subprocess.
    // SAFETY: the item is live and the shell allocates the returned string.
    let identifier = unsafe { item.GetDisplayName(SIGDN_PARENTRELATIVEPARSING) }.ok()?;
    // SAFETY: the shell handed over ownership of the string.
    let identifier = unsafe { take_shell_string(identifier) };

    let readable = identifier.to_string_lossy().into_owned();
    if !readable.contains(PACKAGE_SEPARATOR) {
        return None;
    }

    // SAFETY: as above.
    let name = unsafe { item.GetDisplayName(SIGDN_NORMALDISPLAY) }.ok()?;
    // SAFETY: as above.
    let name = unsafe { take_shell_string(name) };

    // Built from the raw identifier rather than its readable rendering, so the
    // target stays exactly what the shell will parse back (spec 18.3).
    let mut target = OsString::from(APPS_FOLDER_PREFIX);
    target.push(&identifier);

    Some(DiscoveredApplication {
        name: name.to_string_lossy().into_owned(),
        // The same moniker is the only handle an icon pipeline has on a
        // packaged application: it has no icon file to point at, and
        // `IShellItemImageFactory` takes exactly this string.
        icon_reference: Some(target.to_string_lossy().into_owned()),
        target: PlatformPath::new(target),
        // A packaged application is activated, not executed with a command
        // line; the shell rejects arguments appended to the moniker.
        arguments: Vec::new(),
        platform_id: Some(readable),
    })
}

/// Runs one buffer-filling shell call and reads back what it wrote.
///
/// The buffer is cleared first because several of these methods report success
/// without writing anything -- `IShellLinkW::GetPath` returns `S_FALSE` for a
/// link with no path -- and reading the previous shortcut's bytes back out of a
/// reused buffer would attach one program's path to another's name.
fn read_shell_text(
    text: &mut [u16],
    read: impl FnOnce(&mut [u16]) -> windows::core::Result<()>,
) -> Option<OsString> {
    text.fill(0);
    read(text).ok()?;
    let length = text.iter().position(|unit| *unit == 0).unwrap_or(text.len());
    Some(OsString::from_wide(&text[..length]))
}

/// Copies a string the shell allocated, then frees the original.
///
/// # Safety
///
/// `text` must be null or a NUL-terminated string allocated by the COM task
/// allocator, whose ownership the caller is handing over.
unsafe fn take_shell_string(text: PWSTR) -> OsString {
    if text.is_null() {
        return OsString::new();
    }

    // SAFETY: the caller guarantees a NUL-terminated buffer.
    let owned = OsString::from_wide(unsafe { text.as_wide() });
    // SAFETY: the caller guarantees the task allocator owns it.
    unsafe { CoTaskMemFree(Some(text.as_ptr() as *const c_void)) };
    owned
}
