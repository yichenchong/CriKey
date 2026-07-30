"""Documented ``keypirinha`` module of the CriKey Legacy Compatibility Layer.

CriKey is an independent project. This module reimplements the *documented*
Keypirinha plugin API (spec 14.2, 14.4) so unchanged legacy packages keep
working; it is not an official Keypirinha component and never presents itself
as one (spec 14.13), which is why :func:`name` answers ``CriKey``.

The module owns exactly one piece of mutable process state: the optional
*host object* installed with :func:`_set_host` and removed with
:func:`_clear_host`. Everything a plugin can observe beyond its own
interpreter travels through that object. Only ``should_terminate`` is
mandatory on it; every other capability is optional and its absence surfaces
as a typed :class:`HostUnavailableError` naming the operation, never as an
``AttributeError`` escaping from inside the shim (spec 14.12)::

    should_terminate() -> bool                                     (mandatory)
    terminate_event() -> threading.Event | None                    (optional)
    publish_suggestions(plugin, suggestions, match_method, sort_method)
    publish_catalog(plugin, items, merge)
    load_settings(plugin) -> dict[str, dict[str, str]]
    load_resource(plugin, name) -> bytes
    package_full_path(plugin) -> str
    package_cache_path(plugin, create) -> str

Publication is deliberately fire-and-forget: :meth:`Plugin.set_suggestions`
and :meth:`Plugin.set_catalog` hand the host *one complete list* per call and
retain nothing. The shim therefore has no per-plugin collection that could
grow without bound, and "the newest call wins" is a property of the host's
single-slot state rather than of bookkeeping here (spec 7.1).
"""

import enum
import sys
import time

# --------------------------------------------------------------------------
# Product identity (spec 14.13)
# --------------------------------------------------------------------------

_PRODUCT_NAME = "CriKey"

#: Kept in step with the Cargo workspace version by hand. A tuple of ints is
#: the documented shape; legacy plugins compare it with tuple ordering.
_PRODUCT_VERSION = (0, 1, 0)

#: Longest a cooperative wait sleeps before re-reading the host's flag. The
#: flag is level-triggered and raised by another thread, so one long sleep
#: would leave a plugin deaf to cancellation for its whole duration.
_TERMINATE_POLL_SECONDS = 0.05

#: Where :attr:`Plugin.id` is memoised inside the instance dictionary. Spelled
#: out rather than derived so a plugin's own attributes can never collide with
#: it by accident.
_ID_CACHE_KEY = "_keypirinha_plugin_id"

#: ASCII-only case folding. `str.lower()` would also fold non-ASCII letters,
#: which the Rust-side INI parser does not, and a shim that disagreed with the
#: host about whether two keys are the same key is worse than no shim at all.
_ASCII_FOLD = str.maketrans(
    "ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghijklmnopqrstuvwxyz"
)


def _fold(text):
    """ASCII-case-folds a section or key name for lookup."""
    return text.translate(_ASCII_FOLD)


# --------------------------------------------------------------------------
# Error taxonomy (spec 26.2)
# --------------------------------------------------------------------------


class KeypirinhaError(Exception):
    """Root of every error the compatibility layer raises.

    One taxonomy means the diagnostics layer can classify a plugin failure
    without pattern-matching exception spellings.
    """


class UndocumentedApiError(KeypirinhaError, AttributeError):
    """A plugin reached for something outside the documented API surface.

    Subclasses ``AttributeError`` on purpose: ``hasattr``, ``getattr`` with a
    default, ``copy`` and ``pickle`` all depend on a failed attribute lookup
    raising ``AttributeError``, and breaking those protocols to make a point
    would cause failures far from the plugin that caused them. The specific
    diagnostic lives in the attached fields instead (spec 14.12).
    """

    #: Stable identifier the diagnostics layer groups on.
    diagnostic_code = "undocumented-api-access"

    def __init__(self, module, attribute):
        self.module = module
        self.attribute = attribute
        KeypirinhaError.__init__(
            self,
            "{}.{} is not part of the documented Keypirinha API surface that "
            "CriKey implements; the Legacy Compatibility Layer reproduces "
            "documented behaviour only (spec 14.12)".format(module, attribute),
        )


class InvalidItemError(KeypirinhaError, ValueError):
    """A catalog item was constructed from a value the layer cannot honour.

    Also a ``ValueError`` so unchanged plugins that already guard item
    construction with ``except ValueError`` keep working.
    """

    def __init__(self, field, message):
        self.field = field
        KeypirinhaError.__init__(self, message)


