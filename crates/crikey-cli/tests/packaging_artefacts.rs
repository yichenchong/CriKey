//! Contracts the checked-in distribution artefacts have to keep (spec 14.13,
//! 15.4; packaging/README.md).
//!
//! These tests live beside the `crikey` binary because that binary is what the
//! artefacts install, and every contract asserted here is a property of the
//! relationship between the two: where the installer puts the Python payload
//! directories the running executable looks for, what the bundle claims about
//! itself, and what the package asks the operating system for.
//!
//! They read the declaration files rather than a built artefact on purpose. An
//! MSI, an MSIX and a signed `.app` can only be produced on a real Windows or
//! macOS host, so a test that waited for one would never run anywhere; the
//! declarations, on the other hand, are the thing a reviewer reads and the
//! thing that silently rots when a resolver on the Rust side is renamed.
//!
//! Comments are stripped before every assertion. The prose in those files
//! explains, accurately and by name, which Keypirinha API the compatibility
//! layer targets and which entitlements were considered and rejected; the
//! contracts below are about what the artefacts *declare*, not about what the
//! comments discuss.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The workspace root. `CARGO_MANIFEST_DIR` is `<root>/crates/crikey-cli`.
fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the workspace root is reachable from the crate directory")
}

fn read(relative: &str) -> String {
    let path = repository_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{} is readable: {error}", path.display()))
}

/// The document with every `<!-- ... -->` region removed.
///
/// An unterminated comment would otherwise swallow the rest of the file and
/// turn a malformed artefact into a vacuously passing test, so the tail after
/// an unclosed opener is dropped and the caller's `contains` assertions fail.
fn without_comments(document: &str) -> String {
    let mut stripped = String::with_capacity(document.len());
    let mut rest = document;
    while let Some(start) = rest.find("<!--") {
        stripped.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + "-->".len()..],
            None => return stripped,
        }
    }
    stripped.push_str(rest);
    stripped
}

/// The four files that describe an installed CriKey to its operating system.
const DECLARATIONS: [&str; 4] = [
    "packaging/macos/Info.plist",
    "packaging/macos/Entitlements.plist",
    "packaging/windows/crikey.wxs",
    "packaging/windows/AppxManifest.xml",
];

#[test]
fn the_comment_stripper_drops_a_comment_and_keeps_the_markup_around_it() {
    // The assertions below are only as trustworthy as this.
    assert_eq!(without_comments("<a/><!-- x --><b/>"), "<a/><b/>");
    assert_eq!(without_comments("<a/><!-- x"), "<a/>");
    assert_eq!(without_comments("<a/>"), "<a/>");
}

