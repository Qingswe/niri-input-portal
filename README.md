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
| 3 | Forward relative motion, keys, scroll | **done, verified** against a real remote screen |
| 4 | `Release` + restore the local cursor | **done, verified** |
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

## Getting unstuck

An exclusive keyboard grab plus a locked pointer is not a state to be stranded
in, and the portal protocol has no acknowledgement that would let this process
tell a working capture from a broken one. So the escapes are out-of-band.

**Do not rely on a key handler inside this process.** An earlier version used
Ctrl+Alt+Escape handled in `KeyboardHandler`, which does not work: niri resolves
its own bindings before clients see them, so a combination niri claims never
arrives, and one it does not claim arrives only if the grab is already
functioning. Using the grabbed keyboard to escape the grab is circular.

The escapes that do work, in order of convenience:

1. **A niri keybinding.** This is the reliable one, precisely *because* niri
   handles its bindings ahead of clients. Add to your niri config:

   ```kdl
   Mod+Shift+Escape allow-inhibiting=false { spawn "/usr/lib/niri-input-portal" "--release"; }
   ```

   `allow-inhibiting=false` keeps it working even if a client asks to inhibit
   shortcuts.

2. **From another machine or a TTY**, over SSH:

   ```sh
   niri-input-portal --release   # drop the capture, keep the barriers
   niri-input-portal --disarm    # drop the capture and clear the screen edges
   niri-input-portal --status    # what does it think is going on
   ```

3. **Kill the process.** `pkill -f niri-input-portal` always works: the
   compositor destroys the surface and with it the pointer lock as soon as the
   client disconnects.

Automatic releases, needing no intervention:

- The EIS connection dying. A capture feeding a dead socket can never be ended
  by its client, which is exactly the case where the remote screen was never
  connected.
- An idle watchdog, default 15s, set with `NIRI_INPUT_PORTAL_IDLE_TIMEOUT`
  (seconds, `0` disables). A working capture never goes quiet for that long,
  because the user is driving the remote screen from this machine. Note this is
  a backstop, not a primary escape: someone hammering keys to get free would
  keep resetting it.
- The compositor dropping the lock, the output disappearing, or shutdown.

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

## Build

```sh
cargo build --release
```

## Install

```sh
./install.sh
```

It builds, installs, wires up the portal routing and checks the result. Rerun it
after a `git pull` to upgrade. `./uninstall.sh` reverses everything.

Read the script's closing note: it asks you to add one keybinding to your niri
config by hand, and that binding is the only reliable way out of a stuck
capture. See [Getting unstuck](#getting-unstuck).

### Does it survive a reboot?

Yes, and nothing needs enabling. The service is D-Bus activated rather than
started at login: xdg-desktop-portal asks for
`org.freedesktop.impl.portal.desktop.niri-input` the first time a client wants
input capture, and systemd starts it then. It is not resident when unused.

The unit carries `ConditionEnvironment=WAYLAND_DISPLAY`, which holds because
`niri-session` runs `dbus-update-activation-environment --all` on every login.

### What install.sh touches

| Path | Purpose |
|---|---|
| `/usr/lib/niri-input-portal` | the binary |
| `/usr/share/xdg-desktop-portal/portals/niri-input.portal` | declares the backend |
| `/usr/share/dbus-1/services/…niri-input.service` | D-Bus activation |
| `~/.config/systemd/user/niri-input-portal.service` | the unit activation starts |
| `~/.config/xdg-desktop-portal/<desktop>-portals.conf` | routes InputCapture here |

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

## Returning the cursor

Synergy 3.7 sends a constant `cursor_position` of `(1.0, 0.0)` on every
`Release`, so honouring it verbatim pinned the cursor to the same corner every
time. The hint is now used only when it actually lands on the barrier surface it
claims to be about; otherwise the cursor returns to where it crossed, which is
what it looks like it should do anyway.

That needs somewhere to put it. `set_cursor_position_hint` is surface-local, so a
one-pixel barrier could only slide the cursor *along* the edge. Barrier surfaces
are therefore `BARRIER_DEPTH` (64px) deep while their input region stays a single
outer pixel: detection is unchanged, but the release has room to place the cursor
`RELEASE_INSET` (8px) inside the screen, clear of the strip it just came through.

## Known gaps

- `org.freedesktop.impl.portal.Request` objects are not exported, so in-flight
  calls cannot be cancelled. Harmless today because every handler returns
  immediately.
- niri's own compositor keybindings (`Mod`+…) are consumed before any client sees
  them, so those combinations cannot be forwarded to a remote screen. This is the
  same mechanism the escape binding relies on.
- Barrier surfaces occupy the outermost pixel column or row of an output while
  armed, so a click landing exactly there is swallowed.
- Output scale and transform are read but not applied to motion deltas; on a
  fractionally scaled output the remote pointer speed will not match the local
  one exactly.
- Rearming while the pointer already rests on a barrier captures immediately.
  The release inset makes this unlikely in normal use, but restarting the service
  with the cursor parked against an edge can still trigger it.
