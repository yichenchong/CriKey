# CriKey Technical Specification

This document is the normative target for the v1 architecture. It describes
required behavior, including future components; the implementation status of
each delivery phase is tracked separately in [`docs/ROADMAP.md`](../ROADMAP.md).

## 1. Product Definition

CriKey shall be a fast, keyboard-driven application launcher with an
established launcher-style interaction model and a modern, cross-platform,
extensible architecture.

CriKey shall support:

1. Compatibility with existing Keypirinha plugins on Windows through a Legacy Compatibility Layer.
2. A modern Python plugin API with ordinary Python imports and managed dependencies.
3. A compiled-plugin API suitable for Rust and other native languages.
4. Native performance for query processing, indexing, ranking, and user-interface operations.
5. Isolation of third-party plugins from the CriKey user-interface process.
6. Explicit abstractions for platform-dependent functionality.
7. Responsive behavior under rapid user input and slow plugin execution.
8. Deterministic cancellation and rejection of obsolete plugin work.
9. Separate scheduling contracts for legacy and modern plugins.

CriKey is an independent project and shall not represent itself as affiliated with, endorsed by, or sponsored by Keypirinha or its developer.

---

## 2. Scope

### 2.1 Initial scope

The initial implementation shall include:

- A Rust application core.
- A desktop launcher window.
- Global-hotkey activation.
- Application discovery.
- Catalog-based search.
- Incremental query suggestions.
- Configurable actions.
- Plugin lifecycle management.
- Existing Keypirinha plugin support on Windows.
- A new Python plugin SDK.
- A native subprocess plugin SDK.
- Platform abstraction interfaces.
- Query generation tracking.
- Query cancellation.
- Stale-result rejection.
- Debouncing for modern plugins.
- Obsolete-work replacement for legacy plugins.
- Streaming result handling for modern plugins.
- Bounded queues and backpressure.
- Windows as the first complete platform backend.

### 2.2 Later scope

Later releases may include:

- Full macOS backend coverage.
- Full Linux backend coverage.
- WebAssembly plugins.
- A public plugin repository.
- Signed plugin packages.
- Restricted in-process native plugins.
- Distributed or remote indexing.
- Context-aware and learned ranking.
- Shared-memory transport for unusually large plugin result sets.

### 2.3 Non-goals

The initial implementation shall not attempt to:

- Emulate Windows APIs on macOS or Linux.
- Make Windows-specific Keypirinha plugins automatically portable.
- Provide ABI compatibility with arbitrary Rust dynamic libraries.
- Run untrusted third-party native code inside the main process.
- Reproduce undocumented Keypirinha behavior without evidence that plugins depend on it.
- Guarantee that every modern plugin receives every intermediate query typed by the user.
- Apply modern scheduling optimizations to legacy plugins when doing so would alter documented legacy behavior.
- Permit slow plugins to block or delay faster result sources.

---

## 3. Terminology

### 3.1 Legacy plugin

A legacy plugin is a plugin written for the documented Keypirinha Python API and loaded through CriKey's Legacy Compatibility Layer.

### 3.2 Modern plugin

A modern plugin is a plugin written for CriKey's Python, native-process, WebAssembly, or future plugin APIs.

### 3.3 Legacy Compatibility Layer

The Legacy Compatibility Layer is the subsystem that implements documented Keypirinha-compatible APIs, package loading, lifecycle behavior, scheduling semantics, and compatibility diagnostics.

"Keypirinha" may be used descriptively in documentation when identifying the API or package format with which CriKey is compatible. It shall not be used as the branded name of a CriKey subsystem.

### 3.4 Query generation

A query generation is an internal monotonically increasing identifier representing one complete launcher search state.

### 3.5 Debouncing

Debouncing delays dispatch of work for a short interval so that several rapid query changes may be represented by only the latest query.

### 3.6 Obsolete-work replacement

Obsolete-work replacement dispatches work promptly but cancels, invalidates, or replaces older queued work after a newer query arrives.

Legacy strict mode shall use obsolete-work replacement rather than time-based debouncing.

---

## 4. Implementation Languages

### 4.1 Core language

The CriKey core shall be implemented in Rust.

Rust shall be used for:

- Input processing.
- Query normalization.
- Query scheduling.
- Debouncing.
- Cancellation.
- Catalog management.
- Candidate matching.
- Ranking.
- Result merging.
- Plugin supervision.
- Configuration management.
- Caching.
- Persistent state.
- Platform-independent services.
- Performance-sensitive platform integrations.

### 4.2 Python runtime

CriKey shall support CPython for:

- Existing Keypirinha plugins.
- New Python plugins.
- Python plugins containing supported native extension modules.

Python code shall not execute on the CriKey user-interface thread.

### 4.3 Native plugin languages

The native plugin protocol shall be language-neutral.

Official SDK support shall initially be provided for Rust.

The protocol shall permit unofficial SDKs for:

- C.
- C++.
- Go.
- Zig.
- C#.
- Other languages capable of local IPC.

---

## 5. System Architecture

CriKey shall contain the following logical components:

```text
CriKey
├── User Interface
├── Input and Query Scheduler
│   ├── Query Generation Tracker
│   ├── Modern Plugin Debouncer
│   ├── Legacy Obsolete-Work Manager
│   └── Cancellation Manager
├── Query Engine
├── Catalog Store
├── Ranking Engine
├── Result Aggregator
├── Action Dispatcher
├── Plugin Supervisor
│   ├── Legacy Compatibility Layer
│   ├── Modern Python Host
│   └── Native Plugin Host
├── Core Services
└── Platform Backend
    ├── Windows
    ├── macOS
    └── Linux
```

### 5.1 Main process

The main process shall own:

- The launcher window.
- Keyboard input.
- Current query state.
- Query generation identifiers.
- Core catalog indexes.
- Ranking.
- Result presentation.
- User history.
- Plugin supervision.
- Platform services.

Third-party plugin code should not execute directly in the main process.

### 5.2 Plugin processes

Python and native plugins shall normally execute in supervised worker processes.

The supervisor shall be able to:

- Start a plugin.
- Stop a plugin.
- Restart a plugin.
- Detect crashes.
- Detect timeouts.
- Cancel obsolete work.
- Reject stale results.
- Apply backpressure.
- Collect performance metrics.
- Record structured diagnostics.
- Disable repeatedly failing plugins.

### 5.3 Platform separation

Platform-independent crates shall not directly call Windows, macOS, or Linux desktop APIs.

Platform-specific behavior shall be exposed through interfaces implemented by separate backend modules.

---

## 6. User Interaction

### 6.1 Activation

The user shall be able to open or close CriKey using a configurable global keyboard shortcut.

### 6.2 Query flow

For each user-visible query:

1. The UI shall update immediately.
2. The query scheduler shall assign a new generation identifier.
3. The core catalog shall be searched immediately or within the current UI frame.
4. Obsolete plugin requests shall be cancelled or marked stale.
5. Legacy and modern plugins shall be scheduled according to their respective runtime policies.
6. Results shall be accepted incrementally where supported.
7. Results from obsolete generations shall be discarded.
8. Results shall be ranked and deduplicated.
9. The result list shall update without waiting for every plugin.
10. The user shall be able to execute the selected result or choose an alternate action.

