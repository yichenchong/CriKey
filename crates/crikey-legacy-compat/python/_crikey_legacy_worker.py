"""Child-side entry point of the CriKey legacy worker (spec 4.2, 7.1, 24.1).

Spawned as ``<interpreter> -S <shim_dir>/_crikey_legacy_worker.py`` with
``PYTHONPATH=<shim_dir>``, this process is the only place legacy Python code
runs. It speaks newline-delimited JSON with the Rust host — one object per line
on stdin, one object per line on stdout — and its whole job is to keep a
misbehaving plugin from being able to hurt anything outside this process.

Three of the things it does are *contracts*, not conveniences, and each exists
because the obvious implementation is broken:

1. **stdout is stolen before any plugin code runs.** stdout is the protocol
   channel; one stray ``print`` from a plugin desynchronises the stream for
   good, and a desynchronised stream never resynchronises. The real stdout
   handle is captured first, before the plugin module is even located, and
   ``sys.stdout`` is rebound to a per-request capture whose contents are
   mirrored to the real stderr and reported back in the response frame's
   ``log`` field.

2. **stdin is read by its own thread.** Cooperative cancellation has to arrive
   *while a callback is still running* — a plugin spinning in ``on_suggest``
   polls ``should_terminate()`` and must see the flag go true — so the
   ``set_terminate`` control frame can never be queued behind the in-flight
   call. The reader thread applies it to a :class:`threading.Event` the instant
   it is parsed.

3. **Every plugin exception becomes a frame, not a death.** A plugin bug is
   reported as a typed ``failed`` response carrying the exception type, message
   and traceback, and the worker stays usable for the next call (spec 24.1,
   acceptance 31.9). Only a real process death — ``os._exit``, a segfault, the
   host's hard bound — ends this process early, and the host reports those as a
   crash because that is what they are.

Time: this module reads no clock. Deadlines belong to the host, which owns the
child's lifetime; nothing here polls with a timeout of its own.

Bounds: the pending-request queue, the per-request log, the settings file, a
resource read and an outgoing frame all have explicit caps, documented at the
constant that names each one.
"""

import importlib.util
import json
import os
import queue
import sys
import tempfile
import threading
import traceback

# --------------------------------------------------------------------------
# stdout hygiene: FIRST, before anything else can write a byte
#
# `keypirinha` is imported here and its stdout guard installed before the
# plugin module is located, let alone executed. Anything that printed before
# this point would land on the protocol channel and corrupt the handshake.
# --------------------------------------------------------------------------

import keypirinha

#: The process's real stderr, kept before `sys.stderr` is rebound. Every
#: captured line is mirrored here as it is written, so a developer watching the
#: child sees plugin output live and the host's stderr ring stays useful for
#: the crash tail.
_REAL_STDERR = sys.stderr

# --------------------------------------------------------------------------
# Wire protocol constants (agreed with `src/worker.rs`; version 1)
# --------------------------------------------------------------------------

#: Wire protocol version announced in the handshake.
_PROTOCOL_VERSION = 1

ENV_PACKAGE_ROOT = "CRIKEY_LEGACY_PACKAGE_ROOT"
ENV_PLUGIN_ID = "CRIKEY_LEGACY_PLUGIN_ID"
ENV_PACKAGE_ID = "CRIKEY_LEGACY_PACKAGE_ID"
ENV_MAIN_MODULE = "CRIKEY_LEGACY_MAIN_MODULE"
ENV_MAIN_MODULE_PATH = "CRIKEY_LEGACY_MAIN_MODULE_PATH"
ENV_CACHE_DIR = "CRIKEY_LEGACY_CACHE_DIR"

#: Control frames. Neither is a plugin callback: `set_terminate` is answered by
#: no frame at all, and `shutdown` by process exit.
_CALLBACK_SET_TERMINATE = "set_terminate"
_CALLBACK_SHUTDOWN = "shutdown"

#: Outcome for a catalog or suggestion callback that returned without
#: publishing anything. Distinct from an empty publication on purpose: a plugin
#: that abandons obsolete work publishes *nothing*, and reporting an empty list
#: instead would clobber the live results the host still has (spec 8.5, 14.5).
_OUTCOME_ABANDONED = "abandoned"

