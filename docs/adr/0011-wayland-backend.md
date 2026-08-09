# ADR-0011: Wayland global shortcuts through the GlobalShortcuts portal

Status: Accepted
Spec: §6.1, §18.2, §18.6

## Context

Wayland is the default session on the distributions CriKey targets, but it
has no global-grab protocol: a client cannot take a key away from the
compositor. The sanctioned route is
`org.freedesktop.portal.GlobalShortcuts` over the session bus, where the
compositor owns the binding and the user approves it.

## Decision

`WaylandHotkeyService` (`crikey-platform-linux/src/wayland.rs`) is a `zbus` 5
blocking client. It creates a portal session, binds accelerators with
`preferred_trigger` in shortcuts-spec syntax, and turns `Activated` into the
same `HotkeyService` callback X11 delivers.

`BindShortcuts` may be attempted once per portal session. Each `register` and
`unregister` therefore creates a new session bound to the accumulated set and
closes the previous one only after the new binding is confirmed; a refused
rebinding leaves existing shortcuts untouched.

Capability reporting probes the portal once and caches the answer.
`GlobalHotkeys` under Wayland is `Available` only when the portal answered its
`version` property, and `Unavailable` when nothing did. The session type alone
cannot support this claim because the portal is a separate service.

Window enumeration and activation stay `UnsupportedDesktopEnvironment` under
Wayland. `xdg-activation` needs a compositor-issued token, while
`WindowService` additionally requires enumerating other clients' windows,
which no Wayland protocol offers to an ordinary client.

## Consequences

- The activation chord works on GNOME or KDE subject to user approval.
- Registration is user-visible and reports portal refusal rather than
  pretending the chord is live.
- Rebinding may re-prompt; the alternative is refusing every later
  registration.
- `zbus` is target-gated so Windows and macOS checks never see it.

## Alternatives

- **`wlr-virtual-keyboard`/`hyprland-global-shortcuts`.** Real, but neither
  exists on GNOME or KDE.
- **libei / RemoteDesktop portal.** Grants input injection, not a shortcut,
  and prompts for a broader permission.
- **Raw D-Bus FFI.** A second unreviewed authentication and marshalling stack.
