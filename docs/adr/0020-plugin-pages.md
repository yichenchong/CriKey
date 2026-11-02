# ADR-0020: Plugin pages use bounded display lists or supervised web surfaces

Status: Accepted — Linux web-surface route accepted after measurement
Spec: §32, §25.1, §16.2–16.5, §6.3

## Context

Some plugin work does not fit a result list. A unit converter wants a field and
a live answer; a session manager wants a checkbox per host; a diff viewer wants
two columns. Today such a plugin has three options, all bad: encode the whole
interaction into query text, publish a list of items that are really form
controls, or shell out to a separate application and lose the launcher.

The question is what crosses the process boundary when a plugin draws. The
boundary itself is settled — supervised worker processes (ADR-0005, ADR-0015),
proto3 over a local transport with an 8 MiB `MAX_FRAME_BYTES` cap (ADR-0004),
and `winit` + `wgpu` + `egui` behind the `crikey-ui` contracts (ADR-0002). What
is not settled is whether a plugin sends pixels, sends code, or sends drawing
commands.

## Decision

A plugin page chooses one of two exclusive declarations.

The portable/default form is a *display list*. `crikey_core::page` defines a
flat frame of nodes — rectangle, text, line, circle, image, each with geometry,
colours, and a semantic role — and the host draws it with its own `egui`
renderer. Drawing commands and semantics cross the boundary, and plugin code
never does.

On Linux, a plugin may instead name one HTTP(S) document. The launcher spawns a
separate `crikey-web-host` only after the user opens that page. WPE WebKit
renders offscreen in the supervised host process tree; bounded RGBA frames
cross a private native-protocol connection and are uploaded into the same
shaped `egui`/`wgpu` surface as the rest of the launcher. There is no child
window, no direct WPE linkage in the launcher or UI crates, and no web engine
in no-web builds. The display list and web declaration are mutually exclusive
and every plugin-originated `PageFrame` still passes `PageFrame::validate`.

Pixels from a plugin cross in exactly one bounded form: an `Image` node carries
raw RGBA8 for its own rectangle, capped at 1 MiB per node and 4 MiB per frame,
1,024 px a side. That is a deliberate narrowing of this ADR's original
“pixels never cross”, not an abandonment of it. A web host's engine frames use
their own bounded, one-surface transport; they are not plugin-authored image
nodes and cannot be pushed through the plugin protocol. Because a display-list
page is full request/response, its raster is re-sent in every frame the plugin
answers, not only when its pixels change; the host's texture cache spares the
GPU upload, never the wire. And two tiles under the per-node cap reconstruct a
720x400 framebuffer inside the per-frame one, so the caps do not make a surface
canvas unreachable. What they and `MIN_PAGE_REDRAW_MS` establish is a bounded
worst case — 4 MiB per frame, one self-scheduled frame per 16 ms — against the
unbounded rate the rejected design implies, while the rest of the display list
keeps the roles, labels and focus ring a framebuffer cannot carry.

The cycle is host-driven request/response, exactly like `suggest`: the host
sends a `PageRequest` carrying the surface size, the events since the previous
frame and the host palette, and the plugin answers one `PageFrame`. A plugin
cannot push a frame; `legal_plugin_payload` in
`crates/crikey-native-host/src/worker.rs` rejects an unsolicited
`Payload::PageFrame`, which keeps a page on the same generation-and-staleness
footing as every other plugin answer rather than inventing a second, unbounded
one.

### Why not pixels

A 720×400 surface in RGBA8 is 1,152,000 bytes — 1.10 MiB — for one frame. That
is 13.7 % of `MAX_FRAME_BYTES` per frame, so a single canvas frame consumes
roughly 14 % of the largest message the protocol will ever carry, and a
window one size step larger stops fitting at all. At 60 fps it is 66 MiB/s of
socket traffic for a form with four controls on it.