### 6.3 Keyboard support

All principal functionality shall be usable without a mouse.

The interface shall support:

- Result navigation.
- Page navigation.
- Action selection.
- Query completion.
- Argument entry.
- Plugin-specific modes.
- Cancellation.
- Launcher dismissal.

### 6.4 Result display

A result may contain:

- Primary label.
- Secondary description.
- Icon.
- Category.
- Plugin name.
- Match highlights.
- Argument hint.
- Status annotation.
- Default action.
- Alternate actions.

### 6.5 Rapid input behavior

Rapid typing shall not cause:

- One expensive modern-plugin request per keystroke unless explicitly requested.
- An unbounded queue of obsolete legacy or modern queries.
- Result flicker from late responses.
- UI-thread blocking.
- Reordering of results across query generations.
- Excessive process or network activity.

---

## 7. Query Scheduling Profiles

CriKey shall implement at least three scheduling profiles.

### 7.1 `legacy-strict`

`legacy-strict` shall be the default for unchanged Keypirinha plugins.

Its behavior shall be:

```text
Time-based debounce            Disabled
Initial query broadcast        All loaded legacy plugins
Callbacks per plugin           Serial
Obsolete running request       should_terminate() becomes true
Pending obsolete requests      Replaced by newest pending request
Stale results                  Rejected
Dynamic-result cache           Disabled
Minimum query length           None
Prefix relevance gating        None
Hard process termination       Hung-worker recovery only
set_suggestions()              One complete publication
on_events()                    Legacy-compatible delivery
```

### 7.2 `legacy-optimized`

`legacy-optimized` may be enabled per plugin or by explicit user override.

It may permit:

- Time-based debounce.
- Minimum query length.
- Prefix or keyword gating.
- Dynamic result caching.
- Stricter deadlines.
- Reduced event frequency.

CriKey shall label this profile as potentially behavior-changing and shall not enable it by default for an unchanged legacy plugin.

### 7.3 `modern`

The `modern` profile shall apply to modern Python and native plugins.

It may support:

- Manifest-defined debounce.
- Leading-edge execution.
- Trailing-edge execution.
- Maximum wait.
- Activation gates.
- Minimum query length.
- Cancellation tokens.
- Streaming results.
- Backpressure.
- Explicit cache policies.
- Declared concurrency.
- Per-request deadlines.

---

## 8. Input and Query Scheduling

### 8.1 Query generations

Every query state shall receive a monotonically increasing generation identifier.

The generation identifier shall be included internally in:

- Plugin suggestion requests.
- Plugin result batches.
- Cancellation messages.
- Core search results.
- Result aggregation state.

Legacy plugins shall not be required to know or handle query generation identifiers.

CriKey shall display results only for the current generation.

### 8.2 Immediate local search

CriKey shall not debounce inexpensive local operations unnecessarily.

The following should execute immediately:

- Query text rendering.
- Core catalog lookup.
- Cached result lookup.
- Local history lookup.
- Prefix and fuzzy matching over in-memory indexes.

### 8.3 Modern plugin debouncing

CriKey shall support per-plugin query debouncing for modern plugins.

Each modern plugin may declare:

- No debounce.
- A fixed debounce interval.
- An adaptive debounce policy.
- A minimum query length.
- A query-prefix trigger.
- A query mode or keyword trigger.

A reasonable default for ordinary dynamic modern plugins shall be between 30 and 75 milliseconds.

Network-backed modern plugins should default to a longer interval, such as 100 to 250 milliseconds.

### 8.4 Legacy dispatch semantics

CriKey shall not apply time-based query debouncing to `legacy-strict` plugins.

When a relevant query change occurs:

1. If the plugin is idle, CriKey shall dispatch the query promptly.
2. If the plugin is processing an older query, `should_terminate()` shall become true for that work.
3. The newer query shall become the newest pending request.
4. Older undispatched pending queries may be discarded.
5. After the current callback returns, the newest pending request shall be dispatched.
6. No two lifecycle callbacks shall execute concurrently on the same legacy plugin instance.
7. Suggestions returned for obsolete queries shall not be displayed.

This mechanism shall be described as obsolete-work replacement rather than debouncing.

### 8.5 Leading and trailing execution

The modern scheduler shall support:

- Leading-edge execution.
- Trailing-edge execution.
- Leading and trailing execution.
- Maximum-wait execution.

Default modern behavior should be:

- Immediate execution when a plugin becomes newly relevant.
- Trailing execution while the user continues typing.
- A maximum wait preventing indefinite postponement during continuous input.

These options shall not be imposed on `legacy-strict` plugins.

### 8.6 Maximum debounce wait

A modern plugin debounce policy may define a maximum wait.

When the user continues typing for longer than the maximum wait, the latest query shall be dispatched even if the ordinary debounce period has not completed.

### 8.7 Adaptive debouncing

The modern scheduler may adjust debounce intervals using:

- Observed plugin latency.
- Query length.
- Typing speed.
- Whether the plugin accesses the network.
- Whether prior results are available.
- Current CPU load.
- Plugin timeout history.

Adaptive behavior shall remain bounded by configured minimum and maximum intervals.

Adaptive debouncing shall not be applied to `legacy-strict` plugins.

### 8.8 Query coalescing

For modern plugins, when multiple undispatched queries exist for the same plugin, the scheduler shall retain only the newest query unless the plugin explicitly requests full event delivery.

For `legacy-strict` plugins, CriKey may replace obsolete pending requests with the newest pending request while preserving prompt initial dispatch and serial callback execution.

### 8.9 Empty query behavior

Modern plugins shall explicitly declare whether they support empty queries.

Legacy plugins shall receive empty-query callbacks according to the documented legacy lifecycle and their own internal behavior.

The core may show:

- Frequently used items.
- Recently used items.
- Pinned items.
- Default catalog entries.

### 8.10 Minimum query length

A modern plugin may declare a minimum normalized query length.

A `legacy-strict` plugin shall not be subject to a host-imposed minimum query length.

### 8.11 Relevance gating

CriKey shall avoid invoking every modern plugin for every query where declared activation metadata permits this.

A modern plugin may declare:

- Keywords.
- Prefixes.
- Categories.
- Query patterns.
- Modes.
- Context requirements.
- Application-context requirements.

Initial legacy suggestion requests shall be broadcast to all loaded `legacy-strict` plugins as required by the compatibility contract.

### 8.12 Fairness

A high-volume plugin shall not monopolize:

- CPU time.
- IPC capacity.
- Result-list capacity.
- Worker threads.
- UI update frequency.

CriKey shall apply per-plugin budgets and fair queuing where needed, without silently altering legacy-visible semantics except under documented safety limits.

---

## 9. Cancellation and Obsolete Work

### 9.1 Cancellation requirement

