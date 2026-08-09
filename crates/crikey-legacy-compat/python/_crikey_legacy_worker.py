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

import fnmatch
import importlib.util
import io
import json
import os
import queue
import sys
import tempfile
import threading
import traceback
import types

# --------------------------------------------------------------------------
# stdout hygiene: FIRST, before anything else can write a byte
#
# `keypirinha` is imported here and its stdout guard installed before the
# plugin module is located, let alone executed. Anything that printed before
# this point would land on the protocol channel and corrupt the handshake.
# --------------------------------------------------------------------------

import keypirinha
import keypirinha_net
import keypirinha_util
import keypirinha_wintypes

#: The process's real stderr, kept before `sys.stderr` is rebound. Every
#: captured line is mirrored here as it is written, so a developer watching the
#: child sees plugin output live and the host's stderr ring stays useful for
#: the crash tail.
_REAL_STDERR = sys.stderr
# Keep every text boundary explicit. The Rust host supplies these settings in
# the normal launch path, but the worker is also executable directly during
# diagnostics and must not silently adopt a platform locale.
def _configure_utf8(stream, errors):
    reconfigure = getattr(stream, "reconfigure", None)
    if callable(reconfigure):
        reconfigure(encoding="utf-8", errors=errors)
        return
    encoding = getattr(stream, "encoding", "utf-8") or "utf-8"
    if encoding.lower().replace("-", "") != "utf8":
        raise ValueError("the worker stream cannot be configured as UTF-8")


_configure_utf8(sys.stdin, "strict")
_configure_utf8(_REAL_STDERR, "backslashreplace")


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
ENV_CONFIG_DIR = "CRIKEY_LEGACY_CONFIG_DIR"
ENV_INSTALLED_PACKAGE_DIR = "CRIKEY_LEGACY_INSTALLED_PACKAGE_DIR"
ENV_CACHE_ROOT = "CRIKEY_LEGACY_CACHE_ROOT"

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

#: Ceiling on one icon a plugin may name, in bytes. The host applies the same
#: ceiling when it actually reads the file (`crikey-app`'s
#: `MAX_PLUGIN_ICON_BYTES`); this copy exists so `load_icon` refuses at the
#: call site, where the plugin can still report the problem, rather than
#: leaving an item silently iconless several frames later.
_MAX_ICON_BYTES = 256 * 1024

#: Bounds on the per-category action registry. Its contents are attached to
#: every published item, so an unbounded registry is an unbounded frame.
_MAX_ACTION_CATEGORIES = 64
_MAX_ACTIONS_PER_CATEGORY = 32

#: Longest a callback will block waiting for the launcher to answer a
#: host-mediated request.
#:
#: A bound rather than an indefinite wait: the host services these inside the
#: callback deadline and will reap this process if it overruns, but a host
#: that broke its side of the exchange must not be able to wedge the plugin
#: thread here with nothing to report. Exceeding it raises
#: `HostUnavailableError`, so the plugin learns the action did not happen.
_HOST_REQUEST_TIMEOUT_SECONDS = 60.0

#: Bounds on one `find_resources` walk: names reported, and directory entries
#: examined. A package of unknown provenance must not be able to make either
#: side of the boundary hold an unbounded list, and overflow is a refusal —
#: a truncated answer would read as "the package has no such file".
_MAX_FOUND_RESOURCES = 4096
_MAX_SCANNED_ENTRIES = 65536

#: The action name the host reserves for "the user pressed Enter": the
#: `legacy.execute` action `crikey-app` attaches to every legacy row. A plugin
#: action spelled the same way would reach `on_execute` as the *default*
#: action — that is, as `None` — instead of as itself, so it is refused.
_RESERVED_ACTION_NAME = "legacy.execute"

#: Scheme of the documented cross-package icon reference form.
_RES_SCHEME = "res://"

#: Sentinel the reader thread queues when it has reached an input protocol
#: error. The main thread exits non-zero so the host reports a broken peer
#: promptly instead of waiting for a callback timeout.
_PROTOCOL_ERROR = object()

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
        self._lock = threading.RLock()
        self._lines = []
        self._partial = []
        self._partial_chars = 0
        self._dropped = 0

    # -- stream protocol ---------------------------------------------------

    def write(self, text):
        if not isinstance(text, str):
            text = str(text)
        with self._lock:
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
        with self._lock:
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
        with self._lock:
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
        with self._lock:
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


