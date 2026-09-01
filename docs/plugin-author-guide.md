# CriKey plugin author guide

CriKey is an independent project. A plugin may implement compatibility with the
documented Keypirinha API, but must not present itself as an official
Keypirinha component or imply an affiliation. Choose a modern Python plugin or
a supervised native plugin for new work; use legacy compatibility only when
porting an existing package.

## Package and manifest

Modern packages contain `crikey.toml` at their root. The manifest is the
contract used for discovery, activation, permissions, and platform selection:

```toml
manifest-version = 1

[plugin]
id = "dev.example.search"
name = "Example Search"
version = "1.0.0"
runtime = "python"
entrypoint = "example_search.plugin:Plugin"
api = ">=1.0,<2"

[python]
requires-python = ">=3.12"
dependencies = ["httpx>=0.28,<1"]

[platform]
os = ["linux", "macos", "windows"]
arch = ["x86_64", "aarch64"]

[activation]
minimum-query-length = 2
prefixes = ["repo"]
patterns = ["^#[0-9]+$"]

[query]
debounce-ms = 50
maximum-wait-ms = 200
leading-edge = true
trailing-edge = true
max-concurrent-requests = 1

[permissions]
network = false
clipboard = "none"
process = false

[performance]
startup = "lazy"
suggest-soft-timeout-ms = 50
suggest-hard-timeout-ms = 500
maximum-results-per-query = 250
maximum-results-per-batch = 50

[catalog]
persist = true
```

Native manifests use `runtime = "native"` and platform-specific
`entrypoint.<os>-<arch>` keys, for example
`entrypoint.linux-x86_64 = "bin/example-search"`. Supported runtime values
are `python`, `native`, `c-abi`, `wasm`, `legacy-python`, and the built-in
runtime. A `wasm` package is run out of process by `crikey-wasm-host`, which
the launcher supervises like any other native worker; the launcher never
instantiates the module itself. It needs a valid `entrypoint.<os>-<arch>`
naming a `.wasm` file inside the package, and the host executable staged beside
the launcher, which every packager installs. A package missing a matching entrypoint is unavailable,
not silently loaded.

`[activation]` decides which keystrokes reach your plugin at all, and it is the
one field that decides whether your process runs. A plugin with
`startup = "lazy"` — the default — owns no process until a query its activation
metadata admits arrives, so a plugin that declares a gate costs the user
nothing until they ask for it.

The three gates are alternatives, not a conjunction: a query passes when a
declared prefix leads it, *or* its first whole token is a declared keyword,
*or* a declared pattern matches. Declaring none means every query long enough
to satisfy `minimum-query-length` reaches you.

`patterns` are regular expressions, for the queries no literal can describe —
an issue number, a date, a hex colour. Four things to know:

- They are matched against the **normalized** query: trimmed and lowercased.
  An uppercase literal in a pattern can never match, and `(?i)` is redundant.
- Matching is **unanchored**, so `#[0-9]+` admits `see #42`. Write `^…$` when
  you mean the whole query, as the example above does.
- The syntax is Rust's [`regex`](https://docs.rs/regex) crate, which has no
  backtracking: your pattern cannot be slow, but it also has no backreferences
  or lookaround.
- A pattern that does not compile, is longer than 512 bytes, or compiles to an
  unreasonably large program makes the whole manifest invalid. At most 16 may
  be declared, because every one of them runs on every keystroke; use
  alternation rather than a long list. `crikey plugin doctor` quotes the
  compiler's own reason for a rejection.

`[catalog] persist` decides whether the host may keep your published catalog on
disk between runs. It defaults to `true`, and the launcher serves those slices
at the next startup *before* any plugin has run — which is what lets a query
answer immediately, and is right for a catalog that is a list of installed
things.

Set it to `false` when your items are only true while you are running: a
session list, a device list, anything holding a handle, anything that would
execute against state that has since gone. For those a persisted slice is not a
stale convenience but a set of items that look live and are not, offered before
you can correct them. Refusing also withdraws any slice an earlier version of
your plugin left behind, from disk and from the running catalog, so the change
takes effect on the launch you ship it in. Items you publish while running are
unaffected — they are current by definition, and are still served normally.

This governs the on-disk catalog only. It is not a privacy control, and it is
not query-result caching.

