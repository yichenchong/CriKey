import unittest
import struct
from crikey_sdk import (ArgumentSchema, TextField, PasswordField, DropdownField,
                        DropdownOption, TypedValue, TypedValues)


class ArgumentContractTests(unittest.TestCase):
    def test_core_fingerprint_vectors(self):
        empty = ArgumentSchema()
        minimal = ArgumentSchema((TextField(id="text", label="Text"),))
        mixed = ArgumentSchema((
            TextField(id="t", label="é", required=True, placeholder="", guidance="Use text",
                      max_bytes=8, multiline=True, default="x"),
            PasswordField(id="p", label="Password", required=True, min_bytes=1),
            DropdownField(id="d", label="Choice", guidance="",
                          options=(DropdownOption("a", "Alpha"), DropdownOption("β", "Beta")),
                          default="β"),
        ))
        for schema, expected in [
            (empty, "3265b557c583dda06d75f83cdcf9d5e898c972b0772614f475819ffe4edd22bf"),
            (minimal, "26f7c14f86657e87ae8cc9279ad7707244d827bb6c202e4638de9fbf22ba763c"),
            (mixed, "74e577d66d4bed1332aacdcbc1b692c8bf736384edb9cc8d91d303a529baac2e"),
        ]:
            self.assertEqual(schema.fingerprint(), expected)

    def test_password_default_is_unrepresentable(self):
        with self.assertRaises(TypeError):
            PasswordField(id="password", label="Password", default="secret")

    def test_decoded_utf8_byte_limits_and_duplicates(self):
        for schema in (
            ArgumentSchema((TextField(id="é" * 129, label="x"),)),
            ArgumentSchema((TextField(id="x", label="x"),) * 65),
            ArgumentSchema((TextField(id="x", label="x"),) * 2),
            ArgumentSchema((DropdownField(id="d", label="D", options=(DropdownOption("a", "A"),) * 257),)),
            ArgumentSchema((TextField(id="x", label="é" * 4097),)),
        ):
            with self.assertRaises(ValueError):
                schema.to_wire()

    def test_optional_metadata_absent_and_empty_differ(self):
        absent = ArgumentSchema((TextField(id="x", label="X"),))
        empty = ArgumentSchema((TextField(id="x", label="X", placeholder=""),))
        self.assertNotEqual(absent.fingerprint(), empty.fingerprint())

    def test_all_value_repr_is_redacted(self):
        value = TypedValue("password", "password", "secret-marker")
        self.assertNotIn("secret-marker", repr(value))
        values = TypedValues({"password": value})
        self.assertNotIn("secret-marker", repr(values))
        self.assertEqual(values.get("password").value, "secret-marker")
        values.clear()
        self.assertIsNone(values.get("password"))

    def test_shared_schema_budget_accumulates_all_option_records(self):
        options = tuple(DropdownOption("o" + str(n), "O") for n in range(256))
        schema = ArgumentSchema(tuple(DropdownField(id="f" + str(n), label="", options=options) for n in range(64)))
        for operation in (schema.to_wire, schema.fingerprint):
            with self.assertRaises(ValueError):
                operation()

    def test_empty_dropdown_is_not_a_core_schema(self):
        schema = ArgumentSchema((DropdownField(id="choice", label="Choice"),))
        for operation in (schema.to_wire, schema.fingerprint):
            with self.assertRaises(ValueError):
                operation()

    def test_dropdown_defaults_share_the_aggregate_storage_budget(self):
        option = DropdownOption("d" * 256, "L" * 3700)
        schema = ArgumentSchema(tuple(DropdownField(id="f" + str(n), label="", options=(option,), default=option.id) for n in range(64)))
        with self.assertRaises(ValueError):
            schema.to_wire()

    def test_field_records_are_charged_at_the_storage_boundary(self):
        ids = tuple("f" + str(n) for n in range(64))
        baseline = sum(map(len, ids)) + len(ids) * 20 * struct.calcsize("P")
        label_bytes = (256 * 1024 - baseline) // len(ids)
        valid = ArgumentSchema(tuple(TextField(id=id, label="L" * label_bytes) for id in ids))
        self.assertEqual(len(valid.to_wire()["fields"]), 64)
        invalid = ArgumentSchema(tuple(TextField(id=id, label="L" * (label_bytes + 1)) for id in ids))
        with self.assertRaises(ValueError):
            invalid.to_wire()


if __name__ == "__main__":
    unittest.main()
