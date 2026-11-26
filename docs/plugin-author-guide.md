# CriKey plugin author guide

CriKey is an independent project. Choose a modern Python plugin or a supervised
native plugin for new work; use legacy compatibility only when porting an
existing package.

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
digest and signer trust policy. See [ADR-0012](../../notes/adr/0012-package-signing.md)
and [ADR-0013](../../notes/adr/0013-plugin-index.md).

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

Stable item identity is qualified by the owning plugin; a label is not an
execution target. Durable commands require an explicit `commands_v1`
declaration; stable item IDs, suggestions and metadata do not register commands.

### Explicit executable commands (`commands_v1`)

Modern Python opts in by overriding `Plugin.commands()` on the same plugin
instance that builds its catalog. Return an iterable of `Command` values
(or await one); the inherited callback is unsupported:

```python
from crikey_sdk import Command

def commands(self):
    return [Command(
        command_id="open-docs",
        label="Open documentation",
        item_id="docs",
        argument_policy="forbidden",
    )]
```

Place this method on your `Plugin` subclass and publish `docs` from
`build_catalog`. Native Rust opts in with `Plugin::supports_commands() -> true`
and implements `commands(&mut self, &dyn PluginContext) -> Result<PluginCommands>`,
returning `PluginCommands::Declarations`. `CommandBuilder::new(command_id,
label, item_id)` adds optional `.action(action_id)` and `.arguments(policy)`;
see `sdk/rust/examples/commands.rs`. Native wire requests use envelope tags
45/46 after a complete catalog on the same connection. Modern Python uses its
separate JSON `commands`/`commands_result` transport, not protobuf.

Each declaration names a stable command ID, display label, exact catalog item,
optional action ID and `forbidden`, `optional` or `required` argument policy.
The host supplies installed-owner identity and availability, and validates the
current canonical catalog, action policy and permissions again at invocation.
Omitting an action selects the catalog's first action when present; an actionless
item uses the provider's default callback with no action ID. Different commands
may share a target; labels need not be unique and never become identity.
Plugins cannot declare host targets. The only built-in host command is
`crikey.host` / `open-settings` (`OpenSettings`).

Limits apply to the complete declaration response: 10,000 commands, 256 UTF-8
bytes per identity, 8 KiB per label and 4 MiB aggregate strings. Duplicate
command IDs or invalid targets/policies reject the response, not just one row.
Native declaration callbacks run synchronously in the worker. Poll the request
context's cancellation token during expensive work. The SDK checks cancellation
and elapsed deadline after the callback returns; it does not preempt callback
code. Host transport timeouts and worker supervision remain separate safeguards.

Explicit declarations participate in command search, favorites/home, exact
successful command recents, visibility, ordered fallbacks and item-ranking reset.
Users manage them globally in **Settings → Configure Commands**, including
hidden commands and unavailable saved references without favorite membership.
Entries retain the exact installed-owner/command-ID pair; a matching label or
item is never a replacement. Removing favorite or fallback membership leaves
other preferences intact. Ranking reset affects the current owner/item shared
by commands targeting it, not command recents, preferences or query recall.

Users can also assign command aliases and global shortcuts to the exact declared
reference. Hidden declarations are excluded from alias discovery, not explicit
shortcut invocation. Shortcut dispatch revalidates current canonical command
authority and permissions. Optional free-form arguments are omitted; Required
free-form commands open host input without invoking the callback. A typed schema
opens the shared inline editor instead; aliases and shortcuts do not infer
positional fields or flatten structured values into text.
Native `Plugin::execute_outcome` may return `ExecuteOutcome::ShowPage` or an
independently negotiated standard-route navigation outcome. Modern Python
preserves ordinary/typed callback navigation returns as well. A hidden
callback's presentation requires a current, once-consumed deliberate shortcut
intent; late superseded outcomes never revive a dismissed launcher.


Preference controls submit revision-checked semantic edits to the user-global
layer, preserving unrelated fields in that layer rather than copying effective
profile/session values. Higher-priority layers may still mask a saved edit.
Conflicts require refresh/retry; failed writes do not publish the proposed edit.
Commands retain their real catalog owner/source for preview and permission
checks; healthy idle worker release does not remove an installed command, but a
retired preview producer cannot service deferred preview.

Legacy, WASM and restricted C-ABI plugins do not gain a command callback through
this transport. The host supplies alias/global-shortcut controls for explicit
declarations; typed fields require independent `typed_arguments_v1` negotiation
and an admitted executable schema, not `commands_v1` alone.

Cancellation is cooperative, but display admission is not: a late suggestion
from an obsolete query or dismissed launcher session is discarded even if its
callback finishes successfully. Keep request generations separate from any
plugin-owned item IDs.

`on_configuration(values)` receives the complete latest configuration state,
not a delta. Declare configuration fields in the manifest's `[configuration]`
section when schemas are supported, including type, default, validation,
secret, restart, and platform metadata. Secrets are redacted by
`crikey config`; never log them.

Schema-backed fields are browsable in the host Settings surface without running
your plugin. Host edits write the plugin's user-plugin layer under source and
schema revision checks; an external edit requires explicit conflict recovery.
Search excludes values, defaults and secret declarations. Mark secrets in the
schema; never put them in labels, descriptions or diagnostics. A package
install/remove permanently stops every third-party runtime in that launcher
session, so do not assume configuration delivery or another callback after
package mutation. Restart requirements remain distinct from successful saving.

For an explicit command, the host's **Configure command…** action navigates to
that exact command's preferences without calling your execution callback.

## Typed root arguments and presentation

Typed root arguments extend the existing root list, not the custom-page
protocol. `typed_arguments_v1` and `result_presentation_v1` are independently
negotiated optional capabilities. `commands_v1` and `preview_v1` imply neither.
Existing list-returning callbacks remain supported. Standard routes additionally
require independent `structured_view_v1` support; typed execution can now return
the canonical submission-or-navigation outcome described below.

Shared Core models live in `structured_arguments`, `result_presentation` and
`content_accounting`. `ResultPresentation` carries sections, scopes, item
memberships and `ExecutableSchemaBinding` values. One `ArgumentSchema` binds to
an exact `ExecutableKey`: owner, item and optional action, not a label or
arbitrary metadata. Resolve an omitted command action to its actual primary;
`None` means genuinely actionless. Multiple commands may share that schema
without sharing command identity.

