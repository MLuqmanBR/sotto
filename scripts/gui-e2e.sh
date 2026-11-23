#!/usr/bin/env bash
# Drive the real window: press Start, speak, press Stop, check the words appear.
#
# This closes the one gap the other suites could not: everything else tests the
# pipeline *behind* the buttons (`tests/dictation_lifecycle.rs` drives
# `begin_take`/`end_take` directly). That proves the engine, the journal, the
# dictionary and delivery, but not that `#btn-dictate` is wired to any of it.
#
# Under Wayland on this machine synthetic mouse clicks do not register, so the
# button cannot be pressed from outside. The app is therefore built with the
# non-default `e2e` feature, which adds a script hook and a reporting command, and
# the script clicks the button the way the event listener expects. The shipped
# binary never contains that surface: `cargo build` without the feature omits it.
#
# Usage:  scripts/gui-e2e.sh
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

SINK="sotto_e2e_sink"
SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/sotto-e2e-XXXXXX")"
OUT="$SANDBOX/probe.txt"
SCRIPT="$SANDBOX/drive.js"
APP="$ROOT/target/release/sotto"
CLIP="$ROOT/tests/fixtures/audio/medium.wav"
RECORD_MS=13000

PASS=0
FAIL=0
ok()   { echo "  $(printf '\033[32mPASS\033[0m')  $1"; PASS=$((PASS+1)); }
bad()  { echo "  $(printf '\033[31mFAIL\033[0m')  $1${2:+  ($2)}"; FAIL=$((FAIL+1)); }
head_(){ echo; echo "$(printf '\033[1m')$1$(printf '\033[0m')"; }

cleanup() {
  [[ -n "${APP_PID:-}" ]] && kill "$APP_PID" 2>/dev/null
  sleep 1
  for pid in $(pgrep -f "sotto-stt --model $SANDBOX" 2>/dev/null); do kill "$pid" 2>/dev/null; done
  pactl unload-module module-null-sink 2>/dev/null
  [[ -n "${ORIG_SRC:-}" ]] && pactl set-default-source "$ORIG_SRC" 2>/dev/null
  rm -rf "$SANDBOX"
}
trap cleanup EXIT

if pgrep -x sotto >/dev/null 2>&1; then
  echo "A Sotto is already running (pid $(pgrep -x sotto | head -1))." >&2
  echo "The app is single-instance, so this run would hand off to it. Close it first." >&2
  exit 2
fi

head_ "0. Build with the test-only surface"
if ./scripts/build.sh >"$SANDBOX/build.log" 2>&1 \
   && cargo build -p sotto --release --features e2e >>"$SANDBOX/build.log" 2>&1; then
  ok "built with --features e2e"
else
  bad "built with --features e2e" "$(tail -3 "$SANDBOX/build.log")"
  exit 1
fi

head_ "1. Sandbox, virtual audio device"
mkdir -p "$SANDBOX/root/config"

# Auto-insert must be OFF for this run, or the test types the transcript into
# whatever window has focus — which is the developer's desktop, not a test fixture.
# It did exactly that on the first run: delivery reported "inserted (wl-copy+paste)"
# and the words went to the focused window. A test that can type into the machine
# running it is a test that can do damage, so the setting is written explicitly
# rather than relying on a default.
cat > "$SANDBOX/root/config/settings.json" <<'JSON'
{"revision":1,"general":{"palette":"blue","theme":"system","show_overlay":true,
"overlay_position":"bottom_centre","overlay_style":"minimal","autostart":false,
"close_action":"tray"},"audio":{"input_device":null,"gain":1.0,"mute_output":false,
"cues":false},"dictation":{"hold_chord":null,"toggle_chord":null,
"cancel_chord":null,"auto_insert":false,"keep_on_clipboard":true},
"dictionary":{"fuzzy_enabled":false,"phonetic_enabled":false,"fuzzy_threshold":0.18},
"history":{"retention":"three_days","max_entries":10000},
"delivery":{"prefer_paste":true,"restore_clipboard":true,"paste_delay_ms":60},
"model":{"directory":null,"verify_on_start":true,"idle_unload_secs":0}}
JSON
if python3 -c "import json;assert json.load(open('$SANDBOX/root/config/settings.json'))['dictation']['auto_insert'] is False" 2>/dev/null; then
  ok "auto-insert is off for this run"
else
  bad "auto-insert is off for this run"
  exit 1