The bandwidth exists: ADR-0017 measured 2,420–2,516 MB/s over the shipping
socket, so 66 MiB/s is under 3 % of measured throughput. The problem is the
shape of the cost, not its size. ADR-0017 rejected a shared-memory data plane
after measuring catalog transfer at 1.8–3.0 % of end to end — 2.7–3.0 % over
the socket, 1.5–1.8 % through a perfect shared region — because encode and
decode dominated and transport did not. A pixel canvas is the exact inverse
profile: there is nothing to encode, the payload *is* the framebuffer, and
transport becomes very nearly the whole cost. That is ADR-0017's own second
reopen clause arriving through the back door — "the codec gets fast enough that
transport is ~19 % of end to end" — except manufactured by us rather than
observed.

So the honest statement is this: we did **not** build a shared-memory
transport, ADR-0017 still stands, and a display list is what keeps it standing.
A fully populated node is about 60 bytes of scalars plus its own strings, so a
realistic page of forty nodes with short labels is roughly 3 KiB — about 350×
smaller than one pixel frame — and even a maximal 4,096-node frame is under
250 KiB of geometry. Kilobytes per frame need no shared region. Megabytes per
frame would demand one.

### Why not an in-process or child-window webview

One decisive reason, and two that are weaker than they are usually stated. All
three are recorded precisely, because two of them were previously written here
in a form that does not survive scrutiny.

The process-invariant objection needs qualifying rather than asserting.
`docs/architecture.md:37` states that no third-party code runs inside the main
process, and spec §31 acceptance criterion 30 pins the same claim for native
libraries. It does not follow that every webview breaks it: WebView2, WKWebView
and WebKitGTK all execute web content in separate renderer processes by
default, so plugin-authored JavaScript would not run in the launcher process
any more than plugin Python does today. What would be linked into the main
process is the *embedding library*, which is a third-party native dependency in
the same category as `wgpu` or `winit` — not plugin-authored code.

So the honest form of this objection is a requirement, not a rejection: any
embedded engine must keep content execution out of process, and that has to be
enforced and tested rather than assumed from the engine's defaults. An
in-process scripting surface — a JS interpreter linked into the launcher, say —
is what the invariant actually forbids, and that remains categorical.

The cost argument is weaker than it first appears, and it is worth getting
right rather than repeating. Nothing forces an engine to be loaded at
activation: one created only when a page opens costs nothing at startup,
nothing at activation, and nothing in idle memory while no page is open. So
§25.1's 30 ms warm activation and sub-100 MiB *idle* memory are not the
budgets a lazily-loaded webview would miss, and a rejection resting on those
two numbers compares the wrong pair — activating the launcher and opening a
page are different interactions with different budgets.

What survives lazy loading is real but smaller. The cold start moves onto the
first page open, which is where a keyboard-driven launcher can least afford
it: the user has already pressed Enter and is waiting on a form. §25.1 sets no
page-open budget, but only because pages did not exist when it was written.
Tens of MB per live surface likewise lands on peak rather than idle memory.
Neither is decisive, and this decision does not rest on them. ADR-0002
rejected Tauri/Electron for the launcher window itself, where the activation
argument genuinely applies; that reasoning does not transfer to a surface
opened on demand, and it should not be borrowed as though it did.

The decisive objection is compositing, and it is the one lazy loading and
out-of-process rendering do nothing about. Putting a conventional webview
inside the launcher's `wgpu` surface means either an overlaid child window —
which breaks the shaped rounded window, because a child window is rectangular
and does not participate in our rounding or clipping — or offscreen rendering
with an explicit frame path into the launcher's surface. Note that this cost is
caused by keeping content out of process, so it cannot be traded against the
invariant; the two objections do not cancel.

The accepted Linux route pays that cost explicitly rather than hiding it: WPE
exports a bounded software frame from `crikey-web-host`, the launcher receives
it over private IPC, and the UI uploads it as a texture clipped by the existing
shaped surface. The launcher does not link WPE and does not overlay a child
window. Other platforms remain unsupported until they have an equivalent
out-of-process, offscreen route.

Three engines still compound a cross-platform implementation. WebView2,
WKWebView and WebKitGTK differ in embedding model, in what they will render
offscreen, and in whether they are present at all — WebView2 is a runtime the
user may not have installed. A display list has one implementation; an embedded
engine has three, each with its own failure when absent. This ADR accepts only
the measured and implemented Linux WPE route.

### Accessibility is a protocol requirement, not a side effect

