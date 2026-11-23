#!/usr/bin/env bash
# Drive the real tray icon and prove what each entry does.
#
# The tray is the one part of the app a page-level test cannot reach: its menu is
# a D-Bus object owned by the desktop, not a DOM node. This script talks to the
# StatusNotifierItem the app registers, invokes the menu entries the way a click
# does, and measures the result.
#
# What it proved when written:
#   - the icon registers with StatusNotifierWatcher;
#   - the menu names Open Sotto / Start dictation / Quit Sotto;
#   - closing the window hides it (the process stays alive);
#   - Open Sotto brings it back in tens of milliseconds, not seconds;
#   - Start dictation changes the menu verb to Stop, and shows the overlay;
#   - Stop returns the verb to Start and records the take;
#   - quitting mid-recording leaves NO phantom row in History;
#   - Quit removes the process and its engine.
#
# It also encodes the Linux reality that made the first tray version look dead:
# an AppIndicator has no primary-click action, so the menu must open on left
# click. There is no D-Bus method for "left click"; `show_menu_on_left_click(true)`
# is asserted in source, and the menu path below is what that click reaches.
#
# Usage: scripts/tray-test.sh
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export TMPDIR="${TMPDIR:-$HOME/.cache/build-tmp}"

SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/sotto-tray-XXXXXX")"
SINK="sotto_tray_sink"
APP="target/release/sotto"
PASS=0
FAIL=0
APP_PID=""
ORIG_SRC=""

ok()   { echo "  $(printf '\033[32mPASS\033[0m')  $1"; PASS=$((PASS+1)); }
bad()  { echo "  $(printf '\033[31mFAIL\033[0m')  $1${2:+  ($2)}"; FAIL=$((FAIL+1)); }
head_(){ echo; echo "$(printf '\033[1m')$1$(printf '\033[0m')"; }

cleanup() {
  [[ -n "$APP_PID" ]] && kill "$APP_PID" 2>/dev/null
  sleep 0.5
  pgrep -x sotto >/dev/null 2>&1 && pkill -x sotto 2>/dev/null
  pgrep -x sotto-stt >/dev/null 2>&1 && pkill -x sotto-stt 2>/dev/null
  [[ -n "$ORIG_SRC" ]] && pactl set-default-source "$ORIG_SRC" >/dev/null 2>&1
  pactl unload-module module-null-sink >/dev/null 2>&1
  rm -rf "$SANDBOX"
}
trap cleanup EXIT

if pgrep -x sotto >/dev/null 2>&1; then
  echo "A Sotto is already running. Close it first: the app is single-instance." >&2
  exit 2
fi

head_ "0. Build the release binary"
if grep -q 'show_menu_on_left_click(true)' src-tauri/src/main.rs; then
  ok "left click is configured to open the menu"
else
  bad "left click is configured to open the menu" "an AppIndicator has no primary-click action"
fi
if ./scripts/build.sh 2>"$SANDBOX/build.log"; then
  ok "built"
else
  bad "built" "$(tail -3 "$SANDBOX/build.log")"
  exit 1
fi

head_ "1. Sandbox and silent audio"
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
ORIG_SRC="$(pactl get-default-source 2>/dev/null || true)"
if [[ -z "$ORIG_SRC" ]]; then
  bad "an audio server is available"
  exit 1
fi
if ! pactl load-module module-null-sink sink_name="$SINK" >/dev/null 2>&1; then
  bad "a virtual sink exists"
  exit 1
fi
pactl set-default-source "$SINK.monitor" >/dev/null 2>&1 \
  && ok "the default input is silent" \
  || bad "the default input is silent"

head_ "2. Launch and find the tray"
SOTTO_ROOT="$SANDBOX/root" setsid nohup "$APP" >"$SANDBOX/app.log" 2>&1 </dev/null &
APP_PID=$!
sleep 2

# The watcher lists status items as "<bus-name>/<object-path>".
sni() {
  gdbus call --session --dest org.kde.StatusNotifierWatcher \
    --object-path /StatusNotifierWatcher \
    --method org.freedesktop.DBus.Properties.Get \
    org.kde.StatusNotifierWatcher RegisteredStatusNotifierItems 2>/dev/null \
  | grep -o ':[0-9.]*/org/ayatana/NotificationItem/[^"'"'"']*sotto[^"'"'"']*' | head -1
}
SNI=""
for _ in $(seq 1 50); do
  SNI="$(sni)"
  [[ -n "$SNI" ]] && break
  sleep 0.2
