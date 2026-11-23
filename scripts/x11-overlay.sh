#!/usr/bin/env bash
# Verify the overlay's X11 placement, including a monitor that does not start at
# the origin.
#
# Why this exists: on Wayland the compositor positions a layer surface, so the
# X11 path is separate code that only runs when layer-shell is unavailable. It
# went untested, and it was wrong: it computed a position relative to the monitor
# and passed it to `gtk_window_move`, which takes **global** coordinates. On a desk
# where the chosen monitor is not the top-left one, the pill landed on a different
# screen — measured at x=611 when the correct answer was x=1891.
#
# How it runs without a second display: `xrandr --setmonitor` defines a virtual
# monitor region over the existing output. Defining one that starts at x=1280
# reproduces exactly the case the bug broke, and the position can then be measured
# from the X server with `xdotool getwindowgeometry`. The definition is removed on
# exit, including on failure.
#
# Requires an X server. Under KDE Wayland, Xwayland provides one, and
# `GDK_BACKEND=x11` makes the app an X11 client.
#
# Usage:  scripts/x11-overlay.sh
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

MON="sotto_test_offset"
SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/sotto-x11-XXXXXX")"
APP="$ROOT/target/release/sotto"
PASS=0
FAIL=0

ok()   { echo "  $(printf '\033[32mPASS\033[0m')  $1"; PASS=$((PASS+1)); }
bad()  { echo "  $(printf '\033[31mFAIL\033[0m')  $1${2:+  ($2)}"; FAIL=$((FAIL+1)); }
head_(){ echo; echo "$(printf '\033[1m')$1$(printf '\033[0m')"; }

restore() {
  [[ -n "${APP_PID:-}" ]] && kill "$APP_PID" 2>/dev/null
  sleep 1
  xrandr --delmonitor "$MON" 2>/dev/null
  rm -rf "$SANDBOX"
}
trap restore EXIT

if [[ -z "${DISPLAY:-}" ]]; then
  echo "  SKIP  no DISPLAY (this test needs an X server)"
  exit 0
fi
if pgrep -x sotto >/dev/null 2>&1; then
  echo "A Sotto is already running. Close it first: the app is single-instance." >&2
  exit 2
fi

head_ "0. Build with the test-only surface"
if cargo build -p sotto --release --features e2e 2>"$SANDBOX/build.log"; then
  ok "built with --features e2e"
else
  bad "built with --features e2e" "$(tail -3 "$SANDBOX/build.log")"
  exit 1
fi

head_ "1. A monitor that does not start at the origin"
OUTPUT="$(xrandr --query | awk '/ connected primary/{print $1; exit}')"
[[ -z "$OUTPUT" ]] && OUTPUT="$(xrandr --query | awk '/ connected/{print $1; exit}')"
if [[ -z "$OUTPUT" ]]; then
  bad "found a connected output"
  exit 1
fi
ok "output: $OUTPUT"

SCREEN_W="$(xdpyinfo | awk '/dimensions:/{print $2}' | cut -dx -f1)"
SCREEN_H="$(xdpyinfo | awk '/dimensions:/{print $2}' | cut -dx -f2)"
HALF=$(( SCREEN_W / 2 ))
# A monitor covering the right half, so its origin is HALF, not zero.
if xrandr --setmonitor "$MON" "${HALF}/172x${SCREEN_H}/215+${HALF}+0" "$OUTPUT" \
     >"$SANDBOX/xrandr.log" 2>&1; then
  ok "defined a monitor at x=${HALF}, width ${HALF}"
else
  bad "defined a monitor at x=${HALF}" "$(cat "$SANDBOX/xrandr.log")"
  exit 1
fi

# Confirm GDK agrees, because the app reads its geometry from GDK, not xrandr.
GDK_VIEW="$(GDK_BACKEND=x11 python3 - <<'PY' 2>/dev/null
import gi
gi.require_version("Gdk", "3.0")
from gi.repository import Gdk
d = Gdk.Display.get_default()
if d.get_n_monitors() != 1:
    print("n=" + str(d.get_n_monitors()))
else:
    g = d.get_monitor(0).get_geometry()
    print(f"{g.x} {g.y} {g.width} {g.height}")
PY
)"
if [[ "$GDK_VIEW" =~ ^[0-9]+\ [0-9]+\ [0-9]+\ [0-9]+$ ]]; then
  read -r MX MY MW MH <<<"$GDK_VIEW"
  if [[ "$MX" -eq "$HALF" ]]; then
    ok "GDK reports one monitor at x=$MX, width $MW"
  else
    bad "GDK reports the monitor at x=$HALF" "GDK says x=$MX"
  fi
