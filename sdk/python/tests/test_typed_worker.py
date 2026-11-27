"""Drive the actual shim with an old-style JSON host, independently of Rust guards."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from crikey_sdk import ArgumentSchema, TextField


class TypedWorkerProtocolTests(unittest.TestCase):
    def test_legacy_refusal_current_binding_duplicates_and_no_forwarding(self):
        sdk = Path(__file__).resolve().parents[1]
        schema = ArgumentSchema((TextField(id="text", label="Text"),))
        fingerprint = schema.fingerprint()
        item = dict(stable_id="typed", label="Typed", target="typed", category="plugin-defined",
                    description="", icon_reference=None, score_hint=0, search_terms=[],
                    metadata={}, actions=[], argument_policy="forbidden", hit_policy="recorded")
        with tempfile.TemporaryDirectory() as directory:
            Path(directory, "fixture.py").write_text('''
from crikey_sdk import Plugin, Item, ArgumentSchema, TextField, SchemaBinding, ResultPresentation, TypedOutcome
SCHEMA = ArgumentSchema((TextField(id="text", label="Text"),))
class Impl(Plugin):
    def build_catalog(self):
        return [Item("typed", "Typed", "typed")]
    def catalog_presentation(self):
        return ResultPresentation(schemas=(SchemaBinding("typed", SCHEMA),))
    def execute(self, *args):
        print("LEGACY_CALLBACK_MUST_NOT_RUN")
    def execute_typed(self, item, action, values):
        assert action is None
        assert values.get("text").value == "SYNTHETIC_SECRET"
        return TypedOutcome(code="denied")
''', encoding="utf-8")
            def typed(request_id, **changes):
                frame = dict(id=request_id, kind="typed_execute", item=item, action_id=None,
                             schema_fingerprint=fingerprint,
                             values=[dict(field_id="text", kind="text", value="SYNTHETIC_SECRET")])
                frame.update(changes)
                return frame
            encode = lambda frame: json.dumps(frame, separators=(",", ":"))
            duplicate = encode(typed(7)).replace('"schema_fingerprint":',
                '"schema_fingerprint":"' + fingerprint + '","schema_fingerprint":', 1)
            duplicate_field = encode(typed(8)).replace('"field_id":"text"',
                '"field_id":"text","field_id":"text"', 1)
            frames = [
                encode(dict(id=1, kind="handshake", protocol_version=1)),
                encode(dict(id=2, kind="build_catalog")),
                encode(dict(id=20, kind="suggest", generation=1, text="ordinary", normalized="ordinary", selected_item_id=None)),
                encode(dict(id=3, kind="execute", item=item, action_id=None, argument=None)),
                encode(typed(4)),
                encode(typed(5, schema_fingerprint="0" * 64)),
                encode(typed(6, argument=None)),
                duplicate,
                duplicate_field,
                encode(typed(9)),
                encode(dict(id=10, kind="shutdown")),
            ]
            environment = dict(os.environ,
                               PYTHONPATH=os.pathsep.join((directory, str(sdk))),
                               CRIKEY_MODERN_PLUGIN_ID="typed-json-test",
                               CRIKEY_MODERN_ENTRYPOINT="fixture:Impl",
                               CRIKEY_MODERN_PROTOCOL_VERSION="1")
            child = subprocess.run([sys.executable, "-S", str(sdk / "_crikey_modern_worker.py")],
                                   input="\n".join(frames) + "\n", capture_output=True,
                                   text=True, encoding="utf-8", env=environment, timeout=15)
            self.assertEqual(child.returncode, 0, child.stderr)
            self.assertNotIn("SYNTHETIC_SECRET", child.stdout + child.stderr)
            self.assertNotIn("LEGACY_CALLBACK_MUST_NOT_RUN", child.stdout + child.stderr)
            replies = {frame["id"]: frame for frame in map(json.loads, child.stdout.splitlines())}
            self.assertIn("typed_arguments_v1", replies[1]["capabilities"])
            self.assertNotIn("result_presentation_v1", replies[1]["capabilities"])
            self.assertEqual(replies[3]["status"], "failed")
            self.assertEqual(replies[4]["code"], "denied")
            self.assertEqual(replies[5]["code"], "unavailable")
            for request_id in (6, 7, 8):
                self.assertEqual(replies[request_id]["code"], "failed")
            self.assertEqual(replies[9]["code"], "denied")

    def test_optional_empty_text_and_password_match_core_lengths(self):
        sdk = Path(__file__).resolve().parents[1]
        from crikey_sdk import PasswordField
        for field_type, kind in ((TextField, "text"), (PasswordField, "password")):
            for required in (False, True):
                with self.subTest(kind=kind, required=required), tempfile.TemporaryDirectory() as directory:
                    schema = ArgumentSchema((field_type(id="value", label="Value", required=required,
                                                       min_bytes=2, max_bytes=4),))
                    Path(directory, "fixture.py").write_text(f'''
from crikey_sdk import *
SCHEMA = ArgumentSchema(({field_type.__name__}(id="value", label="Value", required={required!r}, min_bytes=2, max_bytes=4),))
class Impl(Plugin):
    def build_catalog(self):
        return [Item("typed", "Typed", "typed")]
    def catalog_presentation(self):
        return ResultPresentation(schemas=(SchemaBinding("typed", SCHEMA),))
    def execute_typed(self, item, action, values):
        return TypedOutcome(code="denied")
''', encoding="utf-8")
                    item = dict(stable_id="typed", label="Typed", target="typed", category="plugin-defined",
                                description="", icon_reference=None, score_hint=0, search_terms=[],
                                metadata={}, actions=[], argument_policy="forbidden", hit_policy="recorded")
                    frames = [dict(id=1, kind="handshake", protocol_version=1),
                              dict(id=2, kind="build_catalog")]
                    for request_id, value in enumerate(("", "x", "é", "éé", "ééé"), 3):
                        frames.append(dict(id=request_id, kind="typed_execute", item=item, action_id=None,
                                           schema_fingerprint=schema.fingerprint(),
                                           values=[dict(field_id="value", kind=kind, value=value)]))
                    frames.append(dict(id=8, kind="shutdown"))
                    environment = dict(os.environ, PYTHONPATH=os.pathsep.join((directory, str(sdk))),
                                       CRIKEY_MODERN_PLUGIN_ID="typed-length-test",
                                       CRIKEY_MODERN_ENTRYPOINT="fixture:Impl",
                                       CRIKEY_MODERN_PROTOCOL_VERSION="1")
                    child = subprocess.run([sys.executable, "-S", str(sdk / "_crikey_modern_worker.py")],
                                           input="\n".join(map(json.dumps, frames)) + "\n", capture_output=True,
                                           text=True, encoding="utf-8", env=environment, timeout=15)
                    self.assertEqual(child.returncode, 0, child.stderr)
                    replies = {frame["id"]: frame for frame in map(json.loads, child.stdout.splitlines())}
                    self.assertEqual(replies[3]["code"], "failed" if required else "denied")
                    for request_id in (4, 7):
                        self.assertEqual(replies[request_id]["code"], "failed")
                    for request_id in (5, 6):
                        self.assertEqual(replies[request_id]["code"], "denied")


if __name__ == "__main__":
    unittest.main()
