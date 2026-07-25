# niri-input-portal

An `org.freedesktop.impl.portal.InputCapture` backend for the [niri](https://github.com/YaLTeR/niri)
compositor, so KVM software that speaks the input-capture portal (Synergy 3,
Deskflow, Input Leap) can act as a **server** under niri.

## Why this exists

Synergy 3 already implements the entire client half of the protocol — its
`synergy-core` binary contains `deskflow::PortalInputCapture`, `deskflow::EiScreen`,
libportal's `inputcapture*.c` and links `libei.so.1`. The chain breaks below it:

| Layer | Under niri |
|---|---|
| `synergy-core` PortalInputCapture | complete |
| `org.freedesktop.portal.InputCapture` (frontend) | present, but `SupportedCapabilities = 0` |
| `org.freedesktop.impl.portal.InputCapture` (backend) | **missing** |
| `org.gnome.Mutter.InputCapture` | **not provided by niri** |

`xdg-desktop-portal-gnome` implements the InputCapture backend on top of
`org.gnome.Mutter.InputCapture`. niri exposes `Mutter.ScreenCast`, `Mutter.DisplayConfig`,
`Mutter.ServiceChannel`, `Shell.Introspect` and `Shell.Screenshot` — but not
`Mutter.InputCapture` — so xdp-gnome never publishes the interface and every
`CreateSession` fails:

```
core - ERROR - failed to initialize input capture session, quitting: CreateSession failed
```

This process publishes that backend directly, built on Wayland protocols niri
does support (`wlr-layer-shell`, `pointer-constraints`, `relative-pointer`).

## Status

| Phase | Scope | State |
|---|---|---|
| 1 | Portal session + client reaches EIS | **done, verified** |
| 2 | Pointer barriers, emit `Activated` | **done, verified** |
| 3 | Forward relative motion, keys, scroll | **done**, not yet verified against a remote screen |
| 4 | `Release` + restore the local cursor | partial — see below |
| 5 | Multi-output, scale, transform, hotplug | partial (zones and barrier placement are correct) |

## How capture works

A barrier is a one-pixel transparent `wlr-layer-shell` surface on the overlay
layer, pinned to the matching output edge. Crossing it means:

1. The pointer enters that surface. The Wayland thread locks the pointer to it
   with `zwp_locked_pointer_v1` **immediately**, before telling the portal
   anything, so no motion leaks onto the local screen during the D-Bus round
   trip. Events observed before the EIS handle arrives are buffered and flushed.
2. The layer surface switches to `KeyboardInteractivity::Exclusive`, which is
   what lets keystrokes reach the remote screen instead of the locally focused
   window.
3. The cursor image is hidden, so the screen looks like the pointer really left.
4. `zwp_relative_pointer_v1` deltas, buttons, scroll and keys are written to the
   session's EIS connection. The EIS `start_emulating` sequence is set to the
   same value as the portal's `activation_id`, which is what lets the client
   attribute an event stream to one activation.

`Release` destroys the lock, drops the keyboard grab and rearms the barriers.

### Escape hatch

An exclusive keyboard grab plus a locked pointer is not something to be stuck
in. If the client stops calling `Release` — it crashed, or the remote screen
never connected — **Ctrl+Alt+Escape** force-releases the capture locally. That
combination is swallowed rather than forwarded.

The Wayland thread also ends the capture by itself if the compositor drops the
lock, if the output the barrier lives on disappears, or on shutdown.

Phase 1 verified against real `synergy-core` 1.21.1-beta:

```
session created                capabilities=3
returning 2 zone(s)            zone_set=2
handed EIS socket to client
EIS handshake complete, client is a Receiver context
client bound seat with Pointer | Keyboard | Scroll | Button
accepted 2 barrier(s), rejected 0
capture enabled                barriers=2
```

`synergy-core` now reaches `connection state: Listening` and stays up, instead of
exiting after 0.13s in a restart loop.

Installed and verified on niri 25.11, xdg-desktop-portal 1.20.4, libei 1.5.0.

**Capture does not work yet.** Barriers are accepted and stored but nothing watches
them, so moving the pointer at a screen edge will not switch to the remote machine.
That is phase 2 and 3.

## Build

```sh
cargo build --release
```

## Install

```sh
sudo install -Dm755 target/release/niri-input-portal /usr/lib/niri-input-portal
sudo install -Dm644 data/niri-input.portal /usr/share/xdg-desktop-portal/portals/niri-input.portal
sudo install -Dm644 data/org.freedesktop.impl.portal.desktop.niri-input.service \
    /usr/share/dbus-1/services/org.freedesktop.impl.portal.desktop.niri-input.service
install -Dm644 data/niri-input-portal.service ~/.config/systemd/user/niri-input-portal.service
systemctl --user daemon-reload
```

Then route the interface to this backend in `~/.config/xdg-desktop-portal/niri-portals.conf`:

```ini
org.freedesktop.impl.portal.InputCapture=niri-input;
```

and restart the portal:

```sh
systemctl --user restart xdg-desktop-portal.service
```

### Do not use XDG_DESKTOP_PORTAL_DIR to test this

It looks like a convenient way to register the backend without root, but setting
it makes xdg-desktop-portal **skip `portals.conf` entirely**. Every interface
routed through that file falls back to `UseIn` matching, and since `gnome.portal`
declares `UseIn=gnome` while niri sets `XDG_CURRENT_DESKTOP=niri`, ScreenCast and
Screenshot silently lose their backend — screen sharing and screenshots break.
Install to `/usr/share` instead.

## Verifying

`SupportedCapabilities` is the quickest check — it reads `0` when no backend is
present and `3` (keyboard | pointer) when this one is wired up:

```sh
busctl --user get-property org.freedesktop.portal.Desktop \
    /org/freedesktop/portal/desktop \
    org.freedesktop.portal.InputCapture SupportedCapabilities
```

The impl interface can also be driven directly, without xdg-desktop-portal:

```sh
busctl --user call org.freedesktop.impl.portal.desktop.niri-input \
    /org/freedesktop/portal/desktop org.freedesktop.impl.portal.InputCapture \
    GetZones 'oosa{sv}' /req/1 /session/1 test.app 0
```

Set `NIRI_INPUT_PORTAL_LOG=debug` for per-call tracing.

## Known gaps

- `Release`'s `cursor_position` hint is passed to `set_cursor_position_hint`, but
  the barrier surface is one pixel wide, so the hint can only move the cursor
  *along* the edge rather than back into the screen. Returning from a remote
  screen therefore leaves the cursor pinned at the edge. Doing this properly
  needs a wider surface that is grown for the release.
- `org.freedesktop.impl.portal.Request` objects are not exported, so in-flight
  calls cannot be cancelled. Harmless today because every handler returns
  immediately.
- niri's own compositor keybindings (`Mod`+…) are consumed before any client sees
  them, so those combinations cannot be forwarded to a remote screen.
- Barrier surfaces occupy the outermost pixel column or row of an output while
  armed, so a click landing exactly there is swallowed.
- Output scale and transform are read but not applied to motion deltas; on a
  fractionally scaled output the remote pointer speed will not match the local
  one exactly.
