# CriKey user guide

CriKey is an independent, keyboard-driven launcher. It is not Keypirinha and is
not affiliated with Keypirinha or its authors. The compatibility layer exists
only to run documented Keypirinha packages.

## Install and start

A packaged release includes the `crikey` console command and the graphical
`crikey-launcher` entry point. A checkout can run the same binaries with Cargo:

```sh
# The package ships two binaries, so `--bin` is required.
cargo run -p crikey-cli --bin crikey -- version
cargo run -p crikey-cli --bin crikey -- run
cargo run -p crikey-cli --bin crikey-launcher
```

`crikey` with no arguments and `crikey-launcher` both start the resident
launcher: the window opens, dismissing it with Escape only hides it, and the
process stays running so the activation hotkey can bring it back. Quit it for
good from the settings surface's Quit button. A subcommand — `crikey version`,
`crikey plugin list` — is command-line output as before, and `crikey --help`
lists them. `crikey run --help` documents the supported one-shot override:
`--set key=value`, which changes that launch only. The launcher takes an
exclusive per-user lock, so a second instance and an install racing a running
launcher are refused.

Python in a release uses the bundled runtime when the packager staged one --
which the Flatpak always does and the other formats do only when built with
`--python-archive` -- and workers run with `-S`, so system-wide `site-packages`
is never on their import path either way. Without a staged runtime the host
falls back to a configured or discovered interpreter. A development checkout falls through to `python3`; set
`CRIKEY_PYTHON` to select an interpreter explicitly. See
[development notes](development.md#python) for runtime layout.

## Configuration

New CriKey and modern-plugin settings are TOML. The global user file is
`config.toml` in the platform configuration directory; named profiles are
`profiles/<name>.toml`, and per-plugin settings are `plugins/<plugin-id>.toml`.
The precedence is built-in defaults, administrator policy, user-global,
profile, plugin defaults, user-plugin settings, then this-session overrides.
Legacy packages retain their legacy configuration syntax.

Inspect the effective settings without starting plugins:

```sh
crikey config list
crikey config get <key>
crikey config layers [<key>]
```

The launcher's own settings — the activation hotkey, the result ceiling, the
selected profile — also have a surface that does not require knowing a key
name. Press `Ctrl+,` in the launcher, or use the footer's Settings button, to
edit them in place; the same rows are readable and writable from a terminal:

```sh
crikey settings
crikey settings set launcher.activation-hotkey Ctrl+Alt+K
```

Both write the user-global layer, so the panel and the command line always
agree. A hotkey the desktop refuses leaves the previous one working and says
so rather than leaving the launcher unreachable.

Secret fields are redacted. Configuration read errors are reported and a
launcher run continues with built-in defaults; fix the reported file before
relying on the setting. For isolated development, the standard-directory
locations can be overridden with `CRIKEY_CONFIG_DIR`, `CRIKEY_DATA_DIR`,
`CRIKEY_CACHE_DIR`, and `CRIKEY_STATE_DIR`.

### Aliases

Some names are not abbreviations of anything. `ss` for Settings and `vsc` for
Visual Studio Code are letters from inside a word, and no matching rule can
separate them from coincidences without dragging every coincidence back in.
They are names *you* have for a thing, so you say what they are:

```toml
[aliases]
ss = "Settings"
vsc = "Visual Studio Code"
snd = "Sound"
```

An alias replaces the word you typed with the words it stands for, so `snd rec`
searches for `Sound Rec` and finds Sound Recorder. It applies to whole words
only: an alias for `ss` never rewrites the middle of `press`.

The alias table obeys the same layer precedence as everything else, and an
empty value retracts an alias a lower layer defined, which is how a profile
drops one it inherits from your global file:

```toml
[aliases]
ss = ""
```

Because an alias replaces the word, the literal reading is not also tried: once
`ss` means Settings, something whose own name reads `ss` is no longer found by
it. Edits take effect on your next query, not your next restart.

Plugin discovery roots are optional environment path lists:

* `CRIKEY_MODERN_PLUGIN_ROOTS` — directories containing modern Python plugin
  directories (`<id>/crikey.toml`).
* `CRIKEY_NATIVE_PLUGIN_ROOTS` — native plugin directories.
* `CRIKEY_LEGACY_PACKAGE_ROOTS` — legacy package directories or archives.

Each root is followed by the corresponding per-user directory used by
`crikey plugin install`; an unset root loads no development plugins.

## Keyboard and query behavior

The launcher is usable without a mouse:

| Key | Action |
| --- | --- |
| Up / Down | Select the previous / next result |
| Page Up / Page Down | Move by a page |
| Tab | Complete the query |
| Enter | Execute the selected result's default action |
| Alt+Enter | Open the selected result's alternate actions |
| Ctrl+, | Open the settings surface |
| Escape | Close the settings surface, else cancel or dismiss the launcher |
| 1–9 (actions open) | Choose that alternate action |

An empty query shows nothing but the query field: the launcher does not list
the catalog at rest, and the window stays compact until there is something to
show. Scrolling the result list with the wheel stays where you put it; the
list only scrolls itself to follow a selection you moved.

Typing updates the visible query immediately. Local catalog search runs in the
current UI frame; plugin results arrive incrementally and late results from an
older query are discarded. Modern plugins apply their manifest's debounce and
activation policy. Unchanged legacy plugins use `legacy-strict`: no time
debounce, prompt replacement of obsolete work, serial callbacks, and
`should_terminate()` cooperative cancellation. Empty-query and minimum-length
behavior is a plugin declaration for modern plugins, not a hidden global rule.

Catalog rows are fuzzy-matched by the host. Plugin suggestion batches are shown
in the order the plugin publishes them; they are not host-fuzzy-matched or
re-ranked by `score_hint`. This distinction is intentional (see
[architecture](architecture.md#two-paths-two-matching-contracts)).

## Plugins, signing, and diagnosis

List, inspect, install, and control plugins with:

```sh
crikey plugin list
crikey plugin search <query>
crikey plugin show <id>
crikey plugin index update
crikey plugin install <directory-or-archive-or-url-or-id>
crikey plugin remove <id>
crikey plugin enable <id>
crikey plugin disable <id>
crikey plugin doctor [<id>]
crikey plugin scheduling-profile <id> [legacy-strict|legacy-optimized|modern]
```

`enable` and `disable` take effect at the next launch. `doctor` reports
manifest, runtime, scheduling, concurrency, and legacy compatibility findings;
exit status 1 means a degraded plugin. An indexed install checks the published
package digest and signer policy. A package without a compatible platform
entrypoint is reported unavailable rather than loaded.

For package authors and operators, package operations are explicit:

```sh
crikey package build --plugin DIR [--out FILE]
crikey package inspect --package FILE
crikey package verify --package FILE
crikey package sign --package FILE --key FILE|--key-env NAME [--out FILE]
crikey package keygen --out FILE [--public-out FILE]
crikey package trust-add --name NAME --key FILE|--key-file FILE
crikey package trust-list
crikey package trust-remove --name NAME
```

Unsigned-policy configuration and trust-store details are in
[ADR-0012](adr/0012-package-signing.md) and the package command's `--help`.
Do not describe a digest as publisher identity: hashes detect changed bytes;
trusted signatures establish provenance.

## Platform limits and honest status

Windows, macOS, and Linux use separate platform backends. Optional capabilities
are reported as available, unavailable, permission-gated, partially supported,
or unsupported by the desktop environment. Linux window control is optional;
Wayland support depends on compositor protocols. macOS accessibility and
Keychain access and Windows registry/shell operations can require OS approval.
Global shortcuts, notifications, secrets, clipboard, and window integration
therefore vary by platform and desktop session.

Manifest permission fields are partly enforced and partly a record of what a
plugin asked for; `crikey plugin doctor` prints which is which, per plugin.
What the operating system enforces is narrower than the manifest vocabulary:
on Linux every supervised plugin process is confined with Landlock so it can
write only beneath the directories the host gave it, and a plugin that did not
request `network` has TCP `bind` and `connect` refused by the kernel. Reads,
execution, UDP and Unix sockets are not restricted, and Windows and macOS
install no equivalent confinement. `CRIKEY_PLUGIN_SANDBOX=off` turns the Linux
confinement off for a whole process. A plugin is therefore not sandboxed
merely because it declared permissions, and the UI must not claim otherwise.
WebAssembly and the restricted C ABI are supported out-of-process runtimes,
each hosted by its own supervised executable rather than loaded into the
launcher. See [architecture](architecture.md),
[ADR index](adr/README.md), and the [v1 specification](spec/crikey-spec-v1.md)
for design boundaries and planned behavior.
