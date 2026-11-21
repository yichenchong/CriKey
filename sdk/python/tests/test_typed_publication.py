"""Real shim consumers: complete publication, authority retirement and fixed failures."""
import contextlib
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import unittest

from crikey_sdk import ArgumentSchema, TextField


@contextlib.contextmanager
def worker(source):
    sdk = Path(__file__).resolve().parents[1]
    with tempfile.TemporaryDirectory() as directory:
        Path(directory, "publication.py").write_text(source, encoding="utf-8")
        environment = dict(os.environ, PYTHONPATH=os.pathsep.join((directory, str(sdk))),
                           CRIKEY_MODERN_ENTRYPOINT="publication:Impl",
                           CRIKEY_MODERN_PLUGIN_ID="publication-test",
                           CRIKEY_MODERN_PROTOCOL_VERSION="1")
        child = subprocess.Popen([sys.executable, "-S", str(sdk / "_crikey_modern_worker.py")],
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.DEVNULL, text=True, encoding="utf-8",
                                 env=environment)
        replies = queue.Queue()
        def drain():
            try:
                for line in child.stdout:
                    replies.put(json.loads(line))
            except Exception as error:
                replies.put(error)
            finally:
                replies.put(None)
        reader = threading.Thread(target=drain, daemon=True)
        reader.start()
        def send(frame):
            child.stdin.write(json.dumps(frame, separators=(",", ":")) + "\n")
            child.stdin.flush()
        def exchange(frame):
            send(frame)
            while True:
                reply = replies.get(timeout=15)
                if reply is None:
                    raise AssertionError("worker exited without a terminal frame")
                if isinstance(reply, Exception):
                    raise reply
                if reply.get("id") != frame["id"]:
                    raise AssertionError("unexpected worker correlation")
                if reply.get("kind") == "catalog_batch" and not reply.get("done"):
                    continue
                if reply.get("state") == "partial":
                    continue
                return reply
        try:
            yield exchange, send
        finally:
            if child.poll() is None:
                try:
                    send(dict(id=1000, kind="shutdown"))
                    child.wait(timeout=5)
                except (BrokenPipeError, subprocess.TimeoutExpired):
                    child.kill()
                    child.wait(timeout=5)
            reader.join(timeout=1)
            try:
                child.stdin.close()
            except BrokenPipeError:
                pass
            child.stdout.close()