#: Most requests the reader thread will hold while a callback runs.
#:
#: The host keeps at most one call in flight per instance (acceptance 31.16),
#: so this is never reached in correct operation. On overflow the reader thread
#: blocks on `put`, which applies backpressure through the OS pipe rather than
#: growing without bound — and a full queue therefore means the host broke its
#: own serialization contract, which is worth stalling over rather than hiding.
_MAX_PENDING_REQUESTS = 64

#: Per-request log bounds. A plugin printing in a loop must not be able to make
#: the response frame unbounded; overflow is reported in the log itself, never
#: dropped silently.
_MAX_LOG_LINES = 2048
_MAX_LOG_LINE_CHARS = 8192

#: Largest frame this process will emit. The host refuses a longer line as a
#: protocol error, so an oversized publication is downgraded to an attributable
#: plugin failure here, where the plugin and callback are still known.
_MAX_FRAME_BYTES = 8 * 1024 * 1024

#: Bounds on the child-side host capabilities. A settings file or a resource is
#: plugin-supplied data, so neither may be read without a ceiling.
_MAX_SETTINGS_BYTES = 1 << 20
_MAX_SETTINGS_ENTRIES = 4096
_MAX_RESOURCE_BYTES = 32 << 20

#: Sentinel the reader thread queues on end of stdin.
_EOF = object()


# --------------------------------------------------------------------------
# Per-request log capture
# --------------------------------------------------------------------------


class _LogCapture:
    """Text stream that records what a plugin writes and mirrors it onward.

    Installed as both ``sys.stdout`` and ``sys.stderr`` for the whole life of
    the worker, so ``print``, ``sys.stdout.write`` and the documented
    ``Plugin.info``/``warn``/``err``/``dbg`` helpers all land here.

    Why the capture and not the child's real stderr is the authoritative
    record: the host reads the response frame off stdout and drains stderr on a
    separate thread, so attributing a stderr line to the call that just
    answered is a race with no non-flaky resolution. Carrying the log *inside*
    the response makes the attribution exact by construction.
    """

    def __init__(self, mirror):
        self._mirror = mirror
        self._lines = []
        self._partial = []
        self._partial_chars = 0
        self._dropped = 0

    # -- stream protocol ---------------------------------------------------

    def write(self, text):
        if not isinstance(text, str):
            text = str(text)
        try:
            self._mirror.write(text)
        except (OSError, ValueError):
            # A closed or broken stderr must not turn plugin chatter into a
            # plugin failure: the capture is the record that matters.
            pass
        start = 0
        while True:
            newline = text.find("\n", start)
            if newline == -1:
                self._extend(text[start:])
                break
            self._extend(text[start:newline])
            self._commit()
            start = newline + 1
        return len(text)

    def flush(self):
        try:
            self._mirror.flush()
        except (OSError, ValueError):
            pass

    def writable(self):
        return True

    def readable(self):
        return False

    def seekable(self):
        return False

    def isatty(self):
        return False

    @property
    def encoding(self):
        return getattr(self._mirror, "encoding", "utf-8")

    @property
    def errors(self):
        return getattr(self._mirror, "errors", "replace")

    # -- capture -----------------------------------------------------------

    def _extend(self, fragment):
        if not fragment:
            return
        room = _MAX_LOG_LINE_CHARS - self._partial_chars
        if room <= 0:
            return
        clipped = fragment[:room]
        self._partial.append(clipped)
        self._partial_chars += len(clipped)

    def _commit(self):
        line = "".join(self._partial)
        self._partial = []
        self._partial_chars = 0
        if len(self._lines) < _MAX_LOG_LINES:
            self._lines.append(line)
        else:
            self._dropped += 1

    def reset(self):
        """Starts a fresh per-request record, discarding anything pending."""
        self._lines = []
        self._partial = []
        self._partial_chars = 0
        self._dropped = 0

    def take(self):
        """The lines written since :meth:`reset`, and starts a new record.

        A trailing fragment with no newline is committed as its own line: a
        plugin's ``sys.stdout.write("no newline")`` is output it produced and
        wants to see, and holding it back until some later request happened to
        write a newline would attribute it to the wrong callback.
        """
        if self._partial:
            self._commit()
        lines = self._lines
        if self._dropped:
            lines.append(
                "[warn][crikey] {} further log line(s) dropped at the {} line "
                "per-request cap".format(self._dropped, _MAX_LOG_LINES)
            )
        self._lines = []
        self._dropped = 0
        return lines