A painted glyph carries no accessible name. Nothing about drawing text produces
a role, a label, or a focus order, so if the protocol does not carry them they
do not exist — and unlike a DOM, there is no tree to infer them from later.
Therefore semantics travel *beside* the drawing: every node carries `role`,
`label` and `focus_order`, `PageFrame::focus_ring` derives the Tab order from
them, and `PageFrame::unlabelled_interactive` names every interactive node that
has no accessible name so the omission is reported rather than shipped.

State plainly what this does and does not deliver. AccessKit is not enabled in
this workspace today — no crate depends on it — so those semantics are carried
and correct, but they are not yet delivered to a screen reader. Wiring them to
a platform accessibility tree is the remaining work, and it is the same tracked
follow-up ADR-0002 recorded. Nothing here should be described as
screen-reader support until that wiring exists and is measured.

## Consequences

- The plugin implements its own layout. Every node is positioned in absolute
  logical pixels relative to the page's top-left corner. There is no DOM, no
  CSS, no reflow, no flexbox, and no automatic wrapping between nodes.
- The plugin implements its own text editing. A text field is a rectangle, a
  string and a role; caret movement, selection, insertion and deletion are the
  plugin's state machine, driven by `PageInputKind::KeyPressed` and
  `TextInput`.
- The plugin implements its own state machine and its own focus order, and
  rebuilds the whole display list on every frame. The host retains the last
  frame for redraw but never diffs or merges frames.
- This remains a real ergonomic cost to display-list authors. A web surface is
  available when an HTTP(S) application is the honest fit, but it opts into a
  separately packaged engine, network policy and higher per-open cost rather
  than turning the display-list protocol into a UI framework.
- The host stays in control of the parts that are its business: it hit-tests,
  it owns the palette, it clips, and it reserves Escape. A page never learns
  where it sits on screen.
- Every frame is validated by `PageFrame::validate` before anything else sees
  it, so a malformed page is refused with a named reason instead of drawn
  partially. Non-finite geometry in particular would otherwise poison `egui`'s
  layout arithmetic for the entire launcher frame, not merely the page.
- Bounds are fixed and small — `MAX_PAGE_NODES` 4,096, `MAX_NODE_TEXT_BYTES`
  8,192, `MAX_PAGE_EDGE` 4,096.0 — so a keypress cannot turn into an unbounded
  host layout pass.

## Alternatives rejected

- **Pixel canvas as the page itself (plugin renders the surface, host blits).**
  1.10 MiB per 720×400 frame, 14 % of `MAX_FRAME_BYTES` per frame, 66 MiB/s at
  60 fps, and the transport profile ADR-0017 was written to avoid. It also
  loses every semantic: a framebuffer has no roles, no labels and no focus
  ring, so accessibility would go from "carried but not yet delivered" to
  "structurally impossible". The `Image` node narrows this rather than
  prevents it, and the difference is worth stating exactly. A page is full
  request/response, so a raster is re-sent in every frame the plugin answers,
  not only when its pixels change - the host's texture cache saves the upload,
  never the transport. Nor do the caps make a full surface unreachable: two
  tiles under 1 MiB each reconstruct a 720x400 framebuffer inside the 4 MiB
  frame budget. What the caps and `MIN_PAGE_REDRAW_MS` do buy is a bounded
  worst case - 4 MiB per frame, at most one self-scheduled frame per 16 ms
  instead of the unbounded rate a page could otherwise ask for - and what the
  node buys over a surface canvas is that the rest of the display list keeps
  its roles, labels and focus ring. The rejection stands on semantics and a
  bounded rate, not on the framebuffer being impossible to rebuild.
- **In-process or child-window webview per surface.** Rejected on launcher
  integration, not merely process placement: mainstream engines already run
  content out of process, so any embedding must keep it there and prove it. What
  is unresolved is that the surface must be either a rectangular child window,
  which breaks the shaped rounded window, or an offscreen render sharing
  textures with our `wgpu` surface — a cost created by out-of-process
  rendering, not avoidable by it — across three engines with three embedding
  models, one of which is a runtime the user may not have installed. Creating
  the engine lazily answers the startup and idle-memory objections; it does not
  answer this one.
