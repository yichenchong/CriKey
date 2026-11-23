"""PR7 typed arguments and root presentation (independent optional capabilities).

The JSON bridge uses ``typed_execute`` with a frozen item object, nullable
``action_id``, lowercase 64-hex ``schema_fingerprint`` and ``values`` entries
``{field_id,kind,value}``. No legacy argument member is permitted. Its correlated
``typed_execute_result`` is completed, field_failures (field_id/fixed code), or
failed (fixed host code); callbacks cannot return diagnostic text or navigation.
"""
from dataclasses import dataclass
import hashlib
import struct

__all__ = ["TextField", "PasswordField", "DropdownField", "DropdownOption",
           "ArgumentSchema", "SchemaBinding", "Section", "Scope", "ItemPresentation",
           "ResultPresentation", "TypedValue", "TypedValues", "FieldFailure",
           "TypedOutcome"]


def _text(value, limit=8192, identity=False):
    if not isinstance(value, str) or len(value) > limit:
        raise ValueError("invalid typed schema text")
    raw = value.encode("utf-8", "strict")
    if len(raw) > limit or (identity and (not value or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in value))):
        raise ValueError("invalid typed schema identity or bound")
    return raw


@dataclass(frozen=True)
class DropdownOption:
    id: str
    label: str


@dataclass(frozen=True, kw_only=True)
class _Field:
    id: str
    label: str
    required: bool = False
    placeholder: str | None = None
    guidance: str | None = None


@dataclass(frozen=True, kw_only=True)
class TextField(_Field):
    min_bytes: int = 0
    max_bytes: int = 8192
    multiline: bool = False
    default: str | None = None


@dataclass(frozen=True, kw_only=True)
class PasswordField(_Field):
    min_bytes: int = 0
    max_bytes: int = 8192


@dataclass(frozen=True, kw_only=True)
class DropdownField(_Field):
    options: tuple[DropdownOption, ...] = ()
    default: str | None = None


@dataclass(frozen=True)
class ArgumentSchema:
    fields: tuple[TextField | PasswordField | DropdownField, ...] = ()

    def to_wire(self):
        if len(self.fields) > 64:
            raise ValueError("typed schema exceeds 64 fields")
        # Match the transport's exact-capacity Core field/option records and
        # charge every retained string, including dropdown defaults, once.
        total = len(self.fields) * 20 * struct.calcsize("P")
        def charge(size):
            nonlocal total
            total += size
            if total > 256 * 1024:
                raise ValueError("typed schema exceeds 256 KiB")
        seen = set()
        for field in self.fields:
            if type(field) not in (TextField, PasswordField, DropdownField):
                raise TypeError("invalid typed field")
            charge(len(_text(field.id, 256, True)))
            if field.id in seen:
                raise ValueError("duplicate typed field")
            seen.add(field.id)
            charge(len(_text(field.label)))
            for value in (field.placeholder, field.guidance):
                if value is not None:
                    charge(len(_text(value)))
            if type(field.required) is not bool:
                raise TypeError("required must be boolean")
            if isinstance(field, DropdownField):
                if not 1 <= len(field.options) <= 256:
                    raise ValueError("dropdown requires 1 to 256 options")
                charge(len(field.options) * 6 * struct.calcsize("P"))
                ids = set()
                for option in field.options:
                    charge(len(_text(option.id, 256, True)) + len(_text(option.label)))
                    if option.id in ids:
                        raise ValueError("duplicate dropdown option")
                    ids.add(option.id)
                if field.default is not None:
                    charge(len(_text(field.default, 256, True)))
                    if field.default not in ids:
                        raise ValueError("invalid dropdown default")
            else:
                if (type(field.min_bytes) is not int or type(field.max_bytes) is not int
                    or not 0 <= field.min_bytes <= field.max_bytes <= 8192):
                    raise ValueError("invalid typed length bounds")
                if isinstance(field, TextField):
                    if type(field.multiline) is not bool:
                        raise TypeError("multiline must be boolean")
                    if field.default is not None:
                        size = len(_text(field.default))
                        charge(size)
                        if size > field.max_bytes or (size and size < field.min_bytes) or (field.required and not size):
                            raise ValueError("invalid text default")
        # Materialize only after the complete shared storage budget passes.
        fields = []
        for field in self.fields:
            value = dict(id=field.id, label=field.label, required=field.required,
                         placeholder=field.placeholder, guidance=field.guidance)
            if isinstance(field, DropdownField):
                value.update(kind="dropdown",
                             options=[dict(id=option.id, label=option.label) for option in field.options],
                             default=field.default)
            else:
                value.update(kind="password" if isinstance(field, PasswordField) else "text",
                             min_bytes=field.min_bytes, max_bytes=field.max_bytes)
                if isinstance(field, TextField):
                    value.update(multiline=field.multiline, default=field.default)
            fields.append(value)
        return {"fields": fields}

    def fingerprint(self):
        wire = self.to_wire()
        digest = hashlib.sha256()
        digest.update(b"crikey.argument-schema\0")
        def u32(value): digest.update(struct.pack("<I", value))
        def text(value):
            raw = value.encode("utf-8")
            u32(len(raw)); digest.update(raw)
        def optional(value):
            digest.update(b"\0" if value is None else b"\1")
            if value is not None: text(value)
        u32(1); u32(len(wire["fields"]))
        for field in wire["fields"]:
            text(field["id"]); text(field["label"])
            digest.update(bytes((int(field["required"]),)))
            optional(field["placeholder"]); optional(field["guidance"])
            kind = field["kind"]
            digest.update(bytes(({"text": 0, "password": 1, "dropdown": 2}[kind],)))
            if kind == "dropdown":
                u32(len(field["options"]))
                for option in field["options"]: text(option["id"]); text(option["label"])
                optional(field["default"])
            else:
                u32(field["min_bytes"]); u32(field["max_bytes"])
                if kind == "text":
                    digest.update(bytes((int(field["multiline"]),)))
                    optional(field["default"])
        return digest.hexdigest()


@dataclass(frozen=True)
class SchemaBinding:
    item_id: str
    schema: ArgumentSchema
    action_id: str | None = None


@dataclass(frozen=True)
class Section:
    id: str
    label: str


@dataclass(frozen=True)
class Scope:
    id: str
    label: str


@dataclass(frozen=True)
class ItemPresentation:
    item_id: str
    section: str | None = None
    scopes: tuple[str, ...] = ()


@dataclass(frozen=True)
class ResultPresentation:
    sections: tuple[Section, ...] = ()
    scopes: tuple[Scope, ...] = ()
    items: tuple[ItemPresentation, ...] = ()
    schemas: tuple[SchemaBinding, ...] = ()


@dataclass(frozen=True, repr=False)
class TypedValue:
    field_id: str
    kind: str
    value: str

    def __repr__(self):
        return "TypedValue(<redacted>)"


class TypedValues:
    """Live callback input, deliberately excluded from repr and serialization."""
    __slots__ = ("_values",)
    def __init__(self, values): self._values = values
    def __repr__(self): return "TypedValues(<redacted>)"
    def get(self, field_id): return self._values.get(field_id)
    def clear(self): self._values.clear()


@dataclass(frozen=True)
class FieldFailure:
    field_id: str
    code: str = "rejected"


@dataclass(frozen=True)
class TypedOutcome:
    """Fixed outcomes only; None is also completed. No messages or routes."""
    failures: tuple[FieldFailure, ...] = ()
    code: str | None = None