Catalog sidecars validate transactionally with the complete accepted owner
catalog. Suggestion sidecars require the exact admitted generation and producing
worker incarnation. Invalid references or duplicates grant no partial authority.
Section headers are not executable rows. Presentation-only support does not
authorize typed execution; schema bindings can exist without sections or scopes.
The host owns selection and scope state. Its scope dropdown chooses All or one
exact owner/source declaration; providers receive only their own selected scope.
Scope changes retire old-generation publication before refresh, and other
providers become nonapplicable without new Legacy callbacks. An unavailable
restored scope remains unavailable until the user chooses another; it does not
silently widen the query. Grouping stably partitions authoritative result order
after command-alias prioritization and result limiting; headers never consume
executable selection/scroll indexes.

Submissions move live `ArgumentValues` through the existing foreground action
queue/budgets, never flattening them into query, shell or legacy text. The
expected canonical schema fingerprint must match the actual current physical
worker before value delivery, including after healthy static-worker release.
A mismatch is a fixed refusal. Dynamic targets require their original live
producer; never regenerate suggestions to retarget an old submission. Keep SDK
schema bindings even with an older host so schema-only targets refuse legacy
execution rather than silently discarding fields.
Python replaces catalog and suggestion execution inventories independently on
successful terminal publication; unsuccessful publications grant no new authority.
After a root schema is declared, legacy execution requires a currently
published ordinary item/action. Retired typed targets cannot fall back to legacy
execution. The 4,096-schema bound applies to current bindings, not every target
ever seen by a worker.

### Bounds, outcomes and privacy

Schemas support Text, Password and Dropdown fields, required flags, placeholders
and static validation guidance. Limits are 64 fields, 256 options per dropdown,
8 KiB UTF-8 per text value, 256 bytes per identifier and a 256 KiB schema
footprint. Presentation allows 128 sections, 128 scopes and 4,096 item
memberships/schema bindings, within aggregate retention and existing transport
limits. Decode budgets apply before allocation; post-construction validation
alone is not allocation protection.

`SubmissionOutcome` is `Completed`, `InvalidFields` or `Failed`. Field failures
name unique current schema fields and only `Required`, `TooShort`, `TooLong`,
`InvalidSelection` or `Rejected`. Guidance comes from the pre-submission schema,
never callback text. Fixed global failures are `Unavailable`, `Denied`,
`Cancelled`, `Timeout`, `Unsupported`, `Failed`, `SchemaMismatch`, `ResourceLimit`
and `ProtocolViolation`.

Passwords have no representable defaults. They are masked live memory, excluded
from nonsecret drafts, navigation snapshots, history, configuration and host
diagnostics. Rust live values are move-only; ordinary buffer filling is best
effort, not cryptographic erasure. Python immutable strings/runtime copies do
not provide secure-memory guarantees. Do not log, return or retain values.

Every typed submission marks its physical worker diagnostically sensitive before
bytes enter a writer queue, even without password fields. The latch lasts until
process exit: plugin logs, captured stdout/stderr, exception text, tracebacks and
background diagnostics are suppressed, including later unrelated callbacks.
Fixed-code host outcomes and cleanup remain. This is not a sandbox against
deliberate file/network I/O or a promise about crash dumps/runtime erasure.

| Runtime | Typed execution | Sections/scopes | Boundary |
| --- | --- | --- | --- |
| Native/Rust | Optional `typed_arguments_v1` | Optional `result_presentation_v1` | Independent capabilities; current source/schema check |
| Modern Python | Optional `typed_arguments_v1` | Optional `result_presentation_v1` | Separate bounded JSON contract; frozen item submission |
| Older Native/Modern | Only if negotiated | Only if negotiated | Ordinary operations remain; no synthetic fallback |
| Legacy Python | No new callback | No new callback | Scheduling, arguments and textual aliases unchanged |
| WASM | Not exposed | Not exposed | Native transport does not create a guest API |
| Restricted C ABI | Not exposed | Not exposed | No ABI extension |

Aliases and global shortcuts open the same inline schema; they do not map search
text positionally. Typed commands cannot enroll as literal-query fallbacks;
saved incompatible references remain manageable but unavailable. Typed execution
never authorizes query-history collection. This matrix describes support, not
graphical or cross-platform acceptance evidence.

### Native author API and runnable worker

Rust implements these `Plugin` methods (all callbacks run in the worker):

```rust
fn supports_typed_arguments(&self) -> bool;
fn supports_result_presentation(&self) -> bool;
fn catalog_presentation(
    &mut self, context: &dyn PluginContext,
) -> crikey_core::Result<crikey_core::result_presentation::ResultPresentation>;
fn suggestion_presentation(
    &mut self, query: &Query, context: &dyn PluginContext,
) -> crikey_core::Result<crikey_core::result_presentation::ResultPresentation>;
fn execute_typed(
    &mut self, request: TypedExecuteRequest, context: &dyn PluginContext,
) -> crikey_core::Result<crikey_core::structured_view::TypedExecutionOutcome>;
```

Both support methods default to false. Presentation callbacks default to empty;
typed execution defaults to
`SubmissionOutcome::Failed(SubmissionFailureCode::Unsupported).into()`.
`TypedExecutionOutcome::Submission(SubmissionOutcome)` carries fixed field/global
results; `TypedExecutionOutcome::Navigate(StructuredRouteOutcome)` requests
Push/Replace. Existing Rust callbacks return `SubmissionOutcome::Completed.into()`
or the corresponding fixed failure `.into()`; the former
`Result<SubmissionOutcome>` callback signature is replaced, not retained as a
second method or forwarding shim. Construct the shared Core structs;
there is no separate typed-schema builder. `TypedExecuteRequest` owns `request`,
`item`, optional `action`, `expected_fingerprint: [u8; 32]` and move-only `values`.
`Query.selected_scope_id` is the optional exact scope ID. Presentation metadata
callbacks precede the existing catalog/suggestion callbacks, including for old
hosts so the SDK can refuse legacy execution of bound targets.

