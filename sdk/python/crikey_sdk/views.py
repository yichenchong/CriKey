"""Standard routes use independent structured_view_v1 negotiation.

Wire: structured_view {id,route,session:{logical_view,surface},mode,
previous_revision,reason,state,query,anchor}; structured_view_result echoes
route/session and returns content/descriptor or failed/code. A content reply's
correlation id is its revision. execute/typed_execute optionally carry
view_context {route,session,revision}; these never resolve root definitions.
"""
from dataclasses import dataclass
from .arguments import ArgumentSchema, Section, Scope, ItemPresentation, _text

__all__ = ["Push", "Replace", "ViewSession", "ViewExecutionContext", "ViewState",
           "ViewAnchor", "ViewRequest", "ExecutablePresentation", "ActionSubmenu",
           "ActionSection", "RouteItemDefinition", "ListView", "GridView",
           "DetailView", "FormView", "ViewDescriptor"]


@dataclass(frozen=True)
class Push:
    route: str


@dataclass(frozen=True)
class Replace:
    route: str


@dataclass(frozen=True)
class ViewSession:
    logical_view: int
    surface: int


@dataclass(frozen=True)
class ViewExecutionContext:
    route: str
    session: ViewSession
    revision: int


@dataclass(frozen=True)
class ViewState:
    selected_item_id: str | None = None
    scope_id: str | None = None
    input_item_id: str | None = None
    input_action_id: str | None = None


@dataclass(frozen=True)
class ViewAnchor:
    item_id: str
    row_offset: int = 0


@dataclass(frozen=True, repr=False)
class ViewRequest:
    route: str
    session: ViewSession
    mode: str = "open"
    previous_revision: int | None = None
    reason: str = "explicit"
    state: ViewState | None = None
    query: str = ""
    anchor: ViewAnchor | None = None

    def __repr__(self):
        return "ViewRequest(<host state>)"


@dataclass(frozen=True)
class ExecutablePresentation:
    action_id: str | None = None
    schema: ArgumentSchema | None = None
    shortcut: str | None = None
    destructive: bool = False


@dataclass(frozen=True)
class ActionSubmenu:
    id: str
    label: str
    entries: tuple = ()
    description: str = ""
    icon_reference: str | None = None


@dataclass(frozen=True)
class ActionSection:
    id: str
    label: str
    entries: tuple = ()


@dataclass(frozen=True)
class RouteItemDefinition:
    item: object
    action_sections: tuple[ActionSection, ...] = ()
    actionless_policy: str | None = None
    primary_action: str | None = None


@dataclass(frozen=True)
class ListView:
    items: tuple[ItemPresentation, ...] = ()
    sections: tuple[Section, ...] = ()
    scopes: tuple[Scope, ...] = ()
    initial_selection: str | None = None
    initial_scope: str | None = None


@dataclass(frozen=True)
class GridView(ListView):
    columns: int = 4


@dataclass(frozen=True)
class DetailView:
    content: object
    target_item_id: str | None = None


@dataclass(frozen=True)
class FormView:
    item_id: str
    action_id: str | None = None


@dataclass(frozen=True)
class ViewDescriptor:
    title: str
    definitions: tuple[RouteItemDefinition, ...]
    view: ListView | GridView | DetailView | FormView


def _identity(value):
    _text(value, 256, True)
    return value


def _optional_id(value):
    return None if value is None else _identity(value)


def _session(payload):
    if not isinstance(payload, dict) or getattr(payload, "typed_duplicate", False):
        raise ValueError("invalid view session")
    values = tuple(payload.get(k) for k in ("logical_view", "surface"))
    if any(type(v) is not int or not 0 < v < 2**64 for v in values):
        raise ValueError("invalid view session")
    return ViewSession(*values)


