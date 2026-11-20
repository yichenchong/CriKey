"""Selected-only previews, including bounded failures that preserve their worker."""

import asyncio
import os
from crikey_sdk import Item, Plugin, Preview, PreviewImage, PreviewValue


def rows():
    return [Item(stable_id=key, label=label, target=key, preview_deferred=True,
                 metadata={"fixture.pid": str(os.getpid())})
            for key, label in (("A", "A — delayed preview"),
                               ("B", "B — immediate preview"),
                               ("error", "Preview error"),
                               ("empty", "Empty preview"),
                               ("aggregate", "Oversized escaped metadata"),
                               ("escaped-log", "Oversized escaped preview frame"),
                               ("image", "Bounded RGBA preview"))]


class PreviewPlugin(Plugin):
    def build_catalog(self):
        self._selected_details = {item.stable_id: "Selected " + item.stable_id for item in rows()}
        return rows()

    def suggest(self, query, context):
        # This cache exists only on the exact instance that produced these rows.
        # A second preview process must fail rather than invent matching state.
        self._selected_details = {item.stable_id: "Selected " + item.stable_id for item in rows()}
        for item in rows():
            context.emit(item)

    async def preview(self, item, context):
        detail = self._selected_details[item.stable_id]
        if item.stable_id == "A":
            # Deliberately finish late: cancellation/presentation admission must
            # still prevent A from overwriting a newly selected B.
            await asyncio.sleep(0.25)
        if item.stable_id == "error":
            raise RuntimeError("fixture preview failure")
        if item.stable_id == "empty":
            return None
        if item.stable_id == "aggregate":
            # Every string fits 8 KiB, but together they exceed the 256 KiB
            # content budget and would expand past 12 MiB on the JSON wire.
            return Preview(values=tuple(PreviewValue("\0" * 8192, "\0" * 8192)
                                        for _ in range(128)))
        if item.stable_id == "escaped-log":
            # Content is valid; captured NULs still expand past the frame cap.
            for _ in range(512):
                context.log("\0" * 4096)
            return Preview(text=detail)
        if item.stable_id == "image":
            return Preview(image=PreviewImage(1, 1, bytes((32, 160, 220, 255))))
        return Preview(text=detail,
                       values=(PreviewValue("Item", item.stable_id),
                               PreviewValue("Documentation", "https://example.org/", "link")))


class CatalogOnly(PreviewPlugin):
    """Only catalog construction initializes the selected item's state."""

    def suggest(self, query, context):
        return None


class WithoutPreview(Plugin):
    """Suggestions stay usable even when deferred preview is unavailable."""

    def suggest(self, query, context):
        context.emit(rows()[1])


class CatalogRaises(WithoutPreview):
    """A catalog exception must not disable this lazy suggestion provider."""

    def build_catalog(self):
        raise RuntimeError("fixture catalog failure")


class IdleExit(CatalogOnly):
    """Exit on a filesystem signal, without another host transport call."""

    def build_catalog(self):
        import pathlib
        import threading
        import time

        marker = pathlib.Path(__file__).with_name("exit-now")

        def exit_when_signalled():
            while not marker.exists():
                time.sleep(0.001)
            os._exit(21)

        threading.Thread(target=exit_when_signalled, daemon=True).start()
        return super().build_catalog()
