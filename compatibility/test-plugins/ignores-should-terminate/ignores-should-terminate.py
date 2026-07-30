"""Legacy plugin that never reads the cooperative termination flag.

This is a *named* M3 exit criterion: the conformance suite must catch a plugin
that ignores the cooperative termination flag (spec 9.2, 27.3; acceptance
31.17; roadmap M3 "the synthetic legacy test-plugin suite passes, including a
plugin that ignores `should_terminate`"). The misbehaviour is therefore real
rather than declared: `on_suggest` below runs a long loop and never reads the
flag — its name appears nowhere in this module's code — so a host that marks
the request obsolete mid-flight observes zero polls and the check fails on
evidence.

It is identical to `well-behaved` in every other respect. Exactly one rule may
break here, because a fixture that broke two would let a blanket-failure bug in
the suite pass for a precise one.

Two properties keep it usable as a test fixture rather than a hazard:

* The loop is **bounded by a constant**, not by a clock, so the run is
  reproducible byte for byte (spec 26.3) and the plugin always answers. It
  answers *late*; it does not hang. Nothing in the report may depend on a
  timeout firing.
* It still publishes a query-dependent payload, so it violates the termination
  rule and nothing else — in particular not spec 14.9's no-caching rule.
"""

import keypirinha as kp

#: Iterations of the uninterruptible work loop. Large enough that the answer is
#: observably late next to `well-behaved`'s 512 steps, small enough that the
#: request always completes well inside any sane call budget.
DEFAULT_BUSY_STEPS = 2_000_000

#: Ceiling applied to the configured step count. Without it, an edit to the
#: configuration file could turn "answers late" into "never answers", and the
#: suite would start proving something else.
MAX_BUSY_STEPS = 20_000_000

#: Modulus of the loop's accumulator. The accumulator exists so the loop does
#: real work whose result is used: a loop whose body could be deleted is a loop
#: a future reader will delete.
CHECKSUM_MODULUS = 1_000_003

CATALOG_LABELS = (
    "Ignores Should Terminate Alpha",
    "Ignores Should Terminate Beta",
)


class IgnoresShouldTerminate(kp.Plugin):
    """A legacy plugin that answers late and never looks at the flag."""

    def __init__(self):
        kp.Plugin.__init__(self)
        self.busy_steps = DEFAULT_BUSY_STEPS

    # -- lifecycle ---------------------------------------------------------

    def on_start(self):
        self.busy_steps = self._configured_steps()
        self.info("ignores-should-terminate ready with busy_steps={}".format(self.busy_steps))

    def on_catalog(self):
        # Cataloguing is not the defect: this half of the plugin conforms, so
        # `repeated_on_catalog_permitted` and its neighbours still pass.
        self.set_catalog(self._catalog_items())

    def on_suggest(self, user_input, items_chain):
        # THE DEFECT (spec 9.2, acceptance 31.17). This loop deliberately never
        # consults the cooperative termination flag, and neither does anything
        # else in the module — the flag is named nowhere in this file's code.
        # The host may raise it the instant this request is superseded; this
        # plugin will not look, and will keep computing an answer nobody wants
        # until the loop runs out.
        #
        # Rejected alternative: a `self.ignores_termination = True` marker the
        # harness reads. That would make the suite test a claim instead of a
        # behaviour, and the check would pass on a plugin that merely lied
        # politely.
        checksum = 0
        for step in range(self.busy_steps):
            checksum = (checksum * 31 + step) % CHECKSUM_MODULUS

        # It always answers. Late, but it answers: the report must never depend
        # on a timeout, because a timeout is a property of the host's budget
        # rather than of the plugin.
        self.set_suggestions(
            [self._suggestion(user_input, checksum)], kp.Match.DEFAULT, kp.Sort.DEFAULT
        )

    def on_execute(self, item, action):
        self.info(
            "ignores-should-terminate executed {label!r} via {action}".format(
                label=item.label(),
                action="<default>" if action is None else action.name(),
            )
        )

    def on_activated(self):
        self.dbg("ignores-should-terminate activated")

    def on_deactivated(self):
        self.dbg("ignores-should-terminate deactivated")

    def on_events(self, flags):
        if flags & kp.Events.PACKCONFIG:
            self.busy_steps = self._configured_steps()
            self.on_catalog()

    # -- helpers -----------------------------------------------------------

    def _configured_steps(self):
        try:
            settings = self.load_settings()
            configured = settings.get_int("busy_steps", "main", fallback=DEFAULT_BUSY_STEPS)
        except (kp.HostUnavailableError, kp.KeypirinhaError):
            return DEFAULT_BUSY_STEPS
        return max(1, min(int(configured), MAX_BUSY_STEPS))

    def _catalog_items(self):
        return [
            self.create_item(
                category=kp.ItemCategory.KEYWORD,
                label=label,
                short_desc="Catalog entry of the late-answering fixture",
                target="ignores-should-terminate/catalog/{}".format(index),
                args_hint=kp.ItemArgsHint.FORBIDDEN,
                hit_hint=kp.ItemHitHint.NOARGS,
            )
            for index, label in enumerate(CATALOG_LABELS)
        ]

    def _suggestion(self, user_input, checksum):
        # The payload varies with the query: this fixture breaks the termination
        # rule, not the no-caching rule of spec 14.9.
        query = user_input if user_input else "<empty>"
        return self.create_item(
            category=kp.ItemCategory.KEYWORD,
            label="late suggestion for " + query,
            short_desc="computed over {} uninterruptible steps".format(self.busy_steps),
            target="ignores-should-terminate/suggest/{query}/{checksum}".format(
                query=query, checksum=checksum
            ),
            args_hint=kp.ItemArgsHint.ACCEPTED,
            hit_hint=kp.ItemHitHint.IGNORE,
        )