#: The protocol channel: the process's real stdout, taken away from plugin code
#: before the plugin module is located. Everything below writes frames here and
#: nothing else ever writes here at all.
_CAPTURE = _LogCapture(_REAL_STDERR)
_PROTOCOL = keypirinha._install_stdout_guard(_CAPTURE)
sys.stderr = _CAPTURE


# --------------------------------------------------------------------------
# Item and action translation
#
# Enum fields cross the boundary as strings, never as the documented integers.
# The integers are a Keypirinha ABI and the Rust side has its own category
# type; a string wire means a mismatch is a named decode failure instead of a
# silent renumbering that lands items in the wrong section.
# --------------------------------------------------------------------------

_CATEGORY = keypirinha.ItemCategory
_ARGS_HINT = keypirinha.ItemArgsHint
_HIT_HINT = keypirinha.ItemHitHint

_CATEGORY_TO_WIRE = {
    int(_CATEGORY.KEYWORD): "keyword",
    int(_CATEGORY.CMDLINE): "command",
    int(_CATEGORY.FILE): "file",
    int(_CATEGORY.URL): "url",
    int(_CATEGORY.EXPRESSION): "expression",
    int(_CATEGORY.REFERENCE): "reference",
    int(_CATEGORY.ERROR): "error",
}

_ARGS_HINT_TO_WIRE = {
    int(_ARGS_HINT.FORBIDDEN): "forbidden",
    int(_ARGS_HINT.ACCEPTED): "accepted",
    int(_ARGS_HINT.REQUIRED): "required",
}

_HIT_HINT_TO_WIRE = {
    int(_HIT_HINT.NOARGS): "noargs",
    int(_HIT_HINT.KEEPALL): "keepall",
    int(_HIT_HINT.IGNORE): "ignore",
}

#: Inbound category spellings. The host's own category set is wider than the
#: legacy one, so several of its names collapse onto the nearest documented
#: constant — an item handed to `on_execute` must carry *some* legal category
#: or `CatalogItem` would refuse to be built at all.
_WIRE_TO_CATEGORY = {
    "keyword": int(_CATEGORY.KEYWORD),
    "command": int(_CATEGORY.CMDLINE),
    "cmdline": int(_CATEGORY.CMDLINE),
    "file": int(_CATEGORY.FILE),
    "application": int(_CATEGORY.FILE),
    "directory": int(_CATEGORY.FILE),
    "url": int(_CATEGORY.URL),
    "expression": int(_CATEGORY.EXPRESSION),
    "reference": int(_CATEGORY.REFERENCE),
    "contact": int(_CATEGORY.REFERENCE),
    "clipboard-item": int(_CATEGORY.REFERENCE),
    "error": int(_CATEGORY.ERROR),
}

_WIRE_TO_ARGS_HINT = {
    "forbidden": int(_ARGS_HINT.FORBIDDEN),
    "accepted": int(_ARGS_HINT.ACCEPTED),
    "optional": int(_ARGS_HINT.ACCEPTED),
    "required": int(_ARGS_HINT.REQUIRED),
}

_WIRE_TO_HIT_HINT = {
    "noargs": int(_HIT_HINT.NOARGS),
    "no_args": int(_HIT_HINT.NOARGS),
    "keepall": int(_HIT_HINT.KEEPALL),
    "keep_all": int(_HIT_HINT.KEEPALL),
    "recorded": int(_HIT_HINT.KEEPALL),
    "ignore": int(_HIT_HINT.IGNORE),
    "ignored": int(_HIT_HINT.IGNORE),
}


def _category_to_wire(value):
    """The wire spelling of a category, preserving plugin-defined ones.

    A category at or above ``ItemCategory.USER_BASE`` is the documented
    extension point, so its number is carried in the name rather than folded
    into a built-in category the plugin never asked for (spec 10.3).
    """
    known = _CATEGORY_TO_WIRE.get(value)
    return known if known is not None else "legacy-user-{}".format(value)


def _item_to_wire(item):
    """One :class:`keypirinha.CatalogItem` as a JSON-ready dict.

    ``icon_handle`` is deliberately absent: it is an opaque in-process object
    and a copy of it would not name the same icon. ``plugin_id`` and
    ``stable_id`` are absent because identity is the host's to assign — a
    plugin able to name another plugin's id could inject items into its
    catalog (spec 10.2).
    """
    return {
        "category": _category_to_wire(item.category()),
        "label": item.label(),
        "short_desc": item.short_desc(),
        "target": item.target(),
        "args_hint": _ARGS_HINT_TO_WIRE.get(item.args_hint(), "accepted"),
        "hit_hint": _HIT_HINT_TO_WIRE.get(item.hit_hint(), "keepall"),
        "loop_on_suggest": item.loop_on_suggest(),
        "data_bag": item.data_bag(),
    }