def _request(payload):
    if getattr(payload, "typed_duplicate", False) or type(payload.get("id")) is not int or not 0 < payload["id"] < 2**64:
        raise ValueError("invalid view request correlation")
    route = _identity(payload.get("route"))
    session = _session(payload.get("session"))
    mode, previous = payload.get("mode"), payload.get("previous_revision")
    if mode not in ("open", "refresh") or (mode == "open" and previous is not None) or (mode == "refresh" and (type(previous) is not int or not 0 < previous < 2**64)):
        raise ValueError("invalid view mode or revision")
    reason = payload.get("reason")
    if reason not in ("explicit", "query", "selection", "scope"):
        raise ValueError("invalid view refresh reason")
    state = payload.get("state")
    if mode == "refresh" and state is None:
        raise ValueError("refresh requires current host state")
    if state is not None:
        if not isinstance(state, dict):
            raise ValueError("invalid view state")
        state = ViewState(*(_optional_id(state.get(k)) for k in
                           ("selected_item_id", "scope_id", "input_item_id", "input_action_id")))
        if state.input_action_id is not None and state.input_item_id is None:
            raise ValueError("invalid view input")
    query = payload.get("query", "")
    _text(query)
    anchor = payload.get("anchor")
    if anchor is not None:
        item = _identity(anchor.get("item_id"))
        offset = anchor.get("row_offset")
        if type(offset) is not int or not 0 <= offset < 2**32:
            raise ValueError("invalid anchor offset")
        anchor = ViewAnchor(item, offset)
    return ViewRequest(route, session, mode, previous, reason, state, query, anchor)


def _navigation(result):
    if type(result) not in (Push, Replace):
        raise TypeError("invalid navigation outcome")
    return {"kind": "push" if type(result) is Push else "replace",
            "route": _identity(result.route)}


