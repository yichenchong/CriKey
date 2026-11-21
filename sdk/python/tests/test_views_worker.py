"""Real child-process standard-route and outcome contract regressions."""
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest

from crikey_sdk import ArgumentSchema, TextField, PasswordField, DropdownField, DropdownOption


class StandardViewWorkerTests(unittest.TestCase):
    def start_worker(self, capable=True):
        sdk = Path(__file__).resolve().parents[1]
        fixture = sdk.parents[1] / "compatibility/test-plugins/modern-views"
        environment = dict(os.environ, PYTHONPATH=os.pathsep.join((str(fixture), str(sdk))),
                           CRIKEY_MODERN_PLUGIN_ID="views-fixture",
                           CRIKEY_MODERN_ENTRYPOINT="views_plugin:ViewsPlugin",
                           CRIKEY_MODERN_PROTOCOL_VERSION="1")
        self.child = subprocess.Popen([sys.executable, "-S", str(sdk / "_crikey_modern_worker.py")],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, encoding="utf-8", env=environment)
        self.addCleanup(self.stop_worker)
        self.next_id = 0
        return self.send(kind="handshake", protocol_version=1,
                         capabilities=["structured_view_v1"] if capable else [])

    def stop_worker(self):
        if self.child.poll() is None:
            self.child.stdin.write(json.dumps(dict(id=9999, kind="shutdown")) + "\n")
            self.child.stdin.flush()
        self.child.communicate(timeout=10)

    def send(self, **frame):
        self.next_id += 1
        frame["id"] = self.next_id
        self.child.stdin.write(json.dumps(frame) + "\n")
        self.child.stdin.flush()
        reply = json.loads(self.child.stdout.readline())
        self.assertEqual(reply["id"], self.next_id)
        return reply

    def open(self, route, *, previous=None):
        return self.send(kind="structured_view", route=route,
            session=dict(logical_view=1, surface=1), mode="open" if previous is None else "refresh",
            previous_revision=previous, reason="explicit",
            state=None if previous is None else dict(selected_item_id="shared", scope_id=None,
                                                     input_item_id=None, input_action_id=None),
            query="", anchor=None)

    def test_sync_async_outcomes_namespace_refresh_form_and_privacy(self):
        handshake = self.start_worker()
        self.assertIn("structured_view_v1", handshake["capabilities"])
        self.assertIn("typed_arguments_v1", handshake["capabilities"])
        self.assertNotIn("result_presentation_v1", handshake["capabilities"])
        catalog = self.send(kind="build_catalog")
        root = catalog["items"][0]
        outcome = self.send(kind="execute", item=root, action_id=None, argument=None)
        self.assertEqual(outcome["navigation"], dict(kind="push", route="gallery"))
        gallery = self.open("gallery")
        self.assertEqual(gallery["descriptor"]["view"]["kind"], "grid")
        frozen = gallery["descriptor"]["definitions"][0]["item"]
        context = dict(route="gallery", session=gallery["session"], revision=gallery["id"])
        forged = self.send(kind="execute", item=root, action_id="open", argument=None, view_context=context)
        self.assertEqual(forged["status"], "failed")
        outcome = self.send(kind="execute", item=frozen, action_id="open", argument=None, view_context=context)
        self.assertEqual(outcome["navigation"], dict(kind="push", route="detail"))
        refreshed = self.open("gallery", previous=gallery["id"])
        self.assertEqual(refreshed["status"], "content")
        stale = self.send(kind="execute", item=frozen, action_id="open", argument=None, view_context=context)
        self.assertEqual(stale["status"], "failed")
        detail = self.open("detail")
        self.assertEqual(detail["descriptor"]["view"]["kind"], "detail")
        form = self.open("form")
        definition = form["descriptor"]["definitions"][0]
        schema = self.schema()
        root_typed = self.send(kind="typed_execute", item=catalog["items"][1], action_id=None,
            schema_fingerprint=schema.fingerprint(), values=[
                dict(field_id="name", kind="text", value="valid"),
                dict(field_id="password", kind="password", value="ROOT_TYPED_SECRET"),
                dict(field_id="region", kind="dropdown", value="local")])
        self.assertEqual(root_typed["navigation"], dict(kind="push", route="gallery"))
        def submit(name):
            return self.send(kind="typed_execute", item=definition["item"], action_id=None,
                view_context=dict(route="form", session=form["session"], revision=form["id"]),
                schema_fingerprint=schema.fingerprint(), values=[
                    dict(field_id="name", kind="text", value=name),
                    dict(field_id="password", kind="password", value="ROUTE_SECRET_MUST_NOT_APPEAR"),
                    dict(field_id="region", kind="dropdown", value="local")])
        rejected = submit("reject")
        self.assertEqual(rejected["failures"], [dict(field_id="name", code="rejected")])
        completed = submit("valid")
        self.assertEqual(completed["navigation"], dict(kind="replace", route="done"))
        self.assertNotIn("ROUTE_SECRET_MUST_NOT_APPEAR", json.dumps(completed))
        self.child.stdin.write(json.dumps(dict(id=9999, kind="shutdown")) + "\n")
        self.child.stdin.flush()
        stdout, stderr = self.child.communicate(timeout=10)
        self.assertNotIn("ROUTE_SECRET_MUST_NOT_APPEAR", stdout + stderr)

    def schema(self):
        return ArgumentSchema((
            TextField(id="name", label="Name", required=True, max_bytes=128,
                      guidance="Use reject to exercise a field rejection."),
            PasswordField(id="password", label="Password", required=True, max_bytes=128),
            DropdownField(id="region", label="Region", required=True,
                          options=(DropdownOption("local", "Local"), DropdownOption("remote", "Remote")), default="local"),
        ))

    def test_old_host_gets_refusal_not_discarded_navigation(self):
        handshake = self.start_worker(capable=False)
        self.assertNotIn("structured_view_v1", handshake["capabilities"])
        catalog = self.send(kind="build_catalog")
        outcome = self.send(kind="execute", item=catalog["items"][0], action_id=None, argument=None)
        self.assertEqual(outcome["status"], "failed")
        self.assertEqual(self.open("gallery")["code"], "unsupported")
        typed = self.send(kind="typed_execute", item=catalog["items"][1], action_id=None,
            schema_fingerprint=self.schema().fingerprint(), values=[
                dict(field_id="name", kind="text", value="valid"),
                dict(field_id="password", kind="password", value="OLD_HOST_SECRET"),
                dict(field_id="region", kind="dropdown", value="local")])
        self.assertEqual(typed["status"], "failed")
        self.assertEqual(typed["code"], "unsupported")


if __name__ == "__main__":
    unittest.main()