Declare dependencies in `[python]` or `pyproject.toml`. CriKey resolves a
locked, platform- and architecture-specific environment; system-wide
`site-packages` are excluded by default. Keep binary Python extensions
compatible with the selected runtime and request native-code permission where
applicable. Use `crikey package build --plugin DIR` to create a
`.crikey-package`, then `inspect`, `verify`, and (with a publisher key) `sign`
it before distribution. Index installation additionally checks the published
digest and signer trust policy. See [ADR-0012](adr/0012-package-signing.md)
and [ADR-0013](adr/0013-plugin-index.md).

## Modern Python API

The SDK is in `sdk/python/crikey_sdk`; import `Plugin`, `Query`, `Item`,
`Action`, and `SuggestContext` from `crikey_sdk`. The entrypoint names a
`Plugin` subclass. Every callback is optional and may be synchronous or async:

```python
from crikey_sdk import Item, Plugin as BasePlugin

class Plugin(BasePlugin):
    def build_catalog(self):
        return [Item("docs", "CriKey docs", "https://example.invalid/docs")]

    def suggest(self, query, context):
        if context.cancelled:
            return
        context.emit(Item("help", f"Search for {query.text}", "help"))

    def execute(self, item, action_id, argument):
        pass
```

The concrete API includes `start`, `build_catalog`, `suggest`, `execute`,
`on_configuration`, and `stop`. `Query` supplies `text`, normalized text, and a
monotonic `generation`. `SuggestContext.cancelled` becomes true when work is
obsolete; check it during expensive work. `context.emit(item)` streams an
`Item`; use `context.log(message)` for diagnostics. Register background
coroutines with `context.spawn(coro)`: unregistered raw tasks are cancelled and
reported. Async callbacks run on the host-managed worker event loop.

`Item.stable_id` must not depend on its label. `Action` describes alternate
operations; use `plugin_defined_category()` when a category could collide with
a host category. The current host preserves plugin publication order and does
not apply fuzzy matching or consume `score_hint` for plugin suggestions, so
filter and order suggestions yourself.

`on_configuration(values)` receives the complete latest configuration state,
not a delta. Declare configuration fields in the manifest's `[configuration]`
section when schemas are supported, including type, default, validation,
secret, restart, and platform metadata. Secrets are redacted by
`crikey config`; never log them.

## Native/Rust lifecycle

The supported native mechanism is a supervised executable over local IPC. CriKey
does not load an arbitrary native library into the launcher. Native transport
uses the versioned proto3 protocol and supports Unix-domain sockets, Windows
named pipes, and stdin/stdout for development or fallback. The host negotiates
capabilities, passes an endpoint and session token, monitors health, cancels
requests, restarts a crashed worker, and records exit information.

The Rust SDK's high-level `Plugin` lifecycle is:

```rust
pub trait Plugin {
    fn start(&mut self, context: &PluginContext) -> Result<()>;
    fn build_catalog(&mut self, context: &PluginContext,
                     sink: &mut dyn CatalogSink) -> Result<()>;
    fn suggest(&mut self, query: Query, context: &PluginContext,
               sink: &mut dyn SuggestionSink) -> Result<()>;
    fn execute(&mut self, request: ExecuteRequest,
               context: &PluginContext) -> Result<()>;
    fn stop(&mut self, context: &PluginContext) -> Result<()>;
}
```

Use request IDs, query generations, cancellation, deadlines, bounded batches,
and backpressure as protocol contracts. Keep persistent indexes and native
libraries in the child process. Do not rely on ordinary local IPC being shared
memory; shared-memory transport is an optimization that may be added later.
The native wire details are in [ADR-0010](adr/0010-protobuf-codec.md),
[ADR-0017](adr/0017-shared-memory-transport.md), and the
[native-host architecture](architecture.md).

A C-ABI plugin is supported, but never in process: `crikey-cabi-host` loads the
shared library and speaks the native protocol on its behalf, so a crash or a
hang costs that host process and nothing else (ADR-0015). The library still
needs separately compiled platform and architecture binaries, and the ABI
grants it the full authority of the host process -- it is a compatibility
interface, not a sandbox.

## Pages: drawing your own surface

When a result list cannot express what your plugin does — a form, a checklist,
a converter with a live answer — an action may open a *page*: a surface the
launcher presents in place of the result list and that you draw yourself.

