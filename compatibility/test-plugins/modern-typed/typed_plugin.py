"""Real PR7 root typed arguments, grouping and scope fixture; no standard routes."""
import asyncio
import os
import sys
import threading
from dataclasses import replace
from crikey_sdk import (
    Plugin, Item, Action, Command, TextField, PasswordField, DropdownField,
    DropdownOption, ArgumentSchema, SchemaBinding, ResultPresentation,
    Section, Scope, ItemPresentation, TypedOutcome, FieldFailure,
)

SCHEMA = ArgumentSchema((
    TextField(id="name", label="Name", required=True, min_bytes=1,
              guidance="Enter a name; use reject to exercise field refusal."),
    PasswordField(id="password", label="Password", required=True, min_bytes=1,
                  guidance="Enter a password. It is never recorded."),
    DropdownField(id="region", label="Region", required=True,
                  options=(DropdownOption("local", "Local"), DropdownOption("remote", "Remote")),
                  default="local"),
))


class TypedPlugin(Plugin):
    def __init__(self):
        self._secret = None

    def build_catalog(self):
        return [Item("typed-actionless", "Typed actionless command", "typed-actionless"),
                Item("typed-action", "Typed action command", "typed-action",
                     actions=[Action("submit", "Submit typed values", execution_policy="plugin")])]

    def commands(self):
        return [Command("typed-actionless", "Typed actionless command", "typed-actionless"),
                Command("typed-action", "Typed action command", "typed-action", action_id="submit")]

    def supports_result_presentation(self):
        return True

    def catalog_presentation(self):
        return ResultPresentation(
            sections=(Section("typed", "Typed commands"),),
            scopes=(Scope("local", "Local typed commands"), Scope("remote", "Remote typed commands")),
            items=(ItemPresentation("typed-actionless", "typed", ("local",)),
                   ItemPresentation("typed-action", "typed", ("remote",))),
            schemas=(SchemaBinding("typed-actionless", SCHEMA),
                     SchemaBinding("typed-action", SCHEMA, "submit")),
        )

    def suggestion_presentation(self, query):
        presentation = self.catalog_presentation()
        if query.selected_scope_id is None:
            return presentation
        allowed = {entry.item_id for entry in presentation.items
                   if query.selected_scope_id in entry.scopes}
        return replace(presentation,
                       items=tuple(entry for entry in presentation.items if entry.item_id in allowed),
                       schemas=tuple(entry for entry in presentation.schemas if entry.item_id in allowed))

    async def suggest(self, query, context):
        if query.text == "typed-crash-after" and self._secret is not None:
            os.write(2, self._secret.encode("utf-8"))
            os._exit(3)
        if query.text == "typed-leak-after" and self._secret is not None:
            async def later():
                print(self._secret)
                raise RuntimeError(self._secret)
            context.spawn(later())
            context.log(self._secret)
            raise RuntimeError(self._secret)
        if query.text == "typed-scope-wait":
            while not context.cancelled:
                await asyncio.sleep(0.01)
            return
        for item in self.build_catalog():
            if query.selected_scope_id == "local" and item.stable_id != "typed-actionless":
                continue
            if query.selected_scope_id == "remote" and item.stable_id != "typed-action":
                continue
            context.emit(item)

    async def execute_typed(self, item, action_id, values):
        assert (item.stable_id, action_id) in (("typed-actionless", None), ("typed-action", "submit"))
        name = values.get("name").value
        if name == "reject":
            return TypedOutcome(failures=(FieldFailure("name", "rejected"),))
        if name == "leak":
            self._secret = values.get("password").value
            print(self._secret)
            sys.stderr.write(self._secret)
            os.write(2, self._secret.encode("utf-8"))
            thread = threading.Thread(target=lambda: print(self._secret))
            thread.start(); thread.join()
            await asyncio.sleep(0)
            raise RuntimeError(self._secret)
        return None

    def execute(self, item, action_id, argument):
        raise AssertionError("typed execution must never reach legacy execute")

    def on_configuration(self, values):
        if self._secret is not None:
            raise RuntimeError(self._secret)

    def stop(self):
        if self._secret is not None:
            raise RuntimeError(self._secret)