class SettingsError(KeypirinhaError, ValueError):
    """A configuration value could not be coerced to the requested type."""

    def __init__(self, section, key, message):
        self.section = section
        self.key = key
        KeypirinhaError.__init__(self, message)


class HostUnavailableError(KeypirinhaError, RuntimeError):
    """The installed host cannot perform an operation the plugin asked for.

    Deliberately not an ``AttributeError``: this is an honest "cannot", and
    ``hasattr``/``getattr(..., default)`` must not be able to launder it into
    a silent ``False`` or ``None`` (spec 14.12).
    """

    def __init__(self, operation, reason):
        self.operation = operation
        self.reason = reason
        KeypirinhaError.__init__(
            self,
            "the CriKey host cannot perform `{}`: {}".format(operation, reason),
        )


# --------------------------------------------------------------------------
# Documented constants
#
# The integer values are part of the contract, not an implementation detail:
# legacy packages persist them in caches and compare them against literals,
# so renumbering one would silently corrupt an existing installation.
# --------------------------------------------------------------------------


class ItemCategory(enum.IntEnum):
    """Documented catalog item categories."""

    KEYWORD = 1
    CMDLINE = 2
    FILE = 3
    URL = 4
    EXPRESSION = 5
    REFERENCE = 6
    ERROR = 7

    #: Extension point: a plugin may define its own categories at or above
    #: this value, and the layer must preserve them exactly rather than
    #: folding them into a built-in one.
    USER_BASE = 100


class ItemArgsHint(enum.IntEnum):
    """Whether an item accepts arguments."""

    FORBIDDEN = 0
    ACCEPTED = 1
    REQUIRED = 2


class ItemHitHint(enum.IntEnum):
    """How a hit on an item should be recorded in usage history."""

    NOARGS = 0
    KEEPALL = 1
    IGNORE = 2


class Match(enum.IntEnum):
    """Matching method a plugin requests for a suggestion batch."""

    DEFAULT = 0
    ANY = 1
    FUZZY = 2
    SUBSTR = 3


class Sort(enum.IntEnum):
    """Sort order a plugin requests for a suggestion batch."""

    DEFAULT = 0
    NONE = 1
    SCORE_DESC = 2
    LABEL_ASC = 3
    LABEL_DESC = 4


class Events(enum.IntFlag):
    """Host events a plugin can be notified about.

    Mirrors the Rust-side ``LegacyEventFlags`` bit for bit: the two sides
    exchange these as a single integer, so a disagreement about one bit would
    silently deliver the wrong event.
    """

    APPCONFIG = 0x01
    PACKCONFIG = 0x02
    NETOPTIONS = 0x04
    PACKAGES = 0x08
    FILESYSTEM = 0x10
    DESKTOP = 0x20
    STARTMENU = 0x40

    #: Unchanged plugins write the documented spelling `NETOPTIONS`; `NETWORK`
    #: is an alias for the same bit because a missing name is an
    #: `AttributeError` raised inside somebody else's plugin.
    NETWORK = 0x04

    ALL = 0x7F


#: Categories the layer defines itself. Anything at or above
#: `ItemCategory.USER_BASE` is a legitimate plugin-defined category.
_KNOWN_CATEGORIES = frozenset(int(member) for member in ItemCategory)
_KNOWN_ARGS_HINTS = frozenset(int(member) for member in ItemArgsHint)
_KNOWN_HIT_HINTS = frozenset(int(member) for member in ItemHitHint)


def _coerce_enum(value, field, enum_cls, known, allow_at_or_above=None):
    """Validates one documented constant and returns it as a plain int.

    Rejecting rather than coercing is the whole point: an item silently
    demoted to a category the plugin did not ask for is a bug that surfaces
    as "my results are in the wrong section", days later and nowhere near the
    code that caused it.
    """
    # `bool` is an `int` subclass, and `True` is never a meaningful category
    # or hint. Letting it through would turn a typo into category KEYWORD.
    if isinstance(value, bool) or not isinstance(value, int):
        raise InvalidItemError(
            field,
            "{} must be one of the documented keypirinha.{} constants, got {!r}".format(
                field, enum_cls.__name__, value
            ),
        )

    numeric = int(value)
    if numeric in known:
        return numeric
    if allow_at_or_above is not None and numeric >= allow_at_or_above:
        return numeric

    raise InvalidItemError(
        field,
        "{}={} is not a documented keypirinha.{} constant".format(
            field, numeric, enum_cls.__name__
        ),
    )