The complete runnable implementation is
[`crikey-conformance-plugin.rs`](../compatibility/native-conformance/src/bin/crikey-conformance-plugin.rs),
mode `typed`. From the repository root, stage it on Linux as follows; these
are author instructions, not a recorded execution:

```sh
cargo build --manifest-path compatibility/native-conformance/Cargo.toml \
  --bin crikey-conformance-plugin
demo=$(mktemp -d)
mkdir -p "$demo/native/typed/bin"
cp compatibility/native-conformance/target/debug/crikey-conformance-plugin \
  "$demo/native/typed/bin/"
printf 'typed\n' > "$demo/native/typed/conformance-mode"
cat > "$demo/native/typed/crikey.toml" <<'TOML'
manifest-version = 1
[plugin]
id = "dev.example.typed-native"
name = "Typed Native Example"
version = "1.0.0"
runtime = "native"
entrypoint.linux-x86_64 = "bin/crikey-conformance-plugin"
api = ">=1.0,<2"
TOML
CRIKEY_NATIVE_PLUGIN_ROOTS="$demo/native" \
CRIKEY_CONFIG_DIR="$demo/config" CRIKEY_STATE_DIR="$demo/state" \
CRIKEY_DATA_DIR="$demo/data" CRIKEY_CACHE_DIR="$demo/cache" \
cargo run -p crikey-cli --bin crikey-launcher
```

Use the matching entrypoint platform key/binary on another architecture.
Search `Conformance secure login`. The catalog has actionless `typed-login`
and `typed-action` with explicit action `signin`; suggestions use
`typed-suggestion`. The Accounts section and Secure accounts scope accompany
Username, Password and Region fields. Supply any nonempty username, password
`conformance-secret`, and Local or Remote. A different password returns the
fixed password-field Rejected outcome; successful submission updates worker
session state without retaining the password. `typed-changed` changes schema
guidance/fingerprint for stale-schema refusal exercises. With `commands_v1`,
`secure-login` targets actionless `typed-login`; `secure-login-action` targets
`typed-action` with action `signin`. Both forbid unstructured query arguments
and support exercising schema-driven aliases and command shortcuts.

### Runnable Modern Python author example

Create `plugin.py` in a private package directory:

```python
from crikey_sdk import (
    ArgumentSchema, Command, DropdownField, DropdownOption, FieldFailure, Item,
    ItemPresentation, PasswordField, Plugin as BasePlugin, ResultPresentation,
    SchemaBinding, Scope, Section, TextField, TypedOutcome,
)

SCHEMA = ArgumentSchema((
    TextField(id="username", label="Username", required=True, max_bytes=256),
    PasswordField(id="password", label="Password", required=True, max_bytes=256,
                  guidance="Enter the example password"),
    DropdownField(id="region", label="Region", required=True, default="local",
                  options=(DropdownOption("local", "Local"),
                           DropdownOption("remote", "Remote"))),
))

class Plugin(BasePlugin):
    def supports_result_presentation(self):
        return True

    def build_catalog(self):
        return [Item("login", "Python secure login", "example:login")]

    def commands(self):
        return [Command(command_id="login", label="Python secure login",
                        item_id="login", argument_policy="forbidden")]

    def catalog_presentation(self):
        return ResultPresentation(
            sections=(Section("accounts", "Accounts"),),
            scopes=(Scope("secure", "Secure accounts"),),
            items=(ItemPresentation("login", section="accounts",
                                    scopes=("secure",)),),
            schemas=(SchemaBinding("login", SCHEMA),),
        )

    def execute_typed(self, item, action_id, values):
        password = values.get("password")
        if password is None or password.value != "example-secret":
            return TypedOutcome(failures=(FieldFailure("password", "rejected"),))
        # Commit only nonsecret session state; never retain the live input.
        self.account = (values.get("username").value, values.get("region").value)
        return TypedOutcome()
```

Put this `crikey.toml` beside it:

```toml
manifest-version = 1
[plugin]
id = "dev.example.typed-python"
name = "Typed Python Example"
version = "1.0.0"
runtime = "python"
entrypoint = "plugin:Plugin"
[python]
requires-python = ">=3.12"
```

Point `CRIKEY_MODERN_PLUGIN_ROOTS` at the package directory's **parent**, then
launch `cargo run -p crikey-cli --bin crikey-launcher` from the repository root
with private `CRIKEY_CONFIG_DIR`, `CRIKEY_STATE_DIR`, `CRIKEY_DATA_DIR` and
`CRIKEY_CACHE_DIR` paths, as in the Native staging example. Use an interpreter
matching the manifest (or the packaged runtime); development can select it with
`CRIKEY_PYTHON`. Search `Python secure login`, enter a username, password
`example-secret` and a region, then Run command. A wrong password returns only
the fixed field failure and schema guidance; success stores the nonsecret
account tuple in the worker. No external account service is contacted.

Overriding `execute_typed(self, item, action_id, values)` negotiates typed
support. Visual presentation separately requires
`supports_result_presentation(self) -> bool` returning `True` (default `False`);
overriding metadata callbacks alone does not opt in. This permits schema-only
sidecars without visual support. Metadata callbacks still run for older hosts
so the SDK retains schema bindings and refuses legacy execution.
Execution and both metadata callbacks may be async; the support flag is
synchronous. `TypedValues.get(field_id)` returns a
redacted-repr `TypedValue` or `None`; its `.value` is sensitive plaintext even
though repr is redacted. The callback receives the admitted frozen `Item`,
not a new lookup by ID. `None` or `TypedOutcome()` means Completed.
`suggestion_presentation(self, query)` supplies generation-local sidecars;
`query.selected_scope_id` is the optional scope. Keep schemas for catalog and
suggestion executables distinct where their actual bindings differ.
For a declared typed command, keep the item's and declaration's legacy
`argument_policy="forbidden"`; required typed fields belong to the schema.
The typed path validates those fields independently of legacy free-form policy.
The Python example declares `login` so its alias/global-shortcut controls can
exercise that same schema, without allowing free-form execution.