fi
ORIG_SRC="$(pactl get-default-source 2>/dev/null || true)"
if ! pactl load-module module-null-sink sink_name="$SINK" \
      sink_properties=device.description="$SINK" >/dev/null 2>&1; then
  bad "could not create a virtual sink"
  exit 1
fi
ok "virtual sink created"
if pactl set-default-source "$SINK.monitor" >/dev/null 2>&1; then
  ok "virtual source is the default input"
else
  bad "virtual source is the default input"
  exit 1
fi
[[ -f "$CLIP" ]] && ok "test clip present" || { bad "test clip present"; exit 1; }

# The script the app evaluates. Written here, with the timings baked in, because
# the page cannot read the environment.
cat > "$SCRIPT" <<JS
(async function () {
  const out = (m) => window.__TAURI__.core.invoke('probe_write', { text: m });
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const btn = document.querySelector('#btn-dictate');
  const box = document.querySelector('#hero-result');
  const waitFor = async (fn, ms) => {
    for (let i = 0; i < ms / 100; i++) { await sleep(100); if (fn()) return true; }
    return false;
  };
  try {
    if (!btn) return out('ERROR no #btn-dictate in the page');


    // ── Take 1: the button, start to finish ──────────────────────────────
    await out('STEP started label="' + btn.textContent + '"');
    btn.click();
    const started = await waitFor(() => btn.textContent.toLowerCase().includes('stop'), 10000);
    await out('STEP recording=' + started + ' label="' + btn.textContent + '"');
    if (!started) return out('DONE failed: the button did not switch to Stop');

    await out('STEP speaking');
    await sleep(${RECORD_MS});
    btn.click();
    await out('STEP stopping');

    const shown = await waitFor(() => box && !box.hidden, 60000);
    await out('STEP result_shown=' + shown);
    await out('RESULT title="' + document.querySelector('#hero-title').textContent + '"');
    await out('RESULT sub="' + document.querySelector('#hero-sub').textContent + '"');
    await out('RESULT body="' + (box ? box.innerText.replace(/\\n+/g, ' / ') : '') + '"');

    const hist1 = await window.__TAURI__.core.invoke('list_history', { limit: 20, before: null });
    await out('RESULT history_count=' + hist1.length);
    if (hist1.length) {
      const a = hist1[0].attempt;
      await out('RESULT history_text="' + ((a && (a.corrected_text || a.raw_text)) || '') + '"');
      await out('RESULT history_duration_ms=' + hist1[0].take.duration_ms);
      await out('RESULT history_status=' + hist1[0].take.status);
      await out('RESULT history_audio=' + hist1[0].take.audio_present);
    }

    // ── Take 2: started by something else, stopped by the button ─────────
    //
    // This is the path a shortcut takes. It used to leave state.recording false
    // while the label read "Stop and transcribe", so the button promised to stop
    // and instead tried to start a second take, failing with "already recording".
    await out('STEP second_starting');
    await window.__TAURI__.core.invoke('start_dictation');
    // The window must learn about it from the backend, not from its own click.
    const relabelled = await waitFor(() => btn.textContent.toLowerCase().includes('stop'), 15000);
    await out('STEP second_label="' + btn.textContent + '" relabelled=' + relabelled);

    await out('STEP second_speaking');
    await sleep(${RECORD_MS});

    btn.click();
    await out('STEP second_stopping');
    const shown2 = await waitFor(() => box && !box.hidden, 60000);
    await out('STEP second_result_shown=' + shown2);
    await out('RESULT second_sub="' + document.querySelector('#hero-sub').textContent + '"');
    await out('RESULT second_body="' + (box ? box.innerText.replace(/\\n+/g, ' / ') : '') + '"');

    const hist2 = await window.__TAURI__.core.invoke('list_history', { limit: 20, before: null });
    await out('RESULT second_history_count=' + hist2.length);
    if (hist2.length) {
      await out('RESULT second_history_status=' + hist2[0].take.status);
      await out('RESULT second_history_duration_ms=' + hist2[0].take.duration_ms);
    }

    // ── No success toasts ────────────────────────────────────────────────
    //
    // Settings changes already show their result in the control and its foot
    // line. Confirmation toasts were pure noise, so a change must not raise one.
    // The idle toggle and retention select are safe to exercise: both only save.
    await out('STEP toast_check');
    document.querySelector('.nav-item[data-page="general"]').click();
    const posOpts = Array.from(document.querySelectorAll('#overlay-position option')).map((o) => o.value);
    await out('RESULT overlay_positions=' + posOpts.join(','));
    const idle = document.querySelector('#idle-unload');
    const idleBefore = idle.checked;
    idle.click();
    await sleep(250);
    const idleToast = !document.querySelector('#toast').hidden;
    idle.checked = idleBefore;
    idle.dispatchEvent(new Event('change', { bubbles: true }));

    const sel = document.querySelector('#hist-retention');
    const oldRetention = sel.value;
    sel.value = oldRetention === 'three_days' ? 'forever' : 'three_days';
    sel.dispatchEvent(new Event('change', { bubbles: true }));
    await sleep(250);
    const retToast = !document.querySelector('#toast').hidden;
    sel.value = oldRetention;
    sel.dispatchEvent(new Event('change', { bubbles: true }));
    await out('RESULT toast_idle=' + idleToast + ' toast_retention=' + retToast);

    // ── Playback: Stop must stop, not restart ────────────────────────────
    //
    // The row's playing class is the state. A regression checked the class on
    // the button instead, so Stop fell through to the Play branch and restarted
    // the recording — it looked like pause-then-resume. This drives the real
    // host; PULSE_SINK points at the test null sink, so nothing is heard.
    await out('STEP playback_open');
    document.querySelector('.nav-item[data-page="history"]').click();
    const rowReady = await waitFor(
      () => document.querySelector('#hist-list .hist-item .play-btn'), 15000);
    await out('STEP playback_row=' + rowReady);
    if (rowReady) {
      const row = document.querySelector('#hist-list .hist-item');
      const pb = row.querySelector('.play-btn');
      const invoke = window.__TAURI__.core.invoke;
      pb.click();
      const started = await waitFor(() => row.classList.contains('playing'), 5000);
      await sleep(700);
      const before = await invoke('playback_status');
      await out('STEP playback_before_stop=' + before.playing + ' position=' + before.position_ms);
      pb.click();
      let after = { playing: true, position_ms: 0 };
      for (let i = 0; i < 25; i++) {
        await sleep(120);
        after = await invoke('playback_status');
        if (!after.playing) break;
      }
      // Give a restart time to show itself: the regression restarted within the
      // click, so it was still playing well past this point.
      await sleep(900);
      const settled = await invoke('playback_status');
      await out('RESULT playback_started=' + started);
      await out('RESULT playback_before_stop=' + before.playing);
      await out('RESULT playback_after_stop=' + settled.playing + ' position=' + settled.position_ms);
      await out('RESULT playback_row_playing=' + row.classList.contains('playing'));
      await out('RESULT playback_label=' + pb.textContent.trim());
    }

    await out('DONE ok');
  } catch (e) {
    await out('DONE threw: ' + e);
  }
})();
JS

