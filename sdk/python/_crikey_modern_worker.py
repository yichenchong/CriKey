"""Child-side entry point of the modern CriKey Python worker (spec 15).

Run by the host as ``python -S <dir>/_crikey_modern_worker.py``. ``-S`` skips
``site`` so a user's global site-packages cannot shadow imports, while the
host-assembled ``PYTHONPATH`` (plugin source, packaged modules, managed deps,
the CriKey SDK) is still honoured by the interpreter. This process therefore
sees the stdlib, its own package, its managed dependencies and ``crikey_sdk`` --
and nothing else.

The wire protocol is newline-delimited JSON, one object per line, in both
directions (contract §1, §2). ``stdout`` is a STRICT protocol channel: anything
the plugin ``print``s is captured and returned inside a reply's ``log``, never
written as a bare line that would desync the JSON stream. ``stderr`` is mirrored
onward for a developer watching the child and kept by the host as a crash tail.

The structure mirrors the M3 legacy worker
(``crates/crikey-legacy-compat/python/_crikey_legacy_worker.py``): stdout hygiene
installed before the plugin module is imported, a daemon control-reader thread,
per-request log capture with hard bounds, exactly one terminal frame per request,
and a plugin exception reported as a structured failure that leaves the worker
alive to serve the next request.
"""

import asyncio
import contextvars
import importlib
import inspect
import json
import os
import queue
import sys
import threading
import traceback
import logging

from crikey_sdk import Action, Command, Item, Query, WorkerContext, Plugin, Preview
from crikey_sdk import ResultPresentation, TypedValues, TypedValue, TypedOutcome


def _pin_text_stream(stream, name, newline):
    """Pins a protocol-adjacent text stream to strict UTF-8."""
    reconfigure = getattr(stream, "reconfigure", None)
    if callable(reconfigure):
        reconfigure(encoding="utf-8", errors="strict", newline=newline)
        return
    if str(getattr(stream, "encoding", "")).lower() != "utf-8":
        raise RuntimeError(
            "{} must support explicit UTF-8 configuration".format(name)
        )

# --------------------------------------------------------------------------
# stdout hygiene: FIRST, before the plugin module can write a single byte.
#
# The real stdout is taken away as the protocol channel and stdout/stderr are
# rebound to a capture. Anything imported or executed below therefore cannot
# corrupt the handshake or a reply frame.
# --------------------------------------------------------------------------

#: The real stdout, the protocol channel. Nothing but frames is ever written.
_PROTOCOL = sys.stdout
#: The real stderr, kept before rebinding so captured lines can be mirrored on
#: for a developer watching the child and for the host's crash tail.
_REAL_STDERR = sys.stderr
_pin_text_stream(sys.stdin, "stdin", None)
_pin_text_stream(_PROTOCOL, "stdout", "\n")
_pin_text_stream(_REAL_STDERR, "stderr", "\n")
_SENSITIVE = False
_DIAGNOSTIC_LOCK = threading.RLock()

def _latch_sensitive():
    global _SENSITIVE
    with _CAPTURE._lock, _DIAGNOSTIC_LOCK:
        _SENSITIVE = True
        _CAPTURE.reset()
        logging.disable(sys.maxsize)

def _plugin_error(error):
    with _CAPTURE._lock, _DIAGNOSTIC_LOCK:
        if _SENSITIVE:
            return {"message": "plugin callback failed", "traceback": ""}
        return {"message": str(error), "traceback": traceback.format_exc()}

def _report_exception(label, error):
    with _CAPTURE._lock, _DIAGNOSTIC_LOCK:
        if not _SENSITIVE:
            _stderr("{}: {}\n{}".format(label, error, traceback.format_exc()))

_ORIGINAL_EXCEPTHOOK = sys.excepthook
_ORIGINAL_THREAD_EXCEPTHOOK = threading.excepthook
_ORIGINAL_UNRAISABLEHOOK = sys.unraisablehook
def _uncaught_exception(*args):
    with _CAPTURE._lock, _DIAGNOSTIC_LOCK:
        if not _SENSITIVE:
            _ORIGINAL_EXCEPTHOOK(*args)
def _uncaught_thread_exception(args):
    with _CAPTURE._lock, _DIAGNOSTIC_LOCK:
        if not _SENSITIVE:
            _ORIGINAL_THREAD_EXCEPTHOOK(args)
def _uncaught_unraisable(args):
    with _CAPTURE._lock, _DIAGNOSTIC_LOCK:
        if not _SENSITIVE:
            _ORIGINAL_UNRAISABLEHOOK(args)
def _loop_exception(loop, context):
    with _CAPTURE._lock, _DIAGNOSTIC_LOCK:
        if not _SENSITIVE:
            loop.default_exception_handler(context)
sys.excepthook = _uncaught_exception
threading.excepthook = _uncaught_thread_exception
sys.unraisablehook = _uncaught_unraisable

def _stderr(message):
    """Writes diagnostic text without letting non-UTF-8 data kill the worker."""
    if _SENSITIVE:
        return
    if not isinstance(message, str):
        message = str(message)
    safe = message.encode("utf-8", "backslashreplace").decode("utf-8")
    try:
        _REAL_STDERR.write(safe)
        _REAL_STDERR.flush()
    except (OSError, ValueError):
        pass


# --------------------------------------------------------------------------
# Wire protocol constants (agreed with `crikey-python-host` worker codec).
# --------------------------------------------------------------------------

#: The wire protocol version THIS shim implements. It is the shim's own
#: constant, not whatever the host asks for: on a mismatch the worker refuses
#: the handshake (see ``main``) instead of blindly echoing the host's number.
_PROTOCOL_VERSION = 1

#: §1 bounds, mirrored from the host so an over-long frame is a named failure
#: rather than unbounded growth.
_MAX_FRAME_BYTES = 8 * 1024 * 1024
_MAX_LOG_LINES = 512
_MAX_LOG_LINE_BYTES = 4096
_MAX_MIRROR_BYTES = _MAX_LOG_LINES * _MAX_LOG_LINE_BYTES

#: Appended to a log line clipped at the per-line cap so a truncated line reads
#: as truncated instead of as the whole value (mirrors the legacy clamp marker).
_LOG_TRUNCATION_MARKER = " \u2026[log line truncated]"

#: Items buffered before an intermediate ``result_batch`` / ``catalog_batch`` is
#: flushed. Small on purpose: streaming beats a single fat frame.
_FLUSH_THRESHOLD = 32
_CAPTURE_ACTIVE = contextvars.ContextVar("crikey_capture_active", default=True)
_OVERSIZED = object()
_TYPED_REQUEST_KEYS = frozenset(("id", "kind", "item", "action_id",
                               "schema_fingerprint", "values", "argument",
                               "field_id", "value"))

class _WireObject(dict):
    """Preserve legacy last-wins semantics, flag only new typed known members."""
    __slots__ = ("typed_duplicate", "typed_tag")
    def __init__(self, pairs):
        self.typed_duplicate = False
        self.typed_tag = False
        for key, value in pairs:
            if key in self and key in _TYPED_REQUEST_KEYS:
                self.typed_duplicate = True
            if key == "kind" and value == "typed_execute":
                self.typed_tag = True
            self[key] = value

ENV_PLUGIN_ID = "CRIKEY_MODERN_PLUGIN_ID"
ENV_ENTRYPOINT = "CRIKEY_MODERN_ENTRYPOINT"
ENV_PROTOCOL_VERSION = "CRIKEY_MODERN_PROTOCOL_VERSION"

