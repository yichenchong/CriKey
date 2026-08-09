"""Reference legacy plugin for the presentation APIs: actions, icons, resources.

Written against the documented Keypirinha-compatible API the Legacy
Compatibility Layer ships (spec 14.2, 14.4). Where `well-behaved` is the control
for *scheduling*, this one is the control for everything a row carries once it
reaches the launcher: the alternate actions `set_actions` registers, the icon
`load_icon` names, the error item `create_error_item` builds, and the
package-relative names `find_resources` reports.

It breaks no rule, so it must pass every scheduling conformance check exactly as
the control does.

Determinism is a hard requirement of the whole synthetic suite (spec 26.3): no
wall clock, no randomness, no network, no absolute paths. In particular the
resource pattern below is narrow on purpose — an interpreter that writes
`__pycache__` into the package would make a `**` pattern report a different list
on the second run, and a fixture whose output depends on whether bytecode was
cached tests the cache, not the layer.
"""

import keypirinha as kp

#: Category every item here belongs to, and the one the alternate actions are
#: registered against. A single category keeps "the actions reached the items of
#: *this* category" a statement one item can falsify.
CATEGORY = kp.ItemCategory.KEYWORD

#: Package-relative icon this plugin loads and sets as its default. Committed
#: alongside the module, so the handle names a file that really exists and the
#: host really decodes.
ICON_SOURCE = "icons/badge.png"

#: The same icon named the documented `res://Package/file` way. Both spellings
#: must resolve to one reference, or a package that uses the documented form
#: would silently lose its icon.
ICON_RESOURCE_URL = "res://rich-presentation/icons/badge.png"

#: Pattern whose matches are fixed by what is committed here. One directory
#: deep, so it cannot pick up anything an interpreter dropped at the top level.
RESOURCE_PATTERN = "icons/*.png"

#: A pattern that tries to leave the package. The layer must refuse it; the
#: fixture reports which of the two happened as an item, so the test observes
#: the refusal through the same channel as everything else rather than through
#: a side effect.
ESCAPING_PATTERN = "../*/*.py"

#: Labels the escape probe publishes. Distinct strings, because a test that
#: asserted only the presence of one of them would pass on an empty result.
ESCAPE_REFUSED_LABEL = "find_resources refused an escaping pattern"
ESCAPE_ESCAPED_LABEL = "find_resources escaped the package"

#: Names of the two alternate actions. `on_execute` echoes whichever one the
#: host delivered, so the pair also proves the host picked the right one rather
#: than merely picking an action.
COPY_ACTION = "copy"
REVEAL_ACTION = "reveal"

#: What `on_execute` logs when it was handed no action at all, which is what the
#: default (Enter) means: Keypirinha spells "no secondary action chosen" as
#: `None`, and a plugin distinguishes it from every named action.
DEFAULT_ACTION_MARK = "<default>"


class RichPresentation(kp.Plugin):
    """A legacy plugin whose rows carry actions, an icon and error items."""

    def __init__(self):
        kp.Plugin.__init__(self)
        #: Resolved once in `on_start`. Held on the instance because the handle
        #: is what `create_item` takes; recomputing it per item would ask the
        #: host to re-validate the same file on every keystroke.
        self._icon = None

    # -- lifecycle ---------------------------------------------------------

    def on_start(self):
        self._icon = self.load_icon(ICON_SOURCE)
        # The documented `res://` spelling of the same file. Loading it proves
        # the form is honoured; the item below uses the plain handle, so the
        # two are not silently interchangeable in the assertions either.
        self.load_icon([ICON_RESOURCE_URL])
        self.set_default_icon(self._icon)
        self.set_actions(
            CATEGORY,
            [
                self.create_action(COPY_ACTION, "Copy", "Copy the target"),
                self.create_action(REVEAL_ACTION, "Reveal", "Show where the target lives"),
            ],
        )

    def on_catalog(self):
        self.set_catalog([self._entry()])

    def on_suggest(self, user_input, items_chain):
        # Polled unconditionally and before any work, exactly as the control
        # fixture does: this package must pass the scheduling checks too.
        if self.should_terminate():
            return
        self.set_suggestions([self._entry(), self._resources_item(), self._escape_probe()])

    def on_execute(self, item, action):
        # The fixture's only observable. The worker captures what a plugin
        # writes and returns it inside the reply frame, so a test can assert
        # which action the host delivered without the plugin touching the file
        # system or a clock.
        name = DEFAULT_ACTION_MARK if action is None else action.name()
        self.info("executed", item.target(), "action=" + name)

    # -- items -------------------------------------------------------------

    def _entry(self):
        """The row that carries the loaded icon and inherits the actions."""
        return self.create_item(
            category=CATEGORY,
            label="Rich Presentation Entry",
            short_desc="Carries a loaded icon and two alternate actions",
            target="rich-presentation/entry",
            args_hint=kp.ItemArgsHint.FORBIDDEN,
            hit_hint=kp.ItemHitHint.IGNORE,
            icon_handle=self._icon,
        )

    def _resources_item(self):
        """Reports what `find_resources` found, in the item's own description."""
        found = self.find_resources(RESOURCE_PATTERN)
        return self.create_item(
            category=CATEGORY,
            label="Rich Presentation Resources",
            short_desc=" ".join(found) if found else "(none)",
            target="rich-presentation/resources",
            args_hint=kp.ItemArgsHint.FORBIDDEN,
            hit_hint=kp.ItemHitHint.IGNORE,
        )

    def _escape_probe(self):
        """Reports what the layer did with a pattern that leaves the package.

        An error item on the good path: a refusal is what this probe is for, and
        `create_error_item` is the documented way a plugin says so.
        """
        try:
            escaped = self.find_resources(ESCAPING_PATTERN)
        except kp.HostUnavailableError as error:
            return self.create_error_item(
                label=ESCAPE_REFUSED_LABEL,
                short_desc=str(error),
                target="rich-presentation/escape-refused",
            )
        return self.create_item(
            category=CATEGORY,
            label=ESCAPE_ESCAPED_LABEL,
            short_desc=" ".join(escaped) if escaped else "(nothing, but it was not refused)",
            target="rich-presentation/escape-escaped",
            args_hint=kp.ItemArgsHint.FORBIDDEN,
            hit_hint=kp.ItemHitHint.IGNORE,
        )