def _item_from_wire(payload):
    """A :class:`keypirinha.CatalogItem` from a host item frame.

    Tolerant on purpose: this item is the row the *user* selected, and refusing
    to build it would turn a vocabulary mismatch into an unexecutable item
    rather than into a diagnostic nobody can act on.
    """
    category = payload.get("category")
    hint = payload.get("args_hint")
    hit = payload.get("hit_hint")
    return keypirinha.CatalogItem(
        category=_WIRE_TO_CATEGORY.get(
            category.lower() if isinstance(category, str) else "",
            int(_CATEGORY.USER_BASE),
        ),
        label=payload.get("label", ""),
        short_desc=payload.get("short_desc", ""),
        target=payload.get("target", ""),
        args_hint=_WIRE_TO_ARGS_HINT.get(
            hint.lower() if isinstance(hint, str) else "", int(_ARGS_HINT.ACCEPTED)
        ),
        hit_hint=_WIRE_TO_HIT_HINT.get(
            hit.lower() if isinstance(hit, str) else "", int(_HIT_HINT.KEEPALL)
        ),
        loop_on_suggest=bool(payload.get("loop_on_suggest", False)),
        data_bag=payload.get("data_bag"),
    )


def _action_from_wire(payload):
    """A :class:`keypirinha.Action`, or ``None`` for the default action."""
    if payload is None:
        return None
    return keypirinha.Action(
        payload.get("name", ""),
        payload.get("label", ""),
        payload.get("short_desc", ""),
    )


# --------------------------------------------------------------------------
# The host object the shim talks to
# --------------------------------------------------------------------------


class _Host:
    """Implements the documented host protocol for one plugin instance.

    Every optional capability is served *here in the child*, from the package
    root the worker was given, rather than by a request back to the Rust side.
    A round trip inside a callback would need a second frame on a channel whose
    contract is exactly one response per request, and the host cannot answer
    while it is blocked waiting for that response anyway.
    """

    def __init__(self, terminate):
        self._terminate = terminate
        self._root = os.environ.get(ENV_PACKAGE_ROOT, "")
        self._package_id = os.environ.get(ENV_PACKAGE_ID) or os.environ.get(
            ENV_MAIN_MODULE, "legacy-package"
        )
        self._cache_dir = os.environ.get(ENV_CACHE_DIR) or os.path.join(
            tempfile.gettempdir(), "crikey-legacy-cache", self._package_id
        )
        self._settings = None

        #: Set by the publication capabilities and read once per request.
        self.publication = None
        #: How many times the plugin asked whether it should stop.
        self.terminate_polls = 0

    # -- per-request state -------------------------------------------------

    def begin_request(self):
        self.publication = None
        self.terminate_polls = 0

    # -- cooperative termination (spec 7.1, 14.5) --------------------------

    def should_terminate(self):
        self.terminate_polls += 1
        return self._terminate.is_set()

    def terminate_event(self):
        """The flag itself, so a delayed poll wakes the instant it is raised.

        Counted as a poll: ``keypirinha.should_terminate(delay)`` reaches the
        event instead of :meth:`should_terminate` when a delay is supplied, and
        a counter that missed those would under-report exactly the plugins that
        throttle politely.
        """
        self.terminate_polls += 1
        return self._terminate

    # -- publication (spec 7.1, 14.8) --------------------------------------

    def publish_catalog(self, plugin, items, merge):
        self.publication = ("set_catalog", [_item_to_wire(item) for item in items], bool(merge))

    def publish_suggestions(self, plugin, suggestions, match_method, sort_method):
        self.publication = (
            "suggestions",
            [_item_to_wire(item) for item in suggestions],
            int(match_method),
            int(sort_method),
        )

    # -- package data ------------------------------------------------------

    def package_full_path(self, plugin):
        return self._root

    def package_cache_path(self, plugin, create):
        if create:
            try:
                os.makedirs(self._cache_dir, exist_ok=True)
            except OSError as error:
                raise keypirinha.HostUnavailableError(
                    "package_cache_path",
                    "the cache directory {!r} could not be created: {}".format(
                        self._cache_dir, error
                    ),
                ) from None
        return self._cache_dir

    def load_resource(self, plugin, name):
        """A package resource, verbatim.

        A read failure is reported as :class:`keypirinha.HostUnavailableError`
        rather than as an ``OSError``: resource loading is a documented
        *optional* host capability, and unchanged plugins guard it with the
        compatibility layer's own error family (spec 14.12).
        """
        root = os.path.abspath(self._root)
        candidate = os.path.abspath(os.path.join(root, name))
        if candidate != root and not candidate.startswith(root + os.sep):
            # A resource name is package-relative by definition. Following one
            # out of the package would let a package read arbitrary files
            # under the identity of the plugin that loaded it.
            raise keypirinha.HostUnavailableError(
                "load_resource",
                "{!r} resolves outside the package root".format(name),
            )
        try:
            size = os.path.getsize(candidate)
            if size > _MAX_RESOURCE_BYTES:
                raise keypirinha.HostUnavailableError(
                    "load_resource",
                    "{!r} is {} bytes, above the {} byte resource bound".format(
                        name, size, _MAX_RESOURCE_BYTES
                    ),
                )
            with open(candidate, "rb") as handle:
                return handle.read(_MAX_RESOURCE_BYTES)
        except OSError as error:
            raise keypirinha.HostUnavailableError(
                "load_resource", "{!r} could not be read: {}".format(name, error)
            ) from None

    def load_settings(self, plugin):
        """This package's configuration as ``{section: {key: value}}``.

        Parsed once and memoised: a configuration reload that changed a value
        in the middle of a callback that had already branched on it would be
        indistinguishable from a plugin bug.
        """
        if self._settings is None:
            self._settings = self._read_settings()
        return self._settings

    def _read_settings(self):
        for name in (self._package_id, os.environ.get(ENV_MAIN_MODULE)):
            if not name:
                continue
            path = os.path.join(self._root, "{}.ini".format(name))
            if os.path.isfile(path):
                return _parse_ini(path)
        # No configuration file is not an error: every documented accessor on
        # `keypirinha.Settings` already answers "not configured" for a missing
        # key, and the plugins fall back to their own defaults.
        return {}