#[test]
fn every_packager_stages_the_payloads_and_supervised_runtime_hosts() {
    // `sdk_root()` in crikey-python-host does `exe.parent().join("modern-sdk")`
    // and `shim_root()` in crikey-legacy-compat does the same for
    // "legacy-shim". An installer that spells either differently produces a
    // launcher whose modern Python plugins and legacy packages fail only when
    // one is first invoked, which is exactly the kind of defect that reaches a
    // user. Renaming a resolver therefore has to fail here.
    let msi = without_comments(&read("packaging/windows/crikey.wxs"));
    for directory in ["modern-sdk", "legacy-shim"] {
        assert!(
            msi.contains(&format!("Name=\"{directory}\"")),
            "the MSI must create a `{directory}` directory beside crikey.exe"
        );
        assert!(
            msi.contains(&format!("\\{directory}\\**")),
            "the MSI must harvest the staged `{directory}` tree"
        );
    }

    let windows_build = read("packaging/windows/build.ps1");
    let macos_build = read("packaging/macos/build.sh");
    for directory in ["modern-sdk", "legacy-shim"] {
        assert!(
            windows_build.contains(&format!("'{directory}'")),
            "build.ps1 must stage `{directory}`"
        );
        assert!(
            macos_build.contains(&format!("/{directory}\"")),
            "build.sh must stage `{directory}` inside Contents/MacOS"
        );
    }

    // The worker entry point each resolver insists on. Staging the directory
    // but not this file is an install that fails the same way.
    assert!(
        macos_build.contains("_crikey_modern_worker.py") && macos_build.contains("_crikey_legacy_worker.py"),
        "build.sh must verify both worker entry points reached the bundle"
    );
    assert!(
        windows_build.contains("_crikey_modern_worker.py")
            && windows_build.contains("_crikey_legacy_worker.py"),
        "build.ps1 must verify both worker entry points reached the staging tree"
    );
    // These workers are part of the installed runtime contract. Both provider
    // resolvers look beside the running executable and deliberately do not
    // search PATH, so a package that omits either host cannot run its runtime.
    for host in ["crikey-wasm-host.exe", "crikey-cabi-host.exe"] {
        assert!(
            windows_build.contains(host) && msi.contains(host),
            "Windows staging and MSI must carry `{host}`"
        );
    }
    for host in ["crikey-wasm-host", "crikey-cabi-host"] {
        assert!(
            macos_build.contains(host),
            "macOS staging must carry `{host}` beside the bundle executable"
        );
    }

    // The Linux packagers stage one tree that the tarball, .deb, .rpm and
    // Flatpak are all cut from, so one check covers four artefacts. The
    // executable there is `lib/crikey/crikey` with `bin/crikey` a symlink to
    // it, which works because `current_exe` reports the resolved target of
    // /proc/self/exe -- so these two directories still land beside the
    // launcher as the resolvers require.
    let linux_build = read("packaging/linux/build.sh");
    for directory in ["modern-sdk", "legacy-shim"] {
        assert!(
            linux_build.contains(&format!("/{directory}\"")),
            "packaging/linux/build.sh must stage `{directory}` beside the executable"
        );
    }
    assert!(
        linux_build.contains("_crikey_modern_worker.py") && linux_build.contains("_crikey_legacy_worker.py"),
        "packaging/linux/build.sh must verify both worker entry points reached the staged tree"
    );
    assert!(
        linux_build.contains("crikey-wasm-host") && linux_build.contains("crikey-cabi-host"),
        "packaging/linux/build.sh must stage both supervised runtime hosts"
    );
    let flatpak = read("packaging/linux/flatpak/org.crikey.CriKey.yaml");
    assert!(
        flatpak.contains("crikey-wasm-host") && flatpak.contains("crikey-cabi-host"),
        "the Flatpak manifest must build and install both supervised runtime hosts"
    );
    assert!(
        flatpak.contains("__CRIKEY_REPOSITORY_SOURCE__")
            && flatpak.contains("__CRIKEY_PYTHON_ARCHIVE_SOURCE__")
            && flatpak.contains("__CRIKEY_PYTHON_ARCHIVE_NAME__")
            && flatpak.contains("__CRIKEY_PYTHON_ARCHIVE_SHA256__"),
        "the Flatpak manifest must expose in-tree source placeholders for the generated manifest"
    );
    assert!(
        flatpak.contains("stage-python-runtime.sh")
            && flatpak.contains("--archive __CRIKEY_PYTHON_ARCHIVE_NAME__"),
        "the Flatpak manifest must validate and stage the supplied Python runtime"
    );
    assert!(
        linux_build.contains("require_tool flatpak-builder flatpak")
            && linux_build.contains("require_tool tar tar flatpak")
            && linux_build.contains("require_tool sha256sum coreutils flatpak")
            && linux_build.contains("require_flatpak_python_archive")
            && linux_build.contains("the \\`flatpak\\` target requires --python-archive"),
        "the Flatpak preflight must validate in-tree inputs before mixed-target staging"
    );
}