head_ "2. Launch and press the button"
: > "$OUT"
SOTTO_ROOT="$SANDBOX/root" \
SOTTO_MODEL_DIR="$ROOT/model" \
SOTTO_STT_BIN="$ROOT/target/release/sotto-stt" \
SOTTO_E2E_SCRIPT="$SCRIPT" \
SOTTO_E2E_OUT="$OUT" \
SOTTO_E2E_DELAY_MS=6000 \
PULSE_SINK="$SINK" \
  setsid nohup "$APP" >"$SANDBOX/app.log" 2>&1 </dev/null &
APP_PID=$!

# Wait for the script to report that recording has begun, then speak into the sink.
for _ in $(seq 1 60); do
  sleep 1
  grep -q "^STEP recording=" "$OUT" 2>/dev/null && break
done
if grep -q "^STEP recording=true" "$OUT" 2>/dev/null; then
  ok "the Start button began a take"
else
  bad "the Start button began a take" "$(grep -E '^(STEP|ERROR|DONE)' "$OUT" 2>/dev/null | tail -3)"
fi

# The overlay is a separate native window. While recording it must exist: a user
# who presses Start sees it, and no library test can show that.
if kdotool search --name "^Sotto overlay$" >/dev/null 2>&1; then
  ok "the recording overlay appeared"
else
  bad "the recording overlay appeared"
fi

paplay -d "$SINK" "$CLIP" >/dev/null 2>&1
ok "spoke into the virtual microphone"

# Second take: started from outside the page, so the window has to learn about it
# from the backend event rather than from its own click handler.
for _ in $(seq 1 120); do
  sleep 1
  grep -q "^STEP second_speaking" "$OUT" 2>/dev/null && break
done
if grep -q "^STEP second_label=.*relabelled=true" "$OUT" 2>/dev/null; then
  ok "a take started outside the page relabelled the button"