For regression exercises, the real worker package
[`compatibility/test-plugins/modern-typed`](../compatibility/test-plugins/modern-typed/)
also declares an actionless command and an explicit `submit` action, plus
Local/Remote scopes. Copy it to a private plugin root; do not use real credentials.
Name `reject` exercises fixed field refusal, and query `typed-scope-wait`
holds suggestion work until cancellation. Name `leak` deliberately attempts to
retain and emit the supplied password across later callbacks/threads to probe
diagnostic suppression; it is adversarial test code, not an author pattern.

## Standard List, Grid, Detail and Form routes

Standard views use Core's `structured_view` model and optional
`structured_view_v1` capability, independently of `typed_arguments_v1`,
`result_presentation_v1`, `commands_v1` and `preview_v1`. A form additionally
needs typed execution; merely returning a schema does not grant it. Protocol
version stays 1. Unsupported callbacks/capability absence are explicit refusals,
not empty successful views or silently completed navigation.

### Descriptor and authority

Return `ViewDescriptor { title, definitions, view }`. `view` is
`StandardView::{List, Grid, Detail, Form}` in Rust, or the corresponding Python
`ListView`, `GridView`, `DetailView`, `FormView` instance:

* List/Grid collections carry item presentations, explicit sections/scopes and
  initial selection/scope hints. Provider order is preserved; hints apply only
  when first opened, not over the host's selection on refresh. Grid has 1–16
  columns and uses the existing validated image-pixel representation.
* Detail uses prepared safe preview content. Rust calls `DetailView::prepare`;
  Python supplies `DetailView(Preview(...), target_item_id)`. Existing inert
  markdown, image and deliberate HTTP(S)-link bounds apply. Unsupported content
  is refused, not interpreted as HTML or fetched as remote assets. Link
  activation rechecks route/revision and the owner's document-opening permission.
* Form references one exact executable already in the descriptor definition
  table: Rust `FormView { submit: ExecutableKey }`, Python
  `FormView(item_id, action_id=None)`. Its `ExecutablePresentation.schema` is the
  same schema used by route typed execution, not a second form schema.

Each `RouteItemDefinition` contains the complete local `Item`/actions, explicit
actionless plugin policy where applicable, optional primary action, and bounded
`ActionSection`/`ExecutablePresentation` menus. `None` is genuinely actionless,
not an instruction to search for a root primary action. Worker definitions
cannot grant host execution authority. SDK descriptors use their own local
namespace; the host assigns endpoint ownership and rejects foreign references.

Execution is bound to exact owner, actual physical producer, route, host session
and descriptor revision. Reused item IDs in root or other routes are not
fallback targets. Refresh validates and atomically replaces presentation,
definitions, schemas and links; old queued events cannot execute new definitions.
While Refresh is in flight, the host coalesces newer query/selection/scope
changes and uses its successful revision for the next request. Do not assume
every such edit cancels the callback; honor its deadline and cancellation token.
The old frame may remain visible while the host drains that exact Refresh's
completion, but it cannot execute or open links against the replacement revision.
This presentation-only retention never survives producer or session loss.
Hide, pop and owner loss still retire authority and cancel outstanding work.
Retained selection, scroll and eligible draft state are bounded host intent, not
requirements that the next descriptor preserve those items or schemas. The host
reconciles them after admission; removed targets never gain replacement authority.
A retained logical entry is restore intent, not executable authority.

### Open, Refresh, execution and outcomes

Rust implements these methods on the existing `Plugin`:

```rust
fn supports_structured_views(&self) -> bool; // default false
fn structured_view(
    &mut self, request: &ViewRequest, context: &dyn PluginContext,
) -> crikey_core::Result<ViewDescriptor>;
fn execute_view(
    &mut self, request: ViewExecuteRequest, context: &dyn PluginContext,
) -> crikey_core::Result<ExecuteOutcome>;
fn execute_view_typed(
    &mut self, request: ViewTypedExecuteRequest, context: &dyn PluginContext,
) -> crikey_core::Result<TypedExecutionOutcome>;
```

`ViewRequest` contains `route`, `session`, Open/Refresh `mode`,
`previous_revision`, optional host `state`, nonsecret `query`, optional `scroll`
anchor and Explicit/Query/Selection/Scope `reason`. Open has no previous revision;
Refresh requires it. The successful correlated response request ID becomes the
descriptor revision, not a root query generation. Back/resume sends Open with a
fresh surface and optional logical restoration state; it never replays execute.
Scroll is host-local state, not an RPC on every wheel event.

`ViewExecuteRequest` owns request ID, `view: ViewExecutionContext`, exact frozen
route `item: Item`, optional action and optional ordinary argument.
`ViewTypedExecuteRequest` instead owns the expected schema fingerprint and
move-only typed values. Context carries route/session/revision. These callbacks
default to Unsupported and never forward to root `execute`/`execute_typed`.
Root typed execution still uses `TypedExecuteRequest.item: ItemId`; route typed
execution receives its frozen descriptor item, not a root item-ID lookup.

Ordinary Rust callbacks return `ExecuteOutcome::Navigate(...)`; root and route
typed callbacks both return canonical `TypedExecutionOutcome::Submission(...)`
or `TypedExecutionOutcome::Navigate(...)`. Navigation is
`StructuredRouteOutcome::Push(RouteId)` or `Replace(RouteId)`. The host intercepts
authorized navigation before sending submission-only field status to the editor.
There is no parallel legacy typed-return method.

Python overrides one sync/async `structured_view(request, context)` to negotiate
standard views. It returns `ViewDescriptor(title, definitions, view)`; context
provides `cancelled` and `log`. Request fields are `route`, `session`,
`mode` (`"open"`/`"refresh"`), `previous_revision`, `reason`, `state`, `query`
and `anchor`. Route entrypoints are distinct:

```python
def execute_view(self, route, item, action_id, argument): ...
def execute_view_typed(self, route, item, action_id, values): ...
```

They may be async, receive the frozen route-local `Item` and never fall back to
root callbacks. Override of root or route typed execution independently
negotiates `typed_arguments_v1`. Ordinary root/route callbacks return `None`
for completion or `Push(route)`/`Replace(route)` for navigation. Typed callbacks
also accept `TypedOutcome` for fixed submission results. These returns survive
the worker bridge; navigation is not discarded or turned into Completed.

### Lifetime, privacy and bounds