# --------------------------------------------------------------------------
# Values handed to and from plugins
# --------------------------------------------------------------------------


class CatalogItem:
    """One catalog entry or suggestion.

    An independent value, not a view over plugin state: a plugin that builds
    a fresh catalog on every `on_catalog` must not find the previous items
    mutating under it. `__slots__` keeps a large catalog cheap — a package
    with ten thousand entries pays ten thousand instance dictionaries
    otherwise.
    """

    __slots__ = (
        "_category",
        "_label",
        "_short_desc",
        "_target",
        "_args_hint",
        "_hit_hint",
        "_loop_on_suggest",
        "_icon_handle",
        "_data_bag",
    )

    def __init__(
        self,
        category,
        label,
        short_desc,
        target,
        args_hint,
        hit_hint,
        loop_on_suggest=False,
        icon_handle=None,
        data_bag=None,
    ):
        self._category = _coerce_enum(
            category,
            "category",
            ItemCategory,
            _KNOWN_CATEGORIES,
            allow_at_or_above=int(ItemCategory.USER_BASE),
        )
        self._args_hint = _coerce_enum(
            args_hint, "args_hint", ItemArgsHint, _KNOWN_ARGS_HINTS
        )
        self._hit_hint = _coerce_enum(
            hit_hint, "hit_hint", ItemHitHint, _KNOWN_HIT_HINTS
        )
        self._label = label
        self._short_desc = short_desc
        self._target = target
        self._loop_on_suggest = bool(loop_on_suggest)
        # Handed back by identity. An icon handle is an opaque host object and
        # a copy of it would not name the same icon.
        self._icon_handle = icon_handle
        self._data_bag = data_bag

    def category(self):
        return self._category

    def label(self):
        return self._label

    def short_desc(self):
        return self._short_desc

    def target(self):
        return self._target

    def args_hint(self):
        return self._args_hint

    def hit_hint(self):
        return self._hit_hint

    def loop_on_suggest(self):
        return self._loop_on_suggest

    def icon_handle(self):
        return self._icon_handle

    def data_bag(self):
        return self._data_bag

    def set_data_bag(self, data_bag):
        self._data_bag = data_bag

    def __repr__(self):
        return "CatalogItem(category={}, label={!r}, target={!r})".format(
            self._category, self._label, self._target
        )


class Action:
    """The action chosen for an item, handed to :meth:`Plugin.on_execute`.

    Maps onto ``crikey_core::Action``: ``action_id.0`` is :meth:`name`,
    ``label`` is :meth:`label` and ``description`` is :meth:`short_desc`.
    ``None`` reaches ``on_execute`` when the default action was taken.
    """

    __slots__ = ("_name", "_label", "_short_desc")

    def __init__(self, name, label="", short_desc=""):
        self._name = name
        self._label = label
        self._short_desc = short_desc

    def name(self):
        return self._name

    def label(self):
        return self._label

    def short_desc(self):
        return self._short_desc

    def __repr__(self):
        return "Action(name={!r}, label={!r})".format(self._name, self._label)


#: Distinguishes "no fallback supplied" from "the fallback is None". Without
#: it `get_int(key)` could not tell a caller who wants None on failure from
#: one who wants the typed error.
_UNSET = object()


