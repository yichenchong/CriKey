"""Modern Python plugin surface (spec 13.2, 15.7, 15.8)."""

from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
from typing import Awaitable, Callable, Iterable, Protocol


@dataclass(frozen=True)
class Action:
    action_id: str
    label: str
    description: str = ""
    icon_reference: str | None = None
    #: Category tags this action applies to (spec 10.4). Empty means "any item
    #: this plugin returns". Use :func:`plugin_defined_category` for a name
    #: that could collide with a built-in category.
    applicable_categories: tuple[str, ...] = ()
    #: "host-mediated" or "plugin" (spec 10.4). Absent keeps the historical
    #: host-mediated behaviour.
    execution_policy: str = "host-mediated"


#: Prefix that marks a category name as plugin-defined rather than one of the
#: host's built-in categories (spec 10.3). A bare name is matched against the
#: built-ins first, so a plugin whose own category is called "application"
#: must write ``plugin_defined_category("application")`` to keep it distinct;
#: the host treats the two as different categories and derives different item
#: identities from them.
PLUGIN_DEFINED_PREFIX = "plugin-defined:"


def plugin_defined_category(name: str) -> str:
    """Returns the category tag for a plugin-defined category called ``name``.

    Use this instead of a bare string whenever the name might collide with a
    built-in category ("application", "file", "directory", "url", "command",
    "expression", "keyword", "contact", "clipboard-item").
    """
    return f"{PLUGIN_DEFINED_PREFIX}{name}"


@dataclass
class Item:
    """A catalog item or suggestion. ``stable_id`` must not depend on the label."""

    stable_id: str
    label: str
    target: str
    category: str = "plugin-defined"
    description: str = ""
    icon_reference: str | None = None
    score_hint: int = 0
    search_terms: list[str] = field(default_factory=list)
    metadata: dict[str, str] = field(default_factory=dict)
    actions: list[Action] = field(default_factory=list)
    #: "forbidden", "optional" or "required" (spec 10.1). Left at the
    #: conservative default unless the plugin declares otherwise.
    argument_policy: str = "forbidden"
    #: "recorded" or "ignored" (spec 10.1).
    hit_policy: str = "recorded"


@dataclass(frozen=True)
class Query:
    text: str
    normalized: str
    generation: int


class SuggestContext(Protocol):
    """Per-request context handed to :meth:`Plugin.suggest`."""

    @property
    def cancelled(self) -> bool:
        """True once the request is obsolete; check it often (spec 9.4)."""

    def emit(self, item: Item) -> None:
        """Streams one result. Batching is handled by the worker."""

    def log(self, message: str) -> None: ...

    def spawn(self, coro: Awaitable[object]) -> object:
        """Registers a background coroutine (spec 15.8).

        A registered task is awaited to completion at the end of the callback;
        an un-registered raw pending task is cancelled and reported instead of
        being left running.
        """


class Plugin:
    """Base class for modern Python plugins.

    Every callback is optional. Synchronous and ``async`` implementations are
    both supported; ``async`` callbacks run on the worker's event loop.
    """

    def start(self) -> None: ...

    def build_catalog(self) -> Iterable[Item]:
        return ()

    def suggest(self, query: Query, context: SuggestContext) -> None: ...

    def execute(self, item: Item, action_id: str | None, argument: str | None) -> None: ...

    def stop(self) -> None: ...


class WorkerContext:
    """The concrete :class:`SuggestContext` the modern worker supplies.

    The worker owns result batching, log capture and the asyncio loop; this
    object is the thin, plugin-facing surface over them. ``cancelled`` reads the
    control-frame event set by the worker's daemon reader thread; ``emit`` and
    ``log`` funnel into worker-provided sinks; ``spawn`` registers a coroutine
    the worker awaits at callback end (spec 15.8).
    """

    __slots__ = ("_is_cancelled", "_sink", "_logger", "_loop", "registered_tasks")

    def __init__(
        self,
        is_cancelled: Callable[[], bool],
        sink: Callable[[Item], None],
        logger: Callable[[str], None],
        loop: asyncio.AbstractEventLoop | None = None,
    ) -> None:
        self._is_cancelled = is_cancelled
        self._sink = sink
        self._logger = logger
        self._loop = loop
        self.registered_tasks: list[object] = []

    @property
    def cancelled(self) -> bool:
        return self._is_cancelled()

    def emit(self, item: Item) -> None:
        self._sink(item)

    def log(self, message: str) -> None:
        self._logger(str(message))

    def spawn(self, coro: Awaitable[object]) -> object:
        if not asyncio.iscoroutine(coro):
            raise TypeError("context.spawn(coro) expects a coroutine")
        # A synchronous suggest/execute has no running loop, so
        # ``asyncio.get_running_loop()`` would raise and abort the whole
        # request. Fall back to the worker's loop (the sync-drain branch in the
        # worker runs it) or, absent one, a private loop kept for this context.
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            loop = self._loop
            if loop is None:
                loop = self._loop = asyncio.new_event_loop()
        task = loop.create_task(coro)
        self.registered_tasks.append(task)
        return task