Every dynamic modern suggestion request shall be cancellable or logically invalidatable.

A plugin runtime shall support at least one of:

- Cooperative cancellation tokens.
- Cancellation messages.
- Request-generation checks.
- Worker termination.
- Result rejection by the host.

### 9.2 Legacy cancellation API

The Legacy Compatibility Layer shall implement the legacy `Plugin.should_terminate()` API as a cheap cooperative cancellation check.

It shall become true when at least one of the following applies:

- The current query has become obsolete.
- The package is being reloaded.
- CriKey is shutting down.
- The plugin is being disabled.
- The current plugin instance has been superseded.

Legacy plugins shall not be required to accept a CriKey-specific cancellation object.

### 9.3 Cancellation triggers

CriKey shall cancel or invalidate a request when:

- The query changes.
- CriKey closes.
- The plugin is disabled.
- The plugin is restarted.
- The request exceeds its hard deadline where the runtime contract permits enforcement.
- The active profile changes.
- Relevant configuration changes.
- The plugin loses a required permission.

### 9.4 Cooperative cancellation

Modern SDKs shall expose a cancellation token or equivalent.

Plugins should check cancellation:

- Before expensive work.
- During long loops.
- Between network or filesystem operations.
- Before emitting large result batches.
- Before committing query-specific state.

Legacy documentation and diagnostics shall encourage frequent checks of `should_terminate()`.

### 9.5 Non-cooperative plugins

CriKey shall remain correct when a plugin ignores cancellation.

In that case:

- Late results shall be discarded.
- The plugin may be throttled where compatible.
- Repeated violations shall be reported.
- The worker may be restarted as fault recovery.
- Dedicated-process isolation may be applied.

### 9.6 Legacy hard deadlines

CriKey shall not kill a legacy plugin worker merely because a suggestion callback exceeds the modern default hard query deadline.

For legacy callbacks, CriKey shall use:

- Soft-latency warnings.
- Cooperative termination requests.
- Stale-result rejection.
- A substantially longer watchdog threshold for a genuinely hung worker.
- Forced restart only as fault recovery.

### 9.7 Action execution

Action execution shall have a separate lifecycle from suggestion requests.

An action shall not be cancelled merely because the launcher query changes.

---

## 10. Core Data Model

### 10.1 Catalog item

Every catalog item shall include:

```text
Item
- stable_id
- plugin_id
- category
- label
- description
- target
- search_terms
- icon_reference
- argument_policy
- hit_policy
- score_hint
- metadata
- actions
```

### 10.2 Stable identity

An item's identity shall not depend solely on its display label.

A modern plugin shall provide a stable item identifier where possible.

The host may derive an identifier from the plugin identifier, category, and target when the plugin does not provide one.

The Legacy Compatibility Layer shall derive stable internal identities where possible without changing the legacy API.

### 10.3 Item categories

The core shall support extensible item categories.

Initial categories shall include:

- Application.
- File.
- Directory.
- URL.
- Command.
- Expression.
- Keyword.
- Contact.
- Clipboard item.
- Plugin-defined item.

### 10.4 Actions

An action shall contain:

```text
Action
- action_id
- label
- description
- applicable_categories
- icon_reference
- execution_policy
```

An item may define a default action and zero or more alternate actions.

---

## 11. Query Engine

### 11.1 Responsibilities

The query engine shall perform:

- Unicode normalization.
- Tokenization.
- Case normalization.
- Prefix matching.
- Substring matching.
- Fuzzy matching.
- Acronym matching.
- Keyword matching.
- Candidate pruning.
- Deduplication.
- Score aggregation.
- Stable ordering.

### 11.2 Native hot path

Common matching and ranking operations shall execute in Rust.

Modern plugins should submit searchable candidate data rather than independently implementing all matching logic.

Legacy matching and sort policies shall be translated through the Legacy Compatibility Layer.

### 11.3 Ranking signals

The default ranker shall support:

- Textual match quality.
- Exact-prefix preference.
- Match position.
- Item category.
- Plugin score hint.
- Selection frequency.
- Selection recency.
- Query-specific history.
- Application context where available.
- Configured user preferences.

User-history signals shall be disableable.

### 11.4 Result generation

The query engine shall support:

- Persistent catalog items.
- Query-dependent suggestions.
- Streaming modern result batches.
- Partial modern results.
- Query cancellation.
- Result-generation identifiers.
- Legacy complete suggestion publications.

### 11.5 Incremental ranking

The result aggregator shall be able to rerank incrementally as new batches arrive.

Incremental reranking shall preserve selection stability where practical.

Current v1 status: `MemoryResultAggregator` preserves first-acceptance order and
replaces enrichment items in place; it does not yet rerank plugin batches.
`SearchService` applies the default ranker on the local catalog path.

### 11.6 Result-list stability

The UI shall reduce disruptive result movement through one or more of:

- Stable tie-breaking.
- Temporary rank hysteresis.
- Selection anchoring.
- Batch-based updates.
- Minimum score-change thresholds.

Correctness and relevance shall take priority over strict visual stability.

### 11.7 Result limits

CriKey shall impose safety limits on:

- Results accepted per modern plugin per batch.
- Results accepted per modern plugin per query.
- Total results retained per query.
- Icon and metadata payload sizes.
- Number of UI updates per frame.

Legacy result limits shall not silently use modern defaults. Any legacy safety limit shall be separately documented and set high enough to preserve ordinary compatibility.

---

## 12. Result Streaming and Backpressure

### 12.1 Modern streaming

Modern plugins shall be able to return:

- A complete result set.
- Multiple result batches.
- An initial fast batch followed by slower enrichment.
- Progress or partial-completion metadata.

### 12.2 Legacy suggestion publication

The Legacy Compatibility Layer shall translate a legacy `set_suggestions()` call into one complete internal result publication for the corresponding callback.

CriKey may transport that publication internally in bounded chunks, but shall preserve:

- Item order where required by the selected sort policy.
- Match method.
- Sort method.
- Error-item behavior.
- Association with the plugin instance.
- Association with the current query.
- Replacement semantics of the suggestion list.

Current v1 status: the compatibility matrix classifies legacy matching and
sorting as partial. The plugin-suggestion path does not host-match or apply a
legacy sort policy; decoded legacy items have a tied score hint, so their
published order is retained. This limitation is not present on catalog search.

### 12.3 Backpressure

The host shall apply backpressure when:

- A plugin emits results faster than they can be consumed.
- The result queue exceeds its configured capacity.
- The query becomes obsolete.
- The UI no longer needs additional low-ranked results.
- A modern plugin exceeds its per-query quota.

For legacy plugins, CriKey should prefer cancellation of obsolete transport and bounded serialization over arbitrary truncation of a complete `set_suggestions()` publication.

### 12.4 Bounded queues

All query and result queues shall be bounded.

When a queue is full, CriKey shall use an explicit policy such as:

- Drop obsolete requests.
- Replace an older pending request with the newest one.
- Reject additional low-priority modern result batches.
- Pause plugin reads.
- Disconnect a plugin violating protocol safety limits.

