"""Real grid/detail/list/form routes with exact, deliberately colliding item IDs."""
import asyncio
import os
from pathlib import Path
from crikey_sdk import (
    Plugin, Item, Action, Command, Push, Replace, ViewDescriptor,
    RouteItemDefinition, ActionSection, ExecutablePresentation, ActionSubmenu,
    GridView, ListView, DetailView, FormView, ItemPresentation, Section, Scope,
    Preview, PreviewImage, PreviewValue, ArgumentSchema, TextField, PasswordField,
    DropdownField, DropdownOption, TypedOutcome, FieldFailure, ResultPresentation, SchemaBinding,
)

SCHEMA = ArgumentSchema((
    TextField(id="name", label="Name", required=True, max_bytes=128,
              guidance="Use reject to exercise a field rejection."),
    PasswordField(id="password", label="Password", required=True, max_bytes=128),
    DropdownField(id="region", label="Region", required=True,
                  options=(DropdownOption("local", "Local"), DropdownOption("remote", "Remote")),
                  default="local"),
))
PIXELS = PreviewImage(2, 2, bytes((48, 110, 190, 255)) * 4)


def definition(route, *, action=None, schema=None, argument_policy="forbidden"):
    item = Item("shared", route.title() + " local item", route,
                description="The same stable ID has different route-local authority.",
                argument_policy=argument_policy,
                actions=[] if action is None else [Action(action, action.title(), execution_policy="plugin")])
    presentation = ExecutablePresentation(action, schema)
    return RouteItemDefinition(item,
        action_sections=(ActionSection("main", "Actions", (
            ActionSubmenu("commands", "Route commands", (presentation,)),)),),
        actionless_policy="plugin" if action is None else None,
        primary_action=action)


class ViewsPlugin(Plugin):
    def build_catalog(self):
        return [Item("shared", "Open standard view fixture", "root"),
                Item("typed-root", "Typed route opener", "typed-root")]

    def commands(self):
        return [Command("views", "Open standard view fixture", "shared"),
                Command("typed-views", "Typed route opener", "typed-root")]

    def suggest(self, query, context):
        for item in self.build_catalog():
            context.emit(item)

    def catalog_presentation(self):
        return ResultPresentation(schemas=(SchemaBinding("typed-root", SCHEMA),))

    def suggestion_presentation(self, query):
        return self.catalog_presentation()

    async def execute_typed(self, item, action_id, values):
        assert item.stable_id == "typed-root" and item.target == "typed-root" and action_id is None
        await asyncio.sleep(0)
        return Push("gallery")

    def execute(self, item, action_id, argument):
        assert item.stable_id == "shared" and item.target == "root"
        assert action_id is None and argument is None
        return Push("gallery")

    async def structured_view(self, request, context):
        if request.query == "source-death":
            os._exit(19)
        if request.query == "delay":
            Path(__file__).with_name("route-running").touch()
            for _ in range(100):
                if context.cancelled:
                    break
                await asyncio.sleep(0.01)
        if request.query == "coalesce":
            loop = asyncio.get_running_loop()
            deadline = loop.time() + 0.25
            release = Path(__file__).with_name("route-coalesce-release")
            Path(__file__).with_name("route-coalesce-running").touch()
            while not release.is_file():
                if context.cancelled:
                    break
                if loop.time() >= deadline:
                    raise RuntimeError("coalesced refresh barrier expired")
                await asyncio.sleep(0.001)
        route = request.route
        if route == "gallery":
            if request.query == "remove-selected":
                return ViewDescriptor("Route gallery (remove-selected)", (), GridView(
                    sections=(Section("entries", "Entries"),),
                    scopes=(Scope("visible", "Visible"), Scope("empty", "Empty")), columns=3))
            local = definition("gallery", action="open")
            image = PIXELS if request.query != "oversized-image" else PreviewImage(1024, 1024, bytes(1024 * 1024 * 4))
            title = "Route gallery" + (f" ({request.query})" if request.query else "")
            return ViewDescriptor(title, (local,), GridView(
                items=(ItemPresentation("shared", "entries", ("visible",), image),),
                sections=(Section("entries", "Entries"),), scopes=(Scope("visible", "Visible"), Scope("empty", "Empty")),
                initial_selection="shared", columns=3))
        if route == "detail":
            return ViewDescriptor("Route detail", (definition("detail", action="configure"),),
                DetailView(Preview(text="# Safe route detail\nOpen the configuration form from Actions.",
                                   markdown=True, image=PIXELS,
                                   values=(PreviewValue("Reference", "https://example.org/", "link"),)), "shared"))
        if route == "list":
            return ViewDescriptor("Route list", (definition("list"),),
                ListView(items=(ItemPresentation("shared"),), initial_selection="shared"))
        if route == "required":
            return ViewDescriptor("Required argument", (definition("required", argument_policy="required"),),
                ListView(items=(ItemPresentation("shared"),), initial_selection="shared"))
        if route == "form":
            return ViewDescriptor("Route form", (definition("form", schema=SCHEMA),), FormView("shared"))
        if route == "done":
            return ViewDescriptor("Submission completed", (definition("done"),),
                ListView(items=(ItemPresentation("shared"),), initial_selection="shared"))
        raise ValueError("unknown fixture route")

    async def execute_view(self, route, item, action_id, argument):
        assert item.stable_id == "shared" and item.target == route
        if route == "required":
            assert action_id is None and argument == "provided"
            return Replace("list")
        assert argument is None
        await asyncio.sleep(0)
        if route == "gallery":
            assert action_id == "open"
            return Push("detail")
        if route == "detail":
            assert action_id == "configure"
            return Push("form")
        if route == "list":
            assert action_id is None
            return Replace("detail")
        if route == "done":
            assert action_id is None
            return Replace("list")
        raise AssertionError("unexpected route execution")

    async def execute_view_typed(self, route, item, action_id, values):
        assert route == "form" and item.target == "form" and action_id is None
        await asyncio.sleep(0)
        if values.get("name").value == "reject":
            return TypedOutcome(failures=(FieldFailure("name"),))
        # Deliberately exercise the process-wide diagnostic latch on live input.
        print(values.get("password").value)
        return Replace("done")
