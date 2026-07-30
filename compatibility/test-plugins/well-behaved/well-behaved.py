"""Reference legacy plugin: the fixture that must pass every conformance check.

Written against the documented Keypirinha-compatible API the Legacy
Compatibility Layer ships (spec 14.2, 14.4). It exists so that
`crikey dev test-legacy-compat` has one package whose only interesting property
is that it breaks no rule: when a check fails here, the check is wrong, not the
plugin.

Determinism is a hard requirement of the whole synthetic suite (spec 26.3): no
wall clock, no randomness, no network, no absolute paths. Every loop below is
bounded by a constant — or by a configured constant that is itself clamped —
rather than by elapsed time, so two runs of one invocation are byte-identical
and a saved report can be diffed against the last release.
"""

import keypirinha as kp

#: Package-relative resource holding the catalog labels this plugin publishes.
#: Reading it is what exercises the loader's module/resource split (spec 14.3)
#: against real data instead of against a file nothing ever opens.
CATALOG_RESOURCE = "data/catalog.txt"

#: Used when the host exposes no resource capability — `load_text_resource` is
#: an optional host operation and raises `HostUnavailableError` when it is
#: absent (spec 14.12). Byte-identical to the committed resource, so the
#: fixture publishes the same catalog either way.
CATALOG_FALLBACK = (
    "Well Behaved Alpha",
    "Well Behaved Beta",
    "Well Behaved Gamma",
)

#: Iterations of the cooperative work loop in `on_suggest`. Small on purpose:
#: this fixture answers promptly, and the fixture next door is the one that
#: answers late.
DEFAULT_SUGGEST_STEPS = 512

#: Ceiling applied to whatever the configuration file asks for. A fixture whose
#: work loop could be configured without bound would let a stray edit turn a
#: conformance run into a hang.
MAX_SUGGEST_STEPS = 100_000

#: Cap on the catalog assembled from the resource. The resource is committed and
#: short, but the loop that reads it must be bounded by construction rather than
#: by trust in the file's current length.
MAX_CATALOG_ITEMS = 64


class WellBehaved(kp.Plugin):
    """A legacy plugin that observes every scheduling rule of spec 14.5."""

    def __init__(self):
        kp.Plugin.__init__(self)
        self.suggest_steps = DEFAULT_SUGGEST_STEPS

    # -- lifecycle ---------------------------------------------------------

    def on_start(self):
        self.suggest_steps = self._configured_steps()
        self.info("well-behaved ready with suggest_steps={}".format(self.suggest_steps))

    def on_catalog(self):
        # Spec 14.8: repeated `on_catalog` is permitted, and each rebuild is a
        # complete replacement rather than an accumulation. `set_catalog`, not
        # `merge_catalog`, is what says that.
        self.set_catalog(self._catalog_items())

    def on_suggest(self, user_input, items_chain):
        # Acceptance 31.17: the cooperative flag is read unconditionally at the
        # top of every iteration, the first one included, so a host that marks
        # this request obsolete before the loop even spins still observes the
        # poll. "We will notice eventually" is the defect the sibling fixture
        # `ignores-should-terminate` exists to demonstrate.
        for _step in range(self.suggest_steps):
            if self.should_terminate():
                # Abandoning without publishing is the cooperative answer: an
                # obsolete request's result is stale by definition (spec 8.5),
                # and publishing it would only give the host something to throw
                # away.
                return

        # Spec 14.9, acceptance 31.18: the answer is recomputed from the query
        # on every request. Nothing about this list outlives the call, which is
        # what makes the suggestion fresh rather than merely recent.
        self.set_suggestions(
            [self._suggestion(user_input)], kp.Match.DEFAULT, kp.Sort.DEFAULT
        )

    def on_execute(self, item, action):
        # Executing routes the selection back to its owner and does nothing
        # else: it must not publish, rebuild the catalog, or block (spec 14.5).
        self.info(
            "well-behaved executed {label!r} via {action}".format(
                label=item.label(),
                action="<default>" if action is None else action.name(),
            )
        )

    def on_activated(self):
        self.dbg("well-behaved activated")

    def on_deactivated(self):
        self.dbg("well-behaved deactivated")

    def on_events(self, flags):
        # Spec 14.4: a configuration change is the documented reason to reread
        # settings and rebuild, which is also the `repeated_on_catalog_permitted`
        # path of spec 14.8.
        if flags & kp.Events.PACKCONFIG:
            self.suggest_steps = self._configured_steps()
            self.on_catalog()

    # -- helpers -----------------------------------------------------------

    def _configured_steps(self):
        try:
            settings = self.load_settings()
            configured = settings.get_int(
                "suggest_steps", "main", fallback=DEFAULT_SUGGEST_STEPS
            )
        except (kp.HostUnavailableError, kp.KeypirinhaError):
            # Settings are an optional host capability. A fixture that died
            # without one would fail conformance for the host's reasons rather
            # than its own.
            return DEFAULT_SUGGEST_STEPS
        return max(1, min(int(configured), MAX_SUGGEST_STEPS))

    def _catalog_labels(self):
        try:
            text = self.load_text_resource(CATALOG_RESOURCE)
        except (kp.HostUnavailableError, kp.KeypirinhaError):
            return CATALOG_FALLBACK
        labels = tuple(
            line.strip() for line in text.splitlines()[:MAX_CATALOG_ITEMS] if line.strip()
        )
        return labels or CATALOG_FALLBACK

    def _catalog_items(self):
        return [
            self.create_item(
                category=kp.ItemCategory.KEYWORD,
                label=label,
                short_desc="Reference legacy catalog entry",
                # Targets are package-relative identifiers, never filesystem
                # paths: the suite must behave identically wherever the
                # workspace is checked out.
                target="well-behaved/catalog/{}".format(index),
                args_hint=kp.ItemArgsHint.FORBIDDEN,
                hit_hint=kp.ItemHitHint.NOARGS,
            )
            for index, label in enumerate(self._catalog_labels())
        ]

    def _suggestion(self, user_input):
        query = user_input if user_input else "<empty>"
        return self.create_item(
            category=kp.ItemCategory.KEYWORD,
            label="well-behaved suggestion for " + query,
            short_desc="recomputed for every request",
            target="well-behaved/suggest/" + query,
            args_hint=kp.ItemArgsHint.ACCEPTED,
            hit_hint=kp.ItemHitHint.IGNORE,
        )