CriKey shall not permit unbounded memory growth due to plugin traffic.

### 12.5 Result completion

A modern plugin shall indicate whether a batch is:

- Partial.
- Final.
- Cancelled.
- Failed.

A legacy `set_suggestions()` call shall be treated as complete for that callback.

### 12.6 Enrichment updates

A modern plugin may update an existing result using its stable item identifier.

Legacy plugins shall not be assumed to support incremental enrichment.

---

## 13. Plugin Model

### 13.1 Plugin types

CriKey shall support:

1. Legacy Keypirinha Python plugins.
2. Modern Python plugins.
3. Native subprocess plugins.
4. Optional future WebAssembly plugins.
5. Restricted first-party in-process components.

### 13.2 Plugin lifecycle

Modern plugins shall support logical lifecycle operations equivalent to:

```text
load
start
build_catalog
suggest
execute
handle_event
stop
unload
```

Legacy callbacks shall be mapped to the documented Keypirinha lifecycle, including equivalents of:

- `on_start`
- `on_catalog`
- `on_suggest`
- `on_execute`
- `on_activated`
- `on_deactivated`
- `on_events`

### 13.3 Scheduling

Modern plugins shall declare one of:

- `serial`
- `concurrent_queries`
- `fully_concurrent`

Legacy plugin callbacks shall be serialized per plugin instance.

Different plugins may run concurrently.

### 13.4 Legacy callback serialization

No two lifecycle callbacks shall execute concurrently against the same legacy plugin instance.

This applies to all legacy lifecycle callbacks.

### 13.5 Per-plugin concurrency limits

A modern plugin manifest may define:

- Maximum simultaneous suggestion requests.
- Maximum simultaneous action requests.
- Maximum background tasks.
- Maximum catalog-build tasks.

The supervisor shall enforce configured limits.

### 13.6 Timeouts

The supervisor shall support:

- Soft deadlines.
- Hard deadlines where appropriate.
- Query cancellation.
- Callback-duration logging.
- Automatic plugin restart.
- Automatic plugin throttling where compatible.
- User-visible warnings.

A plugin exceeding a query deadline shall not block results from other plugins.

### 13.7 Circuit breaking

CriKey shall temporarily suspend a plugin after repeated:

- Crashes.
- Protocol violations.
- Hard timeouts.
- Excessive memory use.
- Failed startups.

Suspension and recovery behavior shall be configurable and diagnosable.

### 13.8 Lazy activation

Modern plugins should be started lazily where possible.

`legacy-strict` plugins shall be loaded according to the compatibility lifecycle rather than inferred modern relevance.

---

## 14. Legacy Compatibility Layer

### 14.1 Compatibility objective

On Windows, CriKey shall run existing Keypirinha plugins unchanged where feasible.

Where unchanged execution is not feasible, CriKey should require only minimal source or packaging changes.

### 14.2 Compatibility modules

The Legacy Compatibility Layer shall provide implementations of the documented public modules:

- `keypirinha`
- `keypirinha_util`
- `keypirinha_net`
- `keypirinha_wintypes`, where applicable

### 14.3 Package formats

CriKey shall load:

- Loose Keypirinha package directories.
- `.keypirinha-package` archives.
- Package-local Python modules.
- Package resources.
- Keypirinha-style configuration files.

### 14.4 API behavior

The Legacy Compatibility Layer shall provide equivalents for:

- `Plugin`.
- Catalog item construction.
- Suggestion construction.
- Item categories.
- Argument hints.
- Hit hints.
- Events.
- Settings access.
- Package resources.
- Logging.
- Icons.
- Clipboard operations.
- URL opening.
- Process execution.
- Environment expansion.
- Filesystem helpers.
- Network helpers.

### 14.5 Legacy query semantics

In `legacy-strict` mode:

- Initial suggestion requests shall be broadcast to all loaded legacy plugins.
- After an item has been selected, subsequent argument suggestions shall be routed to the owning plugin as required by the legacy API.
- Host-side time debouncing shall be disabled.
- Legacy callbacks shall be serialized.
- Obsolete running work shall receive `should_terminate() = true`.
- Only the newest pending undispatched query need be retained.
- Stale results shall be rejected.
- Host-imposed minimum query lengths and prefix gating shall be disabled.
- Dynamic suggestion caching shall be disabled.

### 14.6 Legacy events

The Legacy Compatibility Layer shall reproduce documented legacy event semantics.

It may combine event flags already pending for immediate delivery.

It shall not add arbitrary time-based debounce windows to semantic legacy events.

Raw operating-system event noise may be coalesced before translation into one logical legacy event.

### 14.7 Activation and deactivation coalescing

The Legacy Compatibility Layer shall reproduce documented coalescing behavior for activation and deactivation events where a later activation may supersede a pending deactivation.

Legacy plugins shall not be promised strict alternation between activation and deactivation callbacks.

### 14.8 Catalog construction

Legacy catalog construction shall:

- Permit repeated `on_catalog()` calls.
- Preserve the distinction between one-time initialization and catalog rebuilding.
- Serialize `on_catalog()` with other callbacks for the same plugin instance.
- Permit `set_catalog()` and `merge_catalog()` behavior.
- Reject catalog updates from obsolete plugin instances.
- Avoid modern query hard deadlines.
- Support cooperative termination during reload or shutdown.

### 14.9 Legacy dynamic caching

CriKey shall not cache dynamic legacy suggestions across requests by default.

Caching may be enabled only through:

- Explicit per-plugin compatibility metadata.
- A user-enabled `legacy-optimized` override.
- A future compatibility database establishing safe behavior.

Persistent catalog items may still be indexed and cached after submission.

### 14.10 Compatibility matrix

Each documented legacy API shall be classified as:

- Fully supported.
- Supported with behavioral differences.
- Windows-only.
- Partially supported.
- Unsupported.
- Planned.

The compatibility matrix shall be version-controlled and tested.

### 14.11 Python runtime profiles

CriKey shall support multiple Python runtime profiles when required for compatibility.

A plugin manifest, compatibility record, or user override may select:

- A legacy compatibility runtime.
- The current bundled runtime.
- A managed external runtime.

Plugins requiring incompatible Python versions shall execute in separate processes.

The current implementation exposes explicit runtime-profile selection, but does
not yet map every plugin's declared Python requirement to a profile
automatically; see the M3 verification limit in the roadmap.

### 14.12 Compatibility limitations

CriKey is not required to support unchanged plugins that rely on:

- Undocumented Keypirinha internals.
- A specific private directory layout.
- Windows DLLs on non-Windows systems.
- Unsupported binary Python modules.
- Undefined thread-safety assumptions.
- Direct modification of private internal objects.
- Exact reproduction of undocumented ranking behavior.

CriKey shall produce a specific diagnostic when such behavior is detected.

### 14.13 Branding and attribution

Public documentation may state:

> CriKey supports existing Keypirinha plugins through its Legacy Compatibility Layer.