def _category_from_wire(value):
    """Decodes built-in and plugin-defined category spellings."""
    folded = value.lower() if isinstance(value, str) else ""
    # CriKey's generic category tag is injective. The legacy API's own
    # extension spelling remains ``legacy-user-N``; accepting both lets a
    # selected item cross the host boundary without turning a plugin-defined
    # name such as ``application`` into the built-in category.
    generic_prefix = "plugin-defined:"
    explicitly_plugin_defined = folded.startswith(generic_prefix)
    if explicitly_plugin_defined:
        folded = folded[len(generic_prefix) :]
    known = None if explicitly_plugin_defined else _WIRE_TO_CATEGORY.get(folded)
    if known is not None:
        return known
    prefix = "legacy-user-"
    if folded.startswith(prefix):
        try:
            numeric = int(folded[len(prefix) :], 10)
        except (TypeError, ValueError):
            pass
        else:
            if numeric >= int(_CATEGORY.USER_BASE):
                return numeric
    return int(_CATEGORY.USER_BASE)


def _category_to_wire(value):
    """The wire spelling of a category, preserving plugin-defined ones.

    A category at or above ``ItemCategory.USER_BASE`` is the documented
    extension point, so its number is carried in the name rather than folded
    into a built-in category the plugin never asked for (spec 10.3).
    """
    known = _CATEGORY_TO_WIRE.get(value)
    return known if known is not None else "legacy-user-{}".format(value)