What crosses the boundary is a display list, never pixels and never code. You
return a flat list of nodes — rectangles, text, lines, circles, each with
geometry, colours and a semantic role — and the host draws them with the
launcher's own renderer. This is why a page costs kilobytes a frame instead of
the 1.10 MiB a 720×400 pixel canvas would cost, and why a page cannot crash or
slow the launcher's UI thread. The reasoning, including why there is no
embedded webview, is [ADR-0020](adr/0020-plugin-pages.md); the normative rules
are [spec §32](spec/crikey-spec-v1.md#32-plugin-pages).

Be clear about what that means for you before you start:

- **You position every node**, in logical pixels relative to your page's
  top-left corner. There is no DOM, no CSS, no flexbox, no reflow and no text
  wrapping between nodes. The host clips your page and never moves anything
  inside it.
- **You run your own edit state.** A text field is a rectangle, a string and a
  role. Insertion, deletion, the caret — all yours. The host draws no caret.
- **You rebuild the whole list every frame.** There is no retained widget tree
  to mutate, so your plugin holds the state and the display list is a pure
  function of it.

In exchange the host does the parts that are its business: it hit-tests
pointer events and tells you which node was hit, it hands you its palette so
your page matches the current theme, it derives the Tab ring from your roles
and it owns Escape.

### The cycle

Pages are host-driven request/response, exactly like `suggest`. You never push
a frame; the host asks, you answer once, and an unsolicited frame is a protocol
violation. Each `PageRequest` carries the generation it wants answered, the
surface size, the palette, whether you hold the keyboard, and every input event
since your previous frame in the order it happened. Answer the generation you
were given: a frame answering an older generation is dropped, never drawn, and
a resize advances the generation because your previous layout was for a
different size.

Return `redraw_after_ms` only if your page genuinely changes without input; `0`
means static until the user acts, which is what a form should be.

Escape is reserved by the host. It closes your page and is never delivered to
you, so do not plan an Escape binding. You close your own page by returning a
frame with `close()` set — that frame is not drawn, the close happens instead —
and if the host closed it instead you get one last request carrying
`PageInputKind::Closed`, whose frame is discarded, so your state machine always
has a defined end.

### A worked page

A note editor with a heading, a save button, a checkbox, a text field and a
done button, holding its state across frames:

```rust
use crikey_core::Result;
use crikey_plugin_sdk::{
    ExecuteOutcome, ExecuteRequest, PageBuilder, PageFrame, PageInputKind,
    PageRequest, Plugin, PluginContext,
};

const SAVE: u32 = 1;
const PINNED: u32 = 2;
const FIELD: u32 = 3;
const DONE: u32 = 4;

#[derive(Default)]
struct Notes {
    /// The field's contents. Ours to edit; the host only draws it.
    note: String,
    pinned: bool,
    saves: u32,
    /// Whether FIELD holds focus, so keystrokes are edits and not shortcuts.
    editing: bool,
    finished: bool,
}

impl Plugin for Notes {
    // start / build_catalog / suggest / execute / stop as usual.

    fn execute_outcome(
        &mut self,
        request: ExecuteRequest,
        context: &dyn PluginContext,
    ) -> Result<ExecuteOutcome> {
        if request.item.0.as_str() == "note" {
            *self = Self::default();
            return Ok(ExecuteOutcome::show_page("note"));
        }
        self.execute(request, context).map(ExecuteOutcome::from)
    }

    fn page(
        &mut self,
        request: PageRequest,
        _context: &dyn PluginContext,
    ) -> Result<PageFrame> {
        for event in &request.events {
            match event.kind {
                // Focus follows both Tab and the pointer, so this is the one
                // place editing is decided.
                PageInputKind::FocusChanged => self.editing = event.node_id == FIELD,
                PageInputKind::Activated => match event.node_id {
                    SAVE => self.saves += 1,
                    PINNED => self.pinned = !self.pinned,
                    DONE => self.finished = true,
                    _ => {}
                },
                PageInputKind::TextInput if self.editing => {
                    self.note.push_str(&event.text);
                }
                PageInputKind::KeyPressed if self.editing => {
                    if event.key == "Backspace" {
                        self.note.pop();
                    }
                }
                PageInputKind::Closed => self.finished = true,
                _ => {}
            }
        }

        // `PagePalette` is `Copy`, so the builder taking it does not stop us
        // reading a colour out of it later.
        let palette = request.palette;
        let mut page = PageBuilder::new(request.generation, palette).title("Note");

        if self.finished {
            // Not drawn: the close takes effect instead.
            return Ok(page.close().build());
        }

        let width = request.width as f32;
        // Our own caret, because a text field is a rectangle and a string.
        let shown = if self.editing {
            format!("{}\u{2502}", self.note)
        } else {
            self.note.clone()
        };

        page = page
            .heading(16.0, 16.0, "Note")
            .text_field(FIELD, 16.0, 56.0, width - 32.0, 28.0, "Note text", shown)
            .checkbox(PINNED, 16.0, 100.0, "Pin to top", self.pinned)
            .button(SAVE, 16.0, 136.0, 96.0, 28.0, "Save")
            .button(DONE, 124.0, 136.0, 96.0, 28.0, "Done")
            .text(
                16.0,
                176.0,
                format!("saved {} times", self.saves),
                0.0,
                palette.muted,
            )
            .focus(if self.editing { FIELD } else { SAVE });

        Ok(page.build())
    }
}
```

Three details in there are load-bearing. `execute` is unchanged and still
returns `Result<()>`; opening a page means overriding `execute_outcome`
instead, which is why existing plugins keep compiling. A text size of `0.0`
takes the launcher's body size, so your page inherits its typography rather
than fixing a point size that a theme change would strand. And every colour
comes from `request.palette` — a page that hard-codes `#1e1e1e` looks wrong
the moment the user picks a light theme.

### What the host's renderer actually does

Four renderer facts decide whether your arithmetic is right.

A text node draws its glyph run from `(x, y)` as the **top-left** corner. There
is no centring and no wrapping: `width` and `height` on a text node carry
hit-testing and semantics only, and they do not clip, box or align the glyphs.
Centred or right-aligned text means you compute the offset yourself, and a
paragraph means you emit one node per line.

A rectangle is top-left plus size, and its stroke is centred on the boundary —
so a 2.0 stroke paints one logical pixel either side of the edge you named.
Budget for that when you place a border flush against another node.

The host paints the focus ring. Do not draw your own: you would get two rings,
and yours would disagree with the host's the moment its theme changes.

A builder call is not always one node. `button` emits **two** — the
accent-filled rectangle that owns the `node_id`, role and label, plus an
anonymous centred caption — so a page of buttons spends its `MAX_PAGE_NODES`
budget twice as fast as the call count suggests. Only the rectangle is
addressable; the caption is decoration, which is exactly why it carries no
`node_id`.

### Roles are the accessibility contract

A painted glyph is not an accessible name. Nothing about drawing the word
"Save" tells anything that a button exists, so roles, labels and focus order
travel beside the drawing. `button`, `checkbox`, `text_field` and `heading` set
a role for you; a bare `rect` or `text` node is decoration by definition, which
is correct for a divider and wrong for something the user can click.

An interactive node with no accessible name is reported as a defect of your
page, not quietly drawn, so give every control a label. The focus ring is
derived from the frame — an interactive role plus a non-zero `node_id` — and
ties break by the order you emitted the nodes, so emit controls in the order
you want Tab to visit them or set `focus_order` explicitly. A node with
`node_id` 0 is anonymous: input events can never name it and it never joins the
ring.

These semantics are carried and correct today. They are not yet delivered to a
screen reader — the workspace does not enable a platform accessibility tree —
so they buy keyboard operability now and are the prerequisite for the rest
(ADR-0002, ADR-0020).

### Bounds and refusal

Frames are bounded and validated before anything draws them: at most 4,096
nodes (`MAX_PAGE_NODES`), at most 8,192 bytes of text per node
(`MAX_NODE_TEXT_BYTES`), and every coordinate within 4,096.0 logical pixels of
the page origin (`MAX_PAGE_EDGE`). Two nodes claiming the same non-zero
`node_id` is also a refusal, because an event naming it would be ambiguous, and
so is any coordinate that is `NaN` or infinite.

A refused frame is not drawn in part, and refusal is not a warning: it is a
protocol violation, so the host terminates your process, leaves the last good
frame on screen and ends the page with the named reason. `NaN` is the one to
watch for in practice — it comes from dividing by a zero width during a resize
— so clamp your arithmetic rather than trusting the surface size. Answering an
older generation is the mild case by comparison: that frame is dropped
silently, the previous one stays, and your page keeps running.

### Testing a page

`TestHarness` drives the page path the way the host does, so a page is testable
without a launcher: one request, one frame, the events you say the user
produced.

```rust
use crikey_core::{PageInput, PageInputKind};
use crikey_plugin_sdk::{harness::TestHarness, ExecuteOutcome};

#[test]
fn the_note_action_opens_a_page_and_saving_is_counted() {
    let mut harness = TestHarness::start(Notes::default(), config()).expect("harness");

    assert_eq!(
        harness.execute_outcome("note", None, None).expect("execute"),
        ExecuteOutcome::show_page("note"),
    );

    let opened = vec![PageInput::new(PageInputKind::Opened)];
    let frame = harness.page("note", 1, opened, true).expect("first frame");
    assert_eq!(frame.generation, 1);
    assert_eq!(frame.focus_ring(), vec![FIELD, PINNED, SAVE, DONE]);
    assert!(
        frame.unlabelled_interactive().is_empty(),
        "every control must carry an accessible name"
    );
    frame.validate().expect("the frame must be drawable");

    let mut save = PageInput::new(PageInputKind::Activated);
    save.node_id = SAVE;
    let frame = harness.page("note", 2, vec![save], true).expect("second frame");
    assert_eq!(frame.generation, 2);

    harness.shutdown().expect("shutdown");
}
```

`focus_ring`, `unlabelled_interactive` and `validate` are the three assertions
worth making on every page you write: they pin the Tab order, catch a control
you forgot to label, and prove the host will accept the frame instead of
killing your process for it. The harness returns the frame undecided rather
than dropping a mismatched generation the way the host would, so a generation
bug shows up as a failed assertion instead of a blank page — and the SDK stamps
the requested generation onto your frame anyway, because a page that silently
stops repainting is much harder to diagnose than one that never drew.

## Legacy compatibility

A legacy plugin is a documented Keypirinha Python package loaded by the Legacy
Compatibility Layer, usually a `.keypirinha-package`. Its callbacks are
serialized per plugin instance. In the default `legacy-strict` profile, CriKey
broadcasts the initial query, does not time-debounce, replaces pending obsolete
work, sets `should_terminate()` for running obsolete callbacks, rejects stale
results, disables dynamic cross-request caching, and does not impose a modern
minimum query length or prefix gate. `set_suggestions()` publishes one complete
result batch.

`legacy-optimized` can change behavior (debounce, gating, caching, deadlines,
or event frequency) and must be explicitly selected; never assume it is the
default. Run `crikey plugin doctor [ID]` and
`crikey dev test-legacy-compat` while porting. The compatibility matrix and
limitations are documented in [ADR-0006](adr/0006-legacy-scheduling.md) and
[ADR-0005](adr/0005-python-hosting.md). Legacy dynamic suggestions are not
cached by default, and legacy `Match`/`Sort` requests are recorded but do not
cause host-side matching or sorting of suggestion batches.

## Permissions, security, and troubleshooting

Manifests may request filesystem read/write, network client/listener, clipboard,
process, window, notification, secret-storage, environment, native-library,
and persistent-background capabilities. Five of those reach a real host gate
and one is refused at parse time; the rest record a request, and
`crikey plugin doctor` names every declaration the host does not honour, with
the reason. Independently of the manifest, Linux confines every supervised
plugin process with Landlock: your plugin may write only beneath the
directories the host gave it — its scratch space, and for a legacy package the
`package_cache_path()` directory — and if you did not request `network`,
TCP `bind` and `connect` fail with `EACCES`. Its own package directory is
read-only, so do not plan to write beside your code; use the temporary
directory. Reads are not restricted, there is no syscall filter, and neither
Windows nor macOS installs an equivalent, so a manifest declaration is still
not a sandbox and must not be advertised as one. Legacy code is trusted
compatibility code beyond that write confinement.

Useful commands:

```sh
crikey plugin list
crikey plugin doctor [ID]
crikey config list
crikey config layers [KEY]
crikey dev run --plugin DIR --query text
crikey dev inspect-protocol --plugin EXE
```

If a plugin is absent, verify the relevant `CRIKEY_MODERN_PLUGIN_ROOTS` or
`CRIKEY_NATIVE_PLUGIN_ROOTS` path and that `crikey.toml` is readable. Confirm
`runtime`, entrypoint, API range, platform/architecture, and Python dependency
constraints. `doctor` reports unreadable roots, invalid manifests, disabled
state, admission problems, and legacy compatibility findings. Disable/enable
changes apply on the next launch. A stale or unsigned package is a provenance
or policy failure, not a reason to bypass verification.

Platform services are capability-dependent: Linux window control is optional
and Wayland depends on compositor protocols; macOS accessibility/Keychain and
Windows registry/shell operations may require OS permission. Report unavailable
capabilities rather than assuming every desktop has every service. For the
complete boundary, link readers to [platform services in the spec](spec/crikey-spec-v1.md#18-platform-services),
[ADR-0011](adr/0011-wayland-backend.md), and
[ADR-0015](adr/0015-restricted-c-abi.md).