# Packaging

Distribution artefacts, one directory per platform. Everything here is a
checked-in, reviewable definition plus the scripts that drive a real toolchain;
no built installer, bundle, certificate or signing key is in this repository,
and none ever should be.

## What every artefact must contain

Four things, on every platform:

- **`LICENSE` and `NOTICE.md`** (spec §14.13). The notice is part of the
  product, not of the source repository. `NOTICE.md` is also where the
  Keypirinha non-affiliation statement lives.
- **`modern-sdk/` and `legacy-shim/`, as siblings of the installed executable.**
  `sdk_root()` in `crikey-python-host` resolves `exe.parent().join("modern-sdk")`
  and `shim_root()` in `crikey-legacy-compat` resolves
  `exe.parent().join("legacy-shim")`, each falling back to a repository-relative
  development path that does not exist on a user's machine. An artefact that
  ships only the executable produces a launcher whose modern Python plugins and
  legacy packages fail the first time one is invoked, and not before.
- **`crikey-wasm-host` and `crikey-cabi-host`, beside the launcher.** The WASM
  and C-ABI providers resolve these supervised worker executables beside the
  running launcher and never search `PATH`. A package without either host
  truthfully cannot run that plugin runtime, so every platform packager stages
  and installs both.
- **No Keypirinha branding.** Spec §14.13: the mark is not part of the product
  name, the artefacts carry none of its visual identity, and no declared field
  an operating system reads — display name, manufacturer, publisher, shortcut,
  description, copyright — names it. The word appears only in explanatory
  comments and in the attribution notice, which is the descriptive use §14.13
  permits. The same test enforces this against the four declaration files.

The Python runtime is a required payload for Flatpak and an optional payload
for the tarball, `.deb` and `.rpm`. `python-runtime/` beside the executable is
staged by `packaging/stage-python-runtime.sh` when a python-build-standalone
archive is supplied. For the three non-sandboxed formats, omitting it leaves
CriKey to use an interpreter already on the machine; those packagers say so on
every run rather than implying an interpreter they did not stage (spec §15.4).
The Flatpak target rejects an omitted archive, validates the supplied runtime,
and injects it into the sandbox because `org.freedesktop.Platform` does not
guarantee a usable `python3`.

## windows/

| File | What it is |
| --- | --- |
| `crikey.wxs` | **WiX v5** per-user MSI. Not v3: the toolset is a pinnable `dotnet tool`, and the `Files` element harvests the two Python payload trees without generating an unreviewed `heat.exe` fragment on every build. Not v4, which rejects `Files` (WIX0005), and not v6 or later, which refuse to build without an Open Source Maintenance Fee licence (WIX7015). |
| `AppxManifest.xml` | MSIX manifest for the modern route. Full-trust Win32 application, one capability. |
| `build.ps1` | Stages the release layout and drives whichever toolchain is present. |
| `sign.ps1` | Authenticode signing and verification, parameterised from the environment. |

**The MSI** installs to `%LOCALAPPDATA%\Programs\CriKey` with
`Scope="perUser"` and no elevation. It installs *both* binaries side by side —
`crikey.exe`, the console command line, and `crikey-launcher.exe`, the
graphical entry point — and registers the command line two ways: an `HKCU`
`App Paths\crikey.exe` key for `Win+R` and `ShellExecute`, and a per-user
`PATH` entry for a terminal. The Start Menu shortcut targets
`crikey-launcher.exe` with no arguments.
The same directory also installs `crikey-wasm-host.exe` and
`crikey-cabi-host.exe`; the MSI lists both as required executable components,
and the MSIX stage carries them in the package directory.

Two binaries rather than one because they want opposite things from Windows.
`crikey` is a console-subsystem program: every subcommand's answer goes to
stdout or stderr, and a GUI-subsystem process has no console to write to.
`crikey-launcher` is a GUI-subsystem program (`#![windows_subsystem =
"windows"]`), which is the only way to launch the UI without a console window
appearing beside it. Neither is a copy of the other: both call one
implementation in the `crikey-cli` library.

**Upgrades.** No `ProductCode` is authored, so `wix build` generates a fresh
one per build and a version bump is a major upgrade: `FindRelatedProducts`
matches the installed product on `UpgradeCode` and `RemoveExistingProducts`
takes it away. Re-running an `.msi` that is already installed finds its own
`ProductCode` registered and offers maintenance mode, which is the correct
answer and not an upgrade failure. `AllowSameVersionUpgrades="yes"` because
rebuilding an already-released version number is normal while a release is
being repaired, and without it MSI treats same version plus different
`ProductCode` as two products: two Add/Remove Programs entries over one
directory, and removing either leaves the files behind because the other still
references the same components. `RemoveExistingProducts` is scheduled
`afterInstallInitialize` rather than `afterInstallValidate`; both remove the
installed product before the new one is laid down, but a failed upgrade under
the latter leaves the machine with neither version installed.