else
  bad "a take started outside the page relabelled the button" "$(grep '^STEP second_label' "$OUT" 2>/dev/null)"
fi
paplay -d "$SINK" "$CLIP" >/dev/null 2>&1
ok "spoke into the virtual microphone again"

for _ in $(seq 1 120); do
  sleep 1
  grep -q "^DONE" "$OUT" 2>/dev/null && break
done

head_ "3. What the window showed"
sed 's/^/        /' "$OUT" 2>/dev/null || true

head_ "4. Assertions"
grep -q "^DONE ok" "$OUT" 2>/dev/null \
  && ok "the script completed without error" \
  || bad "the script completed without error" "$(grep '^DONE' "$OUT" 2>/dev/null | tail -1)"

# Note: an earlier version asserted that the status bar had left "loading", as
# evidence the engine-ready event was delivered. It passed with the listener
# deliberately broken, so it proved nothing and was removed. The event path is
# proven by the second take below, where the label can only change via the event:
# `start_dictation` is invoked directly, so the click handler never runs.

grep -q "^STEP result_shown=true" "$OUT" 2>/dev/null \
  && ok "the transcript was rendered in the window" \
  || bad "the transcript was rendered in the window"

BODY="$(grep -oP '(?<=^RESULT body=").*(?="$)' "$OUT" 2>/dev/null | head -1)"
if [[ -n "$BODY" && ${#BODY} -gt 20 ]]; then
  ok "the rendered transcript is real text (${#BODY} chars)"
else
  bad "the rendered transcript is real text" "got: ${BODY:-<empty>}"
fi

HIST="$(grep -oP '(?<=^RESULT history_count=)\d+' "$OUT" 2>/dev/null | head -1)"
[[ "${HIST:-0}" -ge 1 ]] \
  && ok "the take was stored in History" \
  || bad "the take was stored in History" "count=${HIST:-none}"

DUR="$(grep -oP '(?<=^RESULT history_duration_ms=)\d+' "$OUT" 2>/dev/null | head -1)"
[[ "${DUR:-0}" -gt 5000 ]] \
  && ok "the stored take has real audio (${DUR} ms)" \
  || bad "the stored take has real audio" "duration=${DUR:-none} ms"

grep -q "^RESULT history_status=transcribed" "$OUT" 2>/dev/null \
  && ok "the take is marked transcribed" \
  || bad "the take is marked transcribed" "$(grep 'history_status' "$OUT" 2>/dev/null)"

grep -q "^RESULT history_audio=true" "$OUT" 2>/dev/null \
  && ok "the recording was kept on disk" \
  || bad "the recording was kept on disk"

# The words must be the words: compare against the clip's known transcript.
HIST_TEXT="$(grep -oP '(?<=^RESULT history_text=").*(?="$)' "$OUT" 2>/dev/null | head -1)"
if grep -qiE "meeting|schedule|budget|hiring|quarterly" <<<"$HIST_TEXT"; then
  ok "the stored transcript matches what was spoken"
else
  bad "the stored transcript matches what was spoken" "got: ${HIST_TEXT:-<empty>}"
fi

# The window and the store must agree — a transcript shown but not saved, or saved
# but not shown, is the kind of split that makes history untrustworthy.
#
# The body is `innerText` with newlines flattened to " / ", so it opens with a
# separator and ends with a footer ("13s audio · 224 ms · copied to clipboard").
# Comparing its first whitespace-separated token compared the separator "/", which
# History can never contain — the assertion failed against a correct app. Strip the
# separators and footer, then require exact equality: both strings come from the
# same take, so any difference is a real split and not a rounding error.
BODY_TEXT="$(python3 - "$BODY" <<'PY'
import re, sys
parts = [p.strip() for p in sys.argv[1].split(" / ")]
parts = [p for p in parts if p and "audio \u00b7" not in p]
print(re.sub(r"\s+", " ", " ".join(parts)).strip())
PY
)"
if [[ -n "$BODY_TEXT" && -n "$HIST_TEXT" && "$BODY_TEXT" == "$HIST_TEXT" ]]; then
  ok "the window and History show the same transcript"
else
  bad "the window and History show the same transcript" \
      "window=\"${BODY_TEXT:0:70}\" history=\"${HIST_TEXT:0:70}\""
fi

grep -q "^STEP stopping" "$OUT" 2>/dev/null \
  && ok "the Stop button ended the take" \
  || bad "the Stop button ended the take"

# The second take was started outside the page. Clicking the button had to STOP it,
# not try to start a third: that is the bug the sync fix addressed.
# A row existing is not evidence the take ended: a take still in `recording` state
# also has a row. The proof is that it reached `transcribed` with real audio, which
# can only happen if the button actually stopped it. The first version of this
# check counted rows and passed while the second take was still recording.
SECOND_COUNT="$(grep -oP '(?<=^RESULT second_history_count=)\d+' "$OUT" 2>/dev/null | head -1)"
SECOND_DUR="$(grep -oP '(?<=^RESULT second_history_duration_ms=)\d+' "$OUT" 2>/dev/null | head -1)"
if [[ "${SECOND_COUNT:-0}" -eq 2 && "${SECOND_DUR:-0}" -gt 5000 ]]; then
  ok "a second take started outside the page was stopped by the button (${SECOND_DUR} ms)"
else
  bad "a second take started outside the page was stopped by the button" \
      "history=${SECOND_COUNT:-none} duration=${SECOND_DUR:-none} ms"
fi
grep -q "^STEP second_result_shown=true" "$OUT" 2>/dev/null \
  && ok "the second transcript was rendered too" \
  || bad "the second transcript was rendered too"
grep -q "already recording" "$OUT" 2>/dev/null \
  && bad "the button never tried to start a second take" "reported 'already recording'" \
  || ok "the button never tried to start a second take"
SECOND_STATUS="$(grep -oP '(?<=^RESULT second_history_status=)\w+' "$OUT" 2>/dev/null | head -1)"
[[ "$SECOND_STATUS" == "transcribed" ]] \
  && ok "the second take transcribed" \
  || bad "the second take transcribed" "status=${SECOND_STATUS:-none}"

grep -q "^RESULT overlay_positions=bottom_centre,top_centre" "$OUT" 2>/dev/null \
  && ok "the overlay offers only bottom centre and top centre" \
  || bad "the overlay offers only bottom centre and top centre" "$(grep '^RESULT overlay_positions' "$OUT" 2>/dev/null)"

grep -q "^RESULT toast_idle=false toast_retention=false" "$OUT" 2>/dev/null \
  && ok "settings changes raise no confirmation toasts" \
  || bad "settings changes raise no confirmation toasts" "$(grep '^RESULT toast' "$OUT" 2>/dev/null)"

# Playback stop: the real host must be playing before Stop, idle after it, and
# the row must leave its playing state. The regression restarted the audio on the
# second click, so it was still playing well after Stop.
grep -q "^RESULT playback_started=true" "$OUT" 2>/dev/null \
  && ok "the history row entered its playing state" \
  || bad "the history row entered its playing state" "$(grep '^RESULT playback' "$OUT" 2>/dev/null | head -2)"
grep -q "^RESULT playback_before_stop=true" "$OUT" 2>/dev/null \
  && ok "the recording was actually playing before Stop" \
  || bad "the recording was actually playing before Stop" "$(grep '^STEP playback_before' "$OUT" 2>/dev/null)"
grep -q "^RESULT playback_after_stop=false" "$OUT" 2>/dev/null \
  && ok "Stop stopped playback and it stayed stopped" \
  || bad "Stop stopped playback and it stayed stopped" "$(grep '^RESULT playback_after' "$OUT" 2>/dev/null)"
grep -q "^RESULT playback_row_playing=false" "$OUT" 2>/dev/null \
  && ok "the history row left its playing state after Stop" \
  || bad "the history row left its playing state after Stop" "$(grep '^RESULT playback_row' "$OUT" 2>/dev/null)"

# Nothing may have been typed into the real desktop. With auto-insert off, delivery
# must report "copied only", and the page must say so rather than claiming the words
# were inserted.
DELIVERY="$(grep -oP '(?<= · )[a-z ]+(?= \()|(?<=· )[a-z ]+(?=\")' "$OUT" 2>/dev/null | head -1)"
if grep -q "inserted (" "$OUT" 2>/dev/null; then
  bad "no text was pasted into the desktop" "delivery reported insertion"
else
  ok "no text was pasted into the desktop"
fi
if grep -q "Copied to the clipboard" "$OUT" 2>/dev/null; then
  ok "the page says the words are on the clipboard, not inserted"
else
  bad "the page says the words are on the clipboard, not inserted" "$(grep '^RESULT sub=' "$OUT" | head -1)"
fi

echo
echo "─────────────────────────────────────────────"
echo "  passed: $PASS"
echo "  failed: $FAIL"
echo "─────────────────────────────────────────────"
[[ $FAIL -eq 0 ]]
