#!/usr/bin/env bash
# Regenerate the known-audio clips the tests transcribe.
#
# These clips are synthetic speech from espeak-ng, not recordings, so they can be
# rebuilt at any time from this script instead of being kept in git. The expected
# transcripts are in `research/s0/REPORT.md`; if a clip is regenerated with a
# different voice the transcripts there must be updated too.
#
# Usage: ./scripts/make-test-audio.sh
set -euo pipefail

cd "$(dirname "$0")/.."
OUT="tests/fixtures/audio"
mkdir -p "$OUT"

command -v espeak-ng >/dev/null || { echo "espeak-ng is required" >&2; exit 1; }

# 16 kHz mono 16-bit PCM is what the sidecar expects; anything else has to be
# resampled before it can be transcribed, which would test the wrong thing.
say() {
  local name="$1" text="$2"
  espeak-ng -v en-us -s 165 -w "$OUT/$name.raw.wav" "$text"
  # espeak-ng writes 22050 Hz; resample and downmix to the model's rate.
  ffmpeg -y -loglevel error -i "$OUT/$name.raw.wav" \
    -ar 16000 -ac 1 -c:a pcm_s16le "$OUT/$name.wav"
  rm -f "$OUT/$name.raw.wav"
  python3 - "$OUT/$name.wav" <<'PY'
import sys, wave
w = wave.open(sys.argv[1])
print(f"  {sys.argv[1]}: {w.getnframes()/w.getframerate():.2f}s "
      f"{w.getframerate()}Hz {w.getnchannels()}ch {w.getsampwidth()*8}bit")
PY
}

echo "==> writing clips to $OUT"
say short "The quick brown fox jumps over the lazy dog."
say medium "Please schedule a meeting with Sarah and Mohammed for Tuesday afternoon at three thirty. We need to review the quarterly budget and discuss the new hiring plan for the engineering team."
say long "Please schedule a meeting with Sarah and Mohammed for Tuesday afternoon at three thirty. We need to review the quarterly budget and discuss the new hiring plan for the engineering team. Also, could you send the revised timeline to the design team before Friday, and confirm whether the vendor contract needs a legal review. Finally, let us block an hour on Thursday morning to go through the open questions together and agree on what we are shipping this quarter."

echo "==> done"