Push adds to the existing P3 stack; Replace replaces its current entry. Cover,
pop, hide or source death immediately retires presentation/execution authority,
while physical worker slots remain occupied until callback terminal/reaping.
An existing route cannot reacquire a healthy replacement worker behind the
same IDs: producer loss makes it unavailable. Back opens the owner/route anew
under fresh admission; unavailable routes retain host Back and Settings.

Root, Settings, custom/web pages and standard routes share one 16-entry/8 MiB
mixed-content retention budget, with actual allocation capacities/shared
allocations accounted once and renderer copies charged separately. The
individual descriptor ceiling is 8 MiB, not another allowance. Limits remain
4,096 items/definitions, 128 sections/scopes, 256 actions per target, menu depth
four, and the existing schema/preview/image/frame limits (image edges ≤1,024
and decoded image ≤1 MiB). Neither outgoing sizing nor incoming allocation
budgets may be relaxed to fit a descriptor.

Forms reuse native typed validation, fixed failures and the same schema
fingerprint check. Only matching executable/schema nonsecret drafts restore;
passwords, including editor Undo state, are not navigation snapshots. Cover,
hide, pop and source loss retire live secret editor storage. Every typed
submission still permanently latches that physical process's diagnostic
suppression before writer admission, including later route/background calls.
No sandbox or cryptographic-erasure guarantee follows.

| Runtime | Standard route API | Boundary |
| --- | --- | --- |
| Native/Rust | `supports_structured_views()` plus callbacks above | Negotiated `structured_view_v1`; existing supervised owner/foreground dispatcher |
| Modern Python | Override `structured_view` plus route callbacks | Independent negotiation; sync/async returns preserved |
| Older Native/Modern | Explicitly unsupported without capability | Ordinary operations continue; no metadata-inferred routes |
| Legacy Python | No new callback | Existing callback/scheduling/argument contract unchanged |
| WASM / restricted C ABI | Not exposed | Shared Native envelopes do not add a guest API/ABI extension |

This describes APIs and bounds, not graphical or cross-platform acceptance.

### Runnable standard-view fixtures

Complete implementations are
[`structured-view-fixture.rs`](../compatibility/native-conformance/src/bin/structured-view-fixture.rs)
and [`modern-views`](../compatibility/test-plugins/modern-views/). The Native
fixture is its own binary: **no conformance mode file or environment mode is
needed**. Its manifest ID must be `conformance`, matching its `ServeConfig`.
From the repository root on Linux x86-64:

```sh
cargo build --manifest-path compatibility/native-conformance/Cargo.toml \
  --bin structured-view-fixture
views=$(mktemp -d)
mkdir -p "$views/native/conformance/bin" "$views/modern"
cp compatibility/native-conformance/target/debug/structured-view-fixture \
  "$views/native/conformance/bin/"
cp -R compatibility/test-plugins/modern-views "$views/modern/"
cat > "$views/native/conformance/crikey.toml" <<'TOML'
manifest-version = 1
[plugin]
id = "conformance"
name = "Native Standard Views Fixture"
version = "1.0.0"
runtime = "native"
entrypoint.linux-x86_64 = "bin/structured-view-fixture"
api = ">=1.0,<2"
TOML
CRIKEY_NATIVE_PLUGIN_ROOTS="$views/native" \
CRIKEY_MODERN_PLUGIN_ROOTS="$views/modern" \
CRIKEY_CONFIG_DIR="$views/config" CRIKEY_STATE_DIR="$views/state" \
CRIKEY_DATA_DIR="$views/data" CRIKEY_CACHE_DIR="$views/cache" \
cargo run -p crikey-cli --bin crikey-launcher
```

Use the matching binary/platform entrypoint on other targets and a Python
interpreter satisfying the copied manifest's `>=3.12` requirement. The Modern
manifest is already complete: ID `views-fixture`, entrypoint
`views_plugin:ViewsPlugin`. These commands are runnable author instructions,
not a record that builds or GUI exercises passed.

Native: open **Standard views** → actionless grid item → detail; **Continue**
replaces detail with list, then Continue pushes form. Enter a username,
synthetic password `view-secret`, and After submission `complete` or `navigate`.
Wrong passwords return fixed field refusal; `navigate` replaces form with grid.
The root **Root schema collision** uses the same item/action IDs with a
different schema, intentionally checking namespace separation.

Modern: **Open standard view fixture** pushes gallery; Open pushes detail,
Configure pushes form. Name `reject` returns a fixed Name failure; any other
nonempty name/password and a region replace form with done. Its actionless item
replaces done with list, then list with detail. **Typed route opener** exercises
typed root → gallery. Ordinary root and route items deliberately reuse `shared`.
Use synthetic values only: the fixture intentionally prints submitted passwords
to probe diagnostic suppression, not as an author logging pattern.

