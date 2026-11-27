"""Real callbacks gated by package-adjacent enable-refresh/refresh-mode sidecars.

The host publishes suggestions after this callback returns, not at each emit.
The catalog seed supplies the selectable target while the query waits. Observe
suggestions.jsonl for waiting/returning records with the same query generation;
the distinct returned labels and second row prove actual host publication.
"""

import asyncio
import json
from pathlib import Path

from crikey_sdk import Action, Item, Plugin


def row(item_id, *, without_reveal=False, generation=None, mode="initial"):
    actions = [Action("run", "Run fixture", execution_policy="plugin"),
               Action("copy", "Copy fixture", execution_policy="plugin")]
    if not without_reveal:
        actions.append(Action("reveal", "Reveal fixture", execution_policy="plugin"))
    suffix = "" if generation is None else f" ({mode} g{generation})"
    return Item(item_id, "Action fixture " + item_id + suffix, item_id,
                description="Search Actions for Reveal; execution writes the exact callback identity",
                actions=actions)


def record_suggestion(query, phase, **details):
    with Path(__file__).with_name("suggestions.jsonl").open("a", encoding="utf-8") as stream:
        stream.write(json.dumps({"query": query.text, "generation": query.generation,
                                 "phase": phase, **details}) + "\n")


class ActionFixture(Plugin):
    def build_catalog(self):
        return [row("shared")]

    async def suggest(self, query, context):
        if "action" not in query.text.lower():
            return
        mode = "initial"
        if Path(__file__).with_name("enable-refresh").exists():
            gate = Path(__file__).with_name("refresh-mode")
            record_suggestion(query, "waiting")
            while not gate.exists():
                # SuggestContext.cancelled is a property, not a callback.
                if context.cancelled:
                    record_suggestion(query, "cancelled")
                    return
                await asyncio.sleep(0.05)
            mode = gate.read_text(encoding="utf-8").strip()
            if mode not in ("refresh", "remove-action"):
                raise ValueError("Refresh gate must contain refresh or remove-action")
        if context.cancelled:
            record_suggestion(query, "cancelled")
            return
        items = []
        if mode != "initial":
            items.append(row("other", generation=query.generation, mode=mode))
        items.append(row("shared", without_reveal=mode == "remove-action",
                         generation=query.generation, mode=mode))
        for item in items:
            context.emit(item)
        record_suggestion(query, "returning", mode=mode,
                          items=[{"item": item.stable_id,
                                  "actions": [action.action_id for action in item.actions]}
                                 for item in items])

    def execute(self, item, action_id, argument):
        destination = Path(__file__).with_name("actions.jsonl")
        with destination.open("a", encoding="utf-8") as stream:
            stream.write(json.dumps({"fixture": Path(__file__).parent.name,
                                     "item": item.stable_id,
                                     "action": action_id,
                                     "argument": argument}) + "\n")
