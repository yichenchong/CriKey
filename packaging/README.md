# Packaging

Per-platform distribution artefacts.

- `windows/` - MSI/MSIX, code signing, per-user install layout.
- `macos/`   - `.app` bundle, notarization, hardened runtime entitlements.
- `linux/`   - tarball, `.deb`/`.rpm`, Flatpak manifest, desktop entry.

Packaging must bundle the Python runtime profiles CriKey ships with and must
not depend on a system-wide `site-packages` (spec 15.4).