_KIND_SET_CANCEL = "set_cancel"
KIND_BACKGROUND_ADMIT = "background_admit"
KIND_BACKGROUND_REFUSE = "background_refuse"
KIND_BACKGROUND_CANCEL = "background_cancel"
#: Host → worker configuration delivery (spec 21.4). Deliberately handled on the
#: MAIN loop rather than by the control-reader thread: it invokes a plugin
#: callback, and running plugin code on the reader thread would let a slow
#: ``on_configuration`` stop this worker from ever seeing the next cancellation.
#: It carries no ``id`` and gets no reply.
_KIND_CONFIGURATION = "configuration"
_KIND_SHUTDOWN = "shutdown"

# Worker → host lifecycle events. These are additive frames under the
# negotiated protocol version; their payload is bounded to one task id and a
# short status/error string.
KIND_BACKGROUND_REGISTER = "background_register"
KIND_BACKGROUND_COMPLETE = "background_complete"

#: Sentinel the reader thread queues on end of stdin.
_EOF = object()

#: Maximum number of requests retained while the main callback is occupied.
#: The reader blocks at this bound instead of growing an unbounded queue.
_PENDING_QUEUE_CAPACITY = 64


# --------------------------------------------------------------------------
# Per-request log capture
# --------------------------------------------------------------------------


class _LogCapture:
    """Text stream recording what a foreground plugin writes and mirroring it.

    Installed as both ``sys.stdout`` and ``sys.stderr`` for the worker's life,
    so foreground ``print``, ``sys.stdout.write`` and ``context.log`` all land
    in the active response frame. Host-managed background tasks are not tied to
    a later request; their output is mirrored to stderr but excluded from the
    foreground frame, avoiding cross-request log attribution.
    """

    def __init__(self, mirror):
        self._mirror = mirror
        self._lock = threading.RLock()
        self._lines = []
        self._partial = []
        self._partial_bytes = 0
        self._truncated = False
        self._dropped = 0
        self._mirror_bytes = 0

    # -- stream protocol ---------------------------------------------------
    def write(self, text):
        with self._lock, _DIAGNOSTIC_LOCK:
            return self._write_unlocked(text)

    def _write_unlocked(self, text):
        if _SENSITIVE:
            return len(text) if isinstance(text, str) else 0
        if not isinstance(text, str):
            text = str(text)
        self._mirror_unlocked(text)
        if not _CAPTURE_ACTIVE.get():
            return len(text)
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
    def _mirror_unlocked(self, text):
        if _SENSITIVE:
            return
        """Mirrors a bounded prefix without allowing stderr to grow forever."""
        room = _MAX_MIRROR_BYTES - self._mirror_bytes
        if room <= 0 or not text:
            return
        candidate = text[: min(len(text), _MAX_LOG_LINE_BYTES)]
        encoded = candidate.encode("utf-8", "backslashreplace")
        if len(encoded) > room:
            encoded = encoded[:room].decode("utf-8", "ignore").encode("utf-8")
        if not encoded:
            return
        try:
            self._mirror.write(encoded.decode("utf-8"))
        except (OSError, ValueError):
            pass
        self._mirror_bytes += len(encoded)
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
        if self._truncated:
            # This logical line already lost its tail at the byte cap; refuse
            # further appends until _commit resets the flag, so a later write
            # cannot splice across the dropped gap (mirrors the room<=0 case).
            return
        room = _MAX_LOG_LINE_BYTES - self._partial_bytes
        if room <= 0:
            self._truncated = True
            return
        # Bound the temporary encoding allocation by the remaining character
        # room. A plugin can hand us a multi-gigabyte string, but clipping must
        # still inspect at most one bounded prefix.
        candidate = fragment if len(fragment) <= room else fragment[:room]
        encoded = candidate.encode("utf-8", "backslashreplace")
        if len(encoded) <= room:
            self._partial.append(candidate)
            self._partial_bytes += len(encoded)
            if len(candidate) < len(fragment):
                self._truncated = True
            return
        self._truncated = True
        clipped = encoded[:room].decode("utf-8", "ignore")
        self._partial.append(clipped)
        self._partial_bytes += len(clipped.encode("utf-8"))

    def _commit(self):
        line = "".join(self._partial)
        if self._truncated:
            line += _LOG_TRUNCATION_MARKER
        self._partial = []
        self._partial_bytes = 0
        self._truncated = False
        if len(self._lines) < _MAX_LOG_LINES:
            self._lines.append(line)
        else:
            self._dropped += 1

    def line(self, message):
        """Records one message; suppress before conversion or concatenation."""
        with self._lock, _DIAGNOSTIC_LOCK:
            if _SENSITIVE:
                return
            message = str(message)
            self._write_unlocked(message if message.endswith("\n") else message + "\n")

    def reset(self):
        """Starts a fresh per-request record, discarding anything pending."""
        with self._lock:
            self._lines = []
            self._partial = []
            self._partial_bytes = 0
            self._truncated = False
            self._dropped = 0
            self._mirror_bytes = 0

    def take(self):
        """The lines written since :meth:`reset`, and starts a new record.

        A trailing fragment with no newline is committed as its own line: a
        plugin's ``sys.stdout.write("no newline")`` is output it produced and
        wants to see, and holding it back would attribute it to a later request.
        """
        with self._lock:
            if _SENSITIVE:
                return []
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
            self._mirror_bytes = 0
            return lines


#: The single capture, installed before the plugin module is imported.
_CAPTURE = _LogCapture(_REAL_STDERR)
sys.stdout = _CAPTURE
sys.stderr = _CAPTURE
sys.__stdout__ = _CAPTURE
sys.__stderr__ = _CAPTURE

# QueueListener and already queued LogRecords must not reach Handler.format
# after sensitivity changes, regardless of contextvars or logger admission.
_ORIGINAL_LOG_HANDLE = logging.Handler.handle
def _diagnostic_log_handle(handler, record):
    with _CAPTURE._lock, _DIAGNOSTIC_LOCK:
        if _SENSITIVE:
            return False
        return _ORIGINAL_LOG_HANDLE(handler, record)
logging.Handler.handle = _diagnostic_log_handle


# --------------------------------------------------------------------------
# Item translation (contract §2)
# --------------------------------------------------------------------------


def _item_to_wire(item):
    """One :class:`crikey_sdk.Item` as a §2 JSON-ready dict.

    The plugin supplies its own ``stable_id`` (spec 10.2); ``plugin_id`` is the
    host's to assign and is never sent. ``argument_policy`` and ``hit_policy``
    are part of the item contract and are carried with conservative defaults
    when an older plugin omits them.
    """
    actions = []
    for action in getattr(item, "actions", None) or ():
        actions.append(
            {
                "action_id": getattr(action, "action_id", ""),
                "label": getattr(action, "label", ""),
                "description": getattr(action, "description", "") or "",
                "icon_reference": getattr(action, "icon_reference", None),
                "applicable_categories": [
                    str(c) for c in getattr(action, "applicable_categories", None) or ()
                ],
                "execution_policy": getattr(action, "execution_policy", "host-mediated")
                or "host-mediated",
            }
        )
    metadata = {}
    for key, value in dict(getattr(item, "metadata", None) or {}).items():
        metadata[str(key)] = value if isinstance(value, str) else str(value)
    if getattr(item, "preview_deferred", False):
        metadata["crikey.preview"] = "deferred"
    return {
        "stable_id": getattr(item, "stable_id", ""),
        "label": getattr(item, "label", ""),
        "description": getattr(item, "description", "") or "",
        "target": getattr(item, "target", "") or "",
        "category": getattr(item, "category", "plugin-defined") or "plugin-defined",
        "search_terms": [str(t) for t in getattr(item, "search_terms", None) or ()],
        "icon_reference": getattr(item, "icon_reference", None),
        "score_hint": int(getattr(item, "score_hint", 0) or 0),
        "metadata": metadata,
        "actions": actions,
        "argument_policy": getattr(item, "argument_policy", "forbidden") or "forbidden",
        "hit_policy": getattr(item, "hit_policy", "recorded") or "recorded",
    }