#[test]
fn every_platform_ships_the_licence_and_the_attribution_notice() {
    // Spec 14.13: the notice is part of the product, not of the source
    // repository. A build script that stops copying it produces an artefact
    // that distributes Apache-2.0 code with no licence text in it.
    let macos_build = read("packaging/macos/build.sh");
    assert!(
        macos_build.contains("/LICENSE\""),
        "the bundle must carry LICENSE"
    );
    assert!(
        macos_build.contains("/NOTICE.md\""),
        "the bundle must carry NOTICE.md"
    );

    let windows_build = read("packaging/windows/build.ps1");
    assert!(
        windows_build.contains("'LICENSE.txt'"),
        "the Windows stage must carry the licence"
    );
    assert!(
        windows_build.contains("'NOTICE.md'"),
        "the Windows stage must carry NOTICE.md"
    );

    let msi = without_comments(&read("packaging/windows/crikey.wxs"));
    assert!(msi.contains("LICENSE.txt"), "the MSI must install the licence");
    assert!(msi.contains("NOTICE.md"), "the MSI must install NOTICE.md");

    let linux_build = read("packaging/linux/build.sh");
    assert!(
        linux_build.contains("/LICENSE\""),
        "the Linux stage must carry LICENSE"
    );
    assert!(
        linux_build.contains("/NOTICE.md\""),
        "the Linux stage must carry NOTICE.md"
    );
}

#[test]
fn no_declared_field_of_a_distribution_artefact_carries_keypirinha_branding() {
    // Spec 14.13 and NOTICE.md: CriKey is independent, does not use the mark in
    // its product name, and ships no Keypirinha visual identity. Every string
    // an operating system reads out of these files -- display name,
    // manufacturer, publisher, shortcut, description, copyright -- is markup
    // rather than comment, so stripping the comments leaves precisely the
    // surface the requirement is about.
    for declaration in DECLARATIONS {
        let markup = without_comments(&read(declaration)).to_lowercase();
        assert!(
            !markup.contains("keypirinha"),
            "{declaration} declares a value naming Keypirinha; descriptive mentions \
             belong in comments and in NOTICE.md"
        );
    }
}

#[test]
fn the_macos_entitlements_grant_nothing_because_the_backend_asks_for_nothing() {
    // `MacOsBackend::capability` reports GlobalHotkeys, WindowActivation,
    // WindowEnumeration, Clipboard, Notifications, SecretStorage and FileSearch
    // as Unavailable, and the crate's only outward call is
    // `Command::new("/usr/bin/open")`. Nothing in it needs a hole in the
    // hardened runtime. An entitlement added here without a call site behind it
    // is a privilege in the signature that the program never redeems, so this
    // test fails on the first one and whoever adds it has to bring the
    // implementation with it.
    let entitlements = without_comments(&read("packaging/macos/Entitlements.plist"));
    assert!(
        !entitlements.contains("<key>"),
        "the entitlements file declares an entitlement; add the call site that redeems it first"
    );

    // The other half of the same claim: a usage-description string is a prompt
    // for a permission the program requests. CriKey sends no Apple Events, so a
    // prompt about them could never be answered.
    let info = without_comments(&read("packaging/macos/Info.plist"));
    assert!(
        !info.contains("NSAppleEventsUsageDescription"),
        "Info.plist asks for Apple Events consent that no code path requests"
    );
}

#[test]
fn the_macos_bundle_keeps_its_dock_icon_while_the_hotkey_backend_is_missing() {
    // An LSUIElement bundle has no Dock icon, no menu bar and no application
    // switcher entry. That is the right shape for a hotkey launcher and the
    // wrong shape for this one, because `Capability::GlobalHotkeys` is
    // Unavailable on macOS: with neither a Dock icon nor a hotkey the window
    // could be summoned only from a terminal. This assertion is the reminder
    // that the two flips belong in one change.
    let info = without_comments(&read("packaging/macos/Info.plist"));
    let position = info
        .find("<key>LSUIElement</key>")
        .expect("Info.plist states an LSUIElement decision rather than leaving it to the default");
    assert!(
        info[position..].contains("<false/>"),
        "LSUIElement is true while the macOS backend still reports GlobalHotkeys as Unavailable"
    );

    // The bundle's main executable, which macOS runs on a double click.
    assert!(
        info.contains("<key>CFBundleExecutable</key>"),
        "a bundle without CFBundleExecutable is not launchable"
    );
}