class Settings:
    """Read-only view over one plugin's configuration.

    Section and key lookup is ASCII-case-insensitive on both sides of the
    host boundary: the Rust parser folds case while reporting the first-seen
    spelling, and this view must not disagree with it or a plugin would
    conclude a key is absent that the host can read perfectly well.

    The mapping is snapshotted at construction. A view that read through to a
    live dictionary would let a config reload change a value in the middle of
    a callback that had already branched on it.
    """

    __slots__ = ("_sections",)

    #: Name of the unnamed top-level INI section.
    DEFAULT_SECTION = "DEFAULT"

    def __init__(self, mapping=None):
        # folded section -> (first-seen spelling, {folded key: (spelling, value)})
        sections = {}
        for section_name, entries in (mapping or {}).items():
            folded_section = _fold(section_name)
            display, keys = sections.setdefault(folded_section, (section_name, {}))
            del display
            for key, value in (entries or {}).items():
                keys.setdefault(_fold(key), (key, value))
        self._sections = sections

    def _entries(self, section):
        if section is None:
            section = self.DEFAULT_SECTION
        found = self._sections.get(_fold(section))
        return found[1] if found is not None else None

    def sections(self):
        """Every section, sorted, so a plugin's own output is deterministic."""
        return sorted(spelling for spelling, _keys in self._sections.values())

    def keys(self, section=None):
        """Keys of `section`, sorted; the DEFAULT section when omitted."""
        entries = self._entries(section)
        if entries is None:
            return []
        return sorted(spelling for spelling, _value in entries.values())

    def has(self, key, section=None):
        entries = self._entries(section)
        return entries is not None and _fold(key) in entries

    def get(self, key, section=None, fallback=None):
        """The raw string value, or `fallback` when the key is absent."""
        entries = self._entries(section)
        if entries is None:
            return fallback
        found = entries.get(_fold(key))
        return fallback if found is None else found[1]

    def _coerce(self, key, section, fallback, convert, expected):
        raw = self.get(key, section)
        if raw is None:
            # A missing key is not a coercion failure. With no fallback the
            # answer is the same "not configured" that `get` reports.
            return None if fallback is _UNSET else fallback
        try:
            return convert(raw)
        except (TypeError, ValueError):
            if fallback is not _UNSET:
                # An unchanged plugin that supplied a fallback asked to keep
                # going; honouring that beats failing its whole callback.
                return fallback
            raise SettingsError(
                self.DEFAULT_SECTION if section is None else section,
                key,
                "[{}] {}={!r} is not {}".format(
                    self.DEFAULT_SECTION if section is None else section,
                    key,
                    raw,
                    expected,
                ),
            ) from None

    def get_int(self, key, section=None, fallback=_UNSET):
        return self._coerce(
            key, section, fallback, lambda raw: int(raw.strip(), 10), "an integer"
        )

    def get_float(self, key, section=None, fallback=_UNSET):
        return self._coerce(
            key, section, fallback, lambda raw: float(raw.strip()), "a number"
        )

    def get_bool(self, key, section=None, fallback=_UNSET):
        return self._coerce(
            key, section, fallback, _parse_bool, "a boolean (yes/no, true/false, on/off, 1/0)"
        )

    def __repr__(self):
        return "Settings(sections={!r})".format(self.sections())


#: Every boolean spelling the documented Keypirinha configuration accepts.
_TRUE_WORDS = frozenset(("1", "yes", "true", "on", "y", "t"))
_FALSE_WORDS = frozenset(("0", "no", "false", "off", "n", "f"))


def _parse_bool(raw):
    folded = _fold(raw.strip())
    if folded in _TRUE_WORDS:
        return True
    if folded in _FALSE_WORDS:
        return False
    raise ValueError(raw)


# --------------------------------------------------------------------------
# The host boundary
# --------------------------------------------------------------------------

#: The installed host object, or None outside a worker. Read afresh on every
#: access: `should_terminate` must observe a flag raised by another thread
#: while a callback is still running, so nothing here may be cached.
_HOST = None


def _set_host(host):
    """Installs the host object. Called by the worker, never by a plugin."""
    global _HOST
    _HOST = host


def _clear_host():
    """Removes the host object, returning the module to its inert default."""
    global _HOST
    _HOST = None


def _host_capability(operation):
    """The host's implementation of `operation`, or a typed refusal.

    Returning `None`, or letting an `AttributeError` escape, would let a
    plugin's `hasattr` or bare `except` turn "this host cannot do that" into
    a silent no-op, and the layer would have nothing to report (spec 14.12).
    """
    host = _HOST
    if host is None:
        raise HostUnavailableError(
            operation, "no CriKey host is installed in this interpreter"
        )
    capability = getattr(host, operation, None)
    if not callable(capability):
        raise HostUnavailableError(
            operation, "the installed CriKey host does not implement it"
        )
    return capability


def should_terminate(delay=None):
    """Whether the plugin should stop what it is doing (spec 7.1, 14.5).

    ``False`` when no host is installed, so a plugin exercised outside a
    worker — developer mode, its own unit tests — needs no setup.

    `delay` is the documented optional cooperative wait in seconds. It is
    honoured by waiting on the host's own flag where the host exposes one,
    and otherwise by sleeping in slices short enough that the answer is still
    prompt. A plugin throttling with ``should_terminate(0.25)`` therefore
    neither spins nor goes deaf to cancellation.
    """
    host = _HOST
    if host is None:
        return False

    poll = _host_capability("should_terminate")

    if not delay:
        return bool(poll())

    # The host's own event wakes the instant the flag is raised: no clock
    # reads, no polling interval to get wrong.
    waitable = getattr(host, "terminate_event", None)
    if callable(waitable):
        event = waitable()
        if event is not None:
            return bool(event.wait(delay))

    deadline = time.monotonic() + float(delay)
    while True:
        if poll():
            return True
        remaining = deadline - time.monotonic()
        if remaining <= 0.0:
            return False
        time.sleep(min(remaining, _TERMINATE_POLL_SECONDS))


