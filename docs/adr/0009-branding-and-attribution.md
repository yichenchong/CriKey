# ADR-0009: Branding and attribution

Status: Accepted
Spec: §1, §3.3, §14.13

## Context

CriKey implements a compatibility layer for a third party's documented plugin
API. Naming that layer after the other project, or implying endorsement, is both
a legal risk and inaccurate.

## Decision

- The subsystem is named the **Legacy Compatibility Layer**. "Keypirinha" is
  never part of a CriKey subsystem, product, or crate name.
- The crate is `crikey-legacy-compat`; its types use "legacy", not the other
  project's name.
- "Keypirinha" appears only descriptively — identifying the API and package
  format the layer is compatible with — in documentation, diagnostics, and the
  compatibility matrix.
- The sanctioned public phrasing is: *"CriKey supports existing Keypirinha
  plugins through its Legacy Compatibility Layer."*
- No Keypirinha logos or visual identity. No claim of sponsorship, endorsement,
  partnership or successor status.
- `NOTICE.md` ships with every distributed artefact and is linked from the README.

## Consequences

- Names like `keypirinha-host` are rejected in review, even when convenient.
- Python shim modules must still be *importable* as `keypirinha`,
  `keypirinha_util`, `keypirinha_net` and `keypirinha_wintypes`, because the
  legacy API requires those import names. Compatibility import names are a
  technical requirement and are not branding; they live inside the Legacy
  Compatibility Layer's runtime assets and are documented as such.
- UI surfaces referring to legacy plugins say "legacy plugin" or "Legacy
  Compatibility Layer", not the other project's name.