def _descriptor(descriptor, item_to_wire, preview_to_wire, frame_size):
    """Validate the complete local namespace before returning bounded projection."""
    if not isinstance(descriptor, ViewDescriptor) or len(descriptor.definitions) > 4096:
        raise ValueError("invalid view descriptor")
    _text(descriptor.title)
    produced_bytes = 1024 + frame_size(descriptor.title)
    def admit(value):
        nonlocal produced_bytes
        produced_bytes += frame_size(value) + 1
        if produced_bytes > 8 * 1024 * 1024:
            raise ValueError("view exceeds complete frame bound")
        return value
    definitions, registry = [], {}
    for definition in descriptor.definitions:
        if not isinstance(definition, RouteItemDefinition):
            raise TypeError("invalid route definition")
        item = definition.item
        item_id = _identity(item.stable_id)
        if item_id in registry or len(item.actions) > 256:
            raise ValueError("duplicate or oversized route definition")
        for value in (item.label, item.description, item.target):
            _text(value)
        if len(item.search_terms) > 128 or len(item.metadata) > 128:
            raise ValueError("oversized route item")
        for value in item.search_terms:
            _text(value)
        for key, value in item.metadata.items():
            _text(key); _text(value)
        if item.icon_reference is not None:
            _text(item.icon_reference)
        actions = set()
        for action in item.actions:
            action_id = _identity(action.action_id)
            if action_id in actions or action.execution_policy != "plugin":
                raise ValueError("invalid route action")
            actions.add(action_id)
            _text(action.label); _text(action.description)
            if action.icon_reference is not None:
                _text(action.icon_reference)
            if len(action.applicable_categories) > 128:
                raise ValueError("oversized action categories")
            for category in action.applicable_categories:
                _text(category, 256, True)
        if definition.actionless_policy is not None:
            if actions or definition.actionless_policy != "plugin":
                raise ValueError("invalid actionless execution")
            actions.add(None)
        if definition.primary_action is not None and definition.primary_action not in actions:
            raise ValueError("invalid primary action")
        bindings, submenu_ids, section_ids = {}, set(), set()
        count = 0
        def menu(entries, depth):
            nonlocal count
            if depth > 4 or len(entries) > 256:
                raise ValueError("oversized action menu")
            out = []
            for entry in entries:
                count += 1
                if count > 256:
                    raise ValueError("oversized action menu")
                if isinstance(entry, ExecutablePresentation):
                    if entry.action_id not in actions or entry.action_id in bindings:
                        raise ValueError("invalid executable reference")
                    if entry.shortcut is not None:
                        _text(entry.shortcut)
                    if type(entry.destructive) is not bool:
                        raise TypeError("invalid destructive flag")
                    schema = None if entry.schema is None else entry.schema.to_wire()
                    bindings[entry.action_id] = (entry.schema, None if entry.schema is None else entry.schema.fingerprint())
                    out.append(dict(kind="action", action_id=entry.action_id, schema=schema,
                                    shortcut=entry.shortcut, destructive=entry.destructive))
                elif isinstance(entry, ActionSubmenu):
                    identity = _identity(entry.id)
                    if identity in submenu_ids:
                        raise ValueError("duplicate submenu")
                    submenu_ids.add(identity)
                    _text(entry.label); _text(entry.description)
                    if entry.icon_reference is not None:
                        _text(entry.icon_reference)
                    out.append(dict(kind="submenu", id=identity, label=entry.label,
                                    description=entry.description, icon_reference=entry.icon_reference,
                                    entries=menu(entry.entries, depth + 1)))
                else:
                    raise TypeError("invalid action menu entry")
            return out
        if len(definition.action_sections) > 128:
            raise ValueError("oversized action sections")
        sections = []
        for section in definition.action_sections:
            identity = _identity(section.id)
            if identity in section_ids:
                raise ValueError("duplicate action section")
            section_ids.add(identity)
            _text(section.label)
            sections.append(dict(id=identity, label=section.label, entries=menu(section.entries, 1)))
        if set(bindings) != actions:
            raise ValueError("incomplete executable presentation")
        frozen = item_to_wire(item)
        registry[item_id] = (frozen, bindings)
        definitions.append(admit(dict(item=frozen, actionless_policy=definition.actionless_policy,
                                      primary_action=definition.primary_action, action_sections=sections)))
    view = descriptor.view
    if isinstance(view, ListView):
        if len(view.items) > 4096 or len(view.sections) > 128 or len(view.scopes) > 128:
            raise ValueError("oversized collection")
        def declarations(entries):
            seen, result = set(), []
            for entry in entries:
                identity = _identity(entry.id)
                if identity in seen:
                    raise ValueError("duplicate collection declaration")
                seen.add(identity); _text(entry.label)
                result.append(dict(id=identity, label=entry.label))
            return seen, result
        sections, wire_sections = declarations(view.sections)
        scopes, wire_scopes = declarations(view.scopes)
        items, seen = [], set()
        for entry in view.items:
            identity = _identity(entry.item_id)
            if identity not in registry or identity in seen or (entry.section is not None and entry.section not in sections) or len(entry.scopes) > 128 or len(set(entry.scopes)) != len(entry.scopes) or any(s not in scopes for s in entry.scopes):
                raise ValueError("invalid collection membership")
            seen.add(identity)
            from .plugin import Preview
            items.append(dict(item_id=identity, section=entry.section, scopes=list(entry.scopes),
                              image=None if entry.image is None else preview_to_wire(Preview(image=entry.image))["image"]))
        if view.initial_selection is not None and view.initial_selection not in seen:
            raise ValueError("invalid initial selection")
        if view.initial_scope is not None and view.initial_scope not in scopes:
            raise ValueError("invalid initial scope")
        wire_view = dict(kind="grid" if isinstance(view, GridView) else "list", items=items,
                         sections=wire_sections, scopes=wire_scopes,
                         initial_selection=view.initial_selection, initial_scope=view.initial_scope)
        if isinstance(view, GridView):
            if type(view.columns) is not int or not 1 <= view.columns <= 16:
                raise ValueError("invalid grid columns")
            wire_view["columns"] = view.columns
    elif isinstance(view, DetailView):
        if view.target_item_id is not None and view.target_item_id not in registry:
            raise ValueError("invalid detail target")
        wire_view = dict(kind="detail", content=preview_to_wire(view.content), target_item_id=view.target_item_id)
    elif isinstance(view, FormView):
        target = registry.get(view.item_id)
        binding = None if target is None else target[1].get(view.action_id)
        if binding is None or binding[0] is None:
            raise ValueError("form must reference an exact executable schema")
        wire_view = dict(kind="form", item_id=view.item_id, action_id=view.action_id)
    else:
        raise TypeError("invalid standard view")
    admit(wire_view)
    return dict(title=descriptor.title, definitions=definitions, view=wire_view), registry