Uninstall removes everything the package installed: the files, the directories
it created to hold them (`CriKey\`, `modern-sdk\`, `modern-sdk\crikey_sdk\`,
`legacy-shim\` and `python-runtime\` when present), the shortcut and its
folder, the `App Paths` key, and the `PATH` fragment (`Part="last"`,
`Permanent="no"`, so the rest of `PATH` is left as found). Windows Installer
removes a directory only once it is empty, and the directories `<Files>`
harvesting invents for a payload tree's subdirectories carry no removal row of
their own — so each is declared in `crikey.wxs` with a `RemoveFolder` beside
it, and `build.ps1` fails the build when a staged tree grows one that is not.
The exception is `-PythonRuntimeArchive`: an interpreter tree's directories
are not knowable at author time, so that opt-in build leaves them behind
empty. Uninstall does not touch `%LOCALAPPDATA%\Programs`, which every
per-user install on the machine shares. It deliberately keeps
`%APPDATA%\CriKey` (configuration, plugin state, startup journal) and
`%LOCALAPPDATA%\CriKey` (icon and catalog caches). Reinstalling a launcher must
not silently discard the user's plugins and hotkeys.

**Validation** is not part of `wix build`. The WiX .NET tool exposes it as the
separate `wix msi validate` subcommand, and only an MSBuild `.wixproj`
validates automatically, so the documented build command produces an `.msi`
without ever running an ICE. `build.ps1 -Validate` runs the subcommand, with
`-sice ICE38 -sice ICE61 -sice ICE64 -sice ICE91` and nowhere else — `wix
build` has no such switch. Those four describe a package this one cannot be:
ICE38 and ICE64 are per-user-profile rules, `crikey.wxs` satisfies both for
every component and directory it authors by hand, and the components WiX
generates for the harvested Python trees cannot carry a registry key path at
all; ICE61 objects to a package that upgrades its own version, which is what
`AllowSameVersionUpgrades` deliberately asks for. The reasoning is recorded per
ICE in the file's header, together with why `Scope="perUserOrMachine"` under
`ProgramFiles6432Folder` — the ICE-clean alternative — is not taken: it would
put the default install behind a UAC prompt the user gains nothing from.

**The MSIX** declares exactly one capability, `rescap:runFullTrust`, and
`EntryPoint="Windows.FullTrustApplication"`. The manifest's comment block
records the audit for every capability that is *not* declared —
`internetClient`, `packageQuery`, `broadFileSystemAccess`,
`windows.startupTask` — and why declaring it would advertise a restriction or a
feature that does not exist. It carries two `<Application>` entries: the tile
runs `crikey-launcher.exe`, because a tile starts its executable with no
arguments and has no manifest key for supplying any, and a second entry hidden
with `AppListEntry="none"` carries the `crikey.exe` execution alias, which is
what replaces the MSI's `PATH` entry inside a package. The alias has to hang
off its own entry: an execution alias resolves to the `Executable` of the
application that declares it, so putting it on the tile entry would make
`crikey` in a terminal start the GUI.

**Code signing** takes its certificate from the environment and nowhere else:
`CRIKEY_WINDOWS_CERT_THUMBPRINT` for a certificate in the store (preferred: the
private key never becomes a file), or `CRIKEY_WINDOWS_CERT_PATH` with
`CRIKEY_WINDOWS_CERT_PASSWORD`. The password is never a command-line argument,
because arguments are readable by every process on the machine — which is also
why the PFX route does not reach `signtool /f /p`: `sign.ps1` imports the PFX
into `Cert:\CurrentUser\My` for the duration of the signature, signs by
thumbprint like the preferred route, and removes it again whether the signature
succeeded or failed.

Both the artefact and its payload are signed. The four staged executables are
signed before packaging and the `.msi`/`.msix` after it is built, because an
MSI signature covers the `.msi` file alone: a signed MSI containing an unsigned
`crikey-launcher.exe` still meets SmartScreen on the first Start Menu launch,
and each supervised host would be unsigned beside it. This is the same
nested-executable pass the macOS section describes. Every signature is RFC 3161
timestamped and verified with `signtool verify /pa` immediately after it is
applied; there is no switch anywhere in these scripts that skips, weakens or
disables verification.

## macos/

| File | What it is |
| --- | --- |
| `Info.plist` | Bundle metadata. `__CRIKEY_VERSION__` is substituted at build time. |
| `Entitlements.plist` | Hardened-runtime entitlements, with a per-entitlement audit. |
| `build.sh` | Assembles and signs `CriKey.app`. |
| `notarize.sh` | Submits to Apple's notary service and staples the ticket. |
| `dist.sh` | Produces the distributable `.zip` and/or `.dmg` from the stapled bundle. |

**Bundle identity** is `dev.crikey.CriKey`, the namespace the project already
uses. `LSMinimumSystemVersion` is 11.0: the build ships an arm64 slice and Big
Sur is the first release that exists on Apple silicon, while nothing in the tree
calls a newer API.

**`CFBundleExecutable` is `crikey-launcher`**, not `crikey`. Launch Services
starts a bundle's main executable with no arguments and has no key for
supplying any, and bare `crikey` prints usage and exits — a double-clicked
bundle pointed at it would show nothing. `build.sh` stages all four
executables into `Contents/MacOS`, which is also where `sdk_root()`,
`shim_root()` and the WASM/C-ABI host resolvers need them: those resolve beside
the *running* executable, so whichever of the four started must find the
payload trees and supervised hosts next to itself. The hosts are signed by the
nested-executable pass before the bundle is signed; the bundle signature covers
only the main executable.

**`LSUIElement` is `false`** — a Dock icon — which is the opposite of what a
hotkey launcher normally wants. The reason is that `MacOsBackend::capability`
reports `Capability::GlobalHotkeys` as `Unavailable`: there is no
`RegisterEventHotKey` or `CGEventTap` registration in the macOS backend. An
agent-style bundle with no Dock icon *and* no global hotkey could be summoned
only from a terminal. The flag flips to `true` in the same change that makes
that capability `Available`, and a test asserts the pairing.

**The entitlements dictionary is empty**, and that is the finding rather than
an omission. The hardened runtime is enabled; entitlements are the holes
punched in it, and no call site in `crates/crikey-platform-macos` redeems one.
The file audits each candidate by name — Apple Events, library validation, JIT,
unsigned executable memory, `DYLD_*`, debugger, keychain groups, App Sandbox —
against the code that exists. The macOS backend's entire outward reach is
`Command::new("/usr/bin/open")`, a posix_spawn of a system binary, and every
capability that would need a grant (window activation and enumeration,
clipboard, notifications, secret storage, file search) is reported
`Unavailable`. Third-party plugins are supervised subprocesses, never dlopened,
so no library-validation hole is needed either.

**Signing and notarization** are separate steps on purpose: notarization is a
network round trip that can take minutes and fail for reasons unrelated to the
bundle, and the right response is to retry it, not to reassemble a bundle that
was already correct. Credentials come from `CRIKEY_CODESIGN_IDENTITY` and from
either `CRIKEY_NOTARY_PROFILE` or the
`CRIKEY_NOTARY_APPLE_ID`/`CRIKEY_NOTARY_TEAM_ID`/`CRIKEY_NOTARY_PASSWORD`
triple. `build.sh` signs nested Mach-O objects before the bundle rather than
using the deprecated `--deep`, which would apply the top-level entitlements to
nested code. Order matters at the end too: the distributable is built from the
*stapled* bundle, and `dist.sh` refuses an unstapled one, because a ticket
stapled after the archive was made is not in the download the user gets.

## linux/

| File | What it is |
| --- | --- |
| `build.sh` | The one driver: stages the install tree, then emits each artefact from it. |
| `crikey.desktop` | freedesktop desktop entry. |
| `icons/hicolor/scalable/apps/crikey.svg` | The icon, scalable only. |
| `org.crikey.CriKey.metainfo.xml` | AppStream metadata, which flatpak-builder requires. |
| `deb/control.in` | Debian binary control template. |
| `rpm/crikey.spec` | RPM recipe; packages the staged tree rather than building a second time. |
| `flatpak/org.crikey.CriKey.yaml` | Flatpak manifest. |
| `tests/build.test.sh` | Contract tests for all of the above. |

One script owns the shared install contract, from one staged tree for the
tarball, `.deb` and `.rpm`; the Flatpak manifest mirrors that contract while
building inside the freedesktop SDK sandbox.

```sh
packaging/linux/build.sh --help                       # usage and per-target tools
packaging/linux/build.sh                              # stage + tarball, into target/packaging/linux
packaging/linux/build.sh --targets all /tmp/out       # everything, into /tmp/out
packaging/linux/build.sh --binary target/release/crikey \
  --launcher-binary target/release/crikey-launcher \
  --wasm-host-binary target/release/crikey-wasm-host \
  --cabi-host-binary target/release/crikey-cabi-host --targets deb .
packaging/linux/build.sh --targets flatpak \
  --python-archive /path/to/pinned/python-build-standalone.tar.gz /tmp/flatpak
```

It never needs root, writes only inside the output directory it is given, and
is safe to re-run: each target clears its own working tree first. Timestamps
come from `$SOURCE_DATE_EPOCH` (default: the commit time of `HEAD`), so two
builds of one commit produce byte-identical archives and packages.

| Artefact | Recipe | Required tool |
| Staged tree (see the layout below) | `linux/build.sh` | `cargo`, unless all four executable paths are given |
| `crikey-<version>-<arch>-linux.tar.gz` | `linux/build.sh --targets tarball` | `tar`, `gzip` |
| `crikey_<version>_<arch>.deb` | `linux/deb/control.in` | `dpkg-deb` (package `dpkg`); `dpkg-shlibdeps` (`dpkg-dev`) when present |
| `crikey-<version>-1.<arch>.rpm` | `linux/rpm/crikey.spec` | `rpmbuild` (package `rpm-build`) |
| Flatpak | `linux/flatpak/org.crikey.CriKey.yaml` | `flatpak-builder`, `tar`, `sha256sum` (coreutils), the runtime and SDK from Flathub, and a validated `--python-archive` |

Every target checks for its tool, and for the prefix its format allows, before
anything is built, and fails naming both the tool and the package that provides
it. No target quietly produces nothing, and a missing `rpmbuild` costs no
release compile.

The tarball is prefix-relative, so a distro-agnostic install is:

```sh
sudo tar --strip-components=1 -C /usr/local -xf crikey-<version>-x86_64-linux.tar.gz
```

### Installed layout, and why it is shaped this way

```
<prefix>/bin/crikey                -> ../lib/crikey/crikey            (symlink)
<prefix>/bin/crikey-launcher       -> ../lib/crikey/crikey-launcher   (symlink)
<prefix>/lib/crikey/crikey                          (the command-line entrypoint)
<prefix>/lib/crikey/crikey-launcher     (no arguments; what the menu entry runs)
<prefix>/lib/crikey/crikey-wasm-host    (supervised WASM runtime host)
<prefix>/lib/crikey/crikey-cabi-host    (supervised C-ABI runtime host)
<prefix>/lib/crikey/modern-sdk/                              (sdk/python)
<prefix>/lib/crikey/legacy-shim/   (crates/crikey-legacy-compat/python)
<prefix>/lib/crikey/python-runtime/    (optional for tarball/deb/rpm; required in Flatpak)
<prefix>/share/{applications,icons/hicolor/scalable/apps,metainfo,doc/crikey}/
```

Two executables, because bare `crikey` prints usage and the UI is behind its
`run` subcommand, while a menu entry, a file-manager double-click and a session
autostart all invoke a program with no arguments. `crikey-launcher` is that
program, and `Exec=`/`TryExec=` in the desktop entry name it. The packager
refuses to build without it rather than shipping a menu entry that does
nothing.

The payload directories the preamble describes do not have to clutter `bin/`:
`std::env::current_exe` reads `/proc/self/exe` and reports the resolved target,
so with the symlinks above the directory either executable sees as its own is
`lib/crikey`, which is where the resolvers then find them. `/usr/lib/crikey`
rather than `%{_libdir}`, because that macro is `/usr/lib64` on 64-bit Fedora
and the path has to be the one the `.deb` and the tarball also use.

### Known gaps in the Linux set

- **Wayland windows carry no `app_id`.** The desktop entry's
  `StartupWMClass=crikey-launcher` matches the X11 `WM_CLASS` winit derives
  from `argv[0]`, but `crikey-ui` never sets a window name and winit sets a
  Wayland `app_id` only when asked, so a Wayland compositor cannot associate
  the window with the entry: no icon, no grouping. The fix is in the
  application.
- **The Flatpak cannot launch host applications.** CriKey launches with
  `Command::new`, which inside a sandbox spawns inside the sandbox; routing
  launches through `flatpak-spawn --host` is not implemented. The manifest
  deliberately withholds `--filesystem=host-os` for that reason — listing host
  applications CriKey could not start would be worse than listing none — and
  says so at the top of the file. Treat the Flatpak as buildable, not as a
  supported distribution channel.
- **The Flatpak build needs the network.** Cargo fetches crates during the
  build, which Flathub forbids. A submission there must vendor the dependencies
  first and drop the `--share=network` build argument.
- **The `.deb` maintainer address is a placeholder.** Set `$CRIKEY_MAINTAINER`
  to a real `Name <address>` before publishing anywhere.
- **Nothing is signed and no repository metadata is generated.** Signing keys,
  `dpkg-sig`/`rpm --addsign` and `apt`/`dnf` repository layout are manual.
- **Only one scalable icon ships.** The hicolor theme resolves it at any size,
  so there is no PNG ladder; a desktop that insists on rasterised icons falls
  back to its generic one.
- **No autostart entry.** A launcher's hotkey exists only once the process
  runs, but installing something into every user's session is the operator's
  decision; the menu entry, or a user-created `~/.config/autostart` entry
  running `crikey-launcher`, is the route for now.

### What has actually been run

`packaging/linux/tests/build.test.sh` asserts the contracts that rot quietly:
the licence and notice reach every artefact, the worker trees land beside the
real executable, a package with no bundled interpreter declares `python3`, the
tarball is prefix-relative, two builds agree byte for byte, and every refusal
path exits non-zero with a message. It packages `/usr/bin/true` and
`/usr/bin/false` as stand-ins rather than building CriKey, so it runs in
seconds and needs no Rust toolchain. It passes,
37 checks, with the `.deb` checks included.

The `.rpm` and Flatpak recipes are checked in but have not been executed:
neither `rpmbuild` nor `flatpak-builder` was installed on the machine they were
written on. Treat them as unverified until a release build runs them.

## What each toolchain requires

No artefact can be built, signed or verified anywhere but on its own platform,
and the scripts do not pretend otherwise — each stops with a named error saying
which tool is missing and what provides it.

| Step | Needs | Why |
| --- | --- | --- |
| `cargo build --release` for the target | A Windows or macOS host, or a cross toolchain | The platform backends link Win32 / are `cfg(target_os = "macos")`. |
| `wix build` | Windows | WiX v5 emits an MSI through Windows Installer libraries. |
| `wix msi validate` (`build.ps1 -Validate`) | Windows, under an interactive or administrator account | The stock ICEs are custom actions executed through Windows Installer, which the non-interactive service accounts on hosted CI machines cannot do. |
| `makeappx.exe pack` | Windows SDK on Windows | No cross-platform packer exists. |
| `signtool sign` / `verify` | Windows SDK on Windows | Authenticode signing and the CryptoAPI trust evaluation. |
| `lipo`, `plutil`, `ditto`, `codesign` | macOS with Xcode command line tools | Apple binaries; no substitutes. |
| `xcrun notarytool` / `stapler` | macOS, plus an Apple Developer account | Notarization is an online Apple service. |
| `hdiutil` | macOS | The dmg format is produced by the OS. |
| `dpkg-deb`, `dpkg-shlibdeps` | Any Linux with `dpkg`/`dpkg-dev` | The archive format and the ELF-derived shared-library dependencies. |
| `rpmbuild` | Any Linux with `rpm-build` | Builds the `.rpm` from the checked-in spec. |
| `flatpak-builder` | Linux, plus the freedesktop runtime and SDK from Flathub | The build happens inside the Flatpak sandbox, not on the host. |

## Known gaps

These are open defects, recorded here rather than papered over. They affect the
Windows and macOS artefacts; the Linux ones are listed under `linux/` above.

- **No artwork is checked in.** There is no `.icns` and there are no MSIX
  logos. `build.sh` omits `CFBundleIconFile` entirely rather than pointing at a
  file that is not there, and `build.ps1` refuses the MSIX route without
  `-MsixAssets`, naming the three files and pixel sizes it needs.
- **Nested `RemoveFolder` ordering is unverified on Windows.** `crikey.wxs`
  now declares a removal row for every directory it creates, including the
  nested `modern-sdk\crikey_sdk`. The `RemoveFile` table documentation
  specifies no order in which those rows are processed, and Windows Installer
  removes a directory only when it is already empty, so a parent processed
  before its child would still be left behind. Confirm on a real host with
  `msiexec /x CriKey-<version>-x64.msi /l*v uninstall.log`: the `RemoveFiles`
  section names each directory it removed, and
  `%LOCALAPPDATA%\Programs\CriKey` must not exist afterwards.
- **The per-user `PATH` entry is not visible to an already-running shell.**
  The MSI writes `HKCU\Environment\Path` through the `Environment` table and
  Windows Installer broadcasts `WM_SETTINGCHANGE`, but a terminal that was
  open before the install keeps the environment it inherited, so `crikey`
  appears not to be installed. `reg query HKCU\Environment /v Path` shows
  whether the entry was written; if it names
  `%LOCALAPPDATA%\Programs\CriKey`, a new terminal is all that is needed and
  there is nothing to fix in the package. `Win+R` then `crikey` works
  immediately either way, through the `App Paths` key.