CriKey shall not:

- Use Keypirinha as part of the CriKey product name.
- Present the Legacy Compatibility Layer as an official Keypirinha component.
- Use Keypirinha logos or visual identity without permission.
- Claim sponsorship, endorsement, or official successor status.

A suitable attribution notice should be included in project documentation.

---

## 15. Modern Python Plugin Runtime

### 15.1 Imports

Modern Python plugins shall support standard Python imports.

A plugin may import:

- The Python standard library.
- Modules included in its package.
- Declared third-party dependencies.
- Compatible compiled Python extension modules.
- The CriKey Python SDK.

### 15.2 Dependency declaration

A Python plugin shall declare dependencies in its manifest or `pyproject.toml`.

Example:

```toml
[plugin]
id = "example.search"
runtime = "python"
entrypoint = "example_search.plugin:Plugin"

[python]
requires-python = ">=3.12"
dependencies = [
    "httpx>=0.28,<1",
    "pydantic>=2.9,<3"
]
```

### 15.3 Environment isolation

Dependencies shall not be installed into one unrestricted global environment.

The package manager shall create or reuse an environment based on:

- Python version.
- Operating system.
- CPU architecture.
- Locked dependencies.
- Package hashes.
- Native build options.

Plugins with compatible locked environments may share an environment.

### 15.4 Import path

The plugin import path shall include:

1. Plugin source.
2. Plugin-packaged modules.
3. Managed dependencies.
4. CriKey SDK.
5. Python standard library.

System-wide `site-packages` shall be excluded by default.

### 15.5 Binary extensions

A modern Python plugin may use binary Python extensions where compatible binaries are available.

Loading native Python extensions shall require the corresponding native-code permission.

### 15.6 Worker isolation

Modern Python plugins shall run outside the UI process by default.

A worker may host:

- One plugin.
- A trusted group of compatible plugins.
- A content-addressed dependency environment.

Plugins containing unstable native extensions should receive dedicated workers.

### 15.7 Python cancellation API

The modern Python SDK shall expose a cancellation object.

Illustrative behavior:

```python
def suggest(query, context):
    for item in expensive_search(query.text):
        if context.cancelled:
            return
        context.emit(item)
```

The host shall still reject stale results when a plugin fails to check cancellation.

### 15.8 Asynchronous plugins

The modern Python SDK should support synchronous and asynchronous callbacks.

Asynchronous callbacks shall execute on a host-managed event loop.

Plugins shall not create unbounded background tasks without registering them with the host.

---

## 16. Native Plugin Interface

### 16.1 Standard mechanism

The standard compiled-plugin mechanism shall be a supervised native executable communicating with CriKey over local IPC.

The native plugin shall not be loaded directly into the CriKey process.

### 16.2 Transport

The transport shall support:

- Windows named pipes.
- Unix-domain sockets.
- Standard input and output for development or fallback use.

The protocol shall be transport-independent.

### 16.3 Protocol format

The native v1 implementation uses a versioned Protocol Buffers (proto3) schema
and the hand-written codec selected by ADR-0010; it does not require generated
bindings or `protoc`.

The protocol shall support unknown fields and additive evolution.

The protocol shall define:

- Handshake.
- Protocol-version negotiation.
- Plugin metadata.
- Host capabilities.
- Plugin capabilities.
- Lifecycle requests.
- Catalog batches.
- Suggestion requests.
- Suggestion batches.
- Cancellation.
- Action execution.
- Configuration changes.
- Event delivery.
- Resource requests.
- Logging.
- Health checks.
- Structured errors.
- Shutdown.

### 16.4 Request identifiers

Every request shall have:

- A connection identifier.
- A request identifier.
- A query generation identifier where applicable.
- A deadline where applicable.
- A cancellation state.

Plugin responses shall reference the corresponding request identifier.

### 16.5 Batching

The native protocol shall support batches of catalog items and suggestions.

The interface shall avoid one IPC request per candidate or field.

Large catalog transfers shall support streaming and backpressure.

### 16.6 Native plugin process

CriKey shall be able to:

- Launch the plugin executable.
- Pass a connection endpoint.
- Pass a session token.
- Restrict the plugin environment.
- Monitor process state.
- Terminate an unresponsive plugin.
- Restart a crashed plugin.
- Record exit information.

### 16.7 Rust SDK

The official Rust SDK shall provide:

- Protocol bindings.
- A high-level `Plugin` trait.
- Catalog and result builders.
- Action builders.
- Configuration access.
- Logging.
- Cancellation tokens.
- Event handlers.
- Test harnesses.
- Packaging tools.
- Benchmark tools.

Illustrative API:

```rust
pub trait Plugin {
    fn start(&mut self, context: &PluginContext) -> Result<()>;

    fn build_catalog(
        &mut self,
        context: &PluginContext,
        sink: &mut dyn CatalogSink,
    ) -> Result<()>;

    fn suggest(
        &mut self,
        query: Query,
        context: &PluginContext,
        sink: &mut dyn SuggestionSink,
    ) -> Result<()>;

    fn execute(
        &mut self,
        request: ExecuteRequest,
        context: &PluginContext,
    ) -> Result<()>;

    fn stop(&mut self, context: &PluginContext) -> Result<()>;
}
```

### 16.8 Native performance

Native plugins shall execute unrestricted native code within their own process.

The protocol shall permit plugins to:

- Maintain persistent in-memory indexes.
- Use multiple threads.
- Use SIMD.
- Use native system libraries.
- Use memory-mapped files.
- Perform background catalog construction.
- Return incremental result batches.

Shared-memory transport may be added if profiling demonstrates that ordinary local IPC is insufficient.

### 16.9 In-process native interface

CriKey may later expose a versioned C ABI for trusted plugins.

Such plugins shall:

- Use C-compatible types.
- Use host-provided allocation or explicit ownership functions.
- Prevent exceptions or panics from crossing the ABI.
- Negotiate an ABI version.
- Be separately compiled for every platform and architecture.
- Be clearly marked as capable of crashing CriKey.

The in-process interface shall not be required for third-party plugin compatibility.

---

## 17. Optional WebAssembly Runtime

### 17.1 Status

WebAssembly shall be an optional future runtime, not the primary native plugin mechanism.

### 17.2 Intended use

WebAssembly plugins may be used where the plugin benefits from:

- Cross-platform binary distribution.
- Sandboxing.
- Explicit host capabilities.
- CPU or memory limits.
- Reproducible execution.
- Limited access to the operating system.

### 17.3 Limitations

WebAssembly plugins shall not be assumed to support arbitrary native operating-system APIs.

Plugins requiring unrestricted native libraries or desktop integration should use the native subprocess interface.

---

## 18. Platform Services

### 18.1 Required interfaces

The core shall define interfaces for:

- Application discovery.
- Filesystem access.
- File search.
- Standard directories.
- Clipboard access.
- Global hotkeys.
- Process launching.
- URI opening.
- Window enumeration.
- Window activation.
- Notifications.
- Icons.
- File watching.
- Secret storage.
- Shell integration.
- Environment access.

