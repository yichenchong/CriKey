"""Official Python SDK for CriKey plugins (spec 15).

Modern Python plugins run in supervised worker processes, never on the CriKey
user-interface thread, and use ordinary Python imports with managed
dependencies declared in the plugin manifest's ``[python]`` section.
"""

from .arguments import *
from .arguments import __all__ as _argument_exports
from .plugin import (
    Action,
    Command,
    Item,
    PLUGIN_DEFINED_PREFIX,
    Plugin,
    Preview,
    PreviewContext,
    PreviewImage,
    PreviewValue,
    Query,
    SuggestContext,
    WorkerContext,
    plugin_defined_category,
)

__all__ = [
    "Action",
    "Command",
    "Item",
    "PLUGIN_DEFINED_PREFIX",
    "Plugin",
    "Preview",
    "PreviewContext",
    "PreviewImage",
    "PreviewValue",
    "Query",
    "SuggestContext",
    "WorkerContext",
    "plugin_defined_category",
    "__version__",
] + _argument_exports
__version__ = "0.1.0"
