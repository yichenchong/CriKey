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
    def test_successful_catalog_commits_despite_callback_cancellation_flag(self):
        source = '''
from crikey_sdk import Plugin, Item, ArgumentSchema, TextField, SchemaBinding, ResultPresentation
SCHEMA = ArgumentSchema((TextField(id="text", label="Text"),))
class Impl(Plugin):
    def __init__(self):
        self.calls = 0
    def suggest(self, query, context):
        self.cancel = context._is_cancelled.__self__
    def catalog_presentation(self):
        return ResultPresentation(schemas=(SchemaBinding("typed", SCHEMA),))
    def build_catalog(self):
        self.calls += 1
        self.cancel.set()
        if self.calls == 2:
            raise RuntimeError("catalog failed with cancellation flag set")
        return [Item("typed", "Dynamic", "dynamic"), Item("ordinary", "Dynamic", "dynamic")]
    def execute_typed(self, item, action, values):
        return None
    def execute(self, item, action, argument):
        return None
'''
        fingerprint = ArgumentSchema((TextField(id="text", label="Text"),)).fingerprint()
        with worker(source) as (exchange, send):
            request_id = 0
            def call(kind, **fields):
                nonlocal request_id
                request_id += 1
                return exchange(dict(id=request_id, kind=kind, **fields))
            def item(target):
                return dict(stable_id=target, label="Dynamic", target="dynamic")
            def legacy(target):
                return call("execute", item=item(target), action_id=None, argument=None)["status"]
            def typed():
                return call("typed_execute", item=item("typed"), action_id=None,
                            schema_fingerprint=fingerprint,
                            values=[dict(field_id="text", kind="text", value="value")])

            call("handshake", protocol_version=1)
            self.assertEqual(call("suggest", generation=1, text="", normalized="")["state"], "final")
            published = call("build_catalog")
            self.assertTrue(published["done"])
            self.assertIsNone(published.get("error"))
            self.assertEqual([entry["stable_id"] for entry in published["items"]],
                             ["typed", "ordinary"])
            self.assertEqual(published["presentation"]["schemas"][0]["item_id"], "typed")
            send(dict(kind="set_cancel", cancelled=False))
            self.assertEqual(typed()["status"], "completed")
            self.assertEqual(legacy("typed"), "failed")
            self.assertEqual(legacy("ordinary"), "ok")

            failed = call("build_catalog")
            self.assertTrue(failed["done"])
            self.assertIsNotNone(failed.get("error"))
            self.assertNotIn("presentation", failed)
            send(dict(kind="set_cancel", cancelled=False))
            self.assertEqual(typed()["code"], "unavailable")
            self.assertEqual(legacy("typed"), "failed")
            self.assertEqual(legacy("ordinary"), "failed")

    def test_successive_publications_keep_bounded_current_legacy_admission(self):
        source = '''
from crikey_sdk import Plugin, Item, Action, ArgumentSchema, TextField, SchemaBinding, ResultPresentation
SCHEMA = ArgumentSchema((TextField(id="text", label="Text"),))
def item(target):
    return Item(target, "Dynamic", "dynamic", category="custom",
                metadata={"z": "last", "a": "first"},
                actions=[Action("ordinary", "Ordinary", applicable_categories=("custom",),
                                execution_policy="plugin"),
                         Action("typed", "Typed")])
class Impl(Plugin):
    def __init__(self):
        self.catalog_generation = 0
    def build_catalog(self):
        self.catalog_generation += 1
        # This catalog precedes all schema declarations, and exceeds the
        # schema bound without exceeding the independent host item bound.
        return [item("catalog-" + str(self.catalog_generation) + "-" + str(n))
                for n in range(4100)]
    def suggestion_presentation(self, query):
        if query.text == "ordinary-republish":
            return ResultPresentation()
        target = "newest" if query.text == "typed-original" else query.text
        return ResultPresentation(schemas=(SchemaBinding(target, SCHEMA, action_id="typed"),))
    def suggest(self, query, context):
        target = "newest" if query.text in ("typed-original", "ordinary-republish") else query.text
        context.emit(item(target))
        context.emit(item("ordinary-" + query.text))
        if query.text == "typed-original":
            for n in range(32):
                context.emit(item("successful-partial-" + str(n)))
        if query.text in ("failed", "cancelled"):
            # Force a real emitted partial before failure/cancellation.
            for n in range(32):
                context.emit(item("partial-" + query.text + "-" + str(n)))
            if query.text == "failed":
                raise RuntimeError("failed after partial")
            context._is_cancelled.__self__.set()
    def execute_typed(self, item, action, values):
        return None
    def execute(self, item, action, argument):
        return None
'''
        fingerprint = ArgumentSchema((TextField(id="text", label="Text"),)).fingerprint()
        with worker(source) as (exchange, send):
            request_id = 0
            def call(kind, **fields):
                nonlocal request_id
                request_id += 1
                return exchange(dict(id=request_id, kind=kind, **fields))
            def item(target):
                return dict(stable_id=target, label="Dynamic", target="dynamic",
                            category="plugin-defined:custom", metadata={"a": "first", "z": "last"},
                            actions=[dict(action_id="ordinary", label="Ordinary",
                                          applicable_categories=["plugin-defined:custom"],
                                          execution_policy="plugin"),
                                     dict(action_id="typed", label="Typed")])
            def legacy(identity, action=None, **changes):
                frozen = item(identity)
                frozen.update(changes)
                return call("execute", item=frozen, action_id=action, argument=None)["status"]
            def typed(target):
                return call("typed_execute", item=item(target), action_id="typed",
                            schema_fingerprint=fingerprint,
                            values=[dict(field_id="text", kind="text", value="value")])
            def suggest(text):
                return call("suggest", generation=request_id + 1, text=text, normalized=text)

            call("handshake", protocol_version=1)
            self.assertIsNone(call("build_catalog").get("error"))
            for n in range(4097):
                self.assertEqual(suggest("query-" + str(n))["state"], "final", n)
            self.assertEqual(typed("query-4096")["status"], "completed")
            self.assertEqual(typed("query-0")["code"], "unavailable")
            self.assertEqual(legacy("query-0", "typed"), "failed")
            self.assertEqual(legacy("query-0", "ordinary"), "failed")
            self.assertEqual(legacy("query-4096", "typed"), "failed")
            self.assertEqual(legacy("query-4096"), "failed")
            self.assertEqual(legacy("query-4096", "ordinary"), "ok")
            self.assertEqual(legacy("ordinary-query-4096"), "ok")
            self.assertEqual(legacy("ordinary-query-4096", "ordinary"), "ok")
            self.assertEqual(legacy("ordinary-query-4096", "missing"), "failed")
            self.assertEqual(legacy("ordinary-query-4096", target="forged"), "failed")
            self.assertEqual(legacy("never-published", "ordinary"), "failed")
            self.assertEqual(legacy("catalog-1-0", "ordinary"), "ok")
            self.assertEqual(legacy("catalog-1-4099"), "ok")

            # Catalog replacement retires only catalog authority.
            self.assertIsNone(call("build_catalog").get("error"))
            self.assertEqual(legacy("catalog-1-0", "ordinary"), "failed")
            self.assertEqual(legacy("catalog-2-0", "ordinary"), "ok")
            self.assertEqual(legacy("ordinary-query-4096"), "ok")
            self.assertEqual(typed("query-4096")["status"], "completed")

            for state in ("failed", "cancelled"):
                self.assertEqual(suggest(state)["state"], state)
                send(dict(kind="set_cancel", cancelled=False))
                for target in ("query-4096", state, "ordinary-" + state,
                               "partial-" + state + "-0"):
                    self.assertEqual(legacy(target, "ordinary"), "failed", target)
                    self.assertEqual(typed(target)["code"], "unavailable", target)
                self.assertEqual(legacy("catalog-2-0"), "ok")

            self.assertEqual(suggest("typed-original")["state"], "final")
            self.assertEqual(typed("newest")["status"], "completed")
            self.assertEqual(legacy("successful-partial-0", "ordinary"), "ok")
            self.assertEqual(legacy("newest", "typed"), "failed")
            self.assertEqual(suggest("ordinary-republish")["state"], "final")
            self.assertEqual(typed("newest")["code"], "unavailable")
            self.assertEqual(legacy("newest"), "ok")
            self.assertEqual(legacy("newest", "typed"), "ok")

    def test_schema_free_plugins_keep_legacy_execution_compatibility(self):
        source = '''
from crikey_sdk import Plugin, Item
class Impl(Plugin):
    def suggest(self, query, context):
        context.emit(Item(query.text, "Dynamic", "dynamic"))
    def execute(self, item, action, argument):
        if item.stable_id != "unpublished" or action != "undeclared" or argument != "legacy":
            raise RuntimeError("legacy arguments changed")
'''
        with worker(source) as (exchange, _send):
            exchange(dict(id=1, kind="handshake", protocol_version=1))
            for request_id, target in ((2, "first"), (3, "replacement")):
                self.assertEqual(exchange(dict(id=request_id, kind="suggest", generation=request_id,
                                               text=target, normalized=target))["state"], "final")
            reply = exchange(dict(id=4, kind="execute", item=dict(stable_id="unpublished"),
                                  action_id="undeclared", argument="legacy"))
            self.assertEqual(reply["status"], "ok")

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