### 18.2 Capability reporting

A platform backend shall report whether each optional capability is:

- Available.
- Unavailable.
- Permission-gated.
- Partially supported.
- Unsupported by the current desktop environment.

Plugins shall not assume that every platform supports window control or global shortcuts identically.

### 18.3 Filesystem paths

The internal core shall preserve native platform paths without requiring valid UTF-8.

Display formatting shall be separate from path identity.

IPC and plugin SDKs shall define a lossless path representation.

### 18.4 Windows backend

The Windows backend shall support:

- Start Menu shortcuts.
- Packaged applications.
- Executable discovery.
- `.lnk` parsing.
- AppUserModelIDs.
- Shell execution.
- Known folders.
- File-type icons.
- Window enumeration.
- Window activation.
- Clipboard access.
- Registry access subject to permissions.
- Named pipes.
- Native notifications.
- Global hotkeys.

### 18.5 macOS backend

The macOS backend should support:

- Application-bundle discovery.
- Launch Services.
- Application opening.
- Document opening.
- Spotlight metadata where useful.
- Accessibility-based window integration.
- Keychain secrets.
- Global hotkeys.
- Native notifications.

### 18.6 Linux backend

The Linux backend should support:

- XDG desktop entries.
- XDG base directories.
- DBus.
- Freedesktop notifications.
- Secret Service.
- Desktop portals.
- X11 integrations.
- Wayland integrations where compositor protocols permit them.

Window-control support shall be treated as optional on Linux.

### 18.7 Filesystem event debouncing

CriKey's own filesystem watchers shall debounce bursts of related low-level events.

The watcher layer shall support:

- Path-based coalescing.
- Event-type coalescing.
- Rename correlation where available.
- Configurable debounce windows.
- Maximum-wait flushes.
- Overflow detection.
- Full-rescan fallback.

For modern plugins, coalesced logical events may be delivered according to the modern event contract.

For legacy plugins, low-level event noise may be coalesced before translation, but semantic legacy events shall not receive arbitrary additional debounce delays.

### 18.8 Configuration event debouncing

Rapid sequences of modern configuration changes shall be coalesced before triggering:

- Plugin reloads.
- Catalog rebuilds.
- Index invalidation.
- Worker restarts.

Explicit save or apply actions may bypass the debounce delay.

Legacy event behavior shall remain subject to the Legacy Compatibility Layer's compatibility contract.

---

## 19. Plugin Manifest

### 19.1 Manifest format

New plugins shall include a `crikey.toml` manifest.

Example:

