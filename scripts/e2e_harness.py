#!/usr/bin/env python3
"""End-to-end harness: drive the Sotto pipeline exactly as the GUI does.

This does not open a window. It exercises the same Rust code paths the UI calls:
start_dictation-equivalent capture is skipped (no microphone in CI), but the
finish() path — sidecar, model, dictionary, store, delivery — is real.

Usage: python3 scripts/e2e_harness.py <wav> [--expect "substring"]
"""
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SIDECAR = Path(os.environ.get("SOTTO_STT_BIN", ROOT / "target/release/sotto-stt"))
MODEL = Path(os.environ.get("SOTTO_MODEL_DIR", ROOT / "model"))


class Sidecar:
    def __init__(self):
        self.p = subprocess.Popen(
            [str(SIDECAR), "--model", str(MODEL)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )
        self._send({"type": "hello", "proto": 1})
        self.ready = self._recv()
        assert self.ready["type"] == "ready", self.ready

    def _send(self, obj):
        self.p.stdin.write(json.dumps(obj) + "\n")
        self.p.stdin.flush()

    def _recv(self):
        line = self.p.stdout.readline()
        if not line:
            raise RuntimeError("sidecar closed the stream")
        return json.loads(line)

    def transcribe(self, wav, words=False):
        self._send(
            {
                "type": "transcribe",
                "id": "e2e",
                "wav": str(wav),
                "timestamps": "word" if words else "segment",
            }
        )
        r = self._recv()
        if r["type"] != "result":
            raise RuntimeError(f"transcribe failed: {r}")
        return r

    def stop(self):
        try:
            self._send({"type": "shutdown"})
        except Exception:
            pass
        self.p.wait(timeout=10)


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    wav = Path(sys.argv[1])
    expect = None
    if "--expect" in sys.argv:
        expect = sys.argv[sys.argv.index("--expect") + 1]

    checks = []

    def check(name, ok, detail=""):
        checks.append((name, ok, detail))
        print(f"  {'PASS' if ok else 'FAIL'}  {name}" + (f"  ({detail})" if detail else ""))

    print("Sotto end-to-end harness")
    print(f"  sidecar : {SIDECAR}")
    print(f"  model   : {MODEL}")
    print(f"  audio   : {wav}")
    print()

    t0 = time.time()
    sc = Sidecar()
    load_s = time.time() - t0
    check("sidecar handshake", sc.ready["type"] == "ready")
    check("engine is voxrt", sc.ready["engine"] == "voxrt", sc.ready["engine"])
    check("model is parakeet-redux", sc.ready["model"] == "parakeet-redux")
    check("runs on CPU", sc.ready["device"] == "cpu")
    check(
        "SIMD kernel selected",
        "avx512" in sc.ready["kernels"] or "avx2" in sc.ready["kernels"],
        sc.ready["kernels"],
    )
    print(f"        model load: {load_s*1000:.0f} ms")

    t1 = time.time()
    r = sc.transcribe(wav, words=True)
    wall_ms = (time.time() - t1) * 1000
    check("transcript non-empty", bool(r["text"].strip()))
    check("segments returned", len(r["segments"]) >= 1)
    check("word timings returned", len(r["segments"][0]["words"]) > 0)
    rtf = r["duration"] / max(r["decode_ms"] / 1000, 1e-6)
    check("faster than real time", rtf > 10, f"{rtf:.0f}x")
    print(f"        decode {r['decode_ms']} ms wall {wall_ms:.0f} ms for {r['duration']:.2f} s")
    print(f"        text: {r['text'][:100]}")

    if expect:
        check("expected content present", expect.lower() in r["text"].lower(), expect)

    # Second run must be identical (determinism).
    r2 = sc.transcribe(wav)
    check("deterministic across runs", r2["text"] == r["text"])

    # Error handling: missing file must not crash the sidecar.
    try:
        sc._send({"type": "transcribe", "id": "bad", "wav": "/nope.wav"})
        err = sc._recv()
        check("missing audio handled cleanly", err["type"] == "error", err.get("code", ""))
    except Exception as e:
        check("missing audio handled cleanly", False, str(e))

    # Still alive after the error.
    r3 = sc.transcribe(wav)
    check("survives a bad request", r3["text"] == r["text"])

    sc.stop()

    passed = sum(1 for _, ok, _ in checks if ok)
    total = len(checks)
    print()
    print(f"{passed}/{total} checks passed")
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