Both fixtures support nonsecret query `delay` for pending-refresh exercises and
`source-death` for genuine producer exit. Modern query `oversized-image` exercises
image refusal. See the separately labeled proposed acceptance procedure in the
[user guide](user-guide.md#proposed-standard-view-acceptance-procedure).

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
The native wire details are in [ADR-0010](../../notes/adr/0010-protobuf-codec.md),
[ADR-0017](../../notes/adr/0017-shared-memory-transport.md), and the
[native-host architecture](../../notes/architecture.md).

A C-ABI plugin is supported, but never in process: `crikey-cabi-host` loads the
shared library and speaks the native protocol on its behalf, so a crash or a
hang costs that host process and nothing else (ADR-0015). The library still
needs separately compiled platform and architecture binaries, and the ABI
grants it the full authority of the host process -- it is a compatibility
interface, not a sandbox.

## Pages: display lists and web surfaces

When a result list cannot express what your plugin does — a form, a checklist,
a converter with a live answer — an action may open a *page*: a surface the
launcher presents in place of the result list.

The portable form is a display list. You return flat nodes — rectangles, text,
lines, circles and bounded rasters, each with geometry, colours and a semantic
role — and the host draws them with the launcher's renderer. Your plugin code
never runs on the launcher's UI thread. The host's work of drawing the frame,
including uploading rasters, does run there: its cost is bounded, not absent.
A page of geometry costs kilobytes per frame rather than the 1.10 MiB of a
720×400 pixel canvas; rasters are re-sent on every answered frame.

Alternatively, the native/Rust page API can declare a Linux-only web surface,
described below. The Python SDK does not currently expose that declaration.
The separate engine host sends its own bounded frames to the launcher; these
are not plugin-authored raster nodes. The rationale and limits are in
[ADR-0020](../../notes/adr/0020-plugin-pages.md), and the normative rules are in
[spec §32](../../notes/spec/crikey-spec-v1.md#32-plugin-pages).

For a display-list page, be clear about what that means before you start:

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
surface size, the palette and whether you hold the keyboard. For display-list
pages it also carries accumulated input events in arrival order. Web pointer,
key and IME events go directly to the engine, not to your plugin. Answer the
generation you were given: a frame answering an older generation is dropped,
never drawn, and a resize advances the generation because your previous layout
was for a different size.

For a display-list page, return `redraw_after_ms` only if your page genuinely
changes without input; `0` means static until the user acts, which is what a
form should be. A shorter period than `MIN_PAGE_REDRAW_MS` (16 ms) is raised to
it rather than honoured, because your whole display list - rasters included -
is re-sent on every frame you answer, so a fast timer costs real bandwidth for
a repaint the user is unlikely to perceive. Frames answering input are never
delayed by that floor. Web declarations must leave `redraw_after_ms` at `0`;
the engine drives their repaints, and a nonzero plugin timer is rejected.

Escape is reserved by the host. It closes your page and is never delivered to
you, so do not plan an Escape binding. You close your own page by returning a
frame with `close()` set — that frame is not drawn, the close happens instead —
and if the host closed it instead you get one last request carrying
`PageInputKind::Closed`, whose frame is discarded, so your state machine always
has a defined end.

### Web surfaces and text input

A page may use a host-owned web surface instead of a display list; it cannot
mix the two in one frame. At most one page, and therefore one web surface, is
live at once. Opening another page closes the previous one first. The engine
starts only after the user opens the page and the launcher releases the
supervised host when the page closes.

Web surfaces are currently available only on Linux with an engine-enabled
`crikey-web-host`. `packaging/linux/build.sh --with-web-surface` builds one;
`--web-host-binary` stages a supplied host that passes the packaging checks.
The normal no-web package does not provide that host, although an operator may
independently supply one beside the launcher, on `PATH`, or through the
`CRIKEY_WEB_HOST` override. The current Flatpak recipe neither builds nor
stages the engine host and is explicitly no-web. Windows and macOS do not have
a web-surface host in v1.

Only HTTP and HTTPS document navigation is accepted, including redirects and
links. This is not a general subresource or network sandbox. Auxiliary browser
windows/popups are unsupported and blocked. Downloads are canceled: the page
protocol has no operation for authorizing a download destination. Do not rely
on an ephemeral storage profile alone to prevent download writes.

A stated address requires network permission; persistent cookies and local
storage need a separate grant:

```toml
[permissions]
network = true
web-storage = true
```

The default `WebStorage::Ephemeral` profile is destroyed when the page closes
and does not require `web-storage`. `WebStorage::Persistent` survives later
openings in storage isolated to that plugin; `network = true` does not imply
it. Permission and platform checks happen before the engine is spawned, so a
denied declaration cannot briefly start WebKit. Do not use `file:`, `data:` or
`javascript:` URLs; `WebSurfaceBuilder::build` rejects them before serving the
frame.

On Linux, the host-selected persistent profile directory must be representable
as UTF-8 because WPE's storage APIs take UTF-8 paths. A non-UTF-8 directory
closes a persistent page with a startup diagnostic instead of substituting a
different profile path. The host CLI preserves native path bytes, but that
does not make such a path usable for persistent WPE storage. Ephemeral pages
do not use that directory and are unaffected.

The first frame must name an address. Later frames may use
`WebSurfaceBuilder::stay`, whose empty URL preserves the document's current
navigation. Do not infer that the host accepted your first frame just because
you returned it: a resize can make that generation stale. For a fixed entry
document, repeat its URL as below; the host suppresses navigation to the same
last-requested URL, preserving navigation within the live document. Use a
different URL to request a new navigation.

```rust
use crikey_core::Result;
use crikey_plugin_sdk::{
    ExecuteOutcome, ExecuteRequest, PageFrame, PageRequest, PluginContext,
    WebStorage, WebSurfaceBuilder,
};

struct Dashboard;

impl Dashboard {
    fn execute_page(
        &mut self,
        request: ExecuteRequest,
        _context: &dyn PluginContext,
    ) -> Result<ExecuteOutcome> {
        if request.item.0 == "dashboard" {
            return Ok(ExecuteOutcome::show_page("dashboard"));
        }
        Ok(ExecuteOutcome::Completed)
    }

    fn page_frame(
        &mut self,
        request: PageRequest,
        _context: &dyn PluginContext,
    ) -> Result<PageFrame> {
        let frame = WebSurfaceBuilder::navigate(
            request.generation,
            "https://dashboard.example/plugin",
        );

        frame
            .title("Service dashboard")
            .storage(WebStorage::Persistent)
            .build()
    }
}
```

Use those method bodies from `Plugin::execute_outcome` and `Plugin::page`.
Returning the request's generation is still mandatory; the builder takes it
explicitly for that reason. Keep the same storage choice on stay frames:
`WebSurfaceBuilder::stay` defaults to `WebStorage::Ephemeral`, so a persistent
page must repeat `.storage(WebStorage::Persistent)` on every stay declaration.
Changing storage mode while the page is open closes it with a diagnostic; use a
new page opening to change profiles.

The launcher keeps its in-page loading indicator until the first accepted RGBA
frame arrives; the declaration itself is not painted content. Permission
denial, an unavailable platform or host, missing required cgroup supervision,
a spawn/connect/protocol failure, a host crash, or an engine load failure
closes the page with a specific `Page closed: …` diagnostic while leaving the
launcher running.

On Linux web startup requires a writable delegated cgroup-v2 hierarchy with a
suitable existing ancestor whose `cgroup.subtree_control` already enables
`memory` for children and whose `cgroup.procs` is writable for the kernel's
common-ancestor migration permission check. The launcher considers safely
resolved cgroup-v2 mount views, including writable delegated bind mounts,
searching only proper ancestors within each view, never its own cgroup.
Inherited memory limits visible through any safe view remain binding when
another view supplies the writable parent.
Candidate parents must contain no processes, except at the visible mount root;
that exception still requires the enabled memory controller and every private
leaf control. CriKey creates a private web-host leaf there, separate from the
leaf containing the launcher. A writable populated launcher leaf alone is not enough:
Linux's no-internal-process rule prevents enabling the memory controller for
its children. CriKey does not move the launcher or unrelated processes, change
subtree controller settings, or cross unavailable delegation boundaries.
The private leaf must provide writable memory controls and `cgroup.kill`; the
launcher prepares and verifies these controls and attaches the child before
web-host code runs.

The memory ceiling is the lesser of 768 MiB and one eighth of physical RAM,
reduced further by the launcher's inherited finite cgroup limits, even when the
web-host leaf has a different parent. An effective value below 256 MiB refuses
web startup; this ceiling is not an allocation guarantee.

If the ceiling cannot be determined or the required cgroup controls cannot be
prepared, the page closes instead of starting an unsupervised engine. The
diagnostic begins with
`Page closed: web surface refused: required cgroup-v2 supervision is unavailable:`
and ends with the specific failure, such as no suitable writable delegated
parent with `memory` already enabled for children or an unwritable control.
A ceiling below the floor uses the same prefix followed by
`its effective memory ceiling is … MiB, below the 256 MiB minimum`.
There is no warning-only fallback. This deliberately makes web pages
unavailable on Linux installations without the required delegation, even if an
engine-enabled host is installed; ordinary launcher use and display-list pages
still work.

The required cgroup covers detached and reparented descendants as well as the
host. Its `cgroup.kill` control is the authoritative process-tree escalation;
there is no fallback that scans procfs or kills remembered descendant PIDs.
Graceful process-group SIGTERM is limited to the original root's unreaped
lifetime. After the root is reaped, cleanup uses the cgroup rather than its
potentially reused numeric PID or process group.

Whole-tree cleanup depends on the launcher supervisor remaining alive to write
`cgroup.kill`. After an abrupt launcher crash or SIGKILL, the parent-death
signal targets only the web-host root; it does not guarantee termination of
detached descendants. The cgroup memory ceiling remains enforced, but v1 has
no independent reaper that guarantees whole-tree cleanup after launcher death.

The implemented input route forwards pointer, direct key press and release,
focus, resize, IME preedit and IME commit events to the Linux host; Tab is a web
key and Escape remains reserved by the launcher. It also carries the
engine-reported caret rectangle used to position a candidate window.
The native Linux route uses XKB hardware keycodes (evdev code + 8). The
egui-only fallback forwards available named keys and actual text/IME, without
guessing hardware codes or physical key identity. The UI
forwards Shift, Control, Alt and Super state; the current winit/egui input
source does not expose active Caps Lock or Num Lock modifier state, although
their key events and keyboard-resolved text are forwarded. This wiring is not
a claim of complete IME support: interoperability across real ibus/fcitx input
methods has not been measured, and v1 has no surrounding-text or
delete-surrounding operation for correction of already-committed text. Do not
require CJK reconversion or IME-directed deletion to complete a form.

### A worked page

A note editor with a heading, a save button, a checkbox, a text field and a
done button, holding its state across frames:

```rust
use crikey_core::Result;
use crikey_plugin_sdk::{
    ExecuteOutcome, ExecuteRequest, PageBuilder, PageFrame, PageInputKind, PageRect,
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
            .text_field(FIELD, PageRect::new(16.0, 56.0, width - 32.0, 28.0), "Note text", shown)
            .checkbox(PINNED, 16.0, 100.0, "Pin to top", self.pinned)
            .button(SAVE, PageRect::new(16.0, 136.0, 96.0, 28.0), "Save")
            .button(DONE, PageRect::new(124.0, 136.0, 96.0, 28.0), "Done")
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

### Rasters: a picture, or a surface you paint

Every builder above composes shapes the host draws. Two hand the host finished
pixels instead, and both produce the same node kind:

```rust
page = page
    // A picture the plugin already holds, scaled into the node's rectangle.
    .image(PageRect::new(16.0, 200.0, 48.0, 48.0), "Cover art", 32, 32, cover_rgba)?
    // A surface the plugin paints, addressed in pixels.
    .canvas(
        PageRect::new(80.0, 200.0, 120.0, 48.0),
        "Load over the last minute",
        60,
        24,
        |canvas| {
            for (column, sample) in samples.iter().enumerate() {
                let bar = (canvas.height() * sample) / 100;
                canvas.fill_rect(column as u32 * 5, canvas.height() - bar, 4, bar, palette.accent);
            }
        },
    )?;
```

Both take raw RGBA8 and nothing else: four bytes per pixel, row-major,
non-premultiplied, no row padding. **The host decodes no image format.** There
is no PNG, JPEG or SVG decoder in the launcher process, deliberately: a decoder
is an attack surface, and a compressed byte count says nothing about the
allocation behind it. A plugin that ships a PNG decodes it in its own process,
where a malformed file costs its author a worker and costs the launcher
nothing.

`canvas` is there so you never write `(y * width + x) * 4`. It hands your
closure a correctly sized, fully transparent buffer with `fill`, `fill_rect`
and `set_pixel` in the raster's own pixel coordinates; writes outside it are
clipped rather than fatal, because a bar computed from your own state should
cost a pixel when it overshoots and not the worker. The pixel dimensions are
separate from the node's rectangle because the host scales one into the other:
a chart does not need one raster pixel per logical pixel, and a small raster is
a small frame.

Both refuse a raster that cannot exist — a zero side, a side over the edge
bound, a byte count that disagrees with the dimensions, or more bytes than one
node may carry — by returning `PageError` from the call that was wrong, rather
than building a node the host would refuse the whole frame for. That refusal
consumes the builder, which is the point: the frame was never going to be
drawable.

**A raster carries no accessible name.** A screen reader gets nothing from a
picture of a chart, so a raster with an empty label is decoration and is
reported as nothing at all; pass a label — which sets a `Label` role for you —
whenever the raster carries information the rest of your page does not also say
in text. A raster is never focusable either: when the user has to be able to
operate it, put a `NodeShape::None` node carrying a `Button` role and a
`node_id` over it, which is exactly what that shape is for.

### Roles are the accessibility contract

A painted glyph is not an accessible name. Nothing about drawing the word
"Save" tells anything that a button exists, so roles, labels and focus order
travel beside the drawing. `button`, `checkbox`, `text_field` and `heading` set
a role for you, and `image` and `canvas` do so only when you give them a label;
a bare `rect` or `text` node is decoration by definition, which is correct for a
divider and wrong for something the user can click.

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

Rasters are bounded three ways, and all three are named constants you can
check against: 1 MiB of pixels per node (`MAX_PAGE_IMAGE_BYTES`), 4 MiB summed
across every raster in one frame (`MAX_PAGE_IMAGE_TOTAL_BYTES`), and 1,024
pixels per side (`MAX_PAGE_IMAGE_EDGE`). The edge bound is checked separately
from the byte count, so a 1x262,144 strip is refused on its shape rather than
sneaking under the megabyte. A raster on a node of any other shape, and an
image node carrying no raster, are refusals too: the first would mean the two
sides disagree about what the node is, the second would draw a hole.

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

A legacy plugin is an existing package loaded by the Legacy Compatibility Layer.
Its callbacks are serialized per plugin instance. In the default `legacy-strict`
profile, CriKey broadcasts the initial query, does not time-debounce, replaces pending obsolete
work, sets `should_terminate()` for running obsolete callbacks, rejects stale
results, disables dynamic cross-request caching, and does not impose a modern
minimum query length or prefix gate. `set_suggestions()` publishes one complete
result batch.

`legacy-optimized` can change behavior (debounce, gating, caching, deadlines,
or event frequency) and must be explicitly selected; never assume it is the
default. Run `crikey plugin doctor [ID]` and
`crikey dev test-legacy-compat` while porting. The compatibility matrix and
limitations are documented in [ADR-0006](../../notes/adr/0006-legacy-scheduling.md) and
[ADR-0005](../../notes/adr/0005-python-hosting.md). Legacy dynamic suggestions are not
cached by default, and legacy `Match`/`Sort` requests are recorded but do not
cause host-side matching or sorting of suggestion batches.

## Typed result previews

Preview is host-rendered content, separate from custom display-list/web pages.
It does not consume or create a second custom page session. Declare deferred
content only for items that can be previewed; the host requests the current
selection, not every result, and cancels/coalesces selection changes.

Native Rust plugins opt in with `Plugin::supports_preview()`, declare items
with `ItemBuilder::preview_deferred()`, and implement
`Plugin::preview(item_id, context) -> Result<PreviewOutcome>`. The optional
wire capability is `preview_v1`; old workers keep their existing operations.
The `sdk/rust/examples/preview.rs` worker demonstrates text, formatted markdown,
decoded pixels, metadata, deliberate links, long content and cancellation.

Modern Python plugins use `Item(..., preview_deferred=True)` and
`preview(self, item, context)`, returning `Preview(...)` or `None` for an empty
preview. The SDK supplies `PreviewImage` and `PreviewValue`; the
`compatibility/test-plugins/modern-preview` package demonstrates the real worker
path. Both runtimes execute preview in the same originating worker instance as
the item's catalog or suggestion publication, under its existing concurrency
and call budgets. Catalog-only deferred items do not require suggestions.
Preview must not assume that cancellation reverses any effects.
Deliberate actions and native pages take priority over speculative previews.
The host reserves one foreground intent and requests preview cancellation without
blocking the UI. The preview retains its real action permit until its callback
returns (or its worker is reaped); cancellation alone never frees concurrency.
The waiting action uses the original item's worker, never a replacement process.
Rows carry their exact `PluginId` separately from display names; preview
selection and dispatch use `(PluginId, ItemId)`. Equal item IDs in different
plugins must neither redirect callbacks nor suppress another owner's preview.

Content is bounded to 256 KiB aggregate text, 4,096 prepared nodes/events,
128 labeled values (8 KiB each), and image edges of 1,024 with at most 1 MiB
decoded RGBA per image. Invalid content produces an explicit preview error
without disabling the selected result. Markdown is parsed before paint:
headings, paragraphs, emphasis, code, lists and explicit HTTP/HTTPS links are
supported; HTML, scripts and embedded images are rejected. Supply image pixels
through the typed image field. The renderer does not fetch remote assets.
Provider-side networking remains subject to the existing network policy.

Links are semantic host intents, not browser output from the renderer. A click
is admitted only for the current view/query/selection/request and live producer
incarnation, and the owner's `process` permission gates the platform document
opener. A refused or retired link reports a visible error.

| Runtime | Deferred callback | Host-known metadata | Boundary |
| --- | --- | --- | --- |
| Native Rust/native protocol | Optional `preview_v1` | Yes | Exact originating worker/connection; bounded transport |
| Modern Python | Optional `preview_v1` | Yes | Existing driver supervisor and originating interpreter instance |
| Older native/modern workers | No capability required for existing operations | Yes | Deferred operation explicitly unavailable when unsupported |
| Legacy Python | No new callback | Yes | Existing legacy callbacks, ordering and arguments unchanged |
| WASM | Not exposed | Yes | Adapter does not advertise `preview_v1` |
| Restricted C ABI | Not exposed | Yes | Adapter does not advertise `preview_v1` |

This matrix describes implementation support, not completed platform/runtime
acceptance. Native-window acceptance evidence is recorded separately; macOS
does not gain a platform URL opener from this feature.

## Permissions, security, and troubleshooting

Manifests may request filesystem read/write, network client/listener,
web-storage, clipboard, process, window, notification, secret-storage,
environment, native-library, and persistent-background capabilities. Some are
enforced gates, one is refused at parse time, and declarations the current host
does not honour are reported by `crikey plugin doctor` with the reason. A web
surface's network and persistent-storage grants are enforced before its host
spawns.

Independently of the manifest, Linux confines every supervised plugin process
with Landlock: your plugin may write only beneath the
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
complete boundary, link readers to [platform services in the spec](../../notes/spec/crikey-spec-v1.md#18-platform-services),
[ADR-0011](../../notes/adr/0011-wayland-backend.md), and
[ADR-0015](../../notes/adr/0015-restricted-c-abi.md).