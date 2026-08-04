"""Modern Python plugin surface (spec 13.2, 15.7, 15.8)."""

from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
from typing import Any, Awaitable, Callable, Coroutine, Iterable, Protocol

__all__ = [
    "Action",
    "Item",
    "Plugin",
    "Query",
    "SuggestContext",
    "WorkerContext",
    "PLUGIN_DEFINED_PREFIX",
    "plugin_defined_category",
]


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

    def spawn(self, coro: Coroutine[Any, Any, object]) -> object:
        """Registers a background coroutine (spec 15.8).

        In the worker process, the host admits or refuses the coroutine and
        runs an admitted task independently of the foreground callback. The
        local SDK fallback retains it and awaits it at callback end. In either
        mode, an unregistered raw pending task is cancelled and reported
        instead of being left running.
        """


class Plugin:
    """Base class for modern Python plugins.

    Every callback is optional. Synchronous and ``async`` implementations are
    both supported; asynchronous callbacks run on the worker's event loop.
    """

    def start(self) -> None | Awaitable[None]:
        return None

    def build_catalog(self) -> Iterable[Item] | Awaitable[Iterable[Item]] | None:
        return ()

    def suggest(
        self, query: Query, context: SuggestContext
    ) -> None | Awaitable[None]:
        return None

    def execute(
        self, item: Item, action_id: str | None, argument: str | None
    ) -> None | Awaitable[None]:
        return None

    def stop(self) -> None | Awaitable[None]:
        return None


class WorkerContext:
    """The concrete :class:`SuggestContext` the modern worker supplies.

    The host owns result batching and log capture. ``cancelled`` reads the
    control-frame event set by the worker's daemon reader thread; ``emit`` and
    ``log`` funnel into worker-provided sinks. ``spawn`` is a host-visible
    registration point: the worker sends the coroutine's task id to the Rust
    host, which admits it against the shared per-plugin background budget
    before the coroutine can run. A worker without that registration callback
    retains the small local-loop fallback for SDK embedding tests.
    """

    __slots__ = (
        "_is_cancelled",
        "_sink",
        "_logger",
        "_loop",
        "_spawn_background",
        "registered_tasks",
    )

    def __init__(
        self,
        is_cancelled: Callable[[], bool],
        sink: Callable[[Item], None],
        logger: Callable[[str], None],
        loop: asyncio.AbstractEventLoop | None = None,
        spawn_background: Callable[[Coroutine[Any, Any, object]], object] | None = None,
    ) -> None:
        self._is_cancelled = is_cancelled
        self._sink = sink
        self._logger = logger
        self._loop = loop
        self._spawn_background = spawn_background
        self.registered_tasks: list[asyncio.Task[object]] = []

    @property
    def cancelled(self) -> bool:
        return self._is_cancelled()

    def emit(self, item: Item) -> None:
        self._sink(item)

    def log(self, message: str) -> None:
        self._logger(str(message))

    def spawn(self, coro: Coroutine[Any, Any, object]) -> object:
        if not asyncio.iscoroutine(coro):
            raise TypeError("context.spawn(coro) expects a coroutine")
        if self._spawn_background is not None:
            # Registration and host admission happen before the worker starts
            # this coroutine. It is deliberately not appended to
            # ``registered_tasks``: host-managed tasks outlive the synchronous
            # callback and must not be awaited at callback end.
            return self._spawn_background(coro)
        # A synchronous suggest has no running loop, so
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
