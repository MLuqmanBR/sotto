#!/usr/bin/env bash
# Install the real package, prove it works, remove it, and prove nothing is left.
#
# This is S8's exit criterion, and the last thing in the project that could only
# be checked by argument rather than observation. It needs root, so it is opt-in
# and refuses to touch anything that is already installed.
#
# What it found the first time it ran: uninstall removed every file but left
# `/usr/lib/Sotto` and `/usr/lib/Sotto/model` behind, because the spec listed files
# and never claimed the directories. An empty directory is still residue.
#
# Safety:
#   - refuses to run if `sotto` is already installed (never removes someone's copy)
#   - runs the app with a sandbox SOTTO_ROOT, so nothing is written to ~/.local
#   - uninstalls in a trap, so a failure part-way through does not leave it installed
#
# Usage:  scripts/install-test.sh
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

RPM="$(python3 -c 'import json;v=json.load(open("src-tauri/tauri.conf.json"))["version"];print(f"target/release/bundle/rpm/sotto-{v}-1.x86_64.rpm")')"
SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/sotto-install-XXXXXX")"
PASS=0
FAIL=0
INSTALLED=0

ok()   { echo "  $(printf '\033[32mPASS\033[0m')  $1"; PASS=$((PASS+1)); }
bad()  { echo "  $(printf '\033[31mFAIL\033[0m')  $1${2:+  ($2)}"; FAIL=$((FAIL+1)); }
head_(){ echo; echo "$(printf '\033[1m')$1$(printf '\033[0m')"; }

# A fingerprint of the real home's Sotto directories: paths, sizes and mtimes.
#
# The check this feeds used to be "the default directories must not exist", which
# only worked on a machine where nobody had ever used the app. The first time the
# user ran Sotto for real, their own database made the suite report a leak. The
# property that actually matters is that a sandboxed run changes nothing — so
# compare before and after, whether the directories exist or not.
home_state() {
  find "$HOME/.local/share/sotto" "$HOME/.config/sotto" \
    -printf '%p\t%s\t%T@\n' 2>/dev/null | sort
}

cleanup() {
  pgrep -x sotto >/dev/null 2>&1 && pkill -x sotto 2>/dev/null
  sleep 1
  if [[ "$INSTALLED" == "1" ]]; then
    sudo -n rpm -e sotto >/dev/null 2>&1 || true
  fi
  rm -rf "$SANDBOX"
}
trap cleanup EXIT

if ! sudo -n true 2>/dev/null; then
  echo "  SKIP  passwordless sudo is not available; installing needs root" >&2
  exit 0
fi
if rpm -q sotto >/dev/null 2>&1; then
  # Refusing to remove a package that is already installed is correct: this test
  # must never uninstall someone's working copy. But reporting that refusal as a
  # pass made the gate green while the install path went untested — a check that
  # cannot fail. So the refusal is an explicit failure, with the way out named.
  echo "  FAIL  a package named 'sotto' is installed, so install/uninstall is untested" >&2
  echo "        Remove it first:  sudo dnf remove sotto" >&2
  echo "        Then re-run. This test installs and removes its own copy." >&2
  exit 3
fi
[[ -f "$RPM" ]] || { echo "no package at $RPM — run scripts/package.sh first" >&2; exit 1; }

head_ "0. Nothing installed beforehand"
if rpm -q sotto >/dev/null 2>&1; then
  bad "not installed before the test"
else
  ok "not installed before the test"
fi
for f in /usr/bin/sotto /usr/lib/Sotto /usr/share/applications/sotto.desktop; do
  [[ -e "$f" ]] && bad "absent before: $f" || ok "absent before: $f"
done

head_ "1. Install"
if sudo -n rpm -i "$RPM" >"$SANDBOX/install.log" 2>&1; then
  INSTALLED=1
  ok "rpm -i succeeded"
else
  bad "rpm -i succeeded" "$(tail -5 "$SANDBOX/install.log")"
  exit 1
fi
[[ "$(rpm -q sotto 2>/dev/null)" == sotto-* ]] && ok "the package database knows about it" \
  || bad "the package database knows about it"