#[test]
fn the_msix_package_asks_only_for_full_trust() {
    // A packaged Win32 application needs runFullTrust and, because full trust
    // leaves the AppContainer behind, gets nothing enforceable from any other
    // capability. Declaring one anyway would advertise a restriction that does
    // not exist -- the manifest's comment block records the audit per
    // capability.
    let manifest = without_comments(&read("packaging/windows/AppxManifest.xml"));
    let capabilities = manifest
        .split_once("<Capabilities>")
        .and_then(|(_, rest)| rest.split_once("</Capabilities>"))
        .map(|(inside, _)| inside.to_owned())
        .expect("the manifest has a Capabilities section");

    assert!(
        capabilities.contains("Name=\"runFullTrust\""),
        "a full-trust desktop application must declare runFullTrust"
    );
    assert_eq!(
        capabilities.matches("Capability").count(),
        1,
        "exactly one capability element is expected, found: {capabilities}"
    );

    // No autostart implementation exists, so no startup extension may be
    // declared: it would put an entry in Windows' Startup Apps settings that
    // corresponds to nothing the program does.
    assert!(
        !manifest.contains("windows.startupTask"),
        "the manifest registers a startup task that CriKey does not implement"
    );
}

#[test]
fn the_msi_installs_per_user_and_removes_only_the_directories_it_created() {
    let msi = without_comments(&read("packaging/windows/crikey.wxs"));

    assert!(
        msi.contains("Scope=\"perUser\""),
        "the MSI must install per user: a launcher registers one session's hotkey and \
         reads one user's Start Menu, so an elevated machine-wide install buys nothing"
    );
    assert!(
        msi.contains("<StandardDirectory Id=\"LocalAppDataFolder\">"),
        "a per-user install belongs under %LOCALAPPDATA%"
    );

    // ICE38, which is what a per-user package installed under the profile has
    // to satisfy: a component keyed on one of its own files is reinstalled by
    // repair into every other profile on the machine, so each component is
    // keyed on an HKCU value and carries the explicit GUID that a registry key
    // path then needs. `wix build` runs no validation, so a file key path
    // creeping back is invisible everywhere except here.
    for element in msi.split("<File ").skip(1) {
        let (attributes, _) = element
            .split_once("/>")
            .expect("every File element is self-closing");
        assert!(
            !attributes.contains("KeyPath"),
            "a File element is a component key path, which ICE38 forbids under the user \
             profile: {attributes}"
        );
    }
    for component in msi.split("<Component ").skip(1) {
        let (body, _) = component
            .split_once("</Component>")
            .expect("every Component element is closed");
        assert!(
            body.contains("Root=\"HKCU\"") && body.contains("KeyPath=\"yes\""),
            "a component has no HKCU key path: {body}"
        );
        assert!(
            body.contains("Guid=\""),
            "a component keyed on the registry needs an explicit GUID, because WiX only \
             generates one for a file key path: {body}"
        );
    }

    // ICE64 asks for the directories created in the user profile to be listed
    // for removal. Exactly these, and nothing else: %APPDATA%\CriKey and
    // %LOCALAPPDATA%\CriKey hold the configuration, plugin state and caches
    // this package never created, and %LOCALAPPDATA%\Programs is shared with
    // every other per-user install on the machine.
    let mut removed: Vec<&str> = msi
        .split("<RemoveFolder ")
        .skip(1)
        .map(|element| {
            let (attributes, _) = element
                .split_once("/>")
                .expect("every RemoveFolder element is self-closing");
            let (_, rest) = attributes
                .split_once("Directory=\"")
                .expect("every RemoveFolder names a directory");
            rest.split_once('"').expect("the attribute is quoted").0
        })
        .collect();
    removed.sort_unstable();
    assert_eq!(
        removed,
        [
            "INSTALLFOLDER",
            "LegacyShimFolder",
            "ModernSdkFolder",
            "PythonRuntimeFolder",
            "ShortcutFolder"
        ],
        "the uninstaller removes a directory it did not create, or leaves one it did"
    );
    assert!(
        !msi.contains("<RemoveFile") && !msi.contains("<RemoveRegistryKey"),
        "the uninstaller deletes a file or a registry key it did not install"
    );
    assert!(
        !msi.contains("<StandardDirectory Id=\"AppDataFolder\">"),
        "the MSI reaches into %APPDATA%, where the user's configuration lives"
    );

    // Validation is what would otherwise catch a broken key path, and it is a
    // separate subcommand: `wix build` neither validates nor accepts -sice, so
    // the suppressions belong on `wix msi validate` and nowhere else. Three,
    // exactly: anything further would hide a real finding.
    let build = read("packaging/windows/build.ps1");
    assert!(
        build.contains("msi validate '-sice' 'ICE38' '-sice' 'ICE64' '-sice' 'ICE91'"),
        "build.ps1 must validate the MSI with exactly the three ICEs crikey.wxs argues are \
         inapplicable to a per-user package"
    );
    let (_, wix_arguments) = build
        .split_once("$wixArgs = @(")
        .expect("build.ps1 assembles the wix build arguments");
    let (wix_arguments, _) = wix_arguments
        .split_once("& $wix @wixArgs")
        .expect("build.ps1 invokes wix with those arguments");
    assert!(
        !wix_arguments.contains("sice"),
        "`wix build` has no -sice switch and fails on one: {wix_arguments}"
    );

    // The shortcut targets the GUI binary and passes nothing. It used to pass
    // `run` to `crikey.exe`, which worked for the MSI and could never work for
    // the two formats that cannot pass arguments at all; one graphical entry
    // point that needs none is what makes all three agree.
    assert!(
        msi.contains("Target=\"[#CrikeyLauncherExe]\""),
        "the Start Menu shortcut must launch the GUI binary, not the console CLI"
    );
    assert!(
        !msi.contains("Arguments="),
        "the Start Menu shortcut passes arguments; `crikey-launcher` takes none, and \
         needing them is what broke the MSIX tile and the macOS bundle"
    );
}

