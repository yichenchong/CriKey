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