def _parse_ini(path):
    """Parses a Keypirinha-style configuration file.

    Hand-rolled rather than delegated to :mod:`configparser` for two reasons
    that both bite in practice: ``configparser`` performs ``%`` interpolation,
    and a legacy configuration value is routinely a Windows path or a literal
    percentage (``50% = half``) that interpolation would mangle or reject; and
    its ``DEFAULT`` section has inheritance semantics the Keypirinha format
    does not have.

    Keys and sections are stored with their first-seen spelling.
    :class:`keypirinha.Settings` does the ASCII-case-insensitive folding, so
    exactly one component decides what "the same key" means.

    Bounded by :data:`_MAX_SETTINGS_BYTES` and :data:`_MAX_SETTINGS_ENTRIES`;
    past either, parsing stops and what was read is returned.
    """
    sections = {}
    entries = 0
    consumed = 0
    current = keypirinha.Settings.DEFAULT_SECTION

    try:
        # `utf-8-sig` because a byte-order mark on the first section header
        # would otherwise become part of that section's name.
        with open(path, "r", encoding="utf-8-sig", errors="replace") as handle:
            for raw in handle:
                consumed += len(raw)
                if consumed > _MAX_SETTINGS_BYTES or entries >= _MAX_SETTINGS_ENTRIES:
                    break
                line = raw.strip()
                if not line or line[0] in ";#":
                    continue
                if line[0] == "[" and line.endswith("]"):
                    current = line[1:-1].strip()
                    continue
                separator = "=" if "=" in line else (":" if ":" in line else None)
                if separator is None:
                    continue
                key, _, value = line.partition(separator)
                sections.setdefault(current, {}).setdefault(key.strip(), value.strip())
                entries += 1
    except OSError:
        return sections

    return sections


# --------------------------------------------------------------------------
# Loading the plugin
# --------------------------------------------------------------------------


def _module_key(name):
    """A legal module name for a package whose id may contain hyphens.

    Legacy package directories are named for humans (``well-behaved``), and a
    hyphen cannot appear in a Python module name. The module is therefore
    loaded from its file under a sanitized key rather than imported by name.
    """
    sanitized = "".join(char if char.isalnum() or char == "_" else "_" for char in name)
    return sanitized or "legacy_plugin"


