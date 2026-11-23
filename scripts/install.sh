#!/usr/bin/env bash
# Install Sotto for the current user, including the bundled model.
set -euo pipefail
cd "$(dirname "$0")/.."

APP="$HOME/.local/opt/sotto"
BIN="$HOME/.local/bin"
DESK="$HOME/.local/share/applications"
ICON="$HOME/.local/share/icons/hicolor/128x128/apps"

echo "==> building"
./scripts/build.sh

echo "==> installing to $APP"
rm -rf "$APP"
mkdir -p "$APP/model" "$BIN" "$DESK" "$ICON"
install -m755 target/release/sotto "$APP/sotto"
install -m755 target/release/sotto-stt "$APP/sotto-stt"
install -m644 model/*.json model/model.safetensors "$APP/model/"
install -m644 src-tauri/icons/128x128.png "$ICON/sotto.png"

ln -sf "$APP/sotto" "$BIN/sotto"

# The file name must equal the window's app id ("sotto"), or KDE cannot match
# the window to this entry and shows a generic icon in the taskbar.
install -m644 packaging/sotto.desktop "$DESK/sotto.desktop"
update-desktop-database "$DESK" >/dev/null 2>&1 || true
command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 --noincremental >/dev/null 2>&1 || true

echo "==> done. Launch 'Sotto' from the menu, or run: sotto"
echo "    model bundled at $APP/model ($(du -sh "$APP/model" | cut -f1))"
