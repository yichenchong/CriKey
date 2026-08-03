# Packaging

The per-platform directories are reserved for distribution work:

- `windows/` - MSI/MSIX, code signing, per-user install layout.
- `macos/`   - `.app` bundle, notarization, hardened runtime entitlements.
- `linux/`   - tarball, `.deb`/`.rpm`, Flatpak manifest, desktop entry.

The directories currently contain placeholders; no distribution artefacts are
checked in yet. When packaging is implemented, each artefact must bundle the
Python runtime profiles it ships and must not depend on a system-wide
`site-packages` (spec §15.4).
