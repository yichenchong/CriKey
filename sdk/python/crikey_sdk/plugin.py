"""Modern Python plugin surface (spec 13.2, 15.7)."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Iterable, Protocol


@dataclass(slots=True, frozen=True)
class Action:
    action_id: str
    label: str
    description: str = ""
    icon_reference: str | None = None


@dataclass(slots=True)
class Item:
    """A catalog item or suggestion. ``stable_id`` must not depend on the label."""

    stable_id: str
    label: str
    target: str
    category: str = "plugin-defined"
    description: str = ""
    icon_reference: str | None = None
    score_hint: int = 0
    metadata: dict[str, str] = field(default_factory=dict)
    actions: list[Action] = field(default_factory=list)


@dataclass(slots=True, frozen=True)
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
