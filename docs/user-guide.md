# CriKey user guide

CriKey is an independent, keyboard-driven launcher. New plugins use its modern
Python or supervised native interfaces; a separate compatibility layer runs
supported legacy packages.

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
launcher: the window opens, dismissing it with Escape (the default cancel key)
only hides it, and the process stays running so the activation hotkey can bring it back. Quit it for
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
[development notes](../../notes/development.md#python) for runtime layout.

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
selected profile, whether the footer shows its keyboard hints — also have a
surface that does not require knowing a key name. By default, press `Ctrl+,` in
the launcher outside a web page, or click `Settings` in the hint line along the
bottom, to edit them in place; the same rows are readable and writable from a
terminal:

```sh
crikey settings
crikey settings set launcher.activation-hotkey Ctrl+Alt+K
```

Launcher-setting edits write the user-global layer. A hotkey the desktop
refuses leaves the previous one working and reports the refusal.

Use the category controls for **Launcher**, **Appearance**, **Keyboard**,
**Shortcuts**, **Plugins**, or **Privacy/History**; **All** shows every section.
Search stays scoped to the selected category. A declared command's Actions
panel also offers **Configure command…**, opening its exact Shortcuts controls
without invoking the command. A plugin's **Configure** opens its own schema.

Settings search matches launcher and schema-backed plugin metadata, not stored
values, defaults or secret declarations. Browse a plugin to edit its declared
fields; plugin edits target its own user-plugin file rather than copying
effective profile or session values into the global file. Legacy plugins retain
their own configuration mechanism.

Rows distinguish the effective value, its source and restart requirements.
Higher-priority layers can shadow a saved edit. External changes or changed
schemas invalidate stale drafts: **Rebase draft** explicitly adopts the current
revision before retrying; category/search navigation preserves nonsecret
conflicting text. A saved value whose effective reload failed is reported
separately from a write that never persisted. Secret inputs are masked and
retired on save, discard, category changes or leaving their editor; a stale
schema closes private entry until metadata is refreshed.

An optional plugin boolean without a default or configured value remains
**unset**. Enter `true` or `false` and Save; once it has an effective value,
its editor becomes a checkbox.

With results visible, the footer shows the current primary action and
**Actions**. Navigation hints reflect the applied profile and overrides for the
current host context; the default compact hint line is
`Up/Down navigate   Tab complete   Esc cancel`. Hide optional hints with
`launcher.show-hints = false`; Settings and action affordances remain available.
Long action sheets can be scrolled without changing the selected action.
Manual scrolling survives unchanged provider refreshes. Navigation, search and
changed action layouts reveal the selection when needed. Each newly opened sheet
starts at the top without manufacturing a selection-navigation intent.
Settings normally advertises `Ctrl+,`; on a web page it shows only **Settings**,
because that shortcut belongs to the document. Web-page hints read
`Tab navigate page   Esc close page`. Remapping host navigation does not remap
document keys.

The launcher window is drawn with rounded corners where the desktop can
composite them smoothly: a Wayland session, an X11 session with a compositing
manager running, macOS, and Windows 11. Where nothing can — an X11 session
with no compositing manager, or Windows 10 — the window is square, because the
only other way to round it is to clip whole pixels out of the window, and the
stepped edge that leaves looks like a rendering fault rather than a rounded
corner. Turn them off anywhere with `launcher.rounded-corners = false` or the
matching row in the panel. If you expected rounded corners and have square
ones, the launcher says why on startup; run `crikey run` from a terminal to
read it.

Secret fields are redacted. Configuration read errors are reported and a
launcher run continues with built-in defaults. Invalid external appearance or
navigation preferences retain the last-good applied state (or defaults at
startup) and show a diagnostic, not a false applied confirmation. The effective
value/source row describes configuration provenance, not proof that it was
applied. Fix the reported file before relying on the setting. For isolated
development, the standard-directory
locations can be overridden with `CRIKEY_CONFIG_DIR`, `CRIKEY_DATA_DIR`,
`CRIKEY_CACHE_DIR`, and `CRIKEY_STATE_DIR`.

### Appearance and navigation preferences

In **Settings → Appearance**, Theme offers **System**, **Light**, and **Dark**;
Interface size offers **Small**, **Default**, and **Large**; Result density
offers **Compact**, **Default**, and **Comfortable**. These are closed-choice
drop-down controls, not free-form text. Selecting a choice creates a retained
draft; **Save** applies it live. **Discard** abandons the draft, and
**Rebase draft** adopts the current revision after a conflict. **System** follows
observed OS theme changes; if the system theme is unavailable, the preference
stays System and the interface explicitly reports its dark fallback.

CLI and external file edits are checked when the resident launcher next handles
a host interaction; there is no idle configuration watcher. Clicking a blank
part of the launcher footer triggers that check without closing the current
custom page. Reloading does not require restarting the launcher.

Interface size changes logical UI geometry independently of OS display scaling
and DPI. Result density changes result-row spacing. The default size and density
preserve the existing metrics; neither changes the default compact command home
into a catalog view.

| Key | Default | Accepted values |
| --- | --- | --- |
| `launcher.theme` | `system` | `system`, `light`, `dark` |
| `launcher.interface-size` | `default` | `small`, `default`, `large` |
| `launcher.result-density` | `default` | `compact`, `default`, `comfortable` |
| `launcher.theme-overrides` | `{}` | JSON object of color roles |
| `launcher.navigation-style` | `default` | `default`, `emacs`, `vim` |
| `launcher.navigation-bindings` | `{}` | JSON object of action-to-chord arrays |

**Settings → Keyboard** offers **Default**, **Emacs**, and **Vim** navigation.
Emacs adds `Ctrl+N` / `Ctrl+P` for next/previous and `Ctrl+Alt+N` /
`Ctrl+Alt+P` for page down/up. The modified Vim profile adds `Alt+J` / `Alt+K`
and `Alt+D` / `Alt+U`, respectively. Both retain the default navigation chords
unless an override replaces them. Bare `j` and `k` remain query text.

The advanced **Theme color overrides (JSON object)** and **Navigation bindings
(JSON object)** rows use single-line text editors. Enter the complete JSON object,
not TOML, then choose **Save** or press Enter; **Cancel edit** discards the draft.
From a terminal:

```sh
crikey settings set launcher.theme dark
crikey settings set launcher.interface-size large
crikey settings set launcher.result-density comfortable
crikey settings set launcher.theme-overrides '{"accent":"#A0C8FF"}'
crikey settings set launcher.navigation-style emacs
crikey settings set launcher.navigation-bindings '{"next":["Down","Ctrl+Alt+J"]}'
```

In `config.toml`, the override objects are JSON **strings**:

```toml
[launcher]
theme = "dark"
theme-overrides = '{"accent":"#A0C8FF"}'
navigation-bindings = '{"next":["Down","Ctrl+Alt+J"]}'
```

Theme overrides are atomic, limited to 4,096 bytes and the 11 unique roles
`canvas`, `surface`, `raised`, `input`, `border`, `text`, `text-muted`, `accent`,
`accent-soft`, `warning`, and `error`. Colors must be opaque `#RRGGBB` strings;
unknown or duplicate roles are rejected. Omitted roles keep the chosen theme's
colors. Readability checks reject the whole edit with a reason naming the
foreground/background pair, such as **Insufficient contrast between text and
surface**. System-mode overrides must remain readable in both light and dark.
Use `{}` to remove all role overrides.

Navigation bindings are limited to 8,192 bytes, 11 unique known actions, four
chords per action and 64 bytes per chord. Actions are `next`, `previous`,
`page-down`, `page-up`, `complete`, `activate`, `cancel`, `show-actions`,
`toggle-preview`, `left`, and `right`. Each array replaces that action's entire
profile binding set; `[]` disables that action's host navigation binding.
Omitted actions retain their profile bindings; `{}` restores the selected
profile. Chords use the same accelerator syntax as shortcuts, with exact
modifiers: extra modifiers do not silently match another action. Unknown or
duplicate actions, overlapping chords (including retained profile bindings),
and unmodified text/Space chords are rejected.

Navigation edits also reject conflicts with desired activation and command
shortcuts. Actually held OS registrations are checked separately: a refused
replacement may leave the old grab active, so a saved desired shortcut is not
proof it is active. A collision with an actually held grab makes that navigation
chord unavailable and removes it from applied hints; it does not override OS
authority. The diagnostic distinguishes the held shortcut from desired
configuration. Resolve the shortcut/grab before relying on that chord.
Profiles and overrides only govern eligible host navigation: editors, passwords,
popups, IME composition, shortcut recording, preview documents, and web input
retain their own keys. They do not turn text input into launcher commands.

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

Command aliases are separate from this textual table. Open a declared command's
Actions controls, or **Settings → Shortcuts / Configure Commands**, and save its
command alias or global shortcut. Search settings by command name, alias or key.
Exact command aliases lead alias prefixes, followed by ordinary results, even
with a one-result ceiling. Aliases never execute automatically; typing an alias
followed by a space does not implicitly supply arguments.

Use **Record shortcut** (or **Record** on activation) to capture a chord, then
Save it. Escape, focus loss and hiding stop recording. An already registered
chord is captured instead of toggling the launcher or invoking its command.
Failed saves retain the draft, including newer edits made while saving.

Saved desired shortcuts, their effective configuration layer and actual active
registrations are distinct. An OS refusal preserves the previous usable binding;
cleanup failures remain visible. Unassign occupied shortcuts before swapping
commands. Removed or unavailable commands cannot execute through queued receipts.
Optional free-form commands execute without an argument; Required free-form
commands open fresh input. Commands with a typed schema open that same inline
typed editor instead: aliases and shortcuts do not invent positional values.
Hidden direct actions remain hidden unless a still-current deliberate shortcut
result requests presentation. Native page outcomes and independently negotiated
Native/Modern standard-route outcomes support this flow. New input or dismissal
supersedes that presentation intent; stale results cannot reopen the launcher.

Global registration uses X11, the Wayland GlobalShortcuts portal, or Windows'
native service. Desktop policy can refuse a chord; macOS global registration is
unavailable. Platform compilation is not desktop runtime verification.


Plugin discovery roots are optional environment path lists:

* `CRIKEY_MODERN_PLUGIN_ROOTS` — directories containing modern Python plugin
  directories (`<id>/crikey.toml`).
* `CRIKEY_NATIVE_PLUGIN_ROOTS` — native plugin directories.
* `CRIKEY_LEGACY_PACKAGE_ROOTS` — legacy package directories or archives.

Each root is followed by the corresponding per-user directory used by
`crikey plugin install`; an unset root loads no development plugins.

## Keyboard and query behavior

The launcher is usable without a mouse. The following table lists **default**
query/result bindings; applied profiles and overrides may change host navigation,
and the current hints show the applicable chords:

| Key | Action |
| --- | --- |
| Up / Down | Select the previous / next result |
| Page Up / Page Down | Move by a page |
| Tab | Complete the query |
| Enter | Execute the selected result's default action |
| Ctrl/Cmd+K or Alt+Enter | Discover the selected result's actions |
| Ctrl+, | Open the settings surface |
| Escape | Close the settings surface, else cancel or dismiss the launcher |
| Up / Down, Enter (Actions open) | Select and activate a matching action or submenu |
| F6 | Focus the preview, or return to search |

### Discovering actions

The footer shows the current primary action and **Actions**. Open Actions with
its control or the current show-actions chord (by default `Ctrl/Cmd+K` or
`Alt+Enter`), then type into its separate search field. Search flattens matching
executable actions; clearing it restores sections. With default bindings,
Up/Down and Page Up/Down move the action selection, not the result selection.
Enter or clicking a row activates that exact action. Numeric keys are ordinary search
text, not action indexes.

**Launcher Settings → Open Settings** opens the real host settings surface.
The cancel action (Escape by default) / Back first clears action search, then
leaves a submenu, then closes Actions; the launcher query is unchanged.
Settings editors, custom pages and IME composition retain their own keyboard
input; their owned keys are not replaced by host navigation overrides.

### Declared commands and home

Command controls apply only to explicitly declared commands, not arbitrary
suggestions. Open **Settings → Configure Commands** to manage all declared and
saved commands, including hidden commands and unavailable saved references that
are not favorites. Each entry shows its plugin owner and command ID: that exact
pair identifies the command, not its label or target item. Unavailable entries
show a reason and cannot execute; they are never redirected to similarly named
items.

Here, or in a command's Actions panel, favorite/unfavorite and reorder favorites,
hide/show, enroll/remove/reorder fallbacks, or reset learned item ranking.
An ordinary result may expose several declarations; choose the exact
owner/command entry. Commands sharing an item share its ranking reset. Reset
does not clear preferences, command recents or query recall; host-only and
unavailable targets have no verified item ranking to reset.

Configure the empty-root command home with `launcher.command-home`. Accepted
values are `compact` (default), `favorites`, `recent`, and
`favorites-and-recent`. The last choice shows ordered favorites followed by
successful recent commands, without repeating favorites:

```toml
[launcher]
command-home = "favorites-and-recent"
```

Successful commands also earn recent-command and item-ranking credit when they
open or replace a plugin view, even if their original launcher presentation has
since changed. Errors and cancellation earn no credit; loading the opened view
does not count as another command use. Typed arguments and process hotkeys do
not add query recall or query-ranking affinity.

Unavailable saved favorites remain visible in their saved order at home.
Other unavailable saved preferences remain accessible in **Configure Commands**;
removing a favorite or fallback membership does not erase its other preferences.
Hidden commands are excluded from ordinary command discovery and automatic
Recent home rows, without deleting their history. They remain explicitly
invokable from saved favorites and eligible saved fallbacks. Hiding a command
does not hide an ordinary suggestion that happens to target the same item.

Fallback commands appear in saved order only for a nonblank query after every
applicable ordinary provider successfully finishes with no matches. Pending,
failed or cancelled work does not qualify. Selecting a fallback passes that
captured query as one argument through the normal command dispatcher; it does
not autoexecute or interpret shell syntax. Commands that forbid arguments or
have typed schemas cannot be newly enrolled as text fallbacks. Saved incompatible
fallbacks remain manageable but unavailable; the captured query cannot fill
structured fields. Optional free-form arguments do not implicitly consume
ordinary search text.

Command preference controls save semantic edits to the user-global configuration
layer, not a copy of the effective settings from other layers. A visibility edit
preserves that layer's other command fields; favorite and fallback edits change
their respective saved lists independently. A higher-priority profile or session
override can still determine the displayed effective value. The save notice
reports the effective layer. Configuration changes are reloaded; if an edit
conflicts with a newer revision, refresh and retry rather than expecting it to
overwrite the newer state. Failed persistence does not display an unsaved edit
as successful. Edits are refused while configuration authority is unavailable.

Command preview and link permissions belong to the original plugin/catalog
source, not the host rendering the row. A retired preview producer can make
deferred preview unavailable without removing the command.

Only Open Settings is a declared host command. Plugin commands require explicit
native or Modern Python `commands_v1` support; labels, metadata and legacy
suggestions are not inferred declarations. Command aliases and global shortcuts
refer to those exact declarations, separately from textual `[aliases]`.
Typed arguments require independent `typed_arguments_v1` support; command
declarations alone do not grant it.

### Inline typed arguments

An explicitly supported result or command opens inline fields near the root
query instead of requesting one free-form argument. Text can be single- or
multiline; dropdowns offer declared options; passwords show a fixed mask, not a
character count. **Run command** checks required fields, UTF-8 byte bounds and
dropdown selections before submission. Field failures use current schema
guidance and fixed codes, not exception text or rejected values. Re-enter
passwords after submission.
While inline input is open its editor owns keyboard input; Escape closes the
editor before query clearing, Back or dismissal. Covering/hiding root or changing
view, generation or input revision clears the live native editor. Only the
separate nonsecret logical draft is eligible for restoration.

The schema belongs to the exact plugin/item/action. Commands sharing that
executable, actions, aliases and global shortcuts use the same schema; none
infer positional values. A changed or unavailable schema refuses execution,
without conversion to legacy text. Native and Modern Python independently opt
into typed arguments and result presentation. Older workers keep ordinary
operations; Legacy Python, WASM and restricted C ABI have no typed callback.
Schemas are never inferred from metadata.

Limits are 64 fields, 256 options per dropdown, 8 KiB per text value, 256 UTF-8
bytes per identifier and a 256 KiB schema footprint, also subject to existing
transport and shared navigation retention limits.

Passwords have no defaults. They are memory-only live input, excluded from
query recall, configuration, navigation snapshots and host diagnostics.
Nonsecret drafts may restore only for the same executable/schema. Clearing
buffers is best effort, not cryptographic erasure or protection from crash
collection. A malicious plugin receiving input can deliberately store or send
it; choose trusted plugins.

Every typed submission makes its worker diagnostically sensitive for that
process's remaining lifetime: plugin logs, captured output, exception text,
tracebacks and stderr tails are suppressed. Fixed host codes still report
refusal, timeout, cancellation and transport failure. Cancellation does not
undo effects. Standard list/grid/detail/form routes additionally require their
own capability; inline fields alone do not grant it. Existing previews and
custom pages remain separate surfaces.

### Result groups and scopes

Sections and scopes come from explicit, admitted plugin presentation sidecars,
not labels or undocumented metadata. A section header is not a selectable or
executable result. Grouping keeps the existing result order within each section;
it does not introduce plugin score-hint ranking or change command-alias
precedence.

Choose a scope from the dropdown directly below the query: **All** or an
owner-qualified declared scope. Scope identity includes its declaration source,
not just its label. Changing scope retires old results and input before a fresh
query; only that scope's capable owner receives its scope ID. Other providers
are nonapplicable, and legacy callback arguments do not change.

Back/resume retains the exact scope source. If it is no longer available, the
choice remains **Unavailable scope** with a notice; explicitly choose All or
another scope. It never silently broadens the search. Group headers do not
consume selection or logical scroll indexes.

#### Proposed private-desktop acceptance procedure

These are verification proposals, not evidence that a desktop exercise passed.
Use disposable configuration/state/data/cache directories and private copies of
the Native and Modern Python workers described in the
[plugin author guide](plugin-author-guide.md#typed-root-arguments-and-presentation).
Use synthetic passwords only; never stage adversarial fixtures in a normal
user profile.

1. Open both real workers' actionless and explicit-action targets. Check required
   text, UTF-8 byte bounds, dropdown selection, fixed password masking and
   password re-entry after submission. Exercise successful submission and a
   fixed current-field failure; confirm no legacy callback receives typed input.
2. Invoke the same declared target through ordinary results, its command alias
   and an actual registered OS shortcut. Each must open the same schema, not
   infer positional values. Check unsupported desktop/chord errors separately.
   Refuse new typed fallback enrollment and keep an existing incompatible saved
   reference visible/manageable without executing it.
3. Compare grouped rows with the authoritative ungrouped ordering. Navigate past
   headers by keyboard, use duplicate item/section/scope names in different
   owners, and verify exact command-alias precedence under a low result ceiling.
4. Change scope while `typed-scope-wait` is pending, then release/retire old work.
   Old results must not reappear. Cover root with Settings and return; exercise
   exact scope/selection/nonsecret restoration, unavailable scope declarations
   and fresh schema changes without silently broadening the search.
5. Submit synthetic secret canaries through the regression worker's deliberate
   diagnostic-leak paths. Inspect host diagnostics, query recall, configuration
   and navigation restoration, including later background callbacks in that
   same process. No canary may appear; fixed failures must remain visible.
   Worker cancellation, timeout and replacement must not leave Loading stuck
   or revive a dismissed view. This checks diagnostic boundaries, not secure
   erasure or protection against deliberate plugin I/O.

### Standard plugin views

Capable Native and Modern Python plugins can open host-rendered **List**,
**Grid**, **Detail** or **Form** routes. These are not web documents or custom
display-list pages. A list presents ordinary rows and Actions; a grid presents
bounded image tiles; detail uses the existing safe text/markdown/image/link
renderer; a form edits the same executable schema used by typed arguments.

Lists use the current row-navigation and show-actions bindings. By default,
`Ctrl/Cmd+K` or `Alt+Enter` opens Actions; in a grid, Left/Right move within a row,
Up/Down move spatially between rows, and Page Up/Down move by pages of rows.
Section headers never take selection.

Collections initially show **Collection navigation · type to search**, including
after history restoration. Typing, pasting, IME composition or clicking the query
switches to **Editing query · Up/Down returns to collection navigation**.
Left/Right then move the query caret rather than the grid selection; deletion
and clipboard input also remain query editing. An eligible selection/page chord
(Down by default, for example) returns to collection navigation, where Right
again moves spatially. The query keeps its IME anchor throughout.

Double-click a grid tile to activate that exact item once, including a tile that
was not selected. After a refresh, a keyboard-selected grid item is brought into
view on the next paint without another scroll animation or key. Pointer scrolling,
query anchors and Back restoration retain their existing scroll behavior rather
than snapping back to the selected item.
A tile's selection refresh keeps that grid visible for the second click. Its
activation waits for the fresh definitions and is discarded if the executable
changes or a newer input, Back, dismissal or view failure supersedes the gesture.
Forms reuse the native typed editor: use **Run command**, Enter in a
single-line/password field, or Ctrl/Cmd+Enter for multiline input. Required fields
and fixed field failures follow the inline-input rules above.
Escape closes the form editor without changing its submit target. Enter or the
footer reopens that exact submit schema, even when the item's primary action
is different; unrelated actions cannot replace the form's editor.

A plugin can **Push** a route onto the existing navigation stack or **Replace**
the current entry. Back reopens the saved owner/route with a fresh surface and
current definitions; it never executes the opening action again. The host
retains logical query, selection, scope, scroll anchor and eligible nonsecret
drafts, not plugin processes or rendered snapshots. Refresh replaces content
and executable definitions together; late clicks/results from an older
revision cannot invoke replacement definitions, even when item labels/IDs match.
Rapid query, selection and scope changes are coalesced while a refresh is
pending; the latest intent wins without restoring an earlier input state.
If the selected item disappears, the host does not execute a replacement item.
Drafts whose executable or schema no longer matches are discarded.

Route actions belong to that exact owner, worker, route, session and revision.
They cannot borrow a root result or another route's identically named item.
Losing the producing worker makes the active route unavailable rather than
silently attaching it to a replacement process. Back and Settings remain
reachable on unavailable routes. Cancellation retires presentation immediately,
but a busy worker slot remains occupied until its callback finishes or is reaped.

Covering a form with Settings/another view, Back, hiding or losing its source
retires live password storage. Returning can restore only nonsecret drafts
whose exact executable and schema fingerprint still match; passwords require
re-entry. Forms and all later callbacks retain the typed diagnostic-suppression
boundary. This is not a promise of secure erasure or a plugin sandbox.

Standard routes share the existing 16-entry/8 MiB mixed navigation budget with
root, Settings and custom pages; they do not get another stack or allowance.
Descriptors allow up to 4,096 items/definitions, 128 sections/scopes, 256 actions
per target and menu depth four. Grid columns range from 1 to 16. Existing
schema, preview, image and transport limits still apply; oversized content is
refused rather than partially executable.

`structured_view_v1` is independent of typed arguments, root presentation,
previews and command declarations. Older workers keep ordinary behavior but
cannot service standard routes without negotiation. Legacy Python, WASM and
restricted C ABI have no standard-route callback; metadata does not fabricate
one. Standard views do not add another web surface or fetch image URLs.

#### Proposed standard-view acceptance procedure

This is a procedure proposal, not runtime acceptance evidence. Stage private
copies of the [Native and Modern fixtures](plugin-author-guide.md#standard-list-grid-detail-and-form-routes)
with synthetic passwords and isolated configuration/state/data/cache paths:

1. Traverse root → grid → detail/list → form in both real workers. Exercise
   Push and Replace, actionless and explicit actions, successful and fixed-field
   failed submissions, and a typed root outcome that opens a route.
2. Return with Back and delayed resume; verify fresh opening without action
   replay, spatial grid selection, scroll anchoring and matching nonsecret draft
   restoration. Passwords, including Undo state, must not return.
3. Refresh after query/selection/scope changes while delayed work is pending.
   Old descriptor events must not resolve against new definitions or root
   items sharing the same ID. Test source death/replacement and confirm a
   recoverable unavailable view, not borrowed execution authority.
4. Exercise descriptor/image/mixed-stack limits, independent capability absence,
   cancellation while a callback occupies its worker slot, permission-denied
   detail links and secret-canary diagnostic/history/config exclusion.

### Action refresh and failures

An admitted provider refresh preserves action search and focus for the exact
owner, item and action. A vanished target or action shows an unavailable
explanation and cannot silently execute a replacement. Close Actions to make a
new selection. Actions are bounded to 256 entries, depth four, 8 KiB per text
field and 256 KiB total prepared text; excessive input is refused visibly.
Icons use already prepared result pixels or host vector symbols, never
paint-time file/network loading. Primary and host shortcuts are host labels;
the root plugin action contract does not declare arbitrary shortcuts or
destructive flags. Standard-route presentation can include shortcut labels and
destructive styling; neither registers a global shortcut, grants permission nor
confirms an action.

Action failures replace the selected row's secondary metadata so that long
descriptions cannot hide them. Hover the diagnostic to read its complete
bounded reason (up to 8 KiB). Terminal outcomes wake the host without another
keystroke; outcomes from dismissed or replaced views cannot reopen that view.

### Selected-result previews

Typed preview content appears beside the selected result when available. The
list and detail scroll independently. With default bindings, Up/Down select
results while search is focused and F6 enters the detail controls. Inside the
document, arrow/Page keys scroll, Tab reaches links, and F6 or Escape returns to
search without changing the query or selection. Host navigation remaps do not
take over those document keys.

Drag the divider to resize the panes. The remembered list proportion is
`launcher.preview-split` (default `0.55`, range `0.3`–`0.7`), saved through the
ordinary user-global configuration layer. Below 600 logical pixels of body
width, use **Show detail** / the toggle-preview chord (F6 by default) and
**Back to results** rather than compressing both panes. A result without detail
does not reserve a pane.

Loading, empty, unavailable and failed previews have distinct messages; a
preview failure does not disable the results. Changing the selection or query,
opening settings/a custom page, or hiding the launcher retires the old answer.
Only the selected result requests deferred worker content.

Markdown headings, emphasis, code, lists and links are prepared off the paint
thread. HTML/scripts and embedded assets are not supported. The renderer never
fetches images or network resources: images arrive as bounded decoded pixels.
HTTP/HTTPS links require deliberate activation, a current preview owner, and
that owner's host-mediated document-opening (`process`) permission. Refusal is
shown in the preview. Typed preview is independent of the single custom page;
it never starts a second web surface.

Native and modern Python workers can opt into deferred previews. Older workers
continue to function without the capability. Legacy results use host-known
description/provider/category metadata only, with no additional legacy callback.
WASM and restricted C-ABI workers do not expose deferred preview callbacks.


While a web page is open, keys other than Escape go to the document instead of
performing launcher query/result actions. Tab navigates the document, and
`Ctrl+,` does not open launcher settings. Click the persistent `Settings`
affordance to open settings from a web page. Escape closes the page.

With the default `compact` command home, an empty query shows only the query
field; the launcher does not list the catalog at rest. Other command-home modes
show saved favorites, successful recent commands, or both as described above.
Scrolling the result list with the wheel stays where you put it, including
when the host republishes the same results or updates their display details.
Keyboard navigation reveals the selected result, even at the first or last row;
a new query or a changed selected target also follows the selection.

### Navigation, logical resume, and query recall

Settings, custom pages and standard views have a title and **Back**. With default
navigation bindings, Escape first leaves focused preview/typed editing or an
action search/submenu/panel, then returns to the previous view, then
clears/dismisses root as applicable.
The host retains the root query,
owner-qualified selection, logical scope and list scroll anchor. Returning
issues a fresh search and reconciles the exact owner/item across partial
publications. The saved viewport anchor stays pending until enough results
arrive to restore it, or the search finishes and clamps it. Wheel input,
explicit navigation or query editing supersedes pending viewport restoration.
Explicit navigation or query editing also supersedes the return selection.
Navigation is bounded to 16 entries including the current view
and 8 MiB of retained host content; shared result/icon allocations count once.
An excessive push is refused with a visible reason.

Covering a custom page closes its session and web surface before another
surface opens. Back opens its saved owner/page route with fresh request and
surface identities; it does not execute the original action again. Arbitrary
web-document state, display lists and engine pixels are not snapshots. A missing
owner yields **Page unavailable** with a working Back rather than an endless
loading surface. Logical resume applies to Settings, custom pages and standard
routes; arbitrary web-document state is not restored.

`launcher.resume-delay-ms` defaults to `0`: dismissal immediately resets to root.
A positive millisecond value resumes the logical view only if reopened strictly
before that monotonic deadline. At the boundary or after process restart it
starts at root. Hide always retires asynchronous admission and closes page/web
resources; resume issues fresh work and cannot revive an old answer. Only
explicitly nonsecret typed drafts are eligible for logical restoration against
the same executable and schema; passwords are not snapshots. No navigation
snapshot is written to disk.

`launcher.query-history` accepts `session` (default), `off`, or opt-in
`persistent`. Enter on a root primary action records its submitted query only
after successful admission, using the exact item/action snapshot that execution
accepted. Native/modern non-argument submissions can qualify; legacy/free-form
adapters are conservatively excluded because they cannot attest that their
execution text is nonsecret. Argument-bearing, stale/refused, alternate-action
and ambiguous submissions never authorize collection. Typing and action search
are not submissions. Query recall is separate from ranking history.

Installed canonical catalog targets remain executable after a healthy static
catalog producer is released. A live suggestion can override its exact
owner/item entry; a retired suggestion cannot lend its old worker identity to
a replacement process. Admission and execution consume the same frozen item,
while current owner, permission and worker-liveness checks still apply.

With default navigation bindings, from an empty root query with no results,
press Up to open the submitted-search picker, Up/Down to choose, and Enter or
click to restore the text without executing it. Escape closes the picker.
With remapped host navigation, use the corresponding previous/next/activate/cancel
chords. In populated results, previous continues ordinary result navigation.

Recall keeps at most 100 distinct submissions (4 KiB each, 256 KiB total),
newest first, expiring after 24 hours of the current process's monotonic time.
Session history disappears on restart and never writes a file. Persistent
history uses `query-history.json` in the private state directory; restarting
loads it only with explicit persistent configuration and starts a new recall
expiry interval. Without a safe state directory session recall still works,
but durable opt-in is refused visibly. Switching to session/off removes old
durable history; off stops collection even if deletion fails. **Clear query
history** in Settings clears recall and deletes its file without resetting
ranking. Storage failures are reported; no persistence is silently promised.

For example:

```sh
crikey settings set launcher.resume-delay-ms 30000
crikey settings set launcher.query-history persistent
```

#### Private-desktop navigation acceptance fixture

Build with `cargo build --manifest-path
crates/crikey-ui/tests/fixtures/navigation/Cargo.toml`. Stage its `crikey.toml`
and `target/debug/crikey-navigation-fixture` under a private writable package
directory with the same relative executable path. Launch on an isolated private
desktop with `CRIKEY_NATIVE_PLUGIN_ROOTS` pointing to the parent of that private
package and private `CRIKEY_CONFIG_DIR`, `CRIKEY_STATE_DIR`, `CRIKEY_DATA_DIR`
and `CRIKEY_CACHE_DIR`. Do not use an existing desktop's launcher state.

The fixture writes `navigation-lifecycle.log` adjacent to its executable,
without inheriting an environment variable or relaxing worker isolation.
Startup/page execution fails explicitly if that fixture-only log cannot be
opened or written. The log is bounded to 1 MiB; archive/clear it before another
acceptance run if needed. The visible opening counter is derived from this
shared log on every Opened callback, so replacement workers do not reset it.

Search `Navigation fixture`, move to a non-first result, wheel-scroll, and Enter
to open its real native page. Open host Settings, then Back: the page's opening
counter must increase and the lifecycle log must show `closed` before the new
`opened`, with only one `execute` record. Back again must restore query,
selection and logical list position, including after later provider publications
while the selected row is offscreen. Repeat dismissal/reopen with delay `0`
and a positive delay, then expire the delay.

For genuine unavailability, while Settings covers the page, rename the staged
fixture executable out of its manifest-declared path and stop its existing
worker. Back must fail to open a replacement and show the unavailable route
with working Back to root. Stopping a worker alone is not proof: the supervisor
may restart it. Restore the staged executable before testing recovery. Do not
modify a user's installed package or bypass the running-launcher package lock.

Use the empty-root Up picker to check session/off/persistent/clear behavior
across restart, and type text before/after Return in one input batch to verify
ordered recall acceptance.

On Linux, also exercise native IME preedit → candidate arrow/Return → commit
while recall is open, both before one redraw and across a redraw. Candidate
keys must not select or accept history. The ignored native-window regression
`native_touch_dispatch_precedes_ime_and_cannot_retarget_after_redraw` covers this
actual application path; egui-only IME injection is not equivalent coverage.

Native desktop geometry and actual OS behavior require runtime verification;
these instructions and model tests do not establish them.

Typing updates the visible query immediately. Local catalog search runs in the
current UI frame; plugin results arrive incrementally and late results from an
older query are discarded. Modern plugins apply their manifest's debounce and
activation policy. Unchanged legacy plugins use `legacy-strict`: no time
debounce, prompt replacement of obsolete work, serial callbacks, and
`should_terminate()` cooperative cancellation. Empty-query and minimum-length
behavior is a plugin declaration for modern plugins, not a hidden global rule.

Once modern-plugin callbacks finish, the final answer includes every admitted
result batch; intake budgeting does not leave a plugin's answer waiting for
another keystroke.

Catalog rows are fuzzy-matched by the host. Plugin suggestion batches are shown
in the order the plugin publishes them; they are not host-fuzzy-matched or
re-ranked by `score_hint`. This distinction is intentional (see
[architecture](../../notes/architecture.md#two-paths-two-matching-contracts)).

Plugin-owned catalog entries keep their plugin identity in the host-ranked
results. A live suggestion with the same plugin and item identity replaces its
catalog copy, preserving the plugin's suggestion order; catalog-only entries
remain visible. A failure in synchronous application-catalog intake is reported as
`Application catalog intake failed`; it does not prevent asynchronous plugin or
file queries from being dispatched.

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

Settings also provides package inspection and background operations. Install
and remove require confirmation and stop **all third-party runtimes** before
changing packages, including unrelated plugins. They remain offline until the
next launcher start; built-in search, Settings and activation remain available.
Failure to confirm physical teardown refuses the mutation and keeps plugin
admission closed. Closing Settings does not cancel a running operation.

Cancellation is a request, not rollback: an operation already committing may no
longer be cancellable. Its terminal result reports the actual package/config
state and whether another launch is required. Enable/disable is a serialized
configuration change for the next launch, not live activation. Quit waits for
running jobs and owned runtime teardown before releasing the launcher lock.
An enable/disable failure acknowledges a saved configuration only when the write
persisted but effective reload failed; lookup, load and save failures are not
reported as saved.

**Package details** inspects the selected local runtime/package identity,
location and declared metadata without executing it or requiring an index.
Index search/details and **Update index** use the configured index policy.
Inspection and index operations do not stop running plugins; no configured
index is reported explicitly rather than silently changing policy.
**Diagnose** targets the selected local plugin's namespaced identity, even when
different runtimes share a package id. Index-only rows retain their package-id
lookup behavior.

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
[ADR-0012](../../notes/adr/0012-package-signing.md) and the package command's `--help`.
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
launcher. See [architecture](../../notes/architecture.md),
[ADR index](../../notes/adr/README.md), and the [v1 specification](../../notes/spec/crikey-spec-v1.md)
for design boundaries and planned behavior.