def _item_from_wire(payload):
    """Builds an :class:`crikey_sdk.Item`, rejecting malformed wire shapes."""
    if not isinstance(payload, dict):
        raise TypeError("execute item must be a JSON object")
    raw_actions = payload.get("actions") or []
    if not isinstance(raw_actions, list) or any(
        not isinstance(action, dict) for action in raw_actions
    ):
        raise TypeError("execute item actions must be an array of objects")
    raw_metadata = payload.get("metadata") or {}
    if not isinstance(raw_metadata, dict):
        raise TypeError("execute item metadata must be a JSON object")
    raw_search_terms = payload.get("search_terms") or []
    if not isinstance(raw_search_terms, list):
        raise TypeError("execute item search_terms must be an array")
    actions = [
        Action(
            action_id=action.get("action_id", ""),
            label=action.get("label", ""),
            description=action.get("description", "") or "",
            icon_reference=action.get("icon_reference"),
            applicable_categories=tuple(
                str(category) for category in (action.get("applicable_categories") or ())
            ),
            execution_policy=action.get("execution_policy", "host-mediated")
            or "host-mediated",
        )
        for action in raw_actions
    ]
    return Item(
        stable_id=payload.get("stable_id", ""),
        label=payload.get("label", ""),
        target=payload.get("target", "") or "",
        category=payload.get("category", "plugin-defined") or "plugin-defined",
        description=payload.get("description", "") or "",
        icon_reference=payload.get("icon_reference"),
        score_hint=int(payload.get("score_hint", 0) or 0),
        search_terms=[str(term) for term in raw_search_terms],
        metadata={
            str(key): (value if isinstance(value, str) else str(value))
            for key, value in raw_metadata.items()
        },
        actions=actions,
        argument_policy=payload.get("argument_policy", "forbidden") or "forbidden",
        hit_policy=payload.get("hit_policy", "recorded") or "recorded",
        preview_deferred=raw_metadata.get("crikey.preview") == "deferred",
    )



def _generation_from_wire(frame):
    """Returns a protocol generation, rejecting values outside host ``u64``."""
    generation = frame.get("generation")
    if (
        isinstance(generation, bool)
        or not isinstance(generation, int)
        or generation < 0
        or generation > (1 << 64) - 1
    ):
        raise ValueError("suggest generation must be an unsigned 64-bit integer")
    return generation

# --------------------------------------------------------------------------
# Frames
# --------------------------------------------------------------------------

# Background completion frames can be emitted from the host-managed asyncio
# loop while a foreground callback is writing its result. Serialise writes so
# one JSON line can never be interleaved with another.
_EMIT_LOCK = threading.Lock()


class _FrameTooLarge(ValueError):
    pass


class _PublicationFailed(Exception):
    """A fixed terminal has already been sent; stop the producing callback."""


def _frame_size(frame):
    """Count the complete ensure_ascii JSON line before allocating its encoding."""
    total = 1  # terminating newline
    def charge(size):
        nonlocal total
        total += size
        if total > _MAX_FRAME_BYTES:
            raise _FrameTooLarge("protocol frame exceeds bound")
    def visit(value, depth=0):
        if depth > 64:
            raise ValueError("protocol frame exceeds depth bound")
        if isinstance(value, str):
            charge(2)
            for char in str.__iter__(value):
                code = ord(char)
                charge(2 if char in '"\\\b\f\n\r\t' else 6 if code < 32 or 127 <= code <= 65535 else 12 if code > 65535 else 1)
        elif value is None:
            charge(4)
        elif isinstance(value, bool):
            charge(4 if value else 5)
        elif isinstance(value, int):
            charge(len(int.__repr__(value)))
        elif isinstance(value, float):
            charge(len(json.dumps(value)))
        elif isinstance(value, dict):
            charge(2 + max(0, len(value) - 1))
            for key, entry in value.items():
                if not isinstance(key, str):
                    raise TypeError("protocol object keys must be strings")
                visit(key, depth + 1)
                charge(1)
                visit(entry, depth + 1)
        elif isinstance(value, (list, tuple)):
            charge(2 + max(0, len(value) - 1))
            for entry in value:
                visit(entry, depth + 1)
        else:
            raise TypeError("unsupported protocol member")
    visit(frame)
    return total


def _publication_failure(frame):
    error = {"message": "publication exceeds protocol bounds", "traceback": ""}
    kind = frame.get("kind")
    if kind == "catalog_batch":
        return dict(id=frame.get("id"), kind=kind, items=[], done=True, log=[], error=error)
    if kind == "result_batch":
        return dict(id=frame.get("id"), kind=kind, generation=frame.get("generation"),
                    state="failed", items=[], log=[], error=error)
    if kind == "typed_execute_result":
        return dict(id=frame.get("id"), kind=kind, status="failed", code="failed")
    if kind == KIND_BACKGROUND_COMPLETE:
        return dict(kind=kind, task_id=frame.get("task_id"), status="failed", error=error)
    return dict(id=frame.get("id"), kind=kind, status="failed", log=[], error=error)


def _encode(frame):
    """One frame as its wire line.

    ``ensure_ascii=True`` keeps every frame representable on the strict UTF-8
    protocol stream, including plugin strings containing lone surrogates.
    JSON escaping makes a label containing a newline safe on a line-delimited
    channel. A bounded grammar/size pass precedes allocation of the complete
    encoded line; unsupported members produce a fixed terminal failure.
    """
    if _SENSITIVE:
        frame = {key: ([] if key == "log" else {"message": "plugin callback failed", "traceback": ""} if key == "error" and value is not None else "plugin callback failed" if key == "detail" else value) for key, value in frame.items()}
    _frame_size(frame)
    return json.dumps(frame, ensure_ascii=True, separators=(",", ":")) + "\n"


def _emit(frame):
    """Writes exactly one line to the protocol channel.

    A malformed or oversized publication produces one bounded fixed failure,
    never a success with dropped rows or an oversized line that kills the host.
    Return whether the original frame was actually published successfully.
    """
    try:
        line = _encode(frame)
    except Exception:
        _emit_line(_encode(_publication_failure(frame)))
        return False
    return _emit_line(line)


def _emit_line(line):
    """Writes an already encoded line without serializing it a second time."""
    with _EMIT_LOCK:
        try:
            _PROTOCOL.write(line)
            _PROTOCOL.flush()
            return True
        except (BrokenPipeError, OSError, ValueError):
            # The host can close stdout first during orderly shutdown.
            return False


# --------------------------------------------------------------------------
# Loading the plugin
# --------------------------------------------------------------------------


def _load_plugin(entrypoint):
    """Imports ``module:Class`` off the host-assembled path and instantiates it.

    ``sys.path`` is whatever the interpreter built from ``PYTHONPATH`` (plugin
    source, packaged modules, managed env, SDK); ``site`` was never added under
    ``-S``. No path munging happens here -- the host owns the import path.

    Called at startup, BEFORE the handshake is acknowledged: a missing module,
    a missing entrypoint class or an import-time raise is therefore a spawn-time
    failure (the host's ``ModernWorker::spawn`` returns ``Err``), never a worker
    that only fails on its first request.
    """
    module_name, _, class_name = entrypoint.partition(":")
    module_name = module_name.strip()
    class_name = class_name.strip()
    if not module_name or not class_name:
        raise ValueError(
            "CRIKEY_MODERN_ENTRYPOINT must be 'module:Class', got {!r}".format(entrypoint)
        )
    module = importlib.import_module(module_name)
    try:
        plugin_class = getattr(module, class_name)
    except AttributeError as error:
        raise ImportError(
            "entrypoint class {!r} not found in module {!r}".format(class_name, module_name)
        ) from error
    return plugin_class()


