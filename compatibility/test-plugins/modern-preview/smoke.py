"""Real subprocess SDK smoke. Run from core: python3 compatibility/test-plugins/modern-preview/smoke.py"""

import json
import os
from pathlib import Path
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[3]
FIXTURE = Path(__file__).resolve().parent
SDK = ROOT / "sdk" / "python"


class Worker:
    def __init__(self, entrypoint):
        env = dict(os.environ, PYTHONPATH=os.pathsep.join((str(FIXTURE), str(SDK))),
                   PYTHONDONTWRITEBYTECODE="1", CRIKEY_MODERN_PLUGIN_ID="modern.preview-fixture",
                   CRIKEY_MODERN_ENTRYPOINT=entrypoint, CRIKEY_MODERN_PROTOCOL_VERSION="1")
        self.process = subprocess.Popen([sys.executable, "-S", str(SDK / "_crikey_modern_worker.py")],
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        text=True, encoding="utf-8", env=env)

    def send(self, **frame):
        self.process.stdin.write(json.dumps(frame) + "\n")
        self.process.stdin.flush()

    def receive(self):
        line = self.process.stdout.readline()
        assert line, "worker exited before terminal reply"
        return json.loads(line)

    def close(self):
        try:
            self.send(id=99, kind="shutdown")
            self.process.communicate(timeout=3)
        finally:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait()


def identity(item, selection, request):
    return dict(view=1, query=1, plugin="modern.preview-fixture", incarnation=1,
                item=item, selection=selection, request=request)


def main():
    worker = Worker("preview_plugin:PreviewPlugin")
    try:
        worker.send(id=1, kind="handshake", protocol_version=1)
        assert "preview_v1" in worker.receive()["capabilities"]
        worker.send(id=200, kind="preview", identity=identity("B", 1, 200),
                    item={"stable_id": "B", "label": "B", "target": "B"})
        unprepared = worker.receive()
        assert unprepared["status"] == "failed", "preview must require originating suggestion state"
        worker.send(id=2, kind="suggest", generation=1, text="preview", normalized="preview")
        suggestions = worker.receive()
        items = {item["stable_id"]: item for item in suggestions["items"]}
        assert all(item["metadata"]["crikey.preview"] == "deferred" for item in items.values())
        a, b = identity("A", 1, 1), identity("B", 2, 2)
        worker.send(id=3, kind="preview", identity=a, item=items["A"])
        time.sleep(0.03)
        # Presentation is retired first, then computation is cancelled. Only
        # exact identity equality admits a terminal into this harness's pane.
        selected = b
        worker.send(kind="set_cancel", cancelled=True)
        late_a = worker.receive()
        assert late_a["identity"] == a and late_a["identity"] != selected
        assert late_a["status"] == "cancelled"
        worker.send(kind="set_cancel", cancelled=False)
        worker.send(id=4, kind="preview", identity=b, item=items["B"])
        answer = worker.receive()
        assert answer["identity"] == selected
        assert answer["content"]["text"] == "Selected B"
        for number, key, status in ((5, "error", "failed"), (6, "empty", "empty"), (7, "image", "content")):
            target = identity(key, number, number)
            worker.send(id=number, kind="preview", identity=target, item=items[key])
            reply = worker.receive()
            assert reply["identity"] == target and reply["status"] == status
        pid = worker.process.pid
        for number, key, diagnostic in (
            (8, "aggregate", "preview aggregate text exceeds 256 KiB"),
        ):
            target = identity(key, number, number)
            worker.send(id=number, kind="preview", identity=target, item=items[key])
            failure = worker.receive()
            assert failure["identity"] == target and failure["status"] == "failed"
            assert diagnostic in failure["detail"] and len(failure["detail"].encode("utf-8")) <= 8192
            assert "content" not in failure, "reject invalid preview rather than truncate it"
            worker.send(id=number + 10, kind="suggest", generation=2, text="again", normalized="again")
            fresh = {item["stable_id"]: item for item in worker.receive()["items"]}
            assert all(item["metadata"]["fixture.pid"] == str(pid) for item in fresh.values())
            worker.send(id=number + 20, kind="preview", identity=b, item=fresh["B"])
            valid = worker.receive()
            assert valid["status"] == "content" and valid["content"]["text"] == "Selected B"
            assert worker.process.poll() is None and worker.process.pid == pid
        # Query/view/incarnation changes likewise make this answer ineligible.
        for field in ("view", "query", "incarnation", "selection", "request"):
            changed = dict(b)
            changed[field] += 1
            assert answer["identity"] != changed
    finally:
        worker.close()

    worker = Worker("preview_plugin:WithoutPreview")
    try:
        worker.send(id=1, kind="handshake", protocol_version=1)
        assert "preview_v1" not in worker.receive()["capabilities"]
        worker.send(id=2, kind="suggest", generation=1, text="preview", normalized="preview")
        item = worker.receive()["items"][0]
        worker.send(id=3, kind="preview", identity=b, item=item)
        assert worker.receive()["status"] == "unsupported"
        worker.send(id=4, kind="suggest", generation=2, text="still usable", normalized="still usable")
        assert worker.receive()["items"][0]["stable_id"] == "B"
    finally:
        worker.close()
    print("modern preview SDK subprocess scenarios passed")


if __name__ == "__main__":
    main()
