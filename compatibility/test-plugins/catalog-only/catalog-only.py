"""Legacy plugin that publishes a fixed three-item catalog and nothing else.

This is the fixture `crikey dev inspect-catalog` is pointed at, so its job is to
be a stable, fully specified catalog rather than an interesting scheduler
client (spec 26.3, 10.1).

One label is deliberately awkward:

    Deterministic Fixture Item #2 (50% = half)

It holds a space, an `=` and a `%` — the three characters that would each break
the command's `key=value` output format in a different way. Legacy item labels
are written by plugin authors, not by us, so the encoding has to be exercised by
a value a plugin really published rather than only by an assertion in a test.
The string is written out in full here, once, and is the fixture's single source
of truth for it.

Determinism: no wall clock, no randomness, no network, no absolute paths.
"""

import keypirinha as kp

#: Package-relative resource holding one short description per catalog item.
#: Reading it is what exercises the loader's module/resource split (spec 14.3)
#: with real data. Descriptions rather than labels live out here on purpose: a
#: resource that failed to load must never be able to disturb the label the
#: encoding test depends on.
DESCRIPTION_RESOURCE = "data/descriptions.txt"

#: The three items, in publication order. `inspect-catalog` numbers items from
#: zero in exactly this order.
#:
#: Each tuple is (label, category, args_hint, hit_hint, target). The targets are
#: distinct, which is what keeps the host-derived item identities distinct: two
#: items sharing an identity means neither can be selected reliably (spec 10.2).
CATALOG = (
    (
        "Deterministic Fixture Item #1",
        kp.ItemCategory.KEYWORD,
        kp.ItemArgsHint.FORBIDDEN,
        kp.ItemHitHint.NOARGS,
        "catalog-only/item/1",
    ),
    (
        "Deterministic Fixture Item #2 (50% = half)",
        kp.ItemCategory.CMDLINE,
        kp.ItemArgsHint.ACCEPTED,
        kp.ItemHitHint.KEEPALL,
        "catalog-only/item/2",
    ),
    (
        "Deterministic Fixture Item #3",
        kp.ItemCategory.FILE,
        kp.ItemArgsHint.REQUIRED,
        kp.ItemHitHint.IGNORE,
        "catalog-only/item/3",
    ),
)

#: Used when the host exposes no resource capability, and padded out to the
#: catalog length if the resource is short. Descriptions are never empty,
#: because an item the report prints with an empty description is legal and
#: unreadable.
DESCRIPTION_FALLBACK = "Deterministic catalog fixture entry"


class CatalogOnly(kp.Plugin):
    """A legacy plugin with a catalog and no dynamic suggestions."""

    def on_start(self):
        self.info("catalog-only ready with {} items".format(len(CATALOG)))

    def on_catalog(self):
        # Spec 14.8: one complete publication, and a rebuild replaces rather
        # than accumulates.
        self.set_catalog(self._catalog_items())

    def on_suggest(self, user_input, items_chain):
        # This plugin answers no suggestions: everything it offers is static and
        # already in the catalog. It still reads the cooperative flag once, so
        # that pointing the conformance suite at it does not misreport a plugin
        # with nothing to abandon as one that refuses to abandon anything
        # (spec 9.2, acceptance 31.17).
        #
        # Publishing nothing at all — rather than an empty list — is the honest
        # encoding of "I have no dynamic answers": an empty publication is a
        # complete publication that clears the list, which is a different claim.
        self.should_terminate()

    def on_execute(self, item, action):
        self.info(
            "catalog-only executed {label!r} via {action}".format(
                label=item.label(),
                action="<default>" if action is None else action.name(),
            )
        )

    def on_activated(self):
        self.dbg("catalog-only activated")

    def on_deactivated(self):
        self.dbg("catalog-only deactivated")

    def on_events(self, flags):
        if flags & kp.Events.PACKCONFIG:
            self.on_catalog()

    # -- helpers -----------------------------------------------------------

    def _descriptions(self):
        try:
            text = self.load_text_resource(DESCRIPTION_RESOURCE)
        except (kp.HostUnavailableError, kp.KeypirinhaError):
            # `load_text_resource` is an optional host capability (spec 14.12).
            lines = []
        else:
            lines = [line.strip() for line in text.splitlines()]
        # Bounded by the catalog, never by the file: a longer resource can add
        # nothing, and a shorter one falls back per item rather than leaving a
        # description empty.
        return [
            lines[index] if index < len(lines) and lines[index] else DESCRIPTION_FALLBACK
            for index in range(len(CATALOG))
        ]

    def _catalog_items(self):
        descriptions = self._descriptions()
        return [
            self.create_item(
                category=category,
                label=label,
                short_desc=descriptions[index],
                target=target,
                args_hint=args_hint,
                hit_hint=hit_hint,
            )
            for index, (label, category, args_hint, hit_hint, target) in enumerate(CATALOG)
        ]
