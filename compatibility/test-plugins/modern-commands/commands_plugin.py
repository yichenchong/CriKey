"""Explicit executable declarations on the catalog-producing plugin instance."""

import asyncio
import json
import os
from pathlib import Path
import time
from dataclasses import replace

from crikey_sdk import Action, Command, Item, Plugin, Preview, PreviewValue

PACKAGE_ROOT = Path(__file__).resolve().parent


def integration_catalog():
    return [
        Item("ui-shared", "Shared catalog item", "canonical shared",
             actions=[Action("primary", "Primary", execution_policy="plugin"),
                      Action("alternate", "Alternate", execution_policy="plugin")],
             argument_policy="optional", preview_deferred=True,
             search_terms=["command-ui-conflict"]),
        Item("ui-actionless", "Actionless catalog item", "", argument_policy="optional"),
        Item("ui-required", "Required catalog item", "",
             actions=[Action("primary", "Primary", execution_policy="plugin")],
             argument_policy="required"),
        Item("ui-forbidden", "Forbidden catalog item", "",
             actions=[Action("primary", "Primary", execution_policy="plugin")],
             argument_policy="forbidden"),
    ]


def integration_commands():
    return [
        Command("shared-a", "Shared command", "ui-shared", "primary", "optional"),
        Command("shared-b", "Shared command", "ui-shared", "primary", "optional"),
        Command("alternate", "Alternate command", "ui-shared", "alternate", "optional"),
        Command("actionless", "Actionless command", "ui-actionless", argument_policy="optional"),
        Command("required", "Required command", "ui-required", "primary", "required"),
        Command("forbidden", "Forbidden command", "ui-forbidden", "primary", "forbidden"),
    ]


def record_integration_execution(item, action_id, argument):
    if item.stable_id == "ui-shared":
        valid = action_id in ("primary", "alternate", "suggestion-action")
    elif item.stable_id == "ui-actionless":
        valid = action_id is None
    elif item.stable_id == "ui-required":
        valid = action_id == "primary" and argument is not None and argument != ""
    elif item.stable_id == "ui-forbidden":
        valid = action_id == "primary" and argument is None
    else:
        valid = False
    if not valid:
        raise ValueError("unknown integration command target or argument policy")
    append_witness("command-ui-executions.log",
                   {"item": item.stable_id, "action": action_id, "argument": argument})


def append_witness(path, payload):
    line = (json.dumps(payload, ensure_ascii=False, separators=(",", ":")) + "\n").encode("utf-8")
    with (PACKAGE_ROOT / path).open("ab") as stream:
        if os.fstat(stream.fileno()).st_size + len(line) > 1024 * 1024:
            raise RuntimeError("command fixture witness log reached 1 MiB")
        stream.write(line)
        stream.flush()
        os.fsync(stream.fileno())


class CommandsPlugin(Plugin):
    def __new__(cls):
        # Advertise preview_v1 only in the opt-in integration instance. The
        # original default/test classes keep their previous capabilities.
        try:
            integration = (PACKAGE_ROOT / "command-ui-mode").read_text(encoding="utf-8").strip() == "integration"
        except FileNotFoundError:
            integration = False
        instance_type = IntegrationCommandsPlugin if cls is CommandsPlugin and integration else cls
        instance = object.__new__(instance_type)
        instance.integration_mode = integration
        return instance

    def __init__(self):
        self.catalog_item = None
        self.mode = "integration" if self.integration_mode else "valid"

    def build_catalog(self):
        if self.integration_mode:
            items = integration_catalog()
            self.catalog_item = items[0]
            return items
        self.catalog_item = Item(
            stable_id="catalog-target", label="Catalog command", target="fixture",
            actions=[Action("alternate", "Alternate", execution_policy="plugin")],
            argument_policy="optional",
        )
        return [self.catalog_item]

    def suggest(self, query, context):
        if self.integration_mode:
            if query.text == "command-ui-conflict":
                context.emit(Item("ui-shared", "Conflicting live suggestion",
                                  "suggestion, not canonical catalog",
                                  actions=[Action("suggestion-action", "Suggestion action",
                                                  execution_policy="plugin")],
                                  argument_policy="forbidden", preview_deferred=True))
            elif query.text == "command-ui-fail":
                raise RuntimeError("integration suggestion failure")
            elif query.text == "command-ui-pending":
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    if context.cancelled:
                        return
                    time.sleep(0.01)
            return
        self.mode = query.text
        context.emit(Item(stable_id="suggest-target", label="Suggestion", target="fixture"))

    def commands(self):
        if self.catalog_item is None:
            raise RuntimeError("catalog must run on this instance first")
        if self.integration_mode:
            return integration_commands()
        base = Command("durable-primary", "Declared primary", self.catalog_item.stable_id,
                       argument_policy="optional")
        mode = self.mode
        if mode == "empty":
            return []
        if mode == "limits":
            return [Command("é" * 128, "é" * 4096, "i" * 256, "a" * 256)]
        if mode == "max-count":
            return (replace(base, command_id=str(i), label="x") for i in range(10000))
        if mode == "exact-aggregate":
            # 510 full labels, then one remainder: exactly 4 MiB of fields.
            used = 0
            declarations = []
            for i in range(511):
                identity = str(i)
                overhead = len(identity) + len(base.item_id)
                size = min(8192, 4 * 1024 * 1024 - used - overhead)
                declarations.append(replace(base, command_id=identity, label="x" * size))
                used += overhead + size
            assert used == 4 * 1024 * 1024
            return declarations
        if mode == "malformed":
            return [{"command_id": "not-a-command"}]
        if mode == "duplicate":
            return [base, base]
        if mode == "control":
            return [replace(base, item_id="bad\nitem")]
        if mode == "unicode-id":
            return [replace(base, command_id="é" * 129)]
        if mode == "policy":
            return [replace(base, argument_policy="shell")]
        if mode == "label":
            return [replace(base, label="é" * 4097)]
        if mode == "count":
            return (replace(base, command_id=str(i)) for i in range(10001))
        if mode == "aggregate":
            return (replace(base, command_id=str(i), label="x" * 8192) for i in range(513))
        if mode == "escaped-frame":
            return (replace(base, command_id=str(i), label="\x01" * 8192) for i in range(200))
        if mode == "raises":
            raise RuntimeError("x" * 100000)
        return [base, Command("durable-alternate", "Declared alternate",
                              self.catalog_item.stable_id, "alternate", "required")]

    def execute(self, item, action_id, argument):
        if self.integration_mode:
            return record_integration_execution(item, action_id, argument)
        return super().execute(item, action_id, argument)


