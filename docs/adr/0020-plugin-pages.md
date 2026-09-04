# ADR-0020: Plugin pages are a display list, not pixels and not a webview

Status: Accepted — webview revisit trigger stated below
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

A plugin sends a *display list*. `crikey_core::page` defines a flat frame of
nodes — rectangle, text, line, circle, each with geometry, colours, and a
semantic role — and the host draws it with its own `egui` renderer. Drawing
commands and semantics cross the boundary. Pixels never cross, and code never
crosses.

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

### Why not a webview

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
out-of-process rendering do nothing about. Putting a webview inside the
launcher's `wgpu` surface means either an overlaid child window — which breaks
the shaped rounded window, because a child window is rectangular and does not
participate in our rounding or clipping — or offscreen rendering, which means
shared textures between the engine's renderer and our surface: exactly the
shared-memory machinery ADR-0017 declined to build. Note that this cost is
*caused* by keeping content out of process, so it cannot be traded against the
invariant; the two objections do not cancel.

Three engines compound it. WebView2, WKWebView and WebKitGTK differ in
embedding model, in what they will render offscreen, and in whether they are
present at all — WebView2 is a runtime the user may not have installed. A
display list has one implementation; an embedded engine has three, each with
its own failure when absent.

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
- This is a real ergonomic cost to plugin authors, and it is the price of not
  shipping a browser engine. Documenting it honestly
  (`docs/plugin-author-guide.md`) matters more than pretending the protocol is
  a UI framework.
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

- **Pixel canvas (plugin renders, host blits).** 1.10 MiB per 720×400 frame,
  14 % of `MAX_FRAME_BYTES` per frame, 66 MiB/s at 60 fps, and the transport
  profile ADR-0017 was written to avoid. It also loses every semantic: a
  framebuffer has no roles, no labels and no focus ring, so accessibility would
  go from "carried but not yet delivered" to "structurally impossible".
- **Embedded webview per surface.** Rejected on compositing, not on process
  placement: the mainstream engines already run content out of process, so the
  standing requirement is that any embedding keep it there and prove it. What
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

## Revisit trigger

Reopen the webview decision when all of the following can be measured on the
ADR-0017 reference system (Intel N150, 2 cores, Linux 7.0) and recorded here:
an engine created on demand that cold-starts a surface under 50 ms from the
keystroke that opens it, holds under 20 MiB resident per *live* surface, and
composites into the existing `wgpu` surface without a child window — with
content execution demonstrably out of process, asserted by a test rather than
inherited from an engine default, since a future engine or configuration that
moves it in-process is what `docs/architecture.md:37` forbids.

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
