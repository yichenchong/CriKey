"""Legacy plugin that is correct, conformant, and simply not portable.

It imports `keypirinha_wintypes` at module scope and reaches a Win32 entry point
only from behind `if keypirinha_wintypes.is_available():`. That split is the
whole point of the fixture (spec 14.2, 14.12; acceptance 31.31): the package
loads and schedules fine on this Linux host, so every scheduling check runs and
passes, while the Win32 behaviour itself cannot be exercised here and the report
has to say `unavailable` rather than pass vacuously (roadmap principle 7).

The guard spelling is load-bearing.
`hasattr(kpwt, "kernel32")` and `getattr(kpwt, "kernel32", None)` do NOT work:
attribute access on a Win32 entry point raises
`keypirinha_wintypes.WindowsOnlyError`, which is a `RuntimeError` and
deliberately not an `AttributeError`, precisely so those two probes cannot
launder a Win32 access into a silent `False` or `None`. A fixture written either
way would fail to load on this host instead of loading and honestly reporting
the check unavailable — which is the behaviour under test.

Determinism: no wall clock, no randomness, no network, no absolute paths.
"""

import ctypes

import keypirinha as kp
import keypirinha_wintypes as kpwt

#: Iterations of the cooperative work loop, matching `well-behaved`: needing
#: Windows is a portability fact, not a conformance failure, so this fixture
#: breaks no scheduling rule.
SUGGEST_STEPS = 512

#: `SM_CXSCREEN`, the Win32 system metric read inside the guard. Named rather
#: than inlined so the guarded branch is readable on a host that can never run
#: it.
SM_CXSCREEN = 0

CATALOG_LABELS = (
    "Windows Only Alpha",
    "Windows Only Beta",
)

#: What the Win32 branch reports when it cannot run. The string names the
#: platform because an unavailable result that does not say where it was
#: unavailable is not a diagnostic (spec 26.2).
UNAVAILABLE = "unavailable"


class WindowsOnly(kp.Plugin):
    """A conformant legacy plugin with a declared Windows-only dependency."""

    def on_start(self):
        self.info("windows-only ready; win32 available={}".format(kpwt.is_available()))

    def on_catalog(self):
        self.set_catalog(self._catalog_items())

    def on_suggest(self, user_input, items_chain):
        # Acceptance 31.17: polled unconditionally at the top of every
        # iteration, first one included; an obsolete request abandons without
        # publishing.
        for _step in range(SUGGEST_STEPS):
            if self.should_terminate():
                return

        # Spec 14.9: recomputed from the query on every request.
        self.set_suggestions(
            [self._suggestion(user_input)], kp.Match.DEFAULT, kp.Sort.DEFAULT
        )

    def on_execute(self, item, action):
        self.info(
            "windows-only executed {label!r} via {action}".format(
                label=item.label(),
                action="<default>" if action is None else action.name(),
            )
        )

    def on_activated(self):
        self.dbg("windows-only activated")

    def on_deactivated(self):
        self.dbg("windows-only deactivated")

    def on_events(self, flags):
        if flags & kp.Events.PACKCONFIG:
            self.on_catalog()

    # -- the windows-only half ---------------------------------------------

    def _screen_width(self):
        """Reads a Win32 system metric, or reports honestly that it cannot.

        `is_available()` is the only probe that may be used here; see the module
        docstring for why `hasattr` and `getattr(..., default)` are wrong rather
        than merely unidiomatic.
        """
        if not kpwt.is_available():
            # Spec 14.12: the honest answer on a host without Win32. Returning a
            # plausible number instead — a hard-coded 1920, say — is the
            # vacuous pass the compatibility report exists to prevent.
            return UNAVAILABLE

        # Only reached on Windows. Every name touched below is one of
        # `keypirinha_wintypes.WINDOWS_ONLY_SYMBOLS` and raises `WindowsOnlyError`
        # anywhere else.
        get_system_metric = kpwt.declare_func(
            kpwt.user32, "GetSystemMetrics", ret=ctypes.c_int, arg=[ctypes.c_int]
        )
        return str(get_system_metric(SM_CXSCREEN))

    # -- helpers -----------------------------------------------------------

    def _catalog_items(self):
        width = self._screen_width()
        return [
            self.create_item(
                category=kp.ItemCategory.KEYWORD,
                label=label,
                short_desc="Catalog entry of the windows-only fixture (screen width: {})".format(
                    width
                ),
                target="windows-only/catalog/{}".format(index),
                args_hint=kp.ItemArgsHint.FORBIDDEN,
                hit_hint=kp.ItemHitHint.NOARGS,
            )
            for index, label in enumerate(CATALOG_LABELS)
        ]

    def _suggestion(self, user_input):
        query = user_input if user_input else "<empty>"
        return self.create_item(
            category=kp.ItemCategory.KEYWORD,
            label="windows-only suggestion for " + query,
            short_desc="recomputed for every request",
            target="windows-only/suggest/" + query,
            args_hint=kp.ItemArgsHint.ACCEPTED,
            hit_hint=kp.ItemHitHint.IGNORE,
        )