#[test]
fn the_launch_paths_that_cannot_pass_arguments_all_name_the_gui_binary() {
    // The defect this pins: a macOS Launch Services open and an MSIX tile both
    // start the declared executable with NO arguments, and neither format has
    // a manifest key for supplying any. Pointed at `crikey`, which prints
    // usage and exits when bare, a double click or a tile click produces
    // nothing a user can see. `crikey-launcher` is the entry point that needs
    // no arguments, so these declarations must name it.
    let info = without_comments(&read("packaging/macos/Info.plist"));
    let executable = info
        .split_once("<key>CFBundleExecutable</key>")
        .and_then(|(_, rest)| rest.split_once("</string>"))
        .map(|(value, _)| value.to_owned())
        .expect("Info.plist declares CFBundleExecutable");
    assert!(
        executable.contains(">crikey-launcher"),
        "CFBundleExecutable is not the GUI binary, so a double-clicked bundle shows \
         usage text instead of the launcher: {executable}"
    );

    let manifest = without_comments(&read("packaging/windows/AppxManifest.xml"));
    assert!(
        manifest.contains("Executable=\"crikey-launcher.exe\""),
        "the MSIX tile must run the GUI binary; a tile cannot pass `run`"
    );

    // `crikey-launcher` staged beside `crikey`, on both platforms that install
    // a directory: naming a binary in a manifest and not shipping it produces
    // an artefact that installs and then cannot start.
    let msi = without_comments(&read("packaging/windows/crikey.wxs"));
    assert!(
        msi.contains("\\crikey-launcher.exe\"") && msi.contains("\\crikey.exe\""),
        "the MSI must install both binaries"
    );
    let macos_build = read("packaging/macos/build.sh");
    assert!(
        macos_build.contains("/crikey-launcher\"") && macos_build.contains("/crikey\""),
        "build.sh must stage both binaries into Contents/MacOS"
    );
}