class IntegrationCommandsPlugin(CommandsPlugin):
    def preview(self, item, context):
        if context.cancelled:
            return None
        if item.stable_id != "ui-shared":
            return None
        if item.target == "canonical shared":
            if self.catalog_item is None or self.catalog_item.target != "canonical shared":
                raise RuntimeError("preview requires this catalog's producing worker")
            marker = "COMMAND_UI_CATALOG"
            link = "https://example.invalid/command-fixture"
        elif item.target == "suggestion, not canonical catalog":
            marker = "COMMAND_UI_SUGGESTION"
            link = "https://example.invalid/command-fixture-suggestion"
        else:
            raise RuntimeError("unknown preview source")
        append_witness("command-ui-previews.log",
                       {"item": item.stable_id, "marker": marker, "link": link, "pid": os.getpid()})
        return Preview(text=marker, values=(PreviewValue("Documentation", link, "link"),))


class AsyncCommands(CommandsPlugin):
    async def commands(self):
        await asyncio.sleep(0)
        return super().commands()


class WithoutCommands(Plugin):
    def build_catalog(self):
        # Metadata and labels do not opt into executable declarations.
        return [Item("not-declared", "Declared primary", "fixture",
                     metadata={"command_id": "durable-primary"})]


class HandoffCommands(Plugin):
    """Real cooperative preview and foreground occupancy witnesses."""

    def build_catalog(self):
        self.canonical = Item("handoff", "Canonical handoff", "frozen canonical",
                              actions=[Action("run", "Run", execution_policy="plugin")],
                              argument_policy="optional", preview_deferred=True)
        return [self.canonical,
                Item("latest", "Latest selection", "latest", preview_deferred=True)]

    def commands(self):
        return [Command("handoff", "Handoff", "handoff", "run", "optional")]

    def preview(self, item, context):
        if item.stable_id == "latest":
            (PACKAGE_ROOT / "latest-preview").touch()
            return Preview(text="Latest selection preview")
        assert item.stable_id == self.canonical.stable_id and item.target == self.canonical.target
        (PACKAGE_ROOT / "preview-entered").touch()
        deadline = time.monotonic() + 20
        while not context.cancelled:
            if time.monotonic() >= deadline:
                raise RuntimeError("foreground cancellation not delivered")
            time.sleep(0.001)
        (PACKAGE_ROOT / "preview-cancelled").touch()
        while not (PACKAGE_ROOT / "preview-release").exists():
            if time.monotonic() >= deadline:
                raise RuntimeError("preview terminal release not delivered")
            time.sleep(0.001)
        (PACKAGE_ROOT / "preview-terminal").touch()
        return None

    def execute(self, item, action_id, argument):
        assert item.stable_id == self.canonical.stable_id and item.target == self.canonical.target
        assert action_id == "run"
        append_witness(PACKAGE_ROOT / "executions",
                       {"target": item.target, "action": action_id, "argument": argument})
        deadline = time.monotonic() + 20
        while ((PACKAGE_ROOT / "action-hold").exists()
               and not (PACKAGE_ROOT / "action-release").exists()):
            if time.monotonic() >= deadline:
                raise RuntimeError("foreground release not delivered")
            time.sleep(0.001)