- **Fixed widget vocabulary with no drawing at all.** A closed list of
  `Button`, `Label`, `Field`, `Row` would be easier to draw and easier to make
  accessible, and it is too rigid: it decides on the plugin's behalf what a
  page can be, and every plugin that wants a sparkline, a colour swatch or a
  diff gutter is simply refused. The node set is deliberately shapes *plus*
  roles so that an unanticipated visual does not require a protocol change.
- **Let plugins draw into the launcher's `egui` context directly.** That is
  in-process code by another name, and it puts a plugin panic inside the UI
  event loop.

## Webview revisit criteria

The original decision required all of the following to be measured on the
ADR-0017 reference system (Intel N150, 2 cores, Linux 7.0): an engine created
on demand that composites into the existing `wgpu` surface without a child
window; content execution out of the launcher process; a surface spawned only
when its page opens and released when it closes; and page-open latency disclosed
by a loading state rather than a frozen panel.

The Linux implementation now satisfies the architectural parts: a supervised
host connects over private IPC, sends bounded RGBA frames for composition in
the shaped surface, and exists only for the open page. The first-frame loading
state makes cold start visible. Runtime assertion that the engine's content
process remains a descendant of the web host is still a desirable regression
test; separation of the web host executable from the launcher is implemented,
but is not by itself that ancestry proof.

Two more borrowed criteria are gone alongside the pair below. This trigger used
to require "under 20 MiB resident per live surface" and a 50 ms cold start.
Neither number came from the specification. Spec 25.1 bounds *main-process
idle* memory below 100 MiB, and a surface that lives in a separate host
process and is created when a page opens is neither main-process nor idle; the
spec's 50 ms is a native plugin's *suggestion* latency (`25.2`), not the cost
of opening a page. Both were invented here and then read back as though the
specification had set them, which is the same mistake this ADR already
corrected once when it measured a lazily-created engine against the warm
activation budget.

What replaces them is an attribution rule rather than a number. A web surface
that exists only because a plugin declared it and the user opened its page is
that plugin's cost, in the way an opened application is: it must not be paid
at startup, at activation, or while no page is open, and it must be released
on close. Those are the properties worth testing. A ceiling on how much a
deliberately opened page may cost is a product decision, and this ADR should
not smuggle one in as a measurement.

Two criteria this trigger used to carry are gone on purpose. The activation
bound, because a lazily-created engine satisfies it by never running at
activation, which made the trigger look demanding while testing nothing. And
the flat "no measurement makes `docs/architecture.md:37` true again", because
that was simply wrong: out-of-process content execution does answer it, and
stating otherwise pre-rejected a design on a fact that is not the case.

Reopen the display-list bounds, separately, if a page is observed to need more
than 4,096 nodes for a legitimate surface, or if host-side draw time for a
maximal frame exceeds the 16 ms cached-result budget of §25.1. Both are
measurements, not opinions; take them before changing a constant.

## Measured: WPE WebKit, 2026-09-07

A first measurement now exists, and it is recorded here whatever it says.

Scope, because the result does not generalise past it: WPE WebKit 2.52.6 with
libwpe 1.16.3 and WPEBackend-fdo 1.16.1, the SHM export path, one 696x410
view, in a Debian sid container on the ADR-0017 reference system. This is the
*packaged legacy* generation. WPEPlatform - which carries damage rectangles
and a headless display - is absent from the one package checked directly:
Debian sid's `libwpewebkit-2.0-dev` 2.52.6 ships `wpe-webkit-2.0.pc` and no
`wpe-platform-2.0.pc`, confirmed against the archive's own file list. Reports
that Ubuntu, Fedora, Arch and openSUSE are the same were not confirmed here
and should be treated as unverified. Using it appears to need a custom WebKit
build. It was not measured, and neither was any differently configured
WebKit. Nothing below rules those out.

- **Offscreen export: demonstrated; shaped-window composition: implemented;
  content-process ancestry assertion: still open.** SHM export works with no
  EGL or GPU — the first measured buffer was 696x410, stride 2784, ARGB8888 —
  and each measured view spawned its own `WPEWebProcess`, with no child window.
  The 2026-09 harness did not import that buffer into CriKey and therefore did
  not measure end-to-end composition. The delivered route now converts the
  host frame to bounded RGBA, transports it over private IPC and uploads it
  into the launcher's existing `wgpu` surface. That establishes the
  compositing architecture, not its performance on the original harness.
  Observing a web process still is not the requested runtime assertion that
  content executes in a descendant of the host; that regression proof remains
  open.
