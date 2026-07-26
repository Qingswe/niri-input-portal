#!/usr/bin/env bash
# Remove the InputCapture portal backend and undo the portal routing.
set -euo pipefail

PREFIX="${PREFIX:-/usr}"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
CONF_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/xdg-desktop-portal"
DESKTOP="$(printf '%s' "${XDG_CURRENT_DESKTOP:-niri}" | tr '[:upper:]' '[:lower:]' | cut -d: -f1)"
CONF="$CONF_DIR/$DESKTOP-portals.conf"

echo "==> Releasing any active capture first"
"$PREFIX/lib/niri-input-portal" --release 2>/dev/null || true

echo "==> Stopping"
systemctl --user stop niri-input-portal.service 2>/dev/null || true

echo "==> Removing files"
sudo rm -f "$PREFIX/lib/niri-input-portal" \
           "$PREFIX/share/xdg-desktop-portal/portals/niri-input.portal" \
           "$PREFIX/share/dbus-1/services/org.freedesktop.impl.portal.desktop.niri-input.service"
rm -f "$UNIT_DIR/niri-input-portal.service"
rm -rf "$UNIT_DIR/niri-input-portal.service.d"
systemctl --user daemon-reload

if [ -f "$CONF" ]; then
    echo "==> Dropping the InputCapture and Clipboard lines from $CONF"
    sed -i '/^org\.freedesktop\.impl\.portal\.InputCapture=niri-input;$/d' "$CONF"
    sed -i '/^org\.freedesktop\.impl\.portal\.Clipboard=niri-input;$/d' "$CONF"
fi

echo "==> Restarting xdg-desktop-portal"
systemctl --user restart xdg-desktop-portal.service

echo
echo "Removed. Delete the Mod+Shift+Escape binding from your niri config too;"
echo "it now points at a binary that is gone."