done
if [[ -n "$SNI" ]]; then
  ok "the icon registers with StatusNotifierWatcher"
else
  bad "the icon registers with StatusNotifierWatcher"
  exit 1
fi
BUS="${SNI%%/*}"
ITEM="/${SNI#*/}"
MENU="$(gdbus call --session --dest "$BUS" --object-path "$ITEM" \
  --method org.freedesktop.DBus.Properties.Get \
  org.kde.StatusNotifierItem Menu 2>/dev/null \
  | grep -o '/[^"'"'"' ]*Menu' | head -1)"
[[ -n "$MENU" ]] && ok "the icon exposes a menu object" \
  || { bad "the icon exposes a menu object"; exit 1; }

# Print "<id>\t<label>" for every labelled menu entry, fresh each call: the menu
# is rebuilt when dictation state changes, and ids are not stable across that.
items() {
  gdbus call --session --dest "$BUS" --object-path "$MENU" \
    --method com.canonical.dbusmenu.GetLayout -- 0 -1 '[]' 2>/dev/null \
  | python3 -c '
import re, sys
s = sys.stdin.read()
for i, props in re.findall(r"\((\d+), \{([^}]*)\}", s):
    m = re.search(r"'"'"'label'"'"': <'"'"'([^'"'"']*)'"'"'>", props)
    if m: print(i + "\t" + m.group(1))
'
}
item_id() { items | awk -F'\t' -v want="$1" '$2 == want {print $1; exit}'; }
click() {
  local id="$1"
  gdbus call --session --dest "$BUS" --object-path "$MENU" \
    --method com.canonical.dbusmenu.Event "$id" clicked '<int32 0>' 0 >/dev/null 2>&1
}
wait_label() {
  local want="$1" limit="$2" start end
  start=$(date +%s%N)
  while :; do
    [[ -n "$(item_id "$want")" ]] && { end=$(date +%s%N); echo $(( (end-start)/1000000 )); return 0; }
    end=$(date +%s%N)
    (( (end-start)/1000000 > limit )) && return 1
    sleep 0.05
  done
}
window_id() { kdotool search --name "^Sotto$" 2>/dev/null | head -1; }

head_ "3. The menu says what the entries do"
items > "$SANDBOX/menu.txt"
for label in "Open Sotto" "Start dictation" "Quit Sotto"; do
  grep -qF "$label" "$SANDBOX/menu.txt" \
    && ok "menu has \"$label\"" \
    || bad "menu has \"$label\"" "$(tr '\n' ' ' < "$SANDBOX/menu.txt")"
done

head_ "4. Closing hides; Open brings it back"
WID="$(window_id)"
[[ -n "$WID" ]] && ok "the window is open" || bad "the window is open"
if [[ -n "$WID" ]]; then
  kdotool windowclose "$WID" >/dev/null 2>&1
  sleep 0.5
  [[ -z "$(window_id)" ]] && ok "closing hides the window (app stays in the tray)" \
    || bad "closing hides the window (app stays in the tray)"
  pgrep -x sotto >/dev/null && ok "the process is still running after close" \
    || bad "the process is still running after close"
  START=$(date +%s%N)
  click "$(item_id 'Open Sotto')"
  OPENED=""
  for _ in $(seq 1 100); do
    if [[ -n "$(window_id)" ]]; then OPENED=$(( ($(date +%s%N)-START)/1000000 )); break; fi
    sleep 0.02
  done
  if [[ -n "$OPENED" ]]; then
    ok "Open Sotto shows the window (${OPENED} ms)"
    [[ "$OPENED" -lt 1000 ]] && ok "it feels immediate (under 1 s)" \
      || bad "it feels immediate (under 1 s)" "${OPENED} ms"
  else
    bad "Open Sotto shows the window" "not visible after 2 s"
  fi
fi

head_ "5. Start and Stop from the tray"
DICT="$(item_id 'Start dictation')"
[[ -n "$DICT" ]] && ok "the Start entry is present and enabled" \
  || bad "the Start entry is present and enabled"
click "$DICT"
MS="$(wait_label 'Stop and transcribe' 3000)"
[[ -n "$MS" ]] && ok "the menu verb changes to Stop (${MS} ms)" \
  || bad "the menu verb changes to Stop" "still: $(items | tr '\n' ' ')"
kdotool search --name "Sotto overlay" >/dev/null 2>&1 \
  && ok "the recording overlay appeared" \
  || bad "the recording overlay appeared"

sleep 2
STOP="$(item_id 'Stop and transcribe')"
click "$STOP"
MS="$(wait_label 'Start dictation' 30000)"
[[ -n "$MS" ]] && ok "Stop returns the menu verb to Start (${MS} ms)" \
  || bad "Stop returns the menu verb to Start" "still: $(items | tr '\n' ' ')"
TAKES="$(python3 - "$SANDBOX/root/history/sotto.db" <<'PY'
import sqlite3, sys
try:
    c = sqlite3.connect(sys.argv[1])
    print(c.execute("select count(*) from takes").fetchone()[0])
except Exception:
    print(0)
PY
)"
[[ "${TAKES:-0}" -ge 1 ]] && ok "the tray take reached History" \
  || bad "the tray take reached History"

head_ "6. Quit mid-recording leaves no phantom row"
kill "$APP_PID" 2>/dev/null
for _ in $(seq 1 100); do pgrep -x sotto >/dev/null || break; sleep 0.05; done
pgrep -x sotto >/dev/null && { pkill -x sotto; sleep 0.5; }
sleep 0.4
# Relaunch for a clean, single-purpose quit test.
SOTTO_ROOT="$SANDBOX/root" setsid nohup "$APP" >"$SANDBOX/app.log" 2>&1 </dev/null &
APP_PID=$!
SNI=""
for _ in $(seq 1 50); do SNI="$(sni)"; [[ -n "$SNI" ]] && break; sleep 0.2; done
BUS="${SNI%%/*}"; ITEM="/${SNI#*/}"
MENU="$(gdbus call --session --dest "$BUS" --object-path "$ITEM" \
  --method org.freedesktop.DBus.Properties.Get \
  org.kde.StatusNotifierItem Menu 2>/dev/null \
  | grep -o '/[^"'"'"' ]*Menu' | head -1)"
BEFORE="$(python3 - "$SANDBOX/root/history/sotto.db" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
print(c.execute("select count(*) from takes").fetchone()[0])
PY
)"
click "$(item_id 'Start dictation')"
sleep 0.7
START=$(date +%s%N)
click "$(item_id 'Quit Sotto')"
for _ in $(seq 1 200); do pgrep -x sotto >/dev/null || break; sleep 0.02; done
QUIT_MS=$(( ($(date +%s%N)-START)/1000000 ))
if ! pgrep -x sotto >/dev/null; then
  ok "Quit ends the process (${QUIT_MS} ms)"
  [[ "$QUIT_MS" -lt 3000 ]] && ok "Quit is prompt (under 3 s)" \
    || bad "Quit is prompt (under 3 s)" "${QUIT_MS} ms"
else
  bad "Quit ends the process" "still running after 4 s"
fi
sleep 0.3
pgrep -x sotto-stt >/dev/null \
  && bad "Quit takes the speech engine with it" \
  || ok "Quit takes the speech engine with it"
python3 - "$SANDBOX/root/history/sotto.db" "$BEFORE" <<'PY' > "$SANDBOX/db.txt"
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
total = c.execute("select count(*) from takes").fetchone()[0]
recording = c.execute("select count(*) from takes where status='recording'").fetchone()[0]
print(f"total={total} recording={recording} before={sys.argv[2]}")
PY
cat "$SANDBOX/db.txt" | sed 's/^/        /'
grep -q "recording=0" "$SANDBOX/db.txt" \
  && ok "no take is left in the recording state" \
  || bad "no take is left in the recording state" "$(cat "$SANDBOX/db.txt")"
TOTAL="$(grep -o 'total=[0-9]*' "$SANDBOX/db.txt" | cut -d= -f2)"
[[ "$TOTAL" == "$BEFORE" ]] \
  && ok "the quit did not add a phantom History row" \
  || bad "the quit did not add a phantom History row" "before=$BEFORE after=$TOTAL"

echo
echo "─────────────────────────────────────────────"
echo "  passed: $PASS"
echo "  failed: $FAIL"
echo "─────────────────────────────────────────────"
[[ "$FAIL" -eq 0 ]] || exit 1