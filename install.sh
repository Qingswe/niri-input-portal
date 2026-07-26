#!/usr/bin/env bash
# Build and install the InputCapture portal backend.
#
# Everything here is idempotent, so rerunning it after a `git pull` is the
# normal way to upgrade.
set -euo pipefail

PREFIX="${PREFIX:-/usr}"
BIN="$PREFIX/lib/niri-input-portal"
PORTAL_DIR="$PREFIX/share/xdg-desktop-portal/portals"
DBUS_DIR="$PREFIX/share/dbus-1/services"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/xdg-desktop-portal"

# xdg-desktop-portal looks for "<desktop>-portals.conf", lowercased.
DESKTOP="$(printf '%s' "${XDG_CURRENT_DESKTOP:-niri}" | tr '[:upper:]' '[:lower:]' | cut -d: -f1)"
CONF="$CONF_DIR/$DESKTOP-portals.conf"

cd "$(dirname "$0")"

echo "==> Building"
cargo build --release

echo "==> Installing (needs root for $PREFIX)"
sudo install -Dm755 target/release/niri-input-portal "$BIN"
sudo install -Dm644 data/niri-input.portal "$PORTAL_DIR/niri-input.portal"
sudo install -Dm644 data/org.freedesktop.impl.portal.desktop.niri-input.service \
    "$DBUS_DIR/org.freedesktop.impl.portal.desktop.niri-input.service"

# The unit is D-Bus activated, so it is never enabled; xdg-desktop-portal starts
# it on demand and it exits with the session.
install -Dm644 data/niri-input-portal.service "$UNIT_DIR/niri-input-portal.service"
systemctl --user daemon-reload

echo "==> Routing InputCapture to this backend in $CONF"
mkdir -p "$CONF_DIR"
if [ ! -f "$CONF" ]; then
    printf '[preferred]\ndefault=gnome;gtk;\n' > "$CONF"
    echo "    created $CONF"
fi
if grep -q '^org\.freedesktop\.impl\.portal\.InputCapture=' "$CONF"; then
    sed -i 's|^org\.freedesktop\.impl\.portal\.InputCapture=.*|org.freedesktop.impl.portal.InputCapture=niri-input;|' "$CONF"
    echo "    updated the existing InputCapture line"
else
    printf 'org.freedesktop.impl.portal.InputCapture=niri-input;\n' >> "$CONF"
    echo "    added the InputCapture line"
fi

echo "==> Restarting xdg-desktop-portal"
systemctl --user restart xdg-desktop-portal.service
sleep 2

CAPS="$(busctl --user get-property org.freedesktop.portal.Desktop \
    /org/freedesktop/portal/desktop \
    org.freedesktop.portal.InputCapture SupportedCapabilities 2>/dev/null || true)"

echo
if [ "$CAPS" = "u 3" ]; then
    echo "Installed. SupportedCapabilities is $CAPS (keyboard | pointer)."
else
    echo "Installed, but SupportedCapabilities reads '${CAPS:-<unavailable>}' rather than 'u 3'."
    echo "Check 'journalctl --user -u xdg-desktop-portal -n 30' for which backend it chose."
fi

cat <<'EOF'

One manual step is left, and it matters: add an escape binding to your niri
config. Nothing inside this process can free a stuck pointer, because the
keyboard is grabbed and the pointer is locked at that moment. niri resolves its
own bindings before clients see them, which is exactly why this works:

    Mod+Shift+Escape allow-inhibiting=false { spawn "/usr/lib/niri-input-portal" "--release"; }

Verify it before you trust it: press the combination and confirm
"ForceRelease requested" shows up in

    journalctl --user -u niri-input-portal -f
EOF