```toml
manifest-version = 1

[plugin]
id = "dev.example.repositories"
name = "Repository Search"
version = "1.0.0"
runtime = "native"
entrypoint.windows-x86_64 = "bin/repository-search.exe"
entrypoint.linux-x86_64 = "bin/repository-search"
entrypoint.macos-aarch64 = "bin/repository-search"
api = ">=1.0,<2"

[platform]
os = ["windows", "macos", "linux"]
arch = ["x86_64", "aarch64"]

[activation]
minimum-query-length = 2
prefixes = ["repo", "git"]

[query]
debounce-ms = 50
maximum-wait-ms = 200
leading-edge = true
trailing-edge = true
max-concurrent-requests = 1

[permissions]
filesystem = [
    { scope = "user-selected", access = "read" }
]
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

### 19.2 Runtime values

Supported runtime values shall include:

- `legacy-python`
- `python`
- `native`
- `wasm`, when implemented
- `builtin`

### 19.3 Platform packages

A plugin package may include binaries for multiple platform and architecture combinations.

The package manager shall select the matching entrypoint.

A package lacking a compatible binary shall be reported as unavailable rather than loaded.

### 19.4 Query policy fields

A modern plugin manifest may define:

- Debounce interval.
- Maximum debounce wait.
- Leading-edge execution.
- Trailing-edge execution.
- Minimum query length.
- Maximum concurrent requests.
- Maximum results.
- Query prefixes.
- Keywords.
- Empty-query support.
- Network-backed status.
- Preferred cancellation behavior.

Legacy packages shall not be assumed to provide these fields.

---

## 20. Permissions

### 20.1 Permission types

Plugins shall be able to request:

- Filesystem read.
- Filesystem write.
- Network client.
- Network listener.
- Clipboard read.
- Clipboard write.
- Process execution.
- Window enumeration.
- Window control.
- Notifications.
- Secret storage.
- Environment variables.
- Native library loading.
- Persistent background execution.

### 20.2 Enforcement (target behavior; not implemented in the current release)

The following is the intended design, not a description of the current
permission implementation:

- Host-mediated APIs shall enforce permissions directly.
- Native subprocess plugins shall be restricted using available operating-system
  sandboxing where practical.
- Modern Python subprocesses shall use equivalent sandboxing where practical.
- Legacy plugins shall be treated as trusted legacy code unless explicitly
  restricted.

At present, manifest permission fields are recorded as plugin requests, but
they are not enforced at the host-mediated API boundaries. A third-party
plugin must therefore not be treated as sandboxed because it declares
permissions in its manifest. The UI shall not claim that a native plugin is
fully sandboxed where the operating system does not provide effective
enforcement.

---

## 21. Configuration

### 21.1 Configuration format

New CriKey and modern plugin configuration shall use TOML.

Legacy Keypirinha configuration syntax shall remain supported for legacy plugins.

### 21.2 Configuration layers

Configuration precedence shall be:

1. Built-in defaults.
2. Administrator policy.
3. User-global settings.
4. Profile settings.
5. Plugin defaults.
6. User plugin settings.
7. Session overrides.

### 21.3 Plugin schemas

A modern plugin may provide a configuration schema containing:

- Field name.
- Data type.
- Default value.
- Validation rules.
- Description.
- Secret flag.
- Restart requirement.
- Platform restrictions.

### 21.4 Live configuration updates

Modern plugins may subscribe to configuration changes.

The host shall coalesce rapid changes and send the latest complete configuration state rather than every intermediate edit unless explicitly requested.

Legacy configuration notifications shall follow the compatibility contract.

---

## 22. Caching and Invalidation

### 22.1 Cache types

CriKey shall support caching of:

- Parsed manifests.
- Plugin package metadata.
- Catalog items.
- Search-normalized fields.
- Icons.
- Application metadata.
- Filesystem metadata.
- Python dependency environments.
- Native package validation results.
- Modern plugin query results where safe.
- Ranking history.

### 22.2 Modern query-result caching

A modern plugin may declare whether its suggestion results are:

- Not cacheable.
- Cacheable for an exact query.
- Cacheable by normalized query.
- Cacheable for a time-to-live.
- Cacheable until an event or configuration change.

### 22.3 Legacy query-result caching

Dynamic legacy suggestions shall not be cached across requests by default.

### 22.4 Cache invalidation

Caches shall be invalidated by:

- Plugin upgrades.
- Manifest changes.
- Configuration changes.
- Filesystem events.
- Application-installation changes.
- Platform-backend changes.
- Explicit plugin invalidation.
- Expiration.
- Schema-version changes.

### 22.5 Refresh suppression

Repeated modern invalidation events shall be debounced.

Multiple invalidations affecting the same catalog shall normally trigger one rebuild.

Legacy rebuild behavior shall remain compatible with the documented legacy lifecycle.

---

## 23. Package Management

### 23.1 Installation sources

CriKey shall support installation from:

- Local directories.
- Local archives.
- URLs.
- A future plugin repository.
- Existing Keypirinha package files.

### 23.2 Python installation

The package manager shall:

- Resolve dependencies.
- Produce or consume a lockfile.
- Verify hashes.
- Prefer binary wheels.
- Request permission before building arbitrary native source packages.
- Cache downloaded packages.
- Roll back failed installations.

### 23.3 Native installation

The package manager shall:

- Verify platform compatibility.
- Verify architecture compatibility.
- Verify package hashes.
- Mark unsigned native binaries clearly.
- Preserve previous versions for rollback.
- Stop running plugin processes before replacement.

### 23.4 Package updates

Plugin updates shall be atomic.

A failed update shall leave the previous functional version available.

---

## 24. Reliability

### 24.1 Fault isolation

A plugin crash shall not terminate CriKey.

A Python interpreter crash shall not terminate CriKey.

A native plugin panic or segmentation fault shall not terminate CriKey.

### 24.2 Startup recovery

CriKey shall record which plugins were active during an abnormal shutdown.

On repeated startup failure, CriKey shall enter safe mode with third-party plugins disabled.

### 24.3 Plugin health

CriKey shall track:

- Startup failures.
- Crash count.
- Timeout count.
- Cancellation compliance.
- Average suggestion latency.
- Peak suggestion latency.
- Debounce delay for modern plugins.
- Obsolete-work replacement counts for legacy plugins.
- Queue depth.
- Dropped obsolete requests.
- Rejected stale results.
- Truncated result count.
- Catalog-build duration.
- Catalog size.
- Worker memory use.
- Last successful execution.

### 24.4 Resource limits

The supervisor shall support per-plugin limits for:

- Memory.
- CPU time.
- Process count.
- Open file handles where enforceable.
- IPC message size.
- Queue capacity.
- Result count.
- Catalog size.
- Background task count.

Legacy limits shall be configured separately where modern defaults would alter ordinary compatibility.

---

## 25. Performance Requirements

### 25.1 Target measurements

Performance targets shall be measured on documented reference systems.

Initial targets shall include:

- Warm CriKey activation below 30 ms at the 95th percentile.
- Cached local results available below 16 ms at the 95th percentile.
- Visible query-text updates within the next rendered frame.
- No UI blocking caused by plugin execution.
- Negligible idle CPU use.
- Main-process idle memory below 100 MiB where practical.
- Support for at least 500,000 indexed catalog items.
- Incremental results before every plugin completes.
- No unbounded growth in pending requests during sustained typing.

### 25.2 Modern plugin latency

Default soft query deadlines should be:

- Built-in catalog search: 10 ms.
- Native plugin: 50 ms.
- Modern Python plugin: 100 ms.

The default modern hard query deadline should be 500 ms.

These values shall be configurable.

### 25.3 Legacy plugin latency

Legacy plugins shall use:

- Latency measurement.
- Soft warnings.
- Cooperative termination when obsolete.
- Stale-result rejection.
- A longer hung-worker watchdog.
- Forced restart only as recovery.

CriKey shall not apply the modern 500 ms hard-kill policy to legacy callbacks.

### 25.4 Modern debounce defaults

Recommended modern debounce intervals shall be:

- Core local catalog: 0 ms.
- Cached modern plugin results: 0 ms.
- Native local plugin: 30 to 50 ms.
- Modern Python local plugin: 50 to 75 ms.
- Filesystem-heavy modern plugin: 75 to 150 ms.
- Network-backed modern plugin: 150 to 250 ms.

`legacy-strict` plugins shall use 0 ms time debounce.

### 25.5 UI update frequency

The result list shall not be rerendered for every individual plugin item.

Result updates shall be batched according to:

- The display refresh rate.
- A minimum batch size.
- A short maximum presentation delay.
- Current query generation.
- Selection stability requirements.

### 25.6 Startup

Startup shall proceed in stages:

1. Initialize the CriKey window and hotkey.
2. Load the persisted core catalog.
3. Permit user queries.
4. Start required plugin workers.
5. Load legacy plugins according to compatibility requirements.
6. Refresh stale catalogs in the background.
7. Start lazy modern plugins only when relevant.

---

## 26. Diagnostics

### 26.1 Diagnostic interface

CriKey shall expose:

- Per-plugin logs.
- Structured errors.
- Plugin process state.
- Runtime version.
- Protocol version.
- Permissions.
- Dependency environment.
- Callback timing.
- Modern debounce timing.
- Legacy obsolete-work replacement behavior.
- Queue depth.
- Cancellation count.
- Stale-result count.
- Dropped-request count.
- Crash history.
- Timeout history.
- Catalog statistics.
- Compatibility warnings.

### 26.2 Legacy compatibility diagnostics

For a legacy plugin, CriKey should report:

- Missing API calls.
- Unsupported imports.
- Python-version incompatibilities.
- Windows-only dependencies.
- Native extension requirements.
- Undocumented API access where detectable.
- Scheduling profile.
- Long callbacks that do not check `should_terminate()`.
- Suggested source changes.

### 26.3 Developer mode

Developer mode shall support:

- Loose package loading.
- Automatic plugin restart.
- Verbose protocol logs.
- Debugger attachment.
- Test queries.
- Simulated rapid typing.
- Cancellation testing.
- Backpressure testing.
- Legacy scheduling conformance tests.
- Catalog inspection.
- Performance traces.
- Manifest validation.
- Protocol conformance tests.

### 26.4 Query trace

A developer shall be able to inspect a query trace containing:

- Keystroke timestamps.
- Query generations.
- Modern debounce decisions.
- Legacy dispatch and replacement decisions.
- Plugin dispatch timestamps.
- Cancellation timestamps.
- First-result latency.
- Final-result latency.
- Result-batch sizes.
- Rejected stale responses.
- Ranking and presentation updates.

---

## 27. Testing Requirements

### 27.1 Unit tests

The project shall include unit tests for:

- Query normalization.
- Modern debounce scheduling.
- Leading and trailing execution.
- Maximum-wait behavior.
- Legacy obsolete-work replacement.
- Query coalescing.
- Cancellation.
- `should_terminate()` behavior.
- Stale-result rejection.
- Ranking.
- Deduplication.
- Queue bounds.
- Backpressure.
- Cache invalidation.

### 27.2 Integration tests

Integration tests shall cover:

- Rapid typing while plugins are slow.
- Query deletion and replacement.
- Repeated open and close cycles.
- Plugin crashes during suggestion generation.
- Plugins ignoring cancellation.
- Large streamed modern result sets.
- Large complete legacy suggestion publications.
- Configuration changes during active queries.
- Filesystem event bursts.
- Network plugin timeouts.
- Worker restart during catalog construction.
- Legacy callback serialization.
- Initial legacy query broadcast.
- Selected-item routing to the owning legacy plugin.
- Activation and deactivation event coalescing.

### 27.3 Stress tests

Stress tests shall include:

- At least 500,000 catalog items.
- Hundreds of installed plugins.
- Multiple simultaneously active plugins.
- Sustained typing and deletion.
- Result streams larger than display limits.
- Repeated filesystem event storms.
- Deliberately malformed IPC messages.
- Slow consumers and fast producers.
- Legacy plugins that fail to check `should_terminate()`.

### 27.4 Compatibility tests

A maintained corpus of existing Keypirinha plugins shall be tested against the Legacy Compatibility Layer.

Each plugin shall be classified as:

- Works unchanged.
- Works with configuration changes.
- Works with minimal source changes.
- Windows-only but compatible.
- Blocked by missing APIs.
- Blocked by Python-version requirements.
- Blocked by undocumented behavior.
- Works only under `legacy-optimized`.
- Requires `legacy-strict`.

---

## 28. Command-Line Tools

The project shall provide a command-line interface supporting commands
equivalent to:

This is the target command contract, not a claim that every command is
implemented in the current milestone. An unavailable command must report that
state rather than pretending to complete the operation.

```text
crikey run
crikey plugin list
crikey plugin install
crikey plugin remove
crikey plugin enable
crikey plugin disable
crikey plugin doctor
crikey plugin scheduling-profile