def _load_plugin():
    """Imports the package's main module and instantiates its plugin.

    Deliberately called on the *first request*, not at startup: a package with
    a broken import must surface as an attributable ``failed`` frame on
    ``on_start``, naming the plugin and carrying the traceback, rather than as
    a handshake that never completes and a worker the host can only describe
    as dead.
    """
    root = os.environ.get(ENV_PACKAGE_ROOT, "")
    main_module = os.environ.get(ENV_MAIN_MODULE, "")
    relative = os.environ.get(ENV_MAIN_MODULE_PATH) or "{}.py".format(main_module)
    path = os.path.join(root, relative)

    # Pre-import the sibling shim modules before the package root leads
    # sys.path, so a package shipping its own `keypirinha_util.py` (or _net /
    # _wintypes) cannot shadow the shim once its root sits at sys.path[0] and
    # the plugin triggers the lazy import (Finding 6, spec 14.2). `keypirinha`
    # itself is already cached by the entry pre-import. These are safe to load
    # on every platform (each documents unconditional, side-effect-free import)
    # and NON-shim package-local imports still resolve against the root below.
    import keypirinha_util
    import keypirinha_net
    import keypirinha_wintypes

    # The package root leads sys.path so a package-local `import helpers`
    # resolves to the package's own module and never to a same-named module
    # from the shim directory or the standard library.
    if root and root not in sys.path:
        sys.path.insert(0, root)

    key = _module_key(main_module or os.path.splitext(os.path.basename(relative))[0])
    spec = importlib.util.spec_from_file_location(key, path)
    if spec is None or spec.loader is None:
        raise ImportError("the legacy main module {!r} could not be located".format(path))
    module = importlib.util.module_from_spec(spec)
    # Registered before execution so a module that imports itself, or that is
    # re-entered by a decorator, sees the partially initialised module the way
    # a normal import would.
    sys.modules[key] = module
    spec.loader.exec_module(module)

    return _instantiate(module)


def _instantiate(module):
    """The single :class:`keypirinha.Plugin` subclass defined in `module`.

    Matched by base class, never by a fixed class name: unchanged packages name
    their plugin class whatever they like.
    """
    candidates = [
        value
        for value in vars(module).values()
        if isinstance(value, type)
        and issubclass(value, keypirinha.Plugin)
        and value is not keypirinha.Plugin
        and value.__module__ == module.__name__
    ]
    if not candidates:
        raise TypeError(
            "the legacy module {!r} defines no keypirinha.Plugin subclass".format(
                module.__name__
            )
        )

    # A package may define its own intermediate base class; the plugin is the
    # most derived one. Ordering falls back to definition order, which `vars`
    # preserves, so the choice is deterministic either way.
    leaves = [
        candidate
        for candidate in candidates
        if not any(other is not candidate and issubclass(other, candidate) for other in candidates)
    ]
    return (leaves or candidates)[0]()


# --------------------------------------------------------------------------
# Dispatch
# --------------------------------------------------------------------------


def _dispatch(plugin, host, callback, payload):
    """Runs one documented callback and returns the response body.

    The returned tuple is ``(outcome, extra_fields)``. Only ``on_catalog`` and
    ``on_suggest`` consult what the plugin published: a publication made from
    any other callback is not what that callback answers, and reporting it as
    such would let an ``on_execute`` silently replace the catalog.
    """
    if callback == "on_start":
        plugin.on_start()
        return "acknowledged", {}

    if callback == "on_catalog":
        plugin.on_catalog()
        published = host.publication
        if published is not None and published[0] == "set_catalog":
            return "set_catalog", {"items": published[1], "merge": published[2]}
        return _OUTCOME_ABANDONED, {}

    if callback == "on_suggest":
        selected = payload.get("selected_id")
        # The host sends the selected item's id, not the item: an id is what
        # the scheduler retains for an in-flight query, and shipping a whole
        # item would let a stale copy of it reach the plugin.
        chain = (
            []
            if selected is None
            else [
                keypirinha.CatalogItem(
                    category=int(_CATEGORY.REFERENCE),
                    label=selected,
                    short_desc="",
                    target=selected,
                    args_hint=int(_ARGS_HINT.ACCEPTED),
                    hit_hint=int(_HIT_HINT.IGNORE),
                )
            ]
        )
        plugin.on_suggest(payload.get("query", ""), chain)
        published = host.publication
        if published is not None and published[0] == "suggestions":
            return "suggestions", {
                "items": published[1],
                "match": published[2],
                "sort": published[3],
            }
        return _OUTCOME_ABANDONED, {}

    if callback == "on_execute":
        plugin.on_execute(
            _item_from_wire(payload.get("item") or {}),
            _action_from_wire(payload.get("action")),
        )
        return "executed", {}

    if callback == "on_activated":
        plugin.on_activated()
        return "acknowledged", {}

    if callback == "on_deactivated":
        plugin.on_deactivated()
        return "acknowledged", {}

    if callback == "on_events":
        plugin.on_events(keypirinha.Events(int(payload.get("flags", 0))))
        return "acknowledged", {}

    raise _UnknownCallback(callback)