class _BackgroundTask:
    __slots__ = (
        "task_id",
        "coro",
        "wrapper",
        "decision",
        "refusal_reason",
        "admission_event",
        "future",
        "cancel_requested",
        "started",
        "completion_reported",
    )

    def __init__(self, task_id, coro):
        self.task_id = task_id
        self.coro = coro
        self.wrapper = None
        # ``None`` means the Rust host has not answered registration yet;
        # ``True``/``False`` are the sole start/refuse decisions.
        self.decision = None
        # The host's own words for a refusal, so a plugin author reading the
        # worker's stderr learns WHY a spawn never ran instead of watching it
        # silently not happen.
        self.refusal_reason = None
        self.admission_event = None
        self.future = None
        self.cancel_requested = False
        self.started = False
        self.completion_reported = False


# --------------------------------------------------------------------------
# The worker
# --------------------------------------------------------------------------


class _Worker:
    """Owns the plugin, foreground callback loop and background task loop."""

    def __init__(self, plugin):
        self._plugin = plugin
        self._catalog_schemas = {}
        self._query_schemas = {}
        self._pending_presentation = None
        self._pending_schemas = None
        self._legacy_typed_targets = set()
        # The cancel flag is set/cleared ONLY by the control-reader thread on a
        # `set_cancel` frame and read LIVE by SuggestContext.cancelled.
        self._cancel = threading.Event()
        self._loop = asyncio.new_event_loop()
        self._loop.set_exception_handler(_loop_exception)
        asyncio.set_event_loop(self._loop)

        # Background coroutines run on a continuously serviced loop in a
        # separate thread. Foreground callback execution therefore never waits
        # for a slow admitted task, while registration/admission remains on the
        # same process boundary as the callback that requested it.
        self._background_loop = asyncio.new_event_loop()
        self._background_loop.set_exception_handler(_loop_exception)
        self._background_tasks = {}
        self._background_lock = threading.Lock()
        self._next_background_id = 0
        self._background_thread = threading.Thread(
            target=self._run_background_loop,
            name="crikey-modern-background",
            daemon=True,
        )
        self._background_thread.start()

    def _run_background_loop(self):
        asyncio.set_event_loop(self._background_loop)
        self._background_loop.run_forever()
        pending = asyncio.all_tasks(self._background_loop)
        for task in pending:
            task.cancel()
        if pending:
            self._background_loop.run_until_complete(
                asyncio.gather(*pending, return_exceptions=True)
            )
        self._background_loop.close()

    # -- control ----------------------------------------------------------

    def set_cancel(self, cancelled):
        if cancelled:
            self._cancel.set()
        else:
            self._cancel.clear()

    def handle_background_control(self, frame):
        kind = frame.get("kind")
        if kind in (KIND_BACKGROUND_ADMIT, KIND_BACKGROUND_REFUSE):
            task_id = frame.get("task_id")
            if not isinstance(task_id, int) or isinstance(task_id, bool):
                return
            with self._background_lock:
                task = self._background_tasks.get(task_id)
                if task is None:
                    return
                task.decision = kind == KIND_BACKGROUND_ADMIT
                if not task.decision:
                    reason = frame.get("reason")
                    task.refusal_reason = reason if isinstance(reason, str) else None
                event = task.admission_event
            if event is not None:
                self._background_loop.call_soon_threadsafe(event.set)
            return

        if kind != KIND_BACKGROUND_CANCEL:
            return
        task_id = frame.get("task_id")
        with self._background_lock:
            tasks = (
                list(self._background_tasks.values())
                if task_id is None
                else [self._background_tasks.get(task_id)]
            )
            for task in tasks:
                if task is not None:
                    task.cancel_requested = True
        for task in tasks:
            if task is not None and task.future is not None:
                task.future.cancel()

    def _register_background(self, coro):
        with self._background_lock:
            self._next_background_id += 1
            task_id = self._next_background_id
            task = _BackgroundTask(task_id, coro)
            self._background_tasks[task_id] = task
        # Registration is emitted before scheduling the coroutine. The wrapper
        # cannot pass its admission gate until the Rust host answers.
        _emit({"kind": KIND_BACKGROUND_REGISTER, "task_id": task_id})
        task.wrapper = self._run_background(task)
        try:
            task.future = asyncio.run_coroutine_threadsafe(
                task.wrapper,
                self._background_loop,
            )
        except BaseException:
            task.coro.close()
            with self._background_lock:
                self._background_tasks.pop(task.task_id, None)
            raise
        task.future.add_done_callback(
            lambda future: self._background_future_done(task, future)
        )
        with self._background_lock:
            if task.cancel_requested:
                task.future.cancel()
        return task.future

    def _background_future_done(self, task, future):
        if not future.cancelled():
            return
        with self._background_lock:
            started = task.started
            task.cancel_requested = True
        if not started:
            task.coro.close()
        self._complete_background(task, "cancelled")

    def _complete_background(self, task, status, error=None):
        with self._background_lock:
            if task.completion_reported:
                return
            task.completion_reported = True
            self._background_tasks.pop(task.task_id, None)
        frame = {
            "kind": KIND_BACKGROUND_COMPLETE,
            "task_id": task.task_id,
            "status": status,
        }
        if error is not None:
            frame["error"] = error
        _emit(frame)

    async def _run_background(self, task):
        current = asyncio.current_task()
        with self._background_lock:
            task.started = True
        status = "ok"
        error = None
        capture_token = _CAPTURE_ACTIVE.set(False)
        try:
            task.admission_event = asyncio.Event()
            with self._background_lock:
                admitted = task.decision
            if admitted is None:
                await task.admission_event.wait()
                with self._background_lock:
                    admitted = task.decision
            if not admitted:
                status = "refused"
                with self._background_lock:
                    reason = task.refusal_reason
                reason = reason or "the host refused this background task"
                error = {"message": reason, "traceback": ""}
                # The refusal decision belongs to the host; all this side can
                # do is close the coroutine it will never run and make the
                # host's reason visible to whoever wrote the plugin.
                _stderr(
                    "[warn][crikey] background task {} was refused by the host: {}\n".format(
                        task.task_id, reason
                    )
                )
                task.coro.close()
            else:
                await task.coro
        except asyncio.CancelledError:
            status = "cancelled"
        except Exception as caught:  # noqa: BLE001 -- plugin task boundary
            status = "failed"
            error = _plugin_error(caught)
        finally:
            with self._background_lock:
                managed = {
                    entry.wrapper
                    for entry in self._background_tasks.values()
                    if entry.wrapper is not None
                }
            leftovers = [
                pending
                for pending in asyncio.all_tasks(self._background_loop)
                if (
                    pending is not current
                    and pending.get_coro() not in managed
                    and not pending.done()
                )
            ]
            if leftovers:
                for pending in leftovers:
                    pending.cancel()
                await asyncio.gather(*leftovers, return_exceptions=True)
            self._complete_background(task, status, error)
            _CAPTURE_ACTIVE.reset(capture_token)


    def stop_background(self):
        with self._background_lock:
            tasks = list(self._background_tasks.values())
        for task in tasks:
            if task.future is not None:
                task.future.cancel()
        self._background_loop.call_soon_threadsafe(self._background_loop.stop)
        self._background_thread.join(timeout=1.0)

    def shutdown(self):
        """Runs plugin shutdown once, then closes both worker event loops."""
        try:
            callback = getattr(self._plugin, "stop", None)
            if callable(callback):
                self.run_lifecycle(callback)
        except Exception as error:  # noqa: BLE001 -- shutdown is best effort
            _report_exception("[err][crikey] modern plugin stop failed", error)
        finally:
            self.stop_background()
            self._loop.close()

    # -- dispatch ---------------------------------------------------------

    def serve(self, frame):
        kind = frame.get("kind")
        if kind == "handshake":
            self._handshake(frame)
        elif kind == "suggest":
            self._suggest(frame)
        elif kind == "build_catalog":
            self._build_catalog(frame)
        elif kind == "commands":
            self._commands(frame)
        elif kind == "execute":
            self._execute(frame)
        elif kind == "typed_execute":
            self._typed_execute(frame)
        elif kind == "preview":
            self._preview(frame)
        elif kind == _KIND_CONFIGURATION:
            self._configuration(frame)
        else:
            # Not a request this protocol version defines. Do not answer (a
            # stray reply would desync); note it for a watching developer.
            _stderr("[err][crikey] ignored unknown frame kind {!r}\n".format(kind))

    def _handshake(self, frame):
        _emit(
            {
                "id": frame.get("id"),
                "kind": "handshake_ack",
                "protocol_version": _PROTOCOL_VERSION,
                "capabilities": [
                    "suggest", "build_catalog", "execute",
                    "background-tasks", "configuration",
                ] + (["preview_v1"] if self._supports_preview() else [])
                  + (["commands_v1"] if self._supports_commands() else [])
                  + (["typed_arguments_v1"] if self._supports_typed() else [])
                  + (["result_presentation_v1"] if self._supports_presentation() else []),
            }
        )

    def _supports_typed(self):
        callback = getattr(type(self._plugin), "execute_typed", None)
        return callable(callback) and callback is not Plugin.execute_typed

    def _supports_presentation(self):
        callback = getattr(self._plugin, "supports_result_presentation", None)
        return callable(callback) and callback() is True

    def _begin_publication(self, catalog):
        if catalog:
            self._catalog_schemas = {}
        else:
            self._query_schemas = {}
        self._pending_presentation = None
        self._pending_schemas = None

    def _finish_publication(self, catalog, successful):
        if successful and self._pending_schemas is not None:
            if catalog:
                self._catalog_schemas = self._pending_schemas
            else:
                self._query_schemas = self._pending_schemas
        self._pending_presentation = None
        self._pending_schemas = None

    def _prepare_presentation(self, query):
        self._pending_presentation = None
        callback = getattr(self._plugin, "catalog_presentation" if query is None else "suggestion_presentation", None)
        if not callable(callback):
            return
        result = callback() if query is None else callback(query)
        if inspect.isawaitable(result):
            result = self._loop.run_until_complete(result)
        if not isinstance(result, ResultPresentation):
            raise TypeError("metadata callback must return ResultPresentation")
        if (len(result.sections) > 128 or len(result.scopes) > 128
            or len(result.items) > 4096 or len(result.schemas) > 4096):
            raise ValueError("presentation exceeds bounds")
        if not (result.sections or result.scopes or result.items or result.schemas):
            return
        wire_bytes = 128
        def admit(value):
            nonlocal wire_bytes
            for chunk in json.JSONEncoder(ensure_ascii=True, separators=(",", ":")).iterencode(value):
                wire_bytes += len(chunk)
                if wire_bytes > _MAX_FRAME_BYTES:
                    raise ValueError("presentation exceeds frame allocation bound")
            return value
        from crikey_sdk.arguments import _text
        def declarations(entries):
            seen, out = set(), []
            for entry in entries:
                _text(entry.id, 256, True); _text(entry.label)
                if entry.id in seen:
                    raise ValueError("duplicate presentation identity")
                seen.add(entry.id)
                out.append(admit(dict(id=entry.id, label=entry.label)))
            return out
        sections, scopes = declarations(result.sections), declarations(result.scopes)
        section_ids = {s["id"] for s in sections}
        scope_ids = {s["id"] for s in scopes}
        memberships, seen = [], set()
        for entry in result.items:
            _text(entry.item_id, 256, True)
            if entry.item_id in seen or entry.section is not None and entry.section not in section_ids:
                raise ValueError("invalid item membership")
            if len(entry.scopes) > 128 or len(set(entry.scopes)) != len(entry.scopes) or not set(entry.scopes) <= scope_ids:
                raise ValueError("invalid scope membership")
            seen.add(entry.item_id)
            memberships.append(admit(dict(item_id=entry.item_id, section=entry.section, scopes=list(entry.scopes))))
        schemas, bindings = [], {}
        for binding in result.schemas:
            _text(binding.item_id, 256, True)
            if binding.action_id is not None:
                _text(binding.action_id, 256, True)
            key = (binding.item_id, binding.action_id)
            if key in bindings:
                raise ValueError("duplicate executable schema")
            wire = binding.schema.to_wire()
            fingerprint = binding.schema.fingerprint()
            bindings[key] = (binding.schema, fingerprint)
            schemas.append(admit(dict(item_id=binding.item_id, action_id=binding.action_id, schema=wire)))
        other = self._query_schemas if query is None else self._catalog_schemas
        if len(other) + len(bindings) > 4096:
            raise ValueError("schema registry exceeds bound")
        new_targets = set(bindings).difference(self._legacy_typed_targets)
        if len(self._legacy_typed_targets) + len(new_targets) > 4096:
            raise ValueError("legacy typed refusal registry exceeds bound")
        self._legacy_typed_targets.update(new_targets)
        self._pending_schemas = bindings
        self._pending_presentation = dict(sections=sections, scopes=scopes,
                                          items=memberships, schemas=schemas)

    def _typed_execute(self, frame):
        _latch_sensitive()
        reply = dict(id=frame.get("id"), kind="typed_execute_result",
                     status="failed", code="failed")
        values = TypedValues({})
        try:
            if getattr(frame, "typed_duplicate", False):
                pass
            elif not self._supports_typed():
                reply["code"] = "unsupported"
            elif "argument" in frame:
                pass
            else:
                item = _item_from_wire(frame.get("item"))
                action = frame.get("action_id")
                if action is not None and not isinstance(action, str):
                    raise ValueError("invalid action")
                expected = frame.get("schema_fingerprint")
                if (not isinstance(expected, str) or len(expected) != 64
                    or any(c not in "0123456789abcdef" for c in expected)):
                    raise ValueError("invalid fingerprint")
                binding = None
                key = (item.stable_id, action)
                for registry in (self._query_schemas, self._catalog_schemas):
                    candidate = registry.get(key)
                    if candidate is not None and candidate[1] == expected and candidate[0].fingerprint() == expected:
                        binding = candidate
                        break
                if binding is None:
                    reply["code"] = "unavailable"
                else:
                    schema = binding[0]
                    entries = frame.get("values")
                    if not isinstance(entries, list) or len(entries) > 64:
                        raise ValueError("invalid typed values")
                    declared = {field["id"]: field for field in schema.to_wire()["fields"]}
                    mapped = {}
                    for entry in entries:
                        if not isinstance(entry, dict) or getattr(entry, "typed_duplicate", False) or set(entry) != {"field_id", "kind", "value"}:
                            raise ValueError("invalid typed value")
                        field_id, kind, text = entry["field_id"], entry["kind"], entry["value"]
                        field = declared.get(field_id)
                        if field is None or field_id in mapped or kind != field["kind"] or not isinstance(text, str):
                            raise ValueError("unknown, duplicate or mismatched field")
                        size = len(text.encode("utf-8", "strict"))
                        if size > 8192:
                            raise ValueError("oversized typed value")
                        if kind == "dropdown":
                            if text not in {option["id"] for option in field["options"]}:
                                raise ValueError("invalid selection")
                        elif size > field["max_bytes"] or (size and size < field["min_bytes"]):
                            raise ValueError("invalid length")
                        if field["required"] and not text:
                            raise ValueError("required value")
                        mapped[field_id] = TypedValue(field_id, kind, text)
                    if any(field["required"] and field["id"] not in mapped for field in declared.values()):
                        raise ValueError("missing required field")
                    values = TypedValues(mapped)
                    result = (TypedOutcome(code="cancelled") if self._cancel.is_set()
                              else self._plugin.execute_typed(item, action, values))
                    if inspect.isawaitable(result):
                        context = WorkerContext(self._cancel.is_set, lambda _item: None,
                                                _CAPTURE.line, self._loop, self._register_background)
                        result = self._loop.run_until_complete(self._drain(context, result))
                    if self._cancel.is_set():
                        reply["code"] = "cancelled"
                    elif result is None:
                        reply = dict(id=frame.get("id"), kind="typed_execute_result", status="completed")
                    elif isinstance(result, TypedOutcome):
                        if result.code is not None:
                            if result.code not in ("unavailable", "denied", "cancelled", "timeout", "unsupported", "failed") or result.failures:
                                raise ValueError("invalid typed failure")
                            reply["code"] = result.code
                        elif result.failures:
                            if len(result.failures) > 64:
                                raise ValueError("too many field failures")
                            seen, failures = set(), []
                            for failure in result.failures:
                                if failure.field_id not in declared or failure.field_id in seen or failure.code not in ("required", "too_short", "too_long", "invalid_selection", "rejected"):
                                    raise ValueError("invalid field failure")
                                seen.add(failure.field_id)
                                failures.append(dict(field_id=failure.field_id, code=failure.code))
                            reply = dict(id=frame.get("id"), kind="typed_execute_result", status="field_failures", failures=failures)
                        else:
                            reply = dict(id=frame.get("id"), kind="typed_execute_result", status="completed")
                    else:
                        raise TypeError("invalid typed outcome")
        except BaseException:
            reply = dict(id=frame.get("id"), kind="typed_execute_result", status="failed", code="failed")
        finally:
            values.clear()
            frame.pop("values", None)
        _emit(reply)

    def _supports_commands(self):
        callback = getattr(type(self._plugin), "commands", None)
        return callable(callback) and callback is not Plugin.commands

    def _commands(self, frame):
        reply = {"id": frame.get("id"), "kind": "commands_result"}
        _CAPTURE.reset()
        try:
            if not self._supports_commands():
                reply["status"] = "unsupported"
            else:
                result = self._plugin.commands()
                if inspect.isawaitable(result):
                    result = self._loop.run_until_complete(self._drain(
                        WorkerContext(self._cancel.is_set, lambda _item: None,
                                      _CAPTURE.line, self._loop, self._register_background),
                        result,
                    ))
                declarations, seen, total = [], set(), 0
                for command in result:
                    if self._cancel.is_set():
                        reply["status"] = "cancelled"
                        break
                    if len(declarations) >= 10_000:
                        raise ValueError("commands exceed 10000 declarations")
                    if not isinstance(command, Command):
                        raise TypeError("commands must return Command declarations")
                    fields = (command.command_id, command.item_id)
                    if command.action_id is not None:
                        fields += (command.action_id,)
                    for value in fields:
                        if (not isinstance(value, str) or not value or len(value) > 256
                            or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in value)):
                            raise ValueError("invalid command target or identity")
                        size = len(value.encode("utf-8"))
                        if size > 256:
                            raise ValueError("command IDs exceed 256 bytes")
                        total += size
                    label = command.label
                    if not isinstance(label, str) or len(label) > 8192 or not label.strip():
                        raise ValueError("invalid command label")
                    size = len(label.encode("utf-8"))
                    if size > 8192:
                        raise ValueError("command label exceeds 8 KiB")
                    total += size
                    if command.argument_policy not in ("forbidden", "optional", "required"):
                        raise ValueError("invalid command argument policy")
                    if total > 4 * 1024 * 1024:
                        raise ValueError("command declarations exceed 4 MiB")
                    if command.command_id in seen:
                        raise ValueError("duplicate command identity")
                    seen.add(command.command_id)
                    declarations.append({
                        "command_id": command.command_id, "label": label,
                        "item_id": command.item_id, "action_id": command.action_id,
                        "argument_policy": command.argument_policy,
                    })
                else:
                    reply.update(status="cancelled" if self._cancel.is_set() else "content")
                    if reply["status"] == "content":
                        reply["declarations"] = declarations
            reply["log"] = _CAPTURE.take()
            chunks, wire_bytes = [], 1  # terminating newline
            for chunk in json.JSONEncoder(ensure_ascii=True, separators=(",", ":")).iterencode(reply):
                wire_bytes += len(chunk)  # ensure_ascii makes characters bytes
                if wire_bytes > _MAX_FRAME_BYTES:
                    raise ValueError("commands frame exceeds 8 MiB after JSON escaping")
                chunks.append(chunk)
            line = "".join(chunks) + "\n"
        except Exception:
            # Do not stringify arbitrary plugin exceptions or include unbounded
            # callback values in the failure frame.
            reply = {"id": frame.get("id"), "kind": "commands_result",
                     "status": "failed", "detail": "invalid or failed commands callback",
                     "log": []}
            line = _encode(reply)
        _emit_line(line)

    def _supports_preview(self):
        callback = getattr(type(self._plugin), "preview", None)
        return callable(callback) and callback is not Plugin.preview

    def _preview(self, frame):
        reply = {
            "id": frame.get("id"), "kind": "preview_result",
            "identity": frame.get("identity"), "status": "empty",
        }
        _CAPTURE.reset()
        try:
            if not self._supports_preview():
                reply["status"] = "unsupported"
            else:
                context = WorkerContext(
                    self._cancel.is_set, lambda _item: None, _CAPTURE.line,
                    self._loop, self._register_background,
                )
                result = self._plugin.preview(_item_from_wire(frame["item"]), context)
                if inspect.isawaitable(result):
                    result = self._loop.run_until_complete(self._drain(context, result))
                if self._cancel.is_set():
                    reply["status"] = "cancelled"
                elif result is not None:
                    if not isinstance(result, Preview):
                        raise TypeError("preview must return Preview or None")
                    total_bytes = 0 if result.text is None else len(result.text.encode("utf-8"))
                    if total_bytes > 256 * 1024:
                        raise ValueError("preview aggregate text exceeds 256 KiB")
                    if type(result.markdown) is not bool:
                        raise TypeError("preview markdown must be a boolean")
                    if len(result.values) > 128:
                        raise ValueError("preview exceeds 128 values")
                    values = []
                    for value in result.values:
                        if value.kind not in ("text", "link", "tag"):
                            raise ValueError("invalid preview value kind")
                        label_bytes = len(value.label.encode("utf-8"))
                        value_bytes = len(value.value.encode("utf-8"))
                        if max(label_bytes, value_bytes) > 8192:
                            raise ValueError("preview value exceeds 8 KiB")
                        total_bytes += label_bytes + value_bytes
                        if total_bytes > 256 * 1024:
                            raise ValueError("preview aggregate text exceeds 256 KiB")
                        if value.kind == "link" and (
                            not value.value.startswith(("https://", "http://"))
                            or any(ord(char) < 32 or 127 <= ord(char) <= 159 for char in value.value)
                        ):
                            raise ValueError("invalid preview link")
                        values.append({"label": value.label, "value": value.value, "kind": value.kind})
                    image = None
                    if result.image is not None:
                        img = result.image
                        if (not isinstance(img.rgba, bytes)
                            or type(img.width) is not int or type(img.height) is not int
                            or not 0 < img.width <= 1024 or not 0 < img.height <= 1024
                            or img.width * img.height * 4 > 1024 * 1024
                            or len(img.rgba) != img.width * img.height * 4):
                            raise ValueError("invalid or oversized preview RGBA image")
                        image = {"width": img.width, "height": img.height, "rgba": list(img.rgba)}
                    reply.update(status="content", content={
                        "text": result.text, "markdown": result.markdown,
                        "image": image, "values": values,
                    })
            reply["log"] = _CAPTURE.take()
            # Metadata nodes are bounded above by the 128-value limit. Markdown
            # nodes remain the host parser's responsibility; do not parse twice.
            # Preflight the complete escaped frame before allocating its encoding.
            line = _encode(reply)
        except Exception as error:
            reply.pop("content", None)
            detail = ("preview frame exceeds 8 MiB after JSON escaping"
                      if isinstance(error, _FrameTooLarge) and not _SENSITIVE
                      else _plugin_error(error)["message"][:1024])
            reply.update(status="failed", detail=detail)
            reply.setdefault("log", _CAPTURE.take())
            try:
                line = _encode(reply)
            except _FrameTooLarge:
                # The failed content has already gone, but escaped captured
                # logs can still exceed the line cap. Omit them explicitly.
                reply.update(log=[], detail=detail + " (oversized captured log omitted)")
                line = _encode(reply)
        _emit_line(line)

    def _configuration(self, frame):
        """Hands the latest complete configuration state to the plugin (spec 21.4).

        No reply, because the frame carries no ``id``: a stray reply would desync
        the channel. A plugin that does not implement the hook is not an error —
        every callback is optional — and a plugin that raises is contained here,
        reported on stderr, and left running: configuration delivery is not a
        request whose failure a caller is waiting on.

        A malformed frame is refused rather than partially applied. Delivering
        half a configuration would be indistinguishable, to the plugin, from the
        user having deleted the other half.
        """
        values = frame.get("values")
        if not isinstance(values, dict) or not all(
            isinstance(key, str) and isinstance(value, str) for key, value in values.items()
        ):
            _stderr("[err][crikey] configuration values must be a string map; ignored\n")
            return
        callback = getattr(self._plugin, "on_configuration", None)
        if not callable(callback):
            return
        _CAPTURE.reset()
        try:
            self.run_lifecycle(lambda: callback(dict(values)))
        except Exception as error:  # noqa: BLE001 -- a plugin bug is not ours
            _report_exception("[err][crikey] on_configuration raised", error)
        finally:
            for line in _CAPTURE.take():
                _stderr("{}\n".format(line))

    def _run_callback(self, context, result):
        """Runs sync or awaitable callback results on the worker's event loop."""
        if not inspect.isawaitable(result):
            # A sync callback may still have registered coroutines via spawn
            # (no running loop at call time); drive them now for completeness.
            if context.registered_tasks:
                self._loop.run_until_complete(self._drain(context, None))
            return
        self._loop.run_until_complete(self._drain(context, result))

    def run_lifecycle(self, callback):
        """Runs a lifecycle callback and supports both sync and async methods."""
        context = WorkerContext(
            self._cancel.is_set,
            lambda _item: None,
            _CAPTURE.line,
            self._loop,
            self._register_background,
        )
        self._run_callback(context, callback())

    async def _drain(self, context, coro):
        result = None
        try:
            if coro is not None:
                result = await coro
        finally:
            # Await everything registered via context.spawn to completion.
            registered = list(context.registered_tasks)
            if registered:
                await asyncio.gather(*registered, return_exceptions=True)
            # Any remaining pending task was created raw (not registered):
            # refuse to leave it running and report the cleanup.
            current = asyncio.current_task()
            registered_set = set(registered)
            leftover = [
                task
                for task in asyncio.all_tasks(self._loop)
                if task is not current and task not in registered_set and not task.done()
            ]
            if leftover:
                for task in leftover:
                    task.cancel()
                await asyncio.gather(*leftover, return_exceptions=True)
                context.log(
                    "[warn][crikey] cancelled {} unregistered pending background "
                    "task(s) at callback end; use context.spawn to register "
                    "background work".format(len(leftover))
                )
        return result

    def _suggest(self, frame):
        request_id = frame.get("id")
        self._begin_publication(False)
        _CAPTURE.reset()
        generation = 0
        buffer = []
        active = threading.Event()
        active.set()
        sink_lock = threading.Lock()

        def sink(item):
            if self._cancel.is_set():
                return
            with sink_lock:
                if not active.is_set() or self._cancel.is_set():
                    return
                buffer.append(_item_to_wire(item))
                if len(buffer) >= _FLUSH_THRESHOLD:
                    if not self._emit_batch(request_id, generation, "partial", buffer, []):
                        active.clear()
                        buffer.clear()
                        raise _PublicationFailed()
                    buffer.clear()

        try:
            generation = _generation_from_wire(frame)
            query = Query(
                text=frame.get("text", "") or "",
                normalized=frame.get("normalized", "") or "",
                generation=generation,
                selected_scope_id=frame.get("scope_id"),
            )
            context = WorkerContext(
                self._cancel.is_set,
                sink,
                _CAPTURE.line,
                self._loop,
                self._register_background,
            )
            self._prepare_presentation(query)
            result = self._plugin.suggest(query, context)
            self._run_callback(context, result)
        except _PublicationFailed:
            active.clear()
            return
        except Exception as error:  # noqa: BLE001 -- a plugin bug is not ours
            # Flush anything already streamed, then a terminal failed frame.
            with sink_lock:
                if not active.is_set():
                    return
                active.clear()
                if buffer:
                    if not self._emit_batch(request_id, generation, "partial", buffer, []):
                        buffer.clear()
                        return
                    buffer.clear()
                self._emit_batch(
                    request_id,
                    generation,
                    "failed",
                    [],
                    _CAPTURE.take(),
                    error=_plugin_error(error),
                )
            return

        if not active.is_set():
            return
        with sink_lock:
            active.clear()
            state = "cancelled" if self._cancel.is_set() else "final"
            items = list(buffer)
            buffer.clear()
            self._emit_batch(request_id, generation, state, items, _CAPTURE.take())

    def _emit_batch(self, request_id, generation, state, items, log, error=None):
        if state == "final" and self._cancel.is_set():
            state = "cancelled"
        presentation = self._pending_presentation if state == "final" else None
        sent = _emit({
            "id": request_id, "kind": "result_batch", "generation": generation,
            "state": state, "items": list(items),
            "log": [] if _SENSITIVE else list(log), "error": error,
            **({"presentation": presentation} if presentation is not None else {}),
        })
        if state != "partial" or not sent:
            self._finish_publication(False, sent and state == "final")
        return sent


    def _build_catalog(self, frame):
        request_id = frame.get("id")
        self._begin_publication(True)
        _CAPTURE.reset()
        buffer = []
        context = WorkerContext(
            self._cancel.is_set,
            lambda _item: None,
            _CAPTURE.line,
            self._loop,
            self._register_background,
        )

        try:
            self._prepare_presentation(None)
            produced = self._plugin.build_catalog()
            if inspect.isawaitable(produced):
                produced = self._loop.run_until_complete(
                    self._drain(context, produced)
                )
            # ``build_catalog`` returns an iterable of items rather than
            # emitting; feed each through the same wire translation.
            if produced is not None:
                for item in produced:
                    buffer.append(_item_to_wire(item))
                    if len(buffer) >= _FLUSH_THRESHOLD:
                        if not self._emit_catalog(request_id, buffer, False, []):
                            return
                        buffer.clear()
        except Exception as error:  # noqa: BLE001
            # A raise during catalog load is NOT an empty catalog: surface a
            # terminal frame carrying the fault so the host maps it to
            # HostError::PluginFailed rather than recording a silent empty
            # catalog (pinned decision 2, confirmed with the host codec).
            self._emit_catalog(
                request_id,
                [],
                True,
                _CAPTURE.take(),
                error=_plugin_error(error),
            )
            _report_exception("[err][crikey] build_catalog raised", error)
            return

        self._emit_catalog(request_id, buffer, True, _CAPTURE.take())

    def _emit_catalog(self, request_id, items, done, log, error=None):
        frame = {
            "id": request_id, "kind": "catalog_batch", "items": list(items),
            "done": done, "log": [] if _SENSITIVE else list(log),
        }
        if done and error is None and self._pending_presentation is not None:
            frame["presentation"] = self._pending_presentation
        if error is not None:
            frame["error"] = error
        sent = _emit(frame)
        if done or not sent:
            self._finish_publication(True, sent and done and error is None)
        return sent

    def _execute(self, frame):
        request_id = frame.get("id")
        _CAPTURE.reset()
        action_id = frame.get("action_id")
        argument = frame.get("argument")
        try:
            item = _item_from_wire(frame.get("item") or {})
            key = (item.stable_id, action_id)
            if key in self._legacy_typed_targets or (action_id is None and any(k[0] == item.stable_id for k in self._legacy_typed_targets)):
                raise ValueError("typed target refuses legacy execution")
            context = WorkerContext(
                self._cancel.is_set,
                lambda _item: None,
                _CAPTURE.line,
                self._loop,
                self._register_background,
            )
            result = self._plugin.execute(item, action_id, argument)
            self._run_callback(context, result)
        except Exception as error:  # noqa: BLE001
            _emit(
                {
                    "id": request_id,
                    "kind": "execute_result",
                    "status": "failed",
                    "log": _CAPTURE.take(),
                    "error": _plugin_error(error),
                }
            )
            return

        _emit(
            {
                "id": request_id,
                "kind": "execute_result",
                "status": "ok",
                "log": _CAPTURE.take(),
                "error": None,
            }
        )