#: The process's real stdout, captured by `_install_stdout_guard`. `None`
#: until the guard is installed, which is also how the guard stays idempotent.
_PROTOCOL_STDOUT = None


def _install_stdout_guard(replacement=None):
    """Takes stdout away from plugin code and hands it to the caller.

    stdout is the strict newline-delimited JSON protocol channel: one stray
    ``print`` from a plugin desynchronises the stream for good (spec 7.1).
    The worker calls this before any plugin code runs, keeps the returned
    stream for protocol frames, and lets plugin chatter fall through to
    `replacement` — ``sys.stderr``, the plugin log channel, by default.

    Idempotent: a worker may re-enter setup on reload, and a second redirect
    chained onto the first would send protocol traffic to the log channel.
    """
    global _PROTOCOL_STDOUT
    if _PROTOCOL_STDOUT is None:
        _PROTOCOL_STDOUT = sys.stdout
        sys.stdout = sys.stderr if replacement is None else replacement
    return _PROTOCOL_STDOUT


# --------------------------------------------------------------------------
# The plugin base class (spec 14.4)
# --------------------------------------------------------------------------


class Plugin:
    """Base class every legacy plugin derives from.

    Constructing one is not a host operation: a plugin must be importable and
    instantiable with no host installed, because that is what package loading
    and developer mode do before a worker exists.

    Every lifecycle callback has an inert default so an unchanged plugin can
    override only the ones it cares about.
    """

    def __init__(self):
        """Deliberately inert.

        Legacy plugins call ``kp.Plugin.__init__(self)`` from their own
        ``__init__``, so it must exist and must stay side-effect free. All
        derived state is computed on demand, which also means a plugin that
        forgets the super call still works.
        """

    # -- identity ----------------------------------------------------------

    @property
    def id(self):
        """Stable identifier for this plugin.

        Memoised in the instance dictionary rather than assigned in
        ``__init__``, so it resolves even for a subclass that never called
        the base constructor.
        """
        cached = self.__dict__.get(_ID_CACHE_KEY)
        if cached is None:
            cached = "{}.{}".format(self.package_full_name(), type(self).__name__)
            self.__dict__[_ID_CACHE_KEY] = cached
        return cached

    def friendly_name(self):
        """Human-readable name; the class name unless a plugin overrides it."""
        return type(self).__name__

    def package_full_name(self):
        """Name of the legacy package this plugin was loaded from.

        Derived from the defining module because a legacy plugin carries no
        identifier of its own: the worker imports the package's main module
        under the package's own name, so the two agree by construction.
        """
        return type(self).__module__.partition(".")[0]

    # -- lifecycle callbacks -----------------------------------------------

    def on_start(self):
        """Called once after the plugin is loaded."""

    def on_catalog(self):
        """Called to (re)build the plugin's catalog.

        May be called repeatedly; each rebuild is a complete replacement
        (spec 14.8).
        """

    def on_suggest(self, user_input, items_chain):
        """Called to produce dynamic suggestions for `user_input`."""

    def on_execute(self, item, action):
        """Called when the user executes `item`.

        `action` is ``None`` when the default action was taken.
        """

    def on_activated(self):
        """Called when the launcher becomes visible."""

    def on_deactivated(self):
        """Called when the launcher is dismissed."""

    def on_events(self, flags):
        """Called with an :class:`Events` bit set describing what changed."""

    # -- item construction -------------------------------------------------

    def create_item(
        self,
        category,
        label,
        short_desc,
        target,
        args_hint,
        hit_hint,
        loop_on_suggest=False,
        icon_handle=None,
        data_bag=None,
    ):
        """Builds one :class:`CatalogItem`.

        The parameter order is the documented one; unchanged plugins pass
        these positionally as often as by keyword.
        """
        return CatalogItem(
            category,
            label,
            short_desc,
            target,
            args_hint,
            hit_hint,
            loop_on_suggest,
            icon_handle,
            data_bag,
        )

    # -- publication (spec 7.1, 14.8) --------------------------------------

    def set_catalog(self, items):
        """Publishes a complete catalog, replacing whatever came before."""
        _host_capability("publish_catalog")(self, list(items), False)

    def merge_catalog(self, items):
        """Publishes items to be merged into the existing catalog.

        The host owns merging, not the shim: folding the previous catalog in
        here would make every rebuild quadratic and would hide from the host
        which items this call actually contributed.
        """
        _host_capability("publish_catalog")(self, list(items), True)

    def set_suggestions(self, suggestions, match_method=Match.DEFAULT, sort_method=Sort.DEFAULT):
        """Publishes one complete suggestion list.

        Each call is a full replacement, so calling this twice inside one
        ``on_suggest`` leaves only the newest list live. The list is
        snapshotted: a plugin that keeps mutating its own list afterwards
        must not retroactively change what it already published.
        """
        _host_capability("publish_suggestions")(
            self, list(suggestions), match_method, sort_method
        )

    # -- cooperative termination -------------------------------------------

    def should_terminate(self, delay=None):
        """Delegates to the module-level flag, which lives on the host.

        Host state, not plugin state: every plugin instance in the worker
        observes the same flag.
        """
        return should_terminate(delay)

    # -- settings and package resources ------------------------------------

    def load_settings(self):
        """This plugin's configuration as a :class:`Settings` view."""
        return Settings(_host_capability("load_settings")(self))

    def package_full_path(self):
        """Absolute path of the package this plugin was loaded from."""
        return _host_capability("package_full_path")(self)

    def get_package_cache_path(self, create=False):
        """Absolute path of this package's cache directory.

        `create` defaults to ``False`` so a plugin merely asking where its
        cache would live never creates a directory as a side effect.
        """
        return _host_capability("package_cache_path")(self, create)

    def load_binary_resource(self, name):
        """A packaged resource, verbatim.

        Never decoded: a resource may be an icon or any other binary blob,
        and a lossy round-trip through text would corrupt it silently.
        """
        data = _host_capability("load_resource")(self, name)
        return data if isinstance(data, bytes) else bytes(data)

    def load_text_resource(self, name, encoding="utf-8"):
        """A packaged resource decoded exactly once, with newlines untouched."""
        return self.load_binary_resource(name).decode(encoding)

    # -- logging (spec 14.4, 26.1) -----------------------------------------

    def info(self, *args):
        self._log("info", args)

    def warn(self, *args):
        self._log("warn", args)

    def err(self, *args):
        self._log("err", args)

    def dbg(self, *args):
        self._log("dbg", args)

    def _log(self, level, args):
        """Writes one log line to the plugin log channel.

        `sys.stderr` is resolved at call time, never captured: the worker
        rebinds it to a per-request capture so a line can be attributed to
        the callback that produced it. stdout is never an option — it is the
        protocol channel and one stray byte desynchronises it.
        """
        stream = sys.stderr
        stream.write(
            "[{}][{}] {}\n".format(
                level, self.friendly_name(), " ".join(str(arg) for arg in args)
            )
        )
        stream.flush()

    # -- undocumented internals (spec 14.12) -------------------------------

    def __getattr__(self, name):
        """Turns a reach for a shim internal into an attributable diagnostic.

        Only reached when normal lookup already failed, so attributes a
        plugin sets on itself are untouched.
        """
        # Python's own protocols probe dunders constantly. Answering those
        # with the legacy diagnostic would misattribute `copy`, `pickle` and
        # `inspect` machinery to the plugin.
        if name.startswith("__") and name.endswith("__"):
            raise AttributeError(name)
        raise UndocumentedApiError("keypirinha.Plugin", name)


# --------------------------------------------------------------------------
# Module-level identity and the module-level undocumented-internal guard
# --------------------------------------------------------------------------


def name():
    """The host product. CriKey, never Keypirinha (spec 14.13)."""
    return _PRODUCT_NAME


def version():
    """The host version as a tuple of integers."""
    return _PRODUCT_VERSION


def version_string():
    """The host version as a dotted string."""
    return ".".join(str(part) for part in _PRODUCT_VERSION)


def __getattr__(name):
    """Attributes a plugin reached for that this module does not document.

    A bare ``AttributeError`` from inside the shim tells nobody which plugin
    depended on which undocumented internal. This one names both and carries
    a stable diagnostic code (spec 14.12).
    """
    if name.startswith("__") and name.endswith("__"):
        raise AttributeError(name)
    raise UndocumentedApiError("keypirinha", name)