#[test]
fn the_command_line_is_what_a_terminal_reaches_on_every_platform() {
    // The other half of the split. `crikey` is the CLI and must stay the name
    // a terminal resolves: the MSI's per-user PATH entry and App Paths key,
    // and inside an MSIX -- where both of those are virtualised away -- the
    // execution alias. An alias resolves to the Executable of the Application
    // that declares it, so the alias must not sit on the tile's entry, or
    // typing `crikey` would start the GUI.
    let msi = without_comments(&read("packaging/windows/crikey.wxs"));
    assert!(
        msi.contains("App Paths\\crikey.exe"),
        "the MSI must keep registering the command line under App Paths"
    );
    assert!(
        msi.contains("<Environment Id=\"UserPath\""),
        "the MSI must keep the per-user PATH entry that exposes `crikey`"
    );

    let manifest = without_comments(&read("packaging/windows/AppxManifest.xml"));
    let alias_application = manifest
        .rmatch_indices("<Application ")
        .map(|(start, _)| &manifest[start..])
        .find(|application| application.contains("windows.appExecutionAlias"))
        .expect("the MSIX declares an execution alias");
    assert!(
        alias_application.contains("Executable=\"crikey.exe\""),
        "the `crikey` execution alias resolves to its own Application's Executable, \
         which must be the console CLI: {alias_application}"
    );
    assert!(
        alias_application.contains("AppListEntry=\"none\""),
        "the command line's Application entry must be hidden from the Start menu, or \
         one product appears in the app list twice"
    );
}

#[test]
fn the_gui_binary_refuses_arguments_instead_of_ignoring_them() {
    // `crikey-launcher` exists precisely because its callers cannot pass
    // arguments, so anything on its command line is a mistake -- most likely
    // someone reaching for a `crikey` subcommand. Silently discarding it would
    // start the launcher and look like the subcommand had run and done
    // nothing. EX_USAGE, named, is the only honest answer.
    //
    // Only the refusal is exercised here. The success path opens a window and
    // blocks on an event loop, which is not something an integration test can
    // assert on a headless machine.
    let output = Command::new(env!("CARGO_BIN_EXE_crikey-launcher"))
        .arg("run")
        .output()
        .expect("the launcher binary runs");

    assert_eq!(
        output.status.code(),
        Some(64),
        "an argument to `crikey-launcher` must be EX_USAGE, not a silent launch"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("run"),
        "the refusal must name the argument it rejected, got: {stderr}"
    );
    assert!(
        stderr.contains("crikey-launcher"),
        "the refusal must name the program, got: {stderr}"
    );
}

#[test]
fn the_linux_desktop_entry_runs_the_launcher_the_packager_actually_installs() {
    // Same contract as the Start Menu shortcut and the macOS bundle, in the
    // format that expresses it: a desktop entry launched from a menu, a
    // file-manager double-click or a session autostart passes no arguments,
    // and bare `crikey` prints usage. `StartupWMClass` follows the same name
    // because winit derives the X11 class from argv[0], so pointing the entry
    // at one binary while declaring the other's class silently breaks icon and
    // taskbar association.
    let entry = read("packaging/linux/crikey.desktop");
    for line in [
        "Exec=crikey-launcher",
        "TryExec=crikey-launcher",
        "StartupWMClass=crikey-launcher",
    ] {
        assert!(
            entry.contains(&format!("\n{line}\n")),
            "packaging/linux/crikey.desktop must declare `{line}`"
        );
    }

    // And the entry must name something the packager puts in the tree.
    let build = read("packaging/linux/build.sh");
    assert!(
        build.contains("lib/crikey/crikey-launcher"),
        "packaging/linux/build.sh must install the executable the desktop entry runs"
    );
}