class _UnknownCallback(Exception):
    """The host asked for a callback this protocol version does not define.

    Reported with an ``error.kind`` other than ``plugin-exception``, because it
    is a transport disagreement between the two sides and not a plugin bug; the
    host turns it into a protocol error carrying the whole line.
    """

    def __init__(self, callback):
        self.callback = callback
        Exception.__init__(
            self, "unknown legacy callback {!r} in protocol version {}".format(
                callback, _PROTOCOL_VERSION
            )
        )


# --------------------------------------------------------------------------
# Frames
# --------------------------------------------------------------------------


def _encode(frame):
    """One frame as its wire line.

    ``ensure_ascii=False`` keeps non-ASCII labels readable rather than
    expanding them into escapes, and JSON escaping is what makes a label
    containing a newline safe on a line-delimited channel: the newline becomes
    ``\\n`` and one frame stays one line.

    ``default=repr`` is a guard, not a feature: a plugin may put anything at
    all in an item's data bag, and a ``TypeError`` while serialising a frame
    would cost the request its only response.
    """
    return json.dumps(frame, ensure_ascii=False, separators=(",", ":"), default=repr) + "\n"


def _failure(request_id, callback, log, polls, kind, exception_type, message, tb):
    return {
        "id": request_id,
        "ok": False,
        "callback": callback,
        "outcome": "failed",
        "log": log,
        "terminate_polls": polls,
        "error": {
            "kind": kind,
            "type": exception_type,
            "message": message,
            "traceback": tb,
        },
    }


def _emit(frame):
    """Writes exactly one line to the protocol channel.

    Every path through :func:`_serve_request` ends here exactly once: the host
    reads one response per request id, and a second line — or none — is a
    desynchronised stream, not a degraded one.
    """
    try:
        line = _encode(frame)
    except (TypeError, ValueError) as error:
        line = _encode(
            _failure(
                frame.get("id"),
                frame.get("callback"),
                frame.get("log", []),
                frame.get("terminate_polls", 0),
                "plugin-exception",
                type(error).__name__,
                "the response could not be serialised: {}".format(error),
                traceback.format_exc(),
            )
        )

    if len(line.encode("utf-8", "replace")) > _MAX_FRAME_BYTES:
        line = _encode(
            _failure(
                frame.get("id"),
                frame.get("callback"),
                [],
                frame.get("terminate_polls", 0),
                "plugin-exception",
                "ValueError",
                "the response frame exceeds the {} byte protocol frame bound; the "
                "publication is too large to cross the process boundary".format(
                    _MAX_FRAME_BYTES
                ),
                "",
            )
        )

    _PROTOCOL.write(line)
    _PROTOCOL.flush()


# --------------------------------------------------------------------------
# The worker loop
# --------------------------------------------------------------------------


