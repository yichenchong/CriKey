"""Legacy plugin that memoizes its dynamic suggestions across requests.

Spec 14.9 and acceptance 31.18 forbid this under the default profile: a legacy
plugin's dynamic suggestions must be recomputed per request, because a cached
answer is indistinguishable from a stale one (spec 8.5) and the user cannot tell
which they are looking at. `crikey dev test-legacy-compat` must fail this
package on `dynamic_suggestions_not_cached` and on nothing else.

The defect is real, not declared. `on_suggest` fills a module-level memo on the
first request and republishes it verbatim for every later query, so two
different queries produce a byte-identical payload — which is precisely what the
host compares. It is identical to `well-behaved` in every other respect,
including the unconditional `should_terminate()` poll, so exactly one check may
fail here.

Determinism: no wall clock, no randomness, no network, no absolute paths.
"""

import keypirinha as kp

#: The memo, keyed by plugin id. Its lifetime is the worker process, which is
#: exactly the problem: nothing in it is ever invalidated by a new query.
#:
#: Bounded by construction (a defect fixture is still not licensed to leak).
#: Overflow behaviour: once `CACHE_CAPACITY` distinct plugin ids are held, new
#: ids are simply not admitted and recompute on every request. That degrades
#: towards correct behaviour rather than towards unbounded growth, and the
#: single instance a conformance run creates never reaches the cap.
_SUGGESTION_MEMO = {}

#: Default number of plugin ids the memo may hold.
CACHE_CAPACITY = 16

#: Ceiling on the configured capacity, so a configuration edit cannot turn the
#: bound into a suggestion.
MAX_CACHE_CAPACITY = 256

#: Iterations of the cooperative work loop, matching `well-behaved`: this
#: fixture breaks the caching rule, not the termination rule.
SUGGEST_STEPS = 512

CATALOG_LABELS = (
    "Caches Dynamic Suggestions Alpha",
    "Caches Dynamic Suggestions Beta",
)


class CachesDynamicSuggestions(kp.Plugin):
    """A legacy plugin whose dynamic answers outlive the query that produced them."""

    def __init__(self):
        kp.Plugin.__init__(self)
        self.cache_capacity = CACHE_CAPACITY

    # -- lifecycle ---------------------------------------------------------

    def on_start(self):
        self.cache_capacity = self._configured_capacity()
        self.info(
            "caches-dynamic-suggestions ready with cache_capacity={}".format(
                self.cache_capacity
            )
        )

    def on_catalog(self):
        # A *catalog* is allowed to persist — it is static by definition
        # (spec 14.8). Only dynamic suggestions may not be cached, and
        # conflating the two is the mistake this fixture is calibrated against.
        self.set_catalog(self._catalog_items())

    def on_suggest(self, user_input, items_chain):
        # The cooperative poll comes FIRST, before the memo is even consulted:
        # an obsolete request must abandon rather than serve a warm answer, or
        # this fixture would fail `should_terminate_observed` too and stop
        # isolating the one rule it exists to break (spec 9.2).
        for _step in range(SUGGEST_STEPS):
            if self.should_terminate():
                return

        # THE DEFECT (spec 14.9, acceptance 31.18). The memo is keyed by plugin
        # id and never by the query, so the first request's answer is the answer
        # to every request. This is the real shape of the bug in the wild: a
        # plugin memoizes "the expensive list" and forgets that the list was a
        # function of the user's input.
        memoized = _SUGGESTION_MEMO.get(self.id)
        if memoized is not None:
            self.set_suggestions(memoized, kp.Match.DEFAULT, kp.Sort.DEFAULT)
            return

        computed = [self._suggestion(user_input)]
        self._admit(computed)
        self.set_suggestions(computed, kp.Match.DEFAULT, kp.Sort.DEFAULT)

    def on_execute(self, item, action):
        self.info(
            "caches-dynamic-suggestions executed {label!r} via {action}".format(
                label=item.label(),
                action="<default>" if action is None else action.name(),
            )
        )

    def on_activated(self):
        self.dbg("caches-dynamic-suggestions activated")

    def on_deactivated(self):
        self.dbg("caches-dynamic-suggestions deactivated")

    def on_events(self, flags):
        if flags & kp.Events.PACKCONFIG:
            self.cache_capacity = self._configured_capacity()
            self.on_catalog()

    # -- helpers -----------------------------------------------------------

    def _configured_capacity(self):
        try:
            settings = self.load_settings()
            configured = settings.get_int("cache_capacity", "main", fallback=CACHE_CAPACITY)
        except (kp.HostUnavailableError, kp.KeypirinhaError):
            return CACHE_CAPACITY
        return max(1, min(int(configured), MAX_CACHE_CAPACITY))

    def _admit(self, items):
        # Refusing to admit past the cap keeps the memo bounded without evicting
        # an entry another instance may be about to serve. Documented overflow
        # behaviour, not an accident.
        if self.id in _SUGGESTION_MEMO or len(_SUGGESTION_MEMO) < self.cache_capacity:
            _SUGGESTION_MEMO[self.id] = items

    def _catalog_items(self):
        return [
            self.create_item(
                category=kp.ItemCategory.KEYWORD,
                label=label,
                short_desc="Catalog entry of the memoizing fixture",
                target="caches-dynamic-suggestions/catalog/{}".format(index),
                args_hint=kp.ItemArgsHint.FORBIDDEN,
                hit_hint=kp.ItemHitHint.NOARGS,
            )
            for index, label in enumerate(CATALOG_LABELS)
        ]

    def _suggestion(self, user_input):
        query = user_input if user_input else "<empty>"
        return self.create_item(
            category=kp.ItemCategory.KEYWORD,
            label="cached suggestion for " + query,
            short_desc="computed once and reused for every later query",
            target="caches-dynamic-suggestions/suggest/" + query,
            args_hint=kp.ItemArgsHint.ACCEPTED,
            hit_hint=kp.ItemHitHint.IGNORE,
        )