# --------------------------------------------------------------------------
# stdin reader
# --------------------------------------------------------------------------


def _read_bounded_line(stream):
    """Reads at most one protocol frame, measured in UTF-8 bytes."""
    raw_stream = getattr(stream, "buffer", stream)
    line = raw_stream.readline(_MAX_FRAME_BYTES + 1)
    if isinstance(line, str):
        line = line.encode("utf-8", "strict")
    if not line:
        return None
    newline = line.find(b"\n")
    if newline == -1:
        if len(line) > _MAX_FRAME_BYTES:
            return _OVERSIZED
        payload = line
    else:
        if newline > _MAX_FRAME_BYTES:
            return _OVERSIZED
        payload = line[:newline]
    if payload.endswith(b"\r"):
        payload = payload[:-1]
    return payload


def _read_stdin(stream, pending, worker):
    """Reads bounded frames off stdin forever, on its own daemon thread.

    ``set_cancel`` is applied HERE, the moment it is parsed, so a plugin polling
    ``context.cancelled`` inside ``suggest`` observes cancellation while that
    callback is still on the stack. Everything else is queued for the main loop.
    """
    while True:
        try:
            payload = _read_bounded_line(stream)
        except (OSError, ValueError, UnicodeError):
            pending.put(_EOF)
            return
        if payload is None:
            pending.put(_EOF)
            return
        if payload is _OVERSIZED:
            _stderr(
                "[err][crikey] incoming frame exceeds {} bytes; worker exiting\n".format(
                    _MAX_FRAME_BYTES
                )
            )
            pending.put(_EOF)
            return
        if not payload.strip():
            continue

        try:
            frame = json.loads(payload.decode("utf-8", "strict"), object_pairs_hook=_WireObject)
        except (UnicodeDecodeError, ValueError):
            _stderr("[err][crikey] undecodable request line ignored\n")
            continue
        if not isinstance(frame, dict):
            _stderr("[err][crikey] request line was not an object; ignored\n")
            continue
        if frame.get("kind") == "typed_execute" or getattr(frame, "typed_tag", False):
            _latch_sensitive()
            frame["kind"] = "typed_execute"

        if frame.get("kind") == _KIND_SET_CANCEL:
            cancelled = frame.get("cancelled")
            if isinstance(cancelled, bool):
                worker.set_cancel(cancelled)
            else:
                _stderr("[err][crikey] set_cancel requires a boolean; ignored\n")
            continue
        if frame.get("kind") in (
            KIND_BACKGROUND_ADMIT,
            KIND_BACKGROUND_REFUSE,
            KIND_BACKGROUND_CANCEL,
        ):
            worker.handle_background_control(frame)
            continue

        pending.put(frame)