- **Per-surface memory: ~248 MiB RSS, 98-107 MiB marginal PSS.** Measured at
  1, 2 and 4 concurrent surfaces; RSS is flat per surface because each view
  gets a dedicated web process. The embedder process itself carries 111-118
  MiB simply for linking WebKit.
- **Page open: 276-533 ms** to the first exported frame (533 ms cold, median
  307 ms warm over five runs), dominated by process spawn.
- **Compose input was broken in the measured harness; the delivered route is
  wired but not fully measured.** In the original harness, injected
  `dead_acute` + `a` yielded a bare `a`, and `Multi_key ' e` yielded two
  characters rather than one `e-acute`; no composition events fired because
  the WPE port supplied only the abstract `WebKitInputMethodContext`. The host
  now installs its IM-context shim and carries preedit, commit and caret-area
  events between launcher and engine. End-to-end interoperability across real
  ibus/fcitx input methods, especially CJK reconversion, has not been measured,
  and the route has no correct delete-surrounding operation. Wiring must not be
  reported as complete IME support.

What this settles: the transport and pixel path are not the obstacle. The
host-side cost of a 696x410 frame is 0.149 ms to convert BGRA-premultiplied to
RGBA-premultiplied and 0.49 ms to cross the worker socket at ADR-0017's
measured throughput - together about 4% of a 16 ms frame. The cost is process
spawn and per-process memory, and on this generation there is no
multi-surface-per-process option to trade against it.

The measured cost is accepted for a deliberately opened page. The launcher
computes a policy target equal to the lesser of 768 MiB and one eighth of
physical RAM, reduced further by an inherited finite cgroup limit. It refuses
to open a surface when that effective value is below 256 MiB rather than start
an engine expected to exceed it.

On Linux it attempts to enforce the value with a delegated cgroup-v2 tree
covering the web-host process tree. `RLIMIT_AS` is deliberately not used:
WebKit maps far more virtual address space than it keeps resident. If the value
cannot be determined or a writable delegated cgroup cannot be created, the
launcher warns and the page continues without the new memory ceiling. The host
loop and degradation path are implemented; the protection must still be
described as conditional, never as present on every installation.

At most one page is open at once (§32.2), therefore at most one web surface is
live. A new page closes and releases the old one before its surface is opened;
this is an existing page-lifecycle invariant, not an additional product limit.

Pre-warming is not the answer to the latency half, and is rejected. One
long-lived surface would convert the spawn cost into a startup cost, but it
would pay a few hundred milliseconds and the engine's idle memory for a page
nobody opened. The host starts the web engine when the user opens the page and
releases the whole supervised process tree on close.

The latency is disclosed rather than hidden. A web page keeps the in-page
loading indicator until its first accepted engine frame is painted. The
plugin's `PageFrame.web` declaration is only an answer that says what to open;
treating it as painted content would clear the indicator and leave a blank
sheet for the measured 276-533 ms startup interval. Host unavailability,
protocol failure, crash or engine load failure closes the page with a specific
diagnostic and leaves the launcher running.

The Linux input route forwards preedit, commit and candidate-window position,
but it deliberately does not claim full IME support. End-to-end ibus/fcitx
coverage is unmeasured, and correction of already-committed text has no correct
delete-surrounding operation in v1. Both limitations are documented for
authors.

Linux distribution builds keep the web host explicit. The default remains a
no-web package. `build.sh --with-web-surface` performs the real
`cargo build -p crikey-web-host --features engine` build after checking the WPE
development stack; `--web-host-binary` retains release-CI and distributor
support for staging an already-built host, and verifies its CLI and dynamic
libraries. Debian and RPM artifacts derive or declare the corresponding
runtime requirements. A package without the staged host does not claim to
contain web-surface support. The current Flatpak recipe is intentionally
no-web: it neither builds the engine-enabled host nor provides pinned WPE
modules and runtime dependencies for one.