def _item_to_wire(item, actions, default_icon):
    """One :class:`keypirinha.CatalogItem` as a JSON-ready dict.

    ``icon`` is the *name* behind the item's handle, never the handle itself:
    the handle is an in-process object, and the host is the side that owns the
    package directory and the only side that can bound, read and decode the
    file. An item that names no icon inherits the plugin's default, which is
    the whole purpose of ``set_default_icon``.

    ``actions`` is the plugin's registration for this item's category,
    attached to the item rather than shipped as a registry frame of its own.
    The channel answers exactly one frame per request, so an item that
    travelled without its actions would reach the launcher unlaunchable in the
    very request that published it.

    ``plugin_id`` and ``stable_id`` are absent because identity is the host's
    to assign — a plugin able to name another plugin's id could inject items
    into its catalog (spec 10.2).
    """
    category = _category_to_wire(item.category())
    icon = keypirinha._icon_reference(item.icon_handle())
    return {
        "category": category,
        "label": item.label(),
        "short_desc": item.short_desc(),
        "target": item.target(),
        "args_hint": _ARGS_HINT_TO_WIRE.get(item.args_hint(), "accepted"),
        "hit_hint": _HIT_HINT_TO_WIRE.get(item.hit_hint(), "keepall"),
        "loop_on_suggest": item.loop_on_suggest(),
        "data_bag": item.data_bag(),
        "icon": default_icon if icon is None else icon,
        "actions": actions.get(category, []),
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
        category=_category_from_wire(category),
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
# Containment of plugin-supplied names
#
# Every string below arrives from plugin code and names a file. The rule for
# all of them is the same: refuse, never normalise. Normalising is how a
# traversal check becomes a traversal — `a/../../etc/passwd` only looks
# harmless once you have already resolved it — and refusal is the one answer
# that cannot be subtly wrong.
# --------------------------------------------------------------------------


def _package_relative(operation, name):
    """One plugin-supplied name as a package-relative POSIX path.

    Backslash is refused rather than treated as a separator: it separates
    components on Windows and is an ordinary filename character here, and one
    spelling that means two different files on two hosts is a containment hole
    on whichever host loses the argument.
    """
    if not isinstance(name, str) or not name:
        raise keypirinha.HostUnavailableError(
            operation, "a package resource name must be a non-empty string, got {!r}".format(name)
        )
    if "\x00" in name or "\\" in name:
        raise keypirinha.HostUnavailableError(
            operation,
            "{!r} holds a backslash or a NUL; package resource names are POSIX-style "
            "and package-relative".format(name),
        )
    if name.startswith("/") or os.path.isabs(name) or os.path.splitdrive(name)[0]:
        raise keypirinha.HostUnavailableError(
            operation, "{!r} is absolute; package resource names are package-relative".format(name)
        )
    parts = [part for part in name.split("/") if part not in ("", ".")]
    if not parts or ".." in parts:
        raise keypirinha.HostUnavailableError(
            operation,
            "{!r} would leave the package directory; a legacy package may only name "
            "its own files".format(name),
        )
    return "/".join(parts)


def _icon_source_reference(package_id, source):
    """The package-relative name behind one documented icon source.

    Two spellings are documented: a package-relative path, and
    ``res://Package/path``. The second is honoured only when it names *this*
    package. Cross-package icon loading is refused by name because the host
    resolves a legacy reference against the directory of the plugin that
    published the item and has no way to reach another package's — resolving
    it against this package instead would hand back the wrong picture and call
    it a success.
    """
    if not isinstance(source, str) or not source:
        raise keypirinha.HostUnavailableError(
            "load_icon", "an icon source must be a non-empty string, got {!r}".format(source)
        )
    if source[: len(_RES_SCHEME)].lower() != _RES_SCHEME:
        return _package_relative("load_icon", source)

    package, separator, relative = source[len(_RES_SCHEME) :].partition("/")
    if not separator or not relative:
        raise keypirinha.HostUnavailableError(
            "load_icon", "{!r} is not a `res://Package/file` reference".format(source)
        )
    if keypirinha._fold(package) != keypirinha._fold(package_id):
        raise keypirinha.HostUnavailableError(
            "load_icon",
            "{!r} names the package {!r}; CriKey resolves a legacy icon only inside "
            "the package that loaded it".format(source, package),
        )
    return _package_relative("load_icon", relative)


def _pattern_segments(pattern):
    """`pattern` split into path segments, refused if it could leave the package.

    A pattern is matched against package-relative names, so one that could
    only match something outside the package is a plugin bug worth naming
    rather than an empty result to puzzle over.
    """
    if not isinstance(pattern, str) or not pattern:
        raise keypirinha.HostUnavailableError(
            "find_resources", "a resource pattern must be a non-empty string, got {!r}".format(pattern)
        )
    if "\x00" in pattern or "\\" in pattern:
        raise keypirinha.HostUnavailableError(
            "find_resources",
            "{!r} holds a backslash or a NUL; resource patterns are POSIX-style and "
            "package-relative".format(pattern),
        )
    if pattern.startswith("/") or os.path.isabs(pattern) or os.path.splitdrive(pattern)[0]:
        raise keypirinha.HostUnavailableError(
            "find_resources", "{!r} is absolute; resource patterns are package-relative".format(pattern)
        )
    segments = [segment for segment in pattern.split("/") if segment not in ("", ".")]
    if not segments or ".." in segments:
        raise keypirinha.HostUnavailableError(
            "find_resources",
            "{!r} would leave the package directory; a legacy package may only "
            "enumerate its own files".format(pattern),
        )
    return segments


def _matches(segments, parts):
    """Whether the path `parts` matches the pattern `segments`.

    Segment by segment rather than `fnmatch` over the whole path, because
    `fnmatch`'s ``*`` crosses ``/``: ``data/*`` would then also match
    ``data/nested/file``, and a pattern that says one directory deep has to
    mean one directory deep. ``**`` is the explicit way to span any number of
    segments, including none.
    """
    if not segments:
        return not parts
    if segments[0] == "**":
        return any(_matches(segments[1:], parts[index:]) for index in range(len(parts) + 1))
    if not parts:
        return False
    return fnmatch.fnmatchcase(parts[0], segments[0]) and _matches(segments[1:], parts[1:])



# --------------------------------------------------------------------------
# Asking the launcher to do something (spec 14.8, 15.4)
# --------------------------------------------------------------------------


class _HostChannel:
    """A blocking question to the launcher, asked from inside a callback.

    Publication is fire-and-forget because it is a statement. A host-mediated
    action is a *question*: the plugin must be told whether the launcher
    performed it, and cannot be unless the answer travels back.

    The callback thread blocks here while the reader thread — the only thread
    that reads stdin — matches the answer by sequence number and wakes it. The
    sequence is per-request rather than per-callback because one callback may
    ask more than once.
    """

    def __init__(self):
        self._lock = threading.Lock()
        self._next = 0
        self._waiting = {}

    def request(self, operation, payload):
        with self._lock:
            self._next += 1
            sequence = self._next
            slot = {"event": threading.Event(), "frame": None}
            self._waiting[sequence] = slot

        try:
            self._write(
                operation,
                {"host_request": operation, "seq": sequence, "payload": payload},
            )
            answered = slot["event"].wait(_HOST_REQUEST_TIMEOUT_SECONDS)
            frame = slot["frame"]
        finally:
            with self._lock:
                self._waiting.pop(sequence, None)

        if not answered or frame is None:
            raise keypirinha.HostUnavailableError(
                operation,
                "the launcher did not answer within {:g} seconds, so the "
                "action was not performed".format(_HOST_REQUEST_TIMEOUT_SECONDS),
            )
        if not frame.get("ok"):
            raise keypirinha.HostUnavailableError(
                operation,
                frame.get("reason") or "the launcher refused without saying why",
            )
        return frame.get("value")

    def _write(self, operation, frame):
        """Writes one request line.

        Deliberately not :func:`_emit`: that function's failure path replaces
        an unserialisable frame with a *response* frame, which here would
        answer a request id that is not being asked about and desynchronise
        the channel. A request that cannot be encoded is the plugin's problem
        and is reported to the plugin.
        """
        try:
            line = _encode(frame)
        except BaseException as error:
            raise keypirinha.HostUnavailableError(
                operation,
                "the request could not be encoded: {}".format(error),
            ) from None
        if len(line.encode("utf-8", "replace")) > _MAX_FRAME_BYTES:
            raise keypirinha.HostUnavailableError(
                operation,
                "the request exceeds the {} byte protocol frame bound".format(
                    _MAX_FRAME_BYTES
                ),
            )
        _PROTOCOL.write(line)
        _PROTOCOL.flush()

    def deliver(self, frame):
        """Whether `frame` was an answer, and has been routed if so."""
        sequence = frame.get("host_response")
        if not isinstance(sequence, int) or isinstance(sequence, bool):
            return False
        with self._lock:
            slot = self._waiting.get(sequence)
        if slot is not None:
            slot["frame"] = frame
            slot["event"].set()
        # A late answer to a request that already gave up is dropped, not
        # queued as a request: the caller has already been told the launcher
        # did not answer, and handing this to the dispatcher would make it a
        # protocol error over something the host did nothing wrong in.
        return True


_HOST_CHANNEL = _HostChannel()

# --------------------------------------------------------------------------
# The host object the shim talks to
# --------------------------------------------------------------------------


class _Host:
    """Implements the documented host protocol for one plugin instance.

    Nearly every optional capability is served *here in the child*, from the
    package root the worker was given, rather than by a request back to the
    Rust side: the round trip costs a frame and the answer is already here.

    The exception is anything the launcher must *do* rather than merely know.
    Those go over :class:`_HostChannel`, because performing them here would
    escape the launcher's permission gate and would put whatever was launched
    in this worker's process group, where the next reap would kill it.
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

        #: Alternate actions per wire category, from `set_actions`. Survives
        #: the request that registered it: published packages register in
        #: `on_start` and publish from `on_catalog` and `on_suggest`.
        self._actions = {}
        #: Package-relative icon name inherited by items that name none.
        self._default_icon = None

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
        self.publication = ("set_catalog", self._to_wire(items), bool(merge))

    def publish_suggestions(self, plugin, suggestions, match_method, sort_method):
        self.publication = (
            "suggestions",
            self._to_wire(suggestions),
            int(match_method),
            int(sort_method),
        )

    def _to_wire(self, items):
        """Renders a publication, stamping each item with what it inherits.

        Read at publication time, not at construction time: a plugin that
        registers actions or a default icon and only then publishes must see
        both applied, and one that re-registers afterwards must not have its
        already-published batch rewritten underneath it.
        """
        return [_item_to_wire(item, self._actions, self._default_icon) for item in items]

    # -- alternate actions and icons (spec 14.4) ---------------------------

    def set_actions(self, plugin, category, actions):
        """Registers one category's alternate action list.

        Replacing per category, never merging: Keypirinha's own call replaces,
        and a plugin that rebuilds a category's actions would otherwise
        accumulate every earlier spelling of them.
        """
        wire = _category_to_wire(category)
        if len(actions) > _MAX_ACTIONS_PER_CATEGORY:
            raise keypirinha.HostUnavailableError(
                "set_actions",
                "{} actions for category {}; the layer carries at most {} per "
                "category".format(len(actions), wire, _MAX_ACTIONS_PER_CATEGORY),
            )
        encoded = []
        for action in actions:
            if not isinstance(action, keypirinha.Action):
                raise keypirinha.HostUnavailableError(
                    "set_actions",
                    "set_actions takes keypirinha.Action values from create_action, "
                    "got {!r}".format(action),
                )
            if action.name() == _RESERVED_ACTION_NAME:
                raise keypirinha.HostUnavailableError(
                    "set_actions",
                    "{!r} is the host's own default-action name; an action spelled that "
                    "way would reach on_execute as the default action instead of as "
                    "itself".format(_RESERVED_ACTION_NAME),
                )
            encoded.append(
                {
                    "name": action.name(),
                    "label": action.label(),
                    "short_desc": action.short_desc(),
                }
            )
        if not encoded:
            self._actions.pop(wire, None)
            return
        if wire not in self._actions and len(self._actions) >= _MAX_ACTION_CATEGORIES:
            raise keypirinha.HostUnavailableError(
                "set_actions",
                "actions are already registered for {} categories, the layer's "
                "ceiling".format(_MAX_ACTION_CATEGORIES),
            )
        self._actions[wire] = encoded

    def load_icon(self, plugin, sources):
        """Validates every icon source and returns the names the host resolves.

        The file is confirmed to exist inside the package and to be within the
        icon ceiling *here*, so a plugin learns at the call site that its icon
        will not load. The host checks again when it reads the bytes; that is
        the check that actually protects the launcher, and this one is what
        makes the failure attributable.
        """
        if not sources:
            raise keypirinha.HostUnavailableError("load_icon", "no icon source was supplied")
        references = []
        for source in sources:
            reference = _icon_source_reference(self._package_id, source)
            path = os.path.join(self._root, *reference.split("/"))
            if not os.path.isfile(path):
                raise keypirinha.HostUnavailableError(
                    "load_icon", "{!r} is not a file inside the package".format(source)
                )
            try:
                size = os.path.getsize(path)
            except OSError as error:
                raise keypirinha.HostUnavailableError(
                    "load_icon", "{!r} could not be read: {}".format(source, error)
                ) from None
            if size > _MAX_ICON_BYTES:
                raise keypirinha.HostUnavailableError(
                    "load_icon",
                    "{!r} is {} bytes, above the {} byte icon ceiling; the layer refuses "
                    "an oversized icon rather than truncating it".format(
                        source, size, _MAX_ICON_BYTES
                    ),
                )
            references.append(reference)
        return references

    def set_default_icon(self, plugin, handle):
        """Records the icon items with no handle of their own inherit."""
        self._default_icon = keypirinha._icon_reference(handle)

    def find_resources(self, plugin, pattern):
        """Package-relative names matching `pattern`, sorted, never escaping.

        Two independent containment checks, because either alone is
        insufficient: the pattern cannot name anything outside the package,
        and every candidate is re-resolved before it is reported, which is
        what catches a *symlinked file* inside the package pointing out of it.
        Directory symlinks are simply not followed, so the walk cannot leave
        the tree and cannot loop.
        """
        segments = _pattern_segments(pattern)
        root = os.path.realpath(self._root)
        found = []
        scanned = 0
        for directory, subdirectories, files in os.walk(root, followlinks=False):
            subdirectories.sort()
            for name in sorted(files):
                scanned += 1
                if scanned > _MAX_SCANNED_ENTRIES:
                    raise keypirinha.HostUnavailableError(
                        "find_resources",
                        "the package holds more than {} entries; the walk is bounded "
                        "and refuses rather than reporting a partial "
                        "answer".format(_MAX_SCANNED_ENTRIES),
                    )
                absolute = os.path.join(directory, name)
                resolved = os.path.realpath(absolute)
                if resolved != root and not resolved.startswith(root + os.sep):
                    continue
                relative = os.path.relpath(absolute, root).replace(os.sep, "/")
                if not _matches(segments, relative.split("/")):
                    continue
                if len(found) >= _MAX_FOUND_RESOURCES:
                    raise keypirinha.HostUnavailableError(
                        "find_resources",
                        "{!r} matches more than {} names; the layer refuses rather than "
                        "reporting a truncated list".format(pattern, _MAX_FOUND_RESOURCES),
                    )
                found.append(relative)
        found.sort()
        return found

    # -- package data ------------------------------------------------------

    def package_full_path(self, plugin):
        return self._root

    def package_full_name(self, plugin):
        """Returns the package identifier declared by the host."""
        return self._package_id

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
        root = os.path.realpath(self._root)
        candidate = os.path.realpath(os.path.join(root, name))
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

    # -- installation directories ------------------------------------------

    def _directory(self, operation, variable):
        """One launcher-supplied directory, or an honest refusal.

        Never a computed fallback. CriKey owns the platform directory
        convention and this process was not told the answer; guessing one
        would have the plugin write its configuration into a directory the
        launcher never reads and the user cannot find.
        """
        value = os.environ.get(variable)
        if not value:
            raise keypirinha.HostUnavailableError(
                operation,
                "the launcher did not tell this worker where that directory is",
            )
        return value

    def user_config_dir(self):
        return self._directory("user_config_dir", ENV_CONFIG_DIR)

    def installed_package_dir(self):
        return self._directory("installed_package_dir", ENV_INSTALLED_PACKAGE_DIR)

    def package_cache_dir(self):
        return self._directory("package_cache_dir", ENV_CACHE_ROOT)

    # -- host-mediated actions ---------------------------------------------

    def execute_default_action(self, plugin, item, action):
        """Asks the launcher to act on `item` and reports what it did."""
        return bool(
            _HOST_CHANNEL.request(
                "execute_default_action",
                {
                    "item": self._to_wire([item])[0],
                    "action": None if action is None else action.name(),
                },
            )
        )


def _parse_ini(path):
    """Parses a Keypirinha-style configuration file.

    Repeated names use the last value while retaining the first spelling, and
    indented continuation lines retain their embedded newline. Those are
    ``configparser`` behaviours that legacy packages rely on.
    """
    sections = {}
    entries = 0
    current = keypirinha.Settings.DEFAULT_SECTION
    pending_key = None

    def section_spelling(name):
        folded = keypirinha._fold(name)
        for existing in sections:
            if keypirinha._fold(existing) == folded:
                return existing
        sections[name] = {}
        return name

    def key_spelling(section, name):
        folded = keypirinha._fold(name)
        for existing in sections[section]:
            if keypirinha._fold(existing) == folded:
                return existing
        return name

    try:
        # Read a bounded byte slice before decoding. Iterating a text file can
        # buffer a very long line past the nominal limit, and counting decoded
        # characters is not a byte bound for UTF-8 input.
        with open(path, "rb") as handle:
            raw_bytes = handle.read(_MAX_SETTINGS_BYTES)
    except OSError:
        return sections

    text = raw_bytes.decode("utf-8-sig", errors="replace")
    for raw in io.StringIO(text):
        if entries >= _MAX_SETTINGS_ENTRIES:
            break
        raw_line = raw.rstrip("\r\n")
        line = raw_line.strip()
        if not line:
            pending_key = None
            continue
        if line[0] in ";#":
            pending_key = None
            continue

        # A continuation is identified before parsing separators: a value may
        # itself contain '=' or ':', and configparser treats an indented line
        # as part of the preceding key regardless of those characters.
        if raw_line[:1].isspace() and pending_key is not None:
            sections[current][pending_key] += "\n" + line
            continue

        if line[0] == "[" and line.endswith("]"):
            current = section_spelling(line[1:-1].strip())
            pending_key = None
            continue

        delimiters = []
        for symbol in ("=", ":"):
            index = line.find(symbol)
            if index >= 0:
                delimiters.append((index, symbol))
        if not delimiters:
            pending_key = None
            continue
        separator = min(delimiters)[1]
        key, _, value = line.partition(separator)
        key = key.strip()
        if not key:
            pending_key = None
            continue
        current = section_spelling(current)
        pending_key = key_spelling(current, key)
        sections[current][pending_key] = value.strip()
        entries += 1

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


def _package_key(root):
    """Returns a private import name for a package content root.

    A number of real Keypirinha packages keep helpers beside the plugin and
    use relative imports (``from .helper import ...``). Loading the entry
    file as a top-level module makes those imports fail even though the same
    files work when the package is imported normally. The root name is only a
    hint, so the prefix keeps it from colliding with the compatibility shim.
    """
    return "_crikey_legacy_package_" + _module_key(os.path.basename(os.path.normpath(root)))


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

    # The shim siblings were imported at module scope, before the package root
    # leads sys.path, so a package shipping a same-named module cannot shadow
    # the compatibility surface when the plugin triggers its lazy import.

    # The package root leads sys.path so a package-local `import helpers`
    # resolves to the package's own module and never to a same-named module
    # from the shim directory or the standard library.
    if root and root not in sys.path:
        sys.path.insert(0, root)
    main_key = _module_key(main_module or os.path.splitext(os.path.basename(relative))[0])
    package_key = _package_key(root) if root else ""
    if package_key:
        package_init = os.path.join(root, "__init__.py")
        if os.path.isfile(package_init):
            package_spec = importlib.util.spec_from_file_location(
                package_key,
                package_init,
                submodule_search_locations=[root],
            )
            if package_spec is None or package_spec.loader is None:
                raise ImportError("the legacy package {!r} could not be located".format(root))
            package = importlib.util.module_from_spec(package_spec)
            sys.modules[package_key] = package
            package_spec.loader.exec_module(package)
        else:
            # Namespace-style package roots are common in loose development
            # packages. A synthetic package gives their sibling modules the
            # same relative-import semantics without executing absent init code.
            package = types.ModuleType(package_key)
            package.__file__ = package_init
            package.__path__ = [root]
            package.__package__ = package_key
            sys.modules[package_key] = package
        key = package_key + "." + main_key
    else:
        key = main_key
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
        selected = payload.get("selected_item")
        # The host sends the selected item's complete wire representation. The
        # callback contract exposes its category, label, target, and hints; an
        # id-only placeholder loses that metadata and makes valid plugins such
        # as epoch reject argument suggestions.
        chain = [] if selected is None else [_item_from_wire(selected)]
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

    JSON escapes non-ASCII text as well as control characters. That keeps the
    protocol valid even when a plugin supplies a lone UTF-16 surrogate, which
    cannot be written to a strict UTF-8 stream directly. Non-finite floats are
    rejected because ``NaN`` and ``Infinity`` are not JSON accepted by the Rust
    decoder.

    ``default=repr`` is a guard, not a feature: a plugin may put anything at
    all in an item's data bag, and a serialization failure must still become a
    response frame.
    """
    return json.dumps(
        frame,
        ensure_ascii=True,
        allow_nan=False,
        separators=(",", ":"),
        default=repr,
    ) + "\n"


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
    except BaseException as error:
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

    def protocol_error(message):
        try:
            _REAL_STDERR.write("[err][crikey] {}\n".format(message))
            _REAL_STDERR.flush()
        except (OSError, ValueError):
            pass
        pending.put(_PROTOCOL_ERROR)

    while True:
        try:
            # The size limit is applied while reading, rather than after an
            # unbounded ``readline()``, so a hostile peer cannot make the child
            # retain an arbitrarily large unterminated line.
            line = stream.readline(_MAX_FRAME_BYTES + 1)
        except UnicodeDecodeError:
            protocol_error("request line was not valid UTF-8")
            return
        except (OSError, ValueError):
            # A closed stdin is end of input, not a failure: the host drops
            # the pipe to ask for shutdown.
            pending.put(_EOF)
            return
        if not line:
            pending.put(_EOF)
            return

        try:
            line_bytes = len(line.encode("utf-8"))
        except UnicodeError:
            protocol_error("request line was not valid UTF-8")
            return
        if line_bytes > _MAX_FRAME_BYTES:
            protocol_error(
                "request line exceeded the {} byte protocol frame bound".format(
                    _MAX_FRAME_BYTES
                )
            )
            return

        stripped = line.strip()
        if not stripped:
            protocol_error("empty request line")
            return

        try:
            frame = json.loads(stripped)
        except Exception as error:
            protocol_error(
                "undecodable request line: {}".format(type(error).__name__)
            )
            return
        if not isinstance(frame, dict):
            protocol_error("request line was not an object")
            return

        # Answers are routed here, not queued: the callback thread is blocked
        # on this frame, and the request queue is drained only by that same
        # thread once the callback returns — queueing it would deadlock.
        if _HOST_CHANNEL.deliver(frame):
            continue

        if frame.get("callback") == _CALLBACK_SET_TERMINATE:
            payload = frame.get("payload")
            if payload is None:
                payload = {}
            if not isinstance(payload, dict):
                protocol_error("set_terminate payload was not an object")
                return
            if payload.get("terminate", True):
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
    non-zero status for an input protocol error so the host reports a broken
    peer promptly instead of waiting for a callback timeout.
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
        if frame is _PROTOCOL_ERROR:
            return 1
        if frame.get("callback") == _CALLBACK_SHUTDOWN:
            # No reply. The host drops stdin as it asks, and writing to a
            # stdout it has stopped reading risks a `BrokenPipeError` that
            # would cost this process the zero exit status shutdown requires.
            return 0
        worker.serve(frame)


if __name__ == "__main__":
    sys.exit(main())
