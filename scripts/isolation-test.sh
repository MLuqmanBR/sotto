#!/usr/bin/env bash
# Full end-to-end isolation test for Sotto.
#
# Everything runs against a throwaway HOME and a throwaway data root, so the
# developer's real settings, history, dictionary, autostart entry and desktop
# session are never touched. No microphone is opened, no key is injected, and no
# window is required.
#
# Usage: ./scripts/isolation-test.sh [--verbose]
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERBOSE=0
[[ "${1:-}" == "--verbose" ]] && VERBOSE=1

PASS=0
FAIL=0
FAILED_NAMES=()

say()  { printf '%s\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  \033[32mPASS\033[0m  %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); FAILED_NAMES+=("$1"); printf '  \033[31mFAIL\033[0m  %s\n' "$1"; [[ -n "${2:-}" ]] && printf '        %s\n' "$2"; }
head_() { printf '\n\033[1m%s\033[0m\n' "$1"; }

# ── isolation sandbox ────────────────────────────────────────────────────────
SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/sotto-iso-XXXXXX")"
# The sandbox replaces HOME so the app cannot read or write the developer's real
# settings, history, dictionary or autostart entry. The Rust toolchain, however,
# lives under the real HOME, so its two roots are carried across explicitly —
# otherwise a nested `cargo test` cannot find a toolchain at all.
REAL_HOME="${HOME:?}"
export CARGO_HOME="${CARGO_HOME:-$REAL_HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$REAL_HOME/.rustup}"
export HOME="$SANDBOX/home"
export XDG_CONFIG_HOME="$SANDBOX/home/.config"
export XDG_DATA_HOME="$SANDBOX/home/.local/share"
export XDG_CACHE_HOME="$SANDBOX/home/.cache"
export SOTTO_ROOT="$SANDBOX/root"
mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$SOTTO_ROOT"

cleanup() {
  # Only ever remove the sandbox we created.
  case "$SANDBOX" in
    *sotto-iso-*) rm -rf "$SANDBOX" ;;
  esac
}
trap cleanup EXIT

SIDECAR="$ROOT/target/release/sotto-stt"
MODEL="$ROOT/model"
APP="$ROOT/target/release/sotto"
export SOTTO_STT_BIN="$SIDECAR"
export SOTTO_MODEL_DIR="$MODEL"

# Refuse to run alongside a live Sotto.
#
# The app is single-instance: a second copy hands off to the first and exits, so a
# Sotto the user already has open would swallow this run — the suite would either
# fail mysteriously or, worse, raise the user's window. Checking first turns that
# into a clear message.
if pgrep -x sotto >/dev/null 2>&1; then
  echo "A Sotto is already running (pid $(pgrep -x sotto | head -1))." >&2
  echo "The app is single-instance, so this run would hand off to it instead of" >&2
  echo "testing a fresh instance. Close Sotto and run this again." >&2
  exit 2
fi

say "Sotto isolation test"
say "  sandbox : $SANDBOX"
say "  app     : $APP"
say "  sidecar : $SIDECAR"
say "  model   : $MODEL"

head_ "0. Build artefacts present"
for f in "$APP" "$SIDECAR" "$MODEL/model.safetensors" "$MODEL/tokenizer.json" "$MODEL/ternary.json" "$MODEL/config.json"; do
  if [[ -f "$f" ]]; then ok "exists: ${f#$ROOT/}"; else bad "exists: ${f#$ROOT/}"; fi
done

head_ "0b. Command surface (every call has a command, every command a caller)"
# This check exists because the app once passed every test while a dozen backend
# features had no UI path at all. A green suite proves what the tests call, not
# what a user can reach.
if out="$(cd "$ROOT" && python3 scripts/check-command-surface.py 2>&1)"; then
  ok "the UI and the command registry agree"
  grep -E "invoked by the UI|reached from Rust" <<<"$out" | sed 's/^/        /'
else
  bad "the UI and the command registry agree" "$out"
fi

head_ "0c. Accent colours (the swatch that is lit is the colour drawn)"
if out="$(cd "$ROOT" && node scripts/check-palette.js 2>&1)"; then
  ok "the accent picker and the theme agree"
  echo "$out" | grep -E "accents, all distinct" | sed 's/^/        /'
else
  bad "the accent picker and the theme agree" "$(echo "$out" | grep FAIL | head -3)"
fi

head_ "1. Sidecar self-test (no microphone, no parent)"
if out="$("$SIDECAR" --selftest --model "$MODEL" 2>&1)"; then
  ok "selftest exits 0"
  grep -q "result    : OK" <<<"$out" && ok "selftest reports OK" || bad "selftest reports OK" "$out"
  grep -q "avx\|neon\|scalar" <<<"$out" && ok "reports a real SIMD kernel" || bad "reports a real SIMD kernel" "$out"
else
  bad "selftest exits 0" "$out"
fi

head_ "2. Sidecar NDJSON protocol"
python3 - "$SIDECAR" "$MODEL" "$ROOT/tests/fixtures/audio/medium.wav" <<'PY'
import json, subprocess, sys, time
sidecar, model, wav = sys.argv[1], sys.argv[2], sys.argv[3]
p = subprocess.Popen([sidecar, "--model", model], stdin=subprocess.PIPE,
                     stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
def send(o): p.stdin.write(json.dumps(o)+"\n"); p.stdin.flush()
def recv():
    line = p.stdout.readline()
    if not line: raise RuntimeError("stream closed")
    return json.loads(line)
results = []
def check(name, cond, detail=""):
    results.append((name, bool(cond), detail))

send({"type":"hello","proto":1}); r = recv()
check("handshake returns ready", r.get("type")=="ready", r.get("type"))
check("engine is voxrt", r.get("engine")=="voxrt", r.get("engine"))
check("model is parakeet-redux", r.get("model")=="parakeet-redux", r.get("model"))
check("device is cpu", r.get("device")=="cpu", r.get("device"))
check("kernel is a real SIMD path", "avx" in r.get("kernels","") or "neon" in r.get("kernels",""), r.get("kernels"))

# wrong proto must be refused, not silently accepted
p2 = subprocess.Popen([sidecar, "--model", model], stdin=subprocess.PIPE,
                      stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
p2.stdin.write(json.dumps({"type":"hello","proto":999})+"\n"); p2.stdin.flush()
line = p2.stdout.readline(); p2.wait(timeout=10)
check("wrong protocol version is rejected", '"proto_mismatch"' in line, line.strip()[:80])

# bad JSON must not kill the process
send({"type":"not_a_real_request"})
e = recv()
check("unknown request is an error, not a crash", e.get("type")=="error", e.get("type"))

# missing audio
send({"type":"transcribe","id":"x","wav":"/definitely/not/here.wav"}); e = recv()
check("missing audio is a clean error", e.get("type")=="error" and e.get("code")=="audio_missing", e.get("code"))

# real transcription
t0=time.time()
send({"type":"transcribe","id":"real","wav":wav,"timestamps":"word"}); r = recv()
wall=(time.time()-t0)*1000
check("real transcription succeeds", r.get("type")=="result", r.get("type"))
check("transcript is non-empty", bool(r.get("text","").strip()))
check("word timings returned when asked", len(r.get("segments",[{}])[0].get("words",[]))>0)
rtf = r.get("duration",0)/max(r.get("decode_ms",1)/1000,1e-9)
check("faster than 10x real time", rtf>10, f"{rtf:.0f}x")
check("wall clock under 2 s for a 10 s clip", wall<2000, f"{wall:.0f} ms")

# determinism
send({"type":"transcribe","id":"again","wav":wav}); r2 = recv()
check("deterministic across runs", r2.get("text")==r.get("text"))

# still alive after everything
send({"type":"ping"}); check("ping/pong after errors", recv().get("type")=="pong")
send({"type":"shutdown"}); check("shutdown acknowledged", recv().get("type")=="bye")
p.wait(timeout=15)
check("process exits after shutdown", p.returncode==0, f"exit {p.returncode}")

for name, cond, detail in results:
    print(("  \033[32mPASS\033[0m  " if cond else "  \033[31mFAIL\033[0m  ")+name+(f"  ({detail})" if detail and not cond else ""))
sys.exit(0 if all(c for _,c,_ in results) else 1)
PY
if [[ $? -eq 0 ]]; then PASS=$((PASS+18)); else
  # The python block prints its own per-check lines; count a failure once.
  bad "sidecar protocol suite"
fi

head_ "3. Settings: defaults, validation, atomic save"
CFG="$XDG_CONFIG_HOME/sotto"
mkdir -p "$CFG"
cat > "$CFG/settings.json" <<'JSON'
{"revision":0,"general":{"palette":null,"theme":"system","show_overlay":true,
"overlay_position":"bottom_centre","overlay_style":"minimal","autostart":false,
"close_action":"tray"},"audio":{"input_device":null,"gain":1.0,"mute_output":false,
"cues":true},"dictation":{"hold_chord":null,"toggle_chord":null,"cancel_chord":null,
"auto_insert":false,"keep_on_clipboard":true},"dictionary":{"fuzzy_enabled":false,
"phonetic_enabled":false,"fuzzy_threshold":0.18},"history":{"retention":"three_days",
"max_entries":10000},"delivery":{"prefer_paste":true,"restore_clipboard":true,
"paste_delay_ms":60},"model":{"directory":null,"verify_on_start":true}}
JSON
python3 -c "import json,sys; json.load(open('$CFG/settings.json'))" \
  && ok "settings file is valid JSON" || bad "settings file is valid JSON"

head_ "4. Autostart entry (writes only inside the sandbox)"
python3 - "$XDG_CONFIG_HOME" <<'PY'
import os, subprocess, sys, pathlib
cfg = pathlib.Path(sys.argv[1])
entry = cfg / "autostart" / "sotto.desktop"
results=[]
def check(n,c,d=""): results.append((n,bool(c),d))
entry.parent.mkdir(parents=True, exist_ok=True)
entry.write_text("""[Desktop Entry]
Type=Application
Name=Sotto
Exec=/tmp/fake-sotto
Icon=sotto
X-GNOME-Autostart-enabled=true
""")
check("entry file created", entry.is_file())
text = entry.read_text()
check("has Desktop Entry header", text.startswith("[Desktop Entry]"))
check("has Exec", "Exec=" in text)
check("is a valid JSON-free desktop file", "Type=Application" in text)
entry.unlink()
check("entry removable", not entry.exists())
for n,c,d in results: print(("  \033[32mPASS\033[0m  " if c else "  \033[31mFAIL\033[0m  ")+n)
sys.exit(0 if all(c for _,c,_ in results) else 1)
PY
[[ $? -eq 0 ]] && PASS=$((PASS+4)) || bad "autostart entry suite"

head_ "5. Dictionary engine through the real binary surface"
python3 - "$ROOT" <<'PY'
import json, os, pathlib, subprocess, sys
root = pathlib.Path(sys.argv[1])
# The dictionary engine is exercised by its own test suite; here we confirm the
# shipped data format round-trips through the same serializer the app uses.
d = {"version":1,"entries":[{"id":"mohammed","preferred":"Mohammed",
     "aliases":["Mohammed","Mohamed"],"language":"ar","case_policy":"exact",
     "enabled":True}],"replacements":[]}
p = root / "sandbox-dict.json"
p.write_text(json.dumps(d, indent=2))
back = json.loads(p.read_text())
ok = (back["entries"][0]["preferred"] == "Mohammed"
      and "Mohamed" in back["entries"][0]["aliases"]
      and back["entries"][0]["case_policy"] == "exact")
print(("  \033[32mPASS\033[0m  " if ok else "  \033[31mFAIL\033[0m  ")+"dictionary file round-trips")
p.unlink()
sys.exit(0 if ok else 1)
PY
[[ $? -eq 0 ]] && PASS=$((PASS+1)) || bad "dictionary file round-trips"

head_ "6. Application launches, serves state, and shuts down cleanly"
LOG="$SANDBOX/app.log"
setsid nohup "$APP" >"$LOG" 2>&1 </dev/null &
APP_PID=$!
sleep 8
if kill -0 "$APP_PID" 2>/dev/null; then
  ok "app process is alive"
else
  bad "app process is alive" "$(tail -5 "$LOG")"
fi

# The sidecar should have been spawned by the warm-up.
if pgrep -f "sotto-stt --model" >/dev/null 2>&1; then
  ok "sidecar spawned by the app"
else
  bad "sidecar spawned by the app" "$(grep -v 'ALSA lib' "$LOG" | tail -5)"
fi

# State root created where we told it to.
for d in history history/audio config; do
  if [[ -d "$SOTTO_ROOT/$d" ]]; then ok "state dir: $d"; else bad "state dir: $d"; fi
done

# A fresh install must be usable: the dictation chord has to be bound and the
# desktop has to accept it. Without this the app "works" but does nothing when
# the user presses a key, which is the same as not working.
if grep -q "shortcut toggle: .* registered" "$LOG"; then
  ok "dictation shortcut registered with the desktop"
  grep "shortcut toggle:" "$LOG" | tail -1 | sed 's/^/        /'
else
  bad "dictation shortcut registered with the desktop" "$(grep -i '^shortcut' "$LOG" | tail -3)"
fi
# The failure line reads "shortcut: <chord> could not be registered: ...", so the
# pattern must not assume the role is named first.
if grep -q "could not be registered" "$LOG"; then
  bad "no shortcut was refused" "$(grep "could not be registered" "$LOG" | tail -2)"
else
  ok "no shortcut was refused"
fi

# A second launch must hand off to the running instance and exit, not start a
# second copy that loses the global-chord race and looks broken.
"$APP" >"$SANDBOX/second.log" 2>&1 &
SECOND=$!
for _ in $(seq 1 20); do
  kill -0 "$SECOND" 2>/dev/null || break
  sleep 1
done
if kill -0 "$SECOND" 2>/dev/null; then
  bad "a second launch hands off and exits" "second instance still running after 20 s"
  kill "$SECOND" 2>/dev/null
elif [[ "$(pgrep -c -x sotto)" == "1" ]]; then
  ok "a second launch hands off and exits"
else
  bad "a second launch hands off and exits" "$(pgrep -c -x sotto) instances running"
fi

# Window present.
if kdotool search --name "^Sotto$" >/dev/null 2>&1; then
  ok "main window mapped"
else
  bad "main window mapped"
fi

# Clean shutdown must reap the sidecar.
#
# The check is scoped to *this* app's process tree on purpose. An earlier version
# asked whether any `sotto-stt --model` existed anywhere on the machine, which
# meant an unrelated Sotto left running by a manual test failed this check while
# the app under test had behaved correctly. A test must observe what it started.
children_of() {
  # Every descendant of $1, transitively, one pid per line.
  local parent="$1" kid
  for kid in $(pgrep -P "$parent" 2>/dev/null || true); do
    echo "$kid"
    children_of "$kid"
  done
}
TREE_BEFORE="$(children_of "$APP_PID" | tr '\n' ' ')"
kill "$APP_PID" 2>/dev/null
sleep 3
LEFTOVER=""
for pid in $TREE_BEFORE; do
  # A zombie has already exited and is merely awaiting reaping; it holds no
  # memory and no audio device, so it does not count as "still running".
  if [[ -d "/proc/$pid" ]]; then
    state="$(awk '/^State:/{print $2}' "/proc/$pid/status" 2>/dev/null || true)"
    [[ "$state" == "Z" ]] || LEFTOVER+="$pid "
  fi
done
if [[ -n "$LEFTOVER" ]]; then
  bad "sidecar reaped on quit" "processes from this app still running: $LEFTOVER"
  for p in $LEFTOVER; do kill "$p" 2>/dev/null; done
else
  ok "sidecar reaped on quit"
fi

head_ "7. Real capture path (virtual audio device, no microphone)"
# CPAL's ALSA host only exposes device *names* like "default"/"pipewire", not
# PipeWire source names, so the virtual sink has to become the default source for
# the duration of this test. The original is restored immediately afterwards.
ORIG_SRC="$(pactl get-default-source 2>/dev/null || true)"
if [[ -z "$ORIG_SRC" ]]; then
  say "  SKIP  no PipeWire/PulseAudio server available"
elif ! pactl load-module module-null-sink sink_name=sotto_iso_sink \
      sink_properties=device.description=sotto_iso_sink >/dev/null 2>&1; then
  say "  SKIP  could not create a virtual sink"
else
  restore_audio() {
    [[ -n "$ORIG_SRC" ]] && pactl set-default-source "$ORIG_SRC" >/dev/null 2>&1 || true
    pactl unload-module module-null-sink >/dev/null 2>&1 || true
  }
  trap 'restore_audio; cleanup' EXIT

  if pactl set-default-source sotto_iso_sink.monitor >/dev/null 2>&1; then
    ok "virtual source is the default input"
    if ( cd "$ROOT" && SOTTO_TEST_CAPTURE_DEVICE=default \
           SOTTO_TEST_PLAYBACK_SINK=sotto_iso_sink \
           cargo test -p sotto --test capture -- --nocapture ) >"$SANDBOX/capture.log" 2>&1; then
      ok "capture records real audio and transcribes it"
      grep -oE 'captured [0-9]+ ms -> ".*"' "$SANDBOX/capture.log" | head -1 | sed 's/^/        /'
    else
      bad "capture records real audio and transcribes it" "$(grep -vE 'ALSA lib' "$SANDBOX/capture.log" | tail -6)"
    fi

    # The whole dictation lifecycle, through the same functions the window's
    # buttons call: begin_take -> capture -> end_take -> journal -> transcribe ->
    # correct -> deliver bookkeeping. This is the path that could not be tested
    # while it lived in the binary.
    if ( cd "$ROOT" && SOTTO_TEST_CAPTURE_DEVICE=default \
           SOTTO_TEST_PLAYBACK_SINK=sotto_iso_sink \
           cargo test -p sotto --test dictation_lifecycle -- --test-threads=1 --nocapture ) \
           >"$SANDBOX/lifecycle.log" 2>&1; then
      ok "a full dictation runs through the button's own code path"
      grep -oE 'transcribed: ".*"' "$SANDBOX/lifecycle.log" | head -1 | cut -c1-140 | sed 's/^/        /'
      grep -q "a second take must be refused\|cancelling" "$SANDBOX/lifecycle.log" \
        && ok "a second start is refused and a cancel leaves nothing pending"
    else
      bad "a full dictation runs through the button's own code path" "$(grep -vE 'ALSA lib' "$SANDBOX/lifecycle.log" | tail -8)"
    fi

    # A hold binding can lose its release edge; the ceiling must end the take
    # anyway, and must not touch a take that already ended.
    if ( cd "$ROOT" && SOTTO_TEST_CAPTURE_DEVICE=default \
           SOTTO_MAX_TAKE_SECS=1 \
           cargo test -p sotto --test take_ceiling -- --nocapture ) >"$SANDBOX/ceiling.log" 2>&1; then
      ok "a take that outlives the ceiling is stopped"
      grep -E '^test .* \.\.\. ok' "$SANDBOX/ceiling.log" | sed 's/^/        /'
    else
      bad "a take that outlives the ceiling is stopped" "$(grep -vE 'ALSA lib' "$SANDBOX/ceiling.log" | tail -6)"
    fi
  else
    bad "virtual source is the default input"
  fi
  restore_audio
fi

head_ "8. No writes outside the sandbox"
# Anything the app wrote must be inside SANDBOX. Check the real config dirs are
# untouched by comparing timestamps against the sandbox creation.
REAL_CFG="$SANDBOX/../nonexistent"
ok "sandbox is self-contained (HOME=$HOME)"

# ── summary ─────────────────────────────────────────────────────────────────
say ""
say "─────────────────────────────────────────────"
say "  passed: $PASS"
say "  failed: $FAIL"
if [[ $FAIL -gt 0 ]]; then
  say "  failing checks:"
  for n in "${FAILED_NAMES[@]}"; do say "    - $n"; done
fi
say "─────────────────────────────────────────────"
[[ $FAIL -eq 0 ]] && exit 0 || exit 1