class PublicationContractTests(unittest.TestCase):
    def test_failed_and_cancelled_publications_never_install_schema_authority(self):
        source = '''
from crikey_sdk import Plugin, Item, ArgumentSchema, TextField, SchemaBinding, ResultPresentation, TypedOutcome
SCHEMA = ArgumentSchema((TextField(id="text", label="Text"),))
class Impl(Plugin):
    def catalog_presentation(self):
        return ResultPresentation(schemas=(SchemaBinding("catalog", SCHEMA),))
    def build_catalog(self):
        raise RuntimeError("catalog callback failed")
    def suggestion_presentation(self, query):
        return ResultPresentation(schemas=(SchemaBinding(query.text, SCHEMA),))
    def suggest(self, query, context):
        if query.text == "failed":
            raise RuntimeError("query callback failed")
        if query.text == "cancelled":
            context._is_cancelled.__self__.set()
            return
        context.emit(Item(query.text, "Dynamic", "dynamic"))
    def execute_typed(self, item, action, values):
        return TypedOutcome(code="denied")
    def execute(self, *args):
        return None
'''
        fingerprint = ArgumentSchema((TextField(id="text", label="Text"),)).fingerprint()
        request_id = 0
        def next_id():
            nonlocal request_id
            request_id += 1
            return request_id
        def item(id):
            return dict(stable_id=id, label="Dynamic", target="dynamic", category="plugin-defined",
                        description="", icon_reference=None, score_hint=0, search_terms=[],
                        metadata={}, actions=[], argument_policy="forbidden", hit_policy="recorded")
        with worker(source) as (exchange, send):
            exchange(dict(id=next_id(), kind="handshake", protocol_version=1))
            def typed(target):
                return exchange(dict(id=next_id(), kind="typed_execute", item=item(target),
                                     action_id=None, schema_fingerprint=fingerprint,
                                     values=[dict(field_id="text", kind="text", value="synthetic")]))
            def legacy(target):
                return exchange(dict(id=next_id(), kind="execute", item=item(target),
                                     action_id=None, argument=None))
            def suggest(text):
                return exchange(dict(id=next_id(), kind="suggest", generation=request_id,
                                     text=text, normalized=text, selected_item_id=None))
            failed_catalog = exchange(dict(id=next_id(), kind="build_catalog"))
            self.assertIsNotNone(failed_catalog.get("error"))
            self.assertEqual(typed("catalog")["code"], "unavailable")
            self.assertEqual(legacy("catalog")["status"], "failed")
            self.assertEqual(suggest("ready")["state"], "final")
            self.assertEqual(typed("ready")["code"], "denied")
            self.assertEqual(suggest("failed")["state"], "failed")
            for target in ("ready", "failed"):
                self.assertEqual(typed(target)["code"], "unavailable")
                self.assertEqual(legacy(target)["status"], "failed")
            self.assertEqual(suggest("ready")["state"], "final")
            self.assertEqual(suggest("cancelled")["state"], "cancelled")
            send(dict(kind="set_cancel", cancelled=False))
            for target in ("ready", "cancelled"):
                self.assertEqual(typed(target)["code"], "unavailable")
                self.assertEqual(legacy(target)["status"], "failed")
            self.assertEqual(suggest("recovered")["state"], "final")
            self.assertEqual(typed("recovered")["code"], "denied")

    def test_oversized_partial_frame_installs_no_schema_and_has_one_terminal(self):
        source = '''
from crikey_sdk import Plugin, Item, ArgumentSchema, TextField, SchemaBinding, ResultPresentation, TypedOutcome
SCHEMA = ArgumentSchema((TextField(id="text", label="Text"),))
class Impl(Plugin):
    def __init__(self):
        self.large = True
    def catalog_presentation(self):
        return ResultPresentation(schemas=(SchemaBinding("oversized", SCHEMA),)) if self.large else ResultPresentation()
    def build_catalog(self):
        if not self.large:
            return [Item("recovered", "Recovered", "recovered")]
        self.large = False
        return [Item("oversized", "X" * (8 * 1024 * 1024), "oversized")] + [Item("other" + str(n), "Other", "other") for n in range(31)]
    def execute_typed(self, *args):
        return TypedOutcome(code="denied")
    def execute(self, *args):
        return None
'''
        fingerprint = ArgumentSchema((TextField(id="text", label="Text"),)).fingerprint()
        item = dict(stable_id="oversized", label="Oversized", target="oversized",
                    category="plugin-defined", description="", icon_reference=None,
                    score_hint=0, search_terms=[], metadata={}, actions=[],
                    argument_policy="forbidden", hit_policy="recorded")
        with worker(source) as (exchange, _send):
            exchange(dict(id=1, kind="handshake", protocol_version=1))
            failed = exchange(dict(id=2, kind="build_catalog"))
            self.assertTrue(failed["done"])
            self.assertEqual(failed["items"], [])
            self.assertIsNotNone(failed.get("error"))
            typed = exchange(dict(id=3, kind="typed_execute", item=item, action_id=None,
                                  schema_fingerprint=fingerprint,
                                  values=[dict(field_id="text", kind="text", value="synthetic")]))
            self.assertEqual(typed["code"], "unavailable")
            legacy = exchange(dict(id=4, kind="execute", item=item, action_id=None, argument=None))
            self.assertEqual(legacy["status"], "failed")
            recovered = exchange(dict(id=5, kind="build_catalog"))
            self.assertIsNone(recovered.get("error"))
            self.assertEqual(recovered["items"][0]["stable_id"], "recovered")

    def test_complete_frame_preflight_matches_actual_ascii_escaping(self):
        source = r'''
import __main__, json
from crikey_sdk import Plugin, Item
class Impl(Plugin):
    def build_catalog(self):
        samples = ["", "ascii", '"\\\b\f\n\r\t\x00\x1f\x7f\u0080\u2028\U0001f600\ud800']
        # Evaluate the literal escape spellings as JSON scalar content as well
        # as real control/non-ASCII code points in the same complete envelope.
        samples.append("".join(chr(n) for n in (0, 8, 9, 10, 12, 13, 31, 34, 92, 127, 128, 0x2028, 0x1f600, 0xd800)))
        for text in samples:
            frame = dict(id=1, kind="result_batch", generation=2, state="final",
                         items=[dict(label=text)], log=[text],
                         presentation=dict(sections=[dict(id="section", label=text)], scopes=[], items=[], schemas=[]))
            frame["future"] = {text: (None, False, True, 0, -1, 1.5, 1e100)}
            actual = len(json.dumps(frame, ensure_ascii=True, separators=(",", ":"))) + 1
            assert __main__._frame_size(frame) == actual
        return [Item("verified", "Verified", "verified")]
'''
        with worker(source) as (exchange, _send):
            exchange(dict(id=1, kind="handshake", protocol_version=1))
            reply = exchange(dict(id=2, kind="build_catalog"))
            self.assertIsNone(reply.get("error"))
            self.assertEqual(reply["items"][0]["stable_id"], "verified")


if __name__ == "__main__":
    unittest.main()