def main():
    """Runs the worker until shutdown, end of stdin, or process death."""
    # Negotiate against the shim's OWN supported version. The host announces the
    # version it wants via the env; if it does not match what this shim speaks,
    # refuse BEFORE any handshake ack so the host's spawn (which waits for the
    # ack) sees the child die and returns Err rather than a false agreement.
    requested = os.environ.get(ENV_PROTOCOL_VERSION)
    if requested is not None:
        try:
            requested_version = int(requested)
        except ValueError:
            requested_version = None
        if requested_version != _PROTOCOL_VERSION:
            _stderr(
                "[err][crikey] protocol version mismatch: host requested {!r}, "
                "worker speaks {}\n".format(requested, _PROTOCOL_VERSION)
            )
            return 1

    entrypoint = os.environ.get(ENV_ENTRYPOINT, "")
    worker = None
    try:
        plugin = _load_plugin(entrypoint)
        worker = _Worker(plugin)
        callback = getattr(plugin, "start", None)
        if callable(callback):
            _CAPTURE.reset()
            worker.run_lifecycle(callback)
            _CAPTURE.take()
    except BaseException as error:  # noqa: BLE001
        if worker is not None:
            worker.stop_background()
            worker._loop.close()
        _stderr(
            "[err][crikey] failed to load modern plugin from entrypoint {!r}: {}\n{}".format(
                entrypoint, error, traceback.format_exc()
            )
        )
        return 1

    pending = queue.Queue(maxsize=_PENDING_QUEUE_CAPACITY)
    reader = threading.Thread(
        target=_read_stdin,
        args=(sys.stdin, pending, worker),
        name="crikey-modern-stdin",
        daemon=True,
    )
    reader.start()

    while True:
        frame = pending.get()
        if frame is _EOF:
            worker.shutdown()
            return 0
        if frame.get("kind") == _KIND_SHUTDOWN:
            # No reply: the host drops stdin as it asks, and writing to a stdout
            # it has stopped reading risks a BrokenPipeError.
            worker.shutdown()
            return 0
        worker.serve(frame)


if __name__ == "__main__":
    sys.exit(main())
