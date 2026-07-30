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
import importlib
import json
import os
import queue
import sys
import threading
import traceback

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

#: Appended to a log line clipped at the per-line cap so a truncated line reads
#: as truncated instead of as the whole value (mirrors the legacy clamp marker).
_LOG_TRUNCATION_MARKER = " \u2026[log line truncated]"

#: Items buffered before an intermediate ``result_batch`` / ``catalog_batch`` is
#: flushed. Small on purpose: streaming beats a single fat frame.
_FLUSH_THRESHOLD = 32

ENV_PLUGIN_ID = "CRIKEY_MODERN_PLUGIN_ID"
ENV_ENTRYPOINT = "CRIKEY_MODERN_ENTRYPOINT"
ENV_PROTOCOL_VERSION = "CRIKEY_MODERN_PROTOCOL_VERSION"

#: Control frame: no ``id``, written from the host on a separate thread while a
#: call may be in flight (mirrors legacy ``set_terminate``).
_KIND_SET_CANCEL = "set_cancel"
_KIND_SHUTDOWN = "shutdown"

#: Sentinel the reader thread queues on end of stdin.
_EOF = object()


# --------------------------------------------------------------------------
# Per-request log capture
# --------------------------------------------------------------------------


class _LogCapture:
    """Text stream recording what a plugin writes and mirroring it onward.

    Installed as both ``sys.stdout`` and ``sys.stderr`` for the worker's life,
    so ``print``, ``sys.stdout.write`` and ``context.log`` all land here. The
    captured lines are carried INSIDE the response frame, making attribution to
    the answering request exact by construction (the host reads the reply off
    stdout and drains stderr on a separate thread, so a stderr line cannot be
    reliably attributed to a call).
    """

    def __init__(self, mirror):
        self._mirror = mirror
        self._lines = []
        self._partial = []
        self._partial_bytes = 0
        self._truncated = False
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
        if self._truncated:
            # This logical line already lost its tail at the byte cap; refuse
            # further appends until _commit resets the flag, so a later write
            # cannot splice across the dropped gap (mirrors the room<=0 case).
            return
        room = _MAX_LOG_LINE_BYTES - self._partial_bytes
        if room <= 0:
            self._truncated = True
            return
        # The cap is a BYTE bound, so clip the utf-8 encoding at a char
        # boundary within the remaining budget rather than by character count.
        encoded = fragment.encode("utf-8")
        if len(encoded) <= room:
            self._partial.append(fragment)
            self._partial_bytes += len(encoded)
            return
        self._truncated = True
        truncated = encoded[:room]
        while truncated:
            try:
                clipped = truncated.decode("utf-8")
            except UnicodeDecodeError:
                truncated = truncated[:-1]
                continue
            self._partial.append(clipped)
            self._partial_bytes += len(truncated)
            return

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
        """Records ``message`` as its own log line (used by ``context.log``)."""
        self.write(message if message.endswith("\n") else message + "\n")

    def reset(self):
        """Starts a fresh per-request record, discarding anything pending."""
        self._lines = []
        self._partial = []
        self._partial_bytes = 0
        self._truncated = False
        self._dropped = 0

    def take(self):
        """The lines written since :meth:`reset`, and starts a new record.

        A trailing fragment with no newline is committed as its own line: a
        plugin's ``sys.stdout.write("no newline")`` is output it produced and
        wants to see, and holding it back would attribute it to a later request.
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


#: The single capture, installed before the plugin module is imported.
_CAPTURE = _LogCapture(_REAL_STDERR)
sys.stdout = _CAPTURE
sys.stderr = _CAPTURE


# --------------------------------------------------------------------------
# Item translation (contract §2)
# --------------------------------------------------------------------------


def _item_to_wire(item):
    """One :class:`crikey_sdk.Item` as a §2 JSON-ready dict.

    The plugin supplies its own ``stable_id`` (spec 10.2); ``plugin_id`` is the
    host's to assign and is never sent. ``argument_policy``/``hit_policy`` are
    host-side defaults in M4 and are likewise absent.
    """
    actions = []
    for action in getattr(item, "actions", None) or ():
        actions.append(
            {
                "action_id": getattr(action, "action_id", ""),
                "label": getattr(action, "label", ""),
                "description": getattr(action, "description", "") or "",
                "icon_reference": getattr(action, "icon_reference", None),
            }
        )
    metadata = {}
    for key, value in dict(getattr(item, "metadata", None) or {}).items():
        metadata[str(key)] = value if isinstance(value, str) else str(value)
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
    }


def _item_from_wire(payload):
    """A :class:`crikey_sdk.Item` from a host ``execute`` item frame."""
    from crikey_sdk import Action, Item

    actions = [
        Action(
            action_id=a.get("action_id", ""),
            label=a.get("label", ""),
            description=a.get("description", "") or "",
            icon_reference=a.get("icon_reference"),
        )
        for a in (payload.get("actions") or [])
    ]
    return Item(
        stable_id=payload.get("stable_id", ""),
        label=payload.get("label", ""),
        target=payload.get("target", "") or "",
        category=payload.get("category", "plugin-defined") or "plugin-defined",
        description=payload.get("description", "") or "",
        icon_reference=payload.get("icon_reference"),
        score_hint=int(payload.get("score_hint", 0) or 0),
        search_terms=[str(t) for t in (payload.get("search_terms") or [])],
        metadata={
            str(k): (v if isinstance(v, str) else str(v))
            for k, v in (payload.get("metadata") or {}).items()
        },
        actions=actions,
    )


# --------------------------------------------------------------------------
# Frames
# --------------------------------------------------------------------------


def _encode(frame):
    """One frame as its wire line.

    ``ensure_ascii=False`` keeps non-ASCII labels readable; JSON escaping makes
    a label containing a newline safe on a line-delimited channel. ``default=
    repr`` guards a plugin that put an unserialisable object in metadata from
    costing the request its only response.
    """
    return json.dumps(frame, ensure_ascii=False, separators=(",", ":"), default=repr) + "\n"


def _emit(frame):
    """Writes exactly one line to the protocol channel.

    A frame that cannot be serialised is downgraded (items dropped, a marker
    added to the log) so a worker-side bug does not desync the stream. A frame
    that exceeds the §1 byte bound is NOT downgraded: it is written as-is and
    the host refuses the over-long line as a protocol error and stops the worker
    (contract §1). Legitimate large output stays under the bound by streaming
    many small result batches, never one oversized frame.
    """
    try:
        line = _encode(frame)
    except (TypeError, ValueError) as error:
        trimmed = dict(frame)
        trimmed["items"] = []
        trimmed["log"] = list(trimmed.get("log") or []) + [
            "[err][crikey] a reply frame could not be serialised: {}".format(error)
        ]
        line = _encode(trimmed)
    _PROTOCOL.write(line)
    _PROTOCOL.flush()


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


# --------------------------------------------------------------------------
# The worker
# --------------------------------------------------------------------------


class _Worker:
    """Owns the plugin, the asyncio loop and the cancellation flag."""

    def __init__(self, plugin):
        self._plugin = plugin
        # The cancel flag is set/cleared ONLY by the control-reader thread on a
        # `set_cancel` frame and read LIVE by SuggestContext.cancelled. It is
        # never cleared implicitly at request-start: a cancel that arrives just
        # before a callback begins must not be lost (host latches identically).
        self._cancel = threading.Event()
        # A per-worker asyncio loop (spec 15.8). Async callbacks are driven on it
        # via ``run_until_complete``; it is not otherwise running between calls,
        # so no task can progress once a callback and its cleanup have finished.
        self._loop = asyncio.new_event_loop()
        asyncio.set_event_loop(self._loop)

    # -- control ----------------------------------------------------------

    def set_cancel(self, cancelled):
        if cancelled:
            self._cancel.set()
        else:
            self._cancel.clear()

    # -- dispatch ---------------------------------------------------------

    def serve(self, frame):
        kind = frame.get("kind")
        if kind == "handshake":
            self._handshake(frame)
        elif kind == "suggest":
            self._suggest(frame)
        elif kind == "build_catalog":
            self._build_catalog(frame)
        elif kind == "execute":
            self._execute(frame)
        else:
            # Not a request this protocol version defines. Do not answer (a
            # stray reply would desync); note it for a watching developer.
            _REAL_STDERR.write("[err][crikey] ignored unknown frame kind {!r}\n".format(kind))
            _REAL_STDERR.flush()

    def _handshake(self, frame):
        _emit(
            {
                "id": frame.get("id"),
                "kind": "handshake_ack",
                "protocol_version": _PROTOCOL_VERSION,
                "capabilities": ["suggest", "build_catalog", "execute"],
            }
        )

    def _run_callback(self, context, result):
        """Runs a callback result: sync returns immediately; a coroutine is
        driven on the worker's loop, its registered tasks awaited and any
        un-registered pending task cancelled and reported (spec 15.8)."""
        if not asyncio.iscoroutine(result):
            # A sync callback may still have registered coroutines via spawn
            # (no running loop at call time); drive them now for completeness.
            if context.registered_tasks:
                self._loop.run_until_complete(self._drain(context, None))
            return
        self._loop.run_until_complete(self._drain(context, result))

    async def _drain(self, context, coro):
        if coro is not None:
            await coro
        # Await everything registered via context.spawn to completion.
        registered = list(context.registered_tasks)
        if registered:
            await asyncio.gather(*registered, return_exceptions=True)
        # Any remaining pending task was created raw (not registered): refuse to
        # leave it running -- cancel it and report it, never leak it into a later
        # request (spec 15.8 last sentence).
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

    def _suggest(self, frame):
        from crikey_sdk import Query, WorkerContext

        request_id = frame.get("id")
        _CAPTURE.reset()
        query = Query(
            text=frame.get("text", "") or "",
            normalized=frame.get("normalized", "") or "",
            generation=int(frame.get("generation", 0) or 0),
        )
        buffer = []

        def sink(item):
            buffer.append(_item_to_wire(item))
            if len(buffer) >= _FLUSH_THRESHOLD:
                self._emit_batch(request_id, "partial", buffer, [])
                buffer.clear()

        context = WorkerContext(self._cancel.is_set, sink, _CAPTURE.line, self._loop)

        try:
            result = self._plugin.suggest(query, context)
            self._run_callback(context, result)
        except BaseException as error:  # noqa: BLE001 -- a plugin bug is not ours
            # Flush anything already streamed, then a terminal failed frame.
            if buffer:
                self._emit_batch(request_id, "partial", buffer, [])
                buffer.clear()
            self._emit_batch(
                request_id,
                "failed",
                [],
                _CAPTURE.take(),
                error={"message": str(error), "traceback": traceback.format_exc()},
            )
            return

        state = "cancelled" if self._cancel.is_set() else "final"
        self._emit_batch(request_id, state, buffer, _CAPTURE.take())

    def _emit_batch(self, request_id, state, items, log, error=None):
        _emit(
            {
                "id": request_id,
                "kind": "result_batch",
                "state": state,
                "items": list(items),
                "log": list(log),
                "error": error,
            }
        )

    def _build_catalog(self, frame):
        from crikey_sdk import WorkerContext

        request_id = frame.get("id")
        _CAPTURE.reset()
        buffer = []

        def sink(item):
            buffer.append(_item_to_wire(item))

        context = WorkerContext(self._cancel.is_set, sink, _CAPTURE.line, self._loop)

        try:
            produced = self._plugin.build_catalog()
            # ``build_catalog`` returns an iterable of items rather than emitting;
            # feed each through the same wire translation.
            if produced is not None:
                for item in produced:
                    buffer.append(_item_to_wire(item))
                    if len(buffer) >= _FLUSH_THRESHOLD:
                        self._emit_catalog(request_id, buffer, False, [])
                        buffer.clear()
            # Also honour any items emitted through context.emit for symmetry.
            if context.registered_tasks:
                self._loop.run_until_complete(self._drain(context, None))
        except BaseException as error:  # noqa: BLE001
            # A raise during catalog load is NOT an empty catalog: surface a
            # terminal frame carrying the fault so the host maps it to
            # HostError::PluginFailed rather than recording a silent empty
            # catalog (pinned decision 2, confirmed with the host codec).
            self._emit_catalog(
                request_id,
                [],
                True,
                _CAPTURE.take(),
                error={"message": str(error), "traceback": traceback.format_exc()},
            )
            _REAL_STDERR.write(
                "[err][crikey] build_catalog raised: {}\n{}".format(error, traceback.format_exc())
            )
            _REAL_STDERR.flush()
            return

        self._emit_catalog(request_id, buffer, True, _CAPTURE.take())

    def _emit_catalog(self, request_id, items, done, log, error=None):
        frame = {
            "id": request_id,
            "kind": "catalog_batch",
            "items": list(items),
            "done": done,
            "log": list(log),
        }
        if error is not None:
            frame["error"] = error
        _emit(frame)

    def _execute(self, frame):
        from crikey_sdk import WorkerContext

        request_id = frame.get("id")
        _CAPTURE.reset()
        item = _item_from_wire(frame.get("item") or {})
        action_id = frame.get("action_id")
        argument = frame.get("argument")
        context = WorkerContext(self._cancel.is_set, lambda _item: None, _CAPTURE.line, self._loop)

        try:
            result = self._plugin.execute(item, action_id, argument)
            self._run_callback(context, result)
        except BaseException as error:  # noqa: BLE001
            _emit(
                {
                    "id": request_id,
                    "kind": "execute_result",
                    "status": "failed",
                    "log": _CAPTURE.take(),
                    "error": {"message": str(error), "traceback": traceback.format_exc()},
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


def _read_stdin(stream, pending, worker):
    """Reads frames off stdin forever, on its own daemon thread.

    ``set_cancel`` is applied HERE, the moment it is parsed, so a plugin polling
    ``context.cancelled`` inside ``suggest`` observes cancellation while that
    callback is still on the stack. Everything else is queued for the main loop.
    """
    while True:
        try:
            line = stream.readline()
        except (OSError, ValueError):
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

        if frame.get("kind") == _KIND_SET_CANCEL:
            worker.set_cancel(bool(frame.get("cancelled", True)))
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
            _REAL_STDERR.write(
                "[err][crikey] protocol version mismatch: host requested {!r}, "
                "worker speaks {}\n".format(requested, _PROTOCOL_VERSION)
            )
            _REAL_STDERR.flush()
            return 1

    entrypoint = os.environ.get(ENV_ENTRYPOINT, "")
    try:
        plugin = _load_plugin(entrypoint)
        if hasattr(plugin, "start"):
            plugin.start()
    except BaseException as error:  # noqa: BLE001
        # A plugin that cannot even load is a spawn-time failure: report it on
        # the real stderr (the host's crash tail) and exit non-zero. No frame is
        # written -- the handshake never happened, so there is no id to echo, and
        # the host's spawn sees the child die before the ack and returns Err.
        _REAL_STDERR.write(
            "[err][crikey] failed to load modern plugin from entrypoint {!r}: {}\n{}".format(
                entrypoint, error, traceback.format_exc()
            )
        )
        _REAL_STDERR.flush()
        return 1

    worker = _Worker(plugin)
    pending = queue.Queue()
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
            return 0
        if frame.get("kind") == _KIND_SHUTDOWN:
            # No reply: the host drops stdin as it asks, and writing to a stdout
            # it has stopped reading risks a BrokenPipeError.
            return 0
        worker.serve(frame)


if __name__ == "__main__":
    sys.exit(main())