head_ "2. The installed layout is complete"
for f in \
  /usr/bin/sotto \
  /usr/lib/Sotto/sotto-stt \
  /usr/lib/Sotto/model/model.safetensors \
  /usr/lib/Sotto/LICENSE \
  /usr/share/applications/sotto.desktop \
  /usr/share/icons/hicolor/128x128/apps/sotto.png; do
  [[ -e "$f" ]] && ok "installed: $f" || bad "installed: $f"
done

if desktop-file-validate /usr/share/applications/sotto.desktop 2>"$SANDBOX/dfv.log"; then
  ok "the installed desktop entry validates"
else
  bad "the installed desktop entry validates" "$(head -1 "$SANDBOX/dfv.log")"
fi

head_ "3. The installed app runs and finds its own files"
# No SOTTO_MODEL_DIR and no SOTTO_STT_BIN: the packaged app must resolve both.
mkdir -p "$SANDBOX/root"
home_state > "$SANDBOX/home-before.txt"
SOTTO_ROOT="$SANDBOX/root" setsid nohup /usr/bin/sotto >"$SANDBOX/app.log" 2>&1 </dev/null &
sleep 14
pgrep -x sotto >/dev/null 2>&1 && ok "the installed app is running" \
  || bad "the installed app is running" "$(grep -viE 'ALSA|appindicator' "$SANDBOX/app.log" | tail -3)"

ENGINE="$(ps -o args= -p "$(pgrep -x sotto-stt | head -1)" 2>/dev/null)"
if grep -q "/usr/lib/Sotto/model" <<<"$ENGINE"; then
  ok "it found its bundled model"
  echo "        ${ENGINE:0:110}"
else
  bad "it found its bundled model" "${ENGINE:-no engine process}"
fi

if grep -q "shortcut toggle: .* registered" "$SANDBOX/app.log"; then
  ok "it registered its shortcut"
else
  bad "it registered its shortcut" "$(grep -i '^shortcut' "$SANDBOX/app.log" | tail -2)"
fi

if /usr/lib/Sotto/sotto-stt --selftest --model /usr/lib/Sotto/model 2>&1 | grep -q "result    : OK"; then
  ok "the installed model passes its selftest"
else
  bad "the installed model passes its selftest"
fi

pkill -x sotto 2>/dev/null
sleep 1

head_ "4. The real home was not touched"
# The app ran with SOTTO_ROOT, so it must not have written to the default paths.
# Existing data is the user's, not residue: unchanged is the pass condition.
home_state > "$SANDBOX/home-after.txt"
if diff -q "$SANDBOX/home-before.txt" "$SANDBOX/home-after.txt" >/dev/null; then
  ok "the real home is unchanged (existing user data preserved)"
else
  bad "the real home is unchanged" "$(diff "$SANDBOX/home-before.txt" "$SANDBOX/home-after.txt" | head -4 | tr '\n' ' ')"
fi

head_ "5. Uninstall leaves nothing behind"
if sudo -n rpm -e sotto >"$SANDBOX/erase.log" 2>&1; then
  INSTALLED=0
  ok "rpm -e succeeded"
else
  bad "rpm -e succeeded" "$(tail -5 "$SANDBOX/erase.log")"
fi
rpm -q sotto >/dev/null 2>&1 && bad "the package database no longer lists it" \
  || ok "the package database no longer lists it"

# The failure this test was written for: empty directories left behind.
for f in \
  /usr/bin/sotto \
  /usr/lib/Sotto \
  /usr/lib/Sotto/model \
  /usr/share/applications/sotto.desktop \
  /usr/share/icons/hicolor/128x128/apps/sotto.png; do
  [[ -e "$f" ]] && bad "removed: $f" || ok "removed: $f"
done

LEFT="$(find /usr -name '*sotto*' 2>/dev/null | head -5)"
[[ -z "$LEFT" ]] && ok "nothing named sotto remains anywhere in /usr" \
  || bad "nothing named sotto remains anywhere in /usr" "$LEFT"

echo
echo "─────────────────────────────────────────────"
echo "  passed: $PASS"
echo "  failed: $FAIL"
echo "─────────────────────────────────────────────"
[[ $FAIL -eq 0 ]]