else
  bad "GDK sees exactly one monitor" "got: $GDK_VIEW"
fi

head_ "2. Place the overlay"
mkdir -p "$SANDBOX/root/config"
cat > "$SANDBOX/root/config/settings.json" <<'JSON'
{"revision":1,"general":{"palette":"blue","theme":"system","show_overlay":true,
"overlay_position":"bottom_centre","overlay_style":"minimal","autostart":false,
"close_action":"tray"},"audio":{"input_device":null,"gain":1.0,"mute_output":false,
"cues":false},"dictation":{"hold_chord":null,"toggle_chord":null,"cancel_chord":null,
"auto_insert":false,"keep_on_clipboard":true},"dictionary":{"fuzzy_enabled":false,
"phonetic_enabled":false,"fuzzy_threshold":0.18},"history":{"retention":"three_days",
"max_entries":10000},"delivery":{"prefer_paste":true,"restore_clipboard":true,
"paste_delay_ms":60},"model":{"directory":null,"verify_on_start":true,"idle_unload_secs":0}}
JSON

cat > "$SANDBOX/drive.js" <<'JS'
(async function () {
  const out = (m) => window.__TAURI__.core.invoke('probe_write', { text: m });
  await out('STEP starting');
  await window.__TAURI__.core.invoke('start_dictation');
  await out('STEP started');
  await new Promise((r) => setTimeout(r, 25000));
})();
JS

: > "$SANDBOX/out.txt"
GDK_BACKEND=x11 \
SOTTO_ROOT="$SANDBOX/root" \
SOTTO_MODEL_DIR="$ROOT/model" \
SOTTO_STT_BIN="$ROOT/target/release/sotto-stt" \
SOTTO_E2E_SCRIPT="$SANDBOX/drive.js" \
SOTTO_E2E_OUT="$SANDBOX/out.txt" \
SOTTO_E2E_DELAY_MS=5000 \
  setsid nohup "$APP" >"$SANDBOX/app.log" 2>&1 </dev/null &
APP_PID=$!

for _ in $(seq 1 40); do
  sleep 0.5
  grep -q "STEP started" "$SANDBOX/out.txt" 2>/dev/null && break
done
if grep -q "STEP started" "$SANDBOX/out.txt" 2>/dev/null; then
  ok "a take started under the X11 backend"
else
  bad "a take started under the X11 backend" "$(grep -viE 'ALSA|appindicator' "$SANDBOX/app.log" | tail -3)"
fi

# The overlay is a separate window; wait for it to map.
WID=""
for _ in $(seq 1 30); do
  sleep 0.5
  WID="$(xdotool search --name "Sotto overlay" 2>/dev/null | head -1)"
  [[ -n "$WID" ]] && break
done
if [[ -z "$WID" ]]; then
  bad "the overlay window exists in X"
  exit 1
fi
ok "the overlay window exists in X"

eval "$(xdotool getwindowgeometry --shell "$WID" 2>/dev/null)"
echo "        monitor x=${MX} width=${MW} · pill ${WIDTH}x${HEIGHT} at X=${X} Y=${Y}"

head_ "3. Assertions"
EXPECTED_X=$(( MX + (MW - WIDTH) / 2 ))
if [[ "$X" -eq "$EXPECTED_X" ]]; then
  ok "centred on its own monitor (X=$X, expected $EXPECTED_X)"
else
  bad "centred on its own monitor" "X=$X, expected $EXPECTED_X"
fi

# The specific failure this test was written for: a monitor-relative position.
NAIVE_X=$(( (MW - WIDTH) / 2 ))
if [[ "$X" -eq "$NAIVE_X" && "$NAIVE_X" -ne "$EXPECTED_X" ]]; then
  bad "the position is not monitor-relative" "X=$X equals the monitor-relative value"
else
  ok "the position is not monitor-relative (that would be $NAIVE_X)"
fi

if [[ "$X" -ge "$MX" ]]; then
  ok "the pill is inside the monitor it belongs to"
else
  bad "the pill is inside the monitor it belongs to" "X=$X < monitor origin $MX"
fi

# Vertically it must sit above the bottom edge, allowing for a panel.
if [[ "$Y" -gt "$MY" && "$Y" -lt $(( MY + MH )) ]]; then
  ok "the pill is vertically inside the monitor"
else
  bad "the pill is vertically inside the monitor" "Y=$Y, monitor ${MY}..$(( MY + MH ))"
fi

echo
echo "─────────────────────────────────────────────"
echo "  passed: $PASS"
echo "  failed: $FAIL"
echo "─────────────────────────────────────────────"
[[ $FAIL -eq 0 ]]