crikey dev run
crikey dev test
crikey dev benchmark
crikey dev trace-query
crikey dev simulate-typing
crikey dev inspect-protocol
crikey dev test-legacy-compat

crikey package build
crikey package verify
crikey package inspect
crikey package migrate-keypirinha
```

---

## 29. Repository Structure

The implementation should use a structure similar to:

```text
crikey/
├── crates/
│   ├── crikey-app/
│   ├── crikey-core/
│   ├── crikey-ui/
│   ├── crikey-input-scheduler/
│   ├── crikey-query/
│   ├── crikey-ranking/
│   ├── crikey-catalog/
│   ├── crikey-result-aggregator/
│   ├── crikey-plugin-model/
│   ├── crikey-plugin-supervisor/
│   ├── crikey-python-host/
│   ├── crikey-legacy-compat/
│   ├── crikey-native-protocol/
│   ├── crikey-native-host/
│   ├── crikey-platform/
│   ├── crikey-platform-windows/
│   ├── crikey-platform-macos/
│   ├── crikey-platform-linux/
│   ├── crikey-package-manager/
│   └── crikey-cli/
├── sdk/
│   ├── rust/
│   ├── python/
│   └── protocol/
├── compatibility/
│   ├── api-matrix/
│   ├── test-plugins/
│   └── real-plugin-corpus/
├── plugins/
│   └── builtin/
├── benchmarks/
├── docs/
└── packaging/
```

---

## 30. Delivery Phases

### Phase 1: Core CriKey launcher

The first phase shall deliver:

- Rust core.
- Basic launcher UI.
- Global hotkey.
- Application catalog.
- Native matching and ranking.
- Persistent catalog cache.
- Query-generation tracking.
- Core result aggregation.
- Immediate local search.
- Windows platform backend.
- Plugin supervisor skeleton.

### Phase 2: Query scheduling and resilience

The second phase shall deliver:

- Modern per-plugin debouncing.
- Leading and trailing execution.
- Maximum debounce waits.
- Legacy obsolete-work replacement.
- Query coalescing.
- Cancellation.
- Stale-result rejection.
- Bounded request and result queues.
- Backpressure.
- Query tracing.
- Rapid-input stress tests.

### Phase 3: Legacy Compatibility Layer

The third phase shall deliver:

- Legacy package loading.
- CPython worker.
- `keypirinha` module compatibility.
- `keypirinha_util` compatibility.
- Legacy configuration loading.
- `legacy-strict` scheduling.
- `should_terminate()` integration.
- Legacy event semantics.
- Real-plugin compatibility tests.
- Compatibility diagnostics.

### Phase 4: Modern Python plugins

The fourth phase shall deliver:

- Modern Python SDK.
- Normal imports.
- Dependency manifests.
- Managed environments.
- Lockfiles.
- Python worker isolation.
- Python cancellation API.
- Python development tools.

### Phase 5: Native plugins

The fifth phase shall deliver:

- Versioned IPC schema.
- Native process supervisor.
- Rust SDK.
- Streaming catalogs.
- Query cancellation.
- Native plugin packaging.
- Native plugin test harness.

### Phase 6: Additional platforms

The sixth phase shall deliver:

- macOS backend.
- Linux backend.
- Platform capability reporting.
- Cross-platform packaging.
- Portable built-in plugins.

### Phase 7: Optional runtimes and ecosystem

Later phases may deliver:

- WebAssembly runtime.
- Signed packages.
- Public plugin index.
- Restricted C ABI.
- Advanced sandboxing.
- Shared-memory native transport.

---

## 31. Acceptance Criteria

The first public compatibility release shall satisfy the following:

1. CriKey remains responsive while plugins execute.
2. Query text is rendered without waiting for debounce intervals.
3. Local catalog results appear immediately.
4. Rapid typing does not create an unbounded request queue.
5. Obsolete pending queries are coalesced or replaced according to the applicable scheduling profile.
6. Obsolete in-flight queries are cancelled or logically invalidated.
7. Results from stale query generations are never displayed.
8. Slow plugins do not delay fast plugins.
9. A failed plugin cannot terminate CriKey.
10. A Python interpreter crash cannot terminate CriKey.
11. Existing Keypirinha packages can be discovered and loaded on Windows.
12. A documented compatibility matrix exists.
13. A representative set of legacy plugins works unchanged or with documented minimal changes.
14. `legacy-strict` plugins are not time-debounced.
15. Initial legacy suggestion requests are broadcast according to the documented legacy API.
16. Legacy callbacks are serialized per plugin instance.
17. Legacy cancellation is exposed through `should_terminate()`.
18. Dynamic legacy suggestions are not cached by default.
19. Modern Python plugins can import declared dependencies.
20. Conflicting Python dependencies can coexist.
21. A Rust plugin can connect through the native protocol.
22. Native plugin results can be returned incrementally.
23. Native plugin crashes are detected and recoverable.
24. Result and request queues are bounded.
25. Backpressure prevents uncontrolled result production.
26. Platform-specific services are accessed through explicit interfaces.
27. Catalog search remains responsive with 500,000 items.
28. Raw filesystem notification bursts are coalesced without changing semantic legacy event behavior.
29. Plugin failures produce actionable diagnostics.
30. The main process does not load arbitrary third-party native libraries.
31. Windows-specific legacy plugins are not presented as cross-platform unless they actually support other platforms.
32. CriKey branding and documentation do not imply affiliation with or endorsement by Keypirinha.