class _Worker:
    """Owns the plugin, the host object and the terminate flag."""

    def __init__(self):
        self.terminate = threading.Event()
        self.host = _Host(self.terminate)
        self._plugin = None
        keypirinha._set_host(self.host)

    def plugin(self):
        if self._plugin is None:
            self._plugin = _load_plugin()
        return self._plugin

    def serve(self, frame):
        """Answers one request frame with exactly one response frame."""
        request_id = frame.get("id")
        callback = frame.get("callback")
        payload = frame.get("payload") or {}

        # The host's stamped flag is authoritative and applied at dequeue, not
        # at parse: it is a faithful snapshot of the host atomic taken when this
        # request frame was written, so the child's Event must equal it for the
        # plugin's *first* poll inside this request (Finding 1, acceptance
        # 31.17). set() when raised, clear() when not: a set-only branch would
        # leave the flag stuck true for every later request once any one request
        # raised it, so a cooperative plugin would abandon work forever.
        if frame.get("terminate"):
            self.terminate.set()
        else:
            self.terminate.clear()

        self.host.begin_request()
        _CAPTURE.reset()

        try:
            outcome, extra = _dispatch(self.plugin(), self.host, callback, payload)
        except _UnknownCallback as error:
            _emit(
                _failure(
                    request_id,
                    callback,
                    _CAPTURE.take(),
                    self.host.terminate_polls,
                    "unknown-callback",
                    type(error).__name__,
                    str(error),
                    "",
                )
            )
            return
        except BaseException as error:
            # `BaseException`, not `Exception`: a plugin raising `SystemExit`
            # is a plugin bug like any other, and honouring it would take the
            # worker down and be reported to the user as a crash. A real
            # process death — `os._exit`, a signal — bypasses this entirely,
            # which is exactly the distinction the host needs to draw.
            _emit(
                _failure(
                    request_id,
                    callback,
                    _CAPTURE.take(),
                    self.host.terminate_polls,
                    "plugin-exception",
                    type(error).__name__,
                    str(error),
                    traceback.format_exc(),
                )
            )
            return

        response = {
            "id": request_id,
            "ok": True,
            "callback": callback,
            "outcome": outcome,
            "log": _CAPTURE.take(),
            "terminate_polls": self.host.terminate_polls,
        }
        response.update(extra)
        _emit(response)


def _read_stdin(stream, pending, terminate):
    """Reads frames off stdin forever, on its own thread.

    Runs concurrently with the callback on purpose. ``set_terminate`` is
    applied *here*, the moment it is parsed, because a plugin spinning inside
    ``on_suggest`` can only observe cancellation if the flag is raised while
    that callback is still on the stack. Queuing it behind the in-flight
    request would make cooperative termination unobservable by construction.
    """
    while True:
        try:
            line = stream.readline()
        except (OSError, ValueError):
            # A closed stdin is end of input, not a failure: the host drops
            # the pipe to ask for shutdown.
            pending.put(_EOF)
            return
        if not line:
            pending.put(_EOF)
            return

        stripped = line.strip()
        if not stripped:
            continue

        try:
            frame = json.loads(stripped)
        except ValueError:
            _REAL_STDERR.write("[err][crikey] undecodable request line ignored\n")
            _REAL_STDERR.flush()
            continue
        if not isinstance(frame, dict):
            _REAL_STDERR.write("[err][crikey] request line was not an object; ignored\n")
            _REAL_STDERR.flush()
            continue

        if frame.get("callback") == _CALLBACK_SET_TERMINATE:
            wanted = (frame.get("payload") or {}).get("terminate", True)
            if wanted:
                terminate.set()
            else:
                terminate.clear()
            continue

        pending.put(frame)


def main():
    """Runs the worker until shutdown, end of stdin, or process death.

    Returns the process exit status. ``0`` for every orderly end: a shutdown
    request, and end of stdin — the host drops the pipe immediately after
    asking, so the two are the same event arriving in either order, and a
    non-zero status for either would be reported to the user as a crashed
    plugin.
    """
    # The handshake is the first line on stdout, unconditionally, before the
    # plugin module is located: the host waits for it, and any work done first
    # would turn a broken package into a hung spawn instead of a reported
    # failure on `on_start`.
    _PROTOCOL.write(
        _encode({"ready": True, "pid": os.getpid(), "protocol": _PROTOCOL_VERSION})
    )
    _PROTOCOL.flush()

    worker = _Worker()
    pending = queue.Queue(maxsize=_MAX_PENDING_REQUESTS)
    reader = threading.Thread(
        target=_read_stdin,
        args=(sys.stdin, pending, worker.terminate),
        name="crikey-legacy-stdin",
        # Daemon so a reader blocked in `readline` on a stdin the host never
        # closes cannot keep this process alive past its own exit.
        daemon=True,
    )
    reader.start()

    while True:
        frame = pending.get()
        if frame is _EOF:
            return 0
        if frame.get("callback") == _CALLBACK_SHUTDOWN:
            # No reply. The host drops stdin as it asks, and writing to a
            # stdout it has stopped reading risks a `BrokenPipeError` that
            # would cost this process the zero exit status shutdown requires.
            return 0
        worker.serve(frame)


if __name__ == "__main__":
    sys.exit(main())
