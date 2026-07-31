"""Official Python SDK for CriKey plugins (spec 15).

Modern Python plugins run in supervised worker processes, never on the CriKey
user-interface thread, and use ordinary Python imports with managed
dependencies declared in the plugin manifest's ``[python]`` section.
"""

from .plugin import (
    Action,
    Item,
    Plugin,
    Query,
    SuggestContext,
    WorkerContext,
    plugin_defined_category,
)

__all__ = [
    "Action",
    "Item",
    "Plugin",
    "Query",
    "SuggestContext",
    "WorkerContext",
    "plugin_defined_category",
]
__version__ = "0.1.0"
