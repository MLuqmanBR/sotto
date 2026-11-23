# Sotto

Quiet, local-first dictation for Linux. Hold a key, speak, and your words land
at the cursor in whatever application you are using. Everything runs on your own
CPU: nothing is uploaded, there is no account, and the speech model ships inside
the package — there is nothing to download.

**Version 1.0.0**

---

## Install

Download the package for your distribution from the
[Releases](https://github.com/MLuqmanBR/sotto/releases) page.

**Fedora / RHEL-family**

```bash
sudo dnf install ./sotto-1.0.0-1.x86_64.rpm
```

**Debian / Ubuntu**

```bash
sudo apt install ./Sotto_1.0.0_amd64.deb
```

The model, speech engine, launcher entry and icon are all inside the package.
Installing is the only step; the first dictation does not fetch anything.

For text insertion, install one of:

- Wayland: `wtype` (recommended) or `ydotool`, plus `wl-clipboard`
- X11: `xdotool` and `xclip`

Sotto still records and transcribes without them; delivery falls back to the
clipboard and says so, rather than claiming the text was typed.

Uninstall with `sudo dnf remove sotto` or `sudo apt remove sotto`. Your settings
and history are not touched.

---

## Use

| Action | Default |
|---|---|
| Press and hold to dictate | `Ctrl+Space` |
| Press once to start, again to stop | `Ctrl+Shift+Space` |
| Cancel the current take | `Escape` |

Release the hold chord (or press the toggle chord again) and the recording is
transcribed on the spot, then inserted at the cursor. If insertion is not
available, it goes to the clipboard. Sotto never claims it typed something when
it did not.

Both shortcuts and the cancel chord can be changed in **Settings → Keybindings**;
each has its own Reset button. A chord another application already owns is named
in the window, so a shortcut that does nothing says why.

Other behaviour:

- **Overlay:** choose bottom centre or top centre, and Pill or Live preview.
- **Tray:** left-click the tray icon for the menu. Open Sotto, start/stop
  dictation, or quit. Closing the window keeps Sotto running in the tray by
  default; this is a setting.
- **Idle unload:** the model is released after five minutes without dictation to
  return about 700 MB. Set any duration, or zero to keep it loaded.
- **History:** every recording is kept with the model's raw text and your
  corrected text, so a correction is auditable. Retention is configurable.
- **Dictionary:** repairs names the model mishears, using explicit replacements,
  preferred spellings, fuzzy matching and phonetic matching.

Sotto records only while a take is running. There is no idle microphone and no
background listening.

---

## Why the dictionary matters

The runtime exposes no decoder biasing, so the model cannot be told a name in
advance; the transcript is repaired afterwards. In measurement on the
development machine, the model heard “Mohammed” as “Mohamed”. The dictionary
fixes it, and History keeps both the raw and corrected versions.

| Tier | What it catches | Default |
|---|---|---|
| Explicit replacement | literal text, boundary-aware | on |
| Preferred spelling | exact aliases, case-insensitive | on |
| Fuzzy (Levenshtein ≤ 0.18) | one or two letters off | off |
| Phonetic (Double Metaphone) | transliteration variants | off |

The fuzzy and phonetic tiers ship off because they can change ordinary words.
The Dictionary page shows which rule fires before you rely on it.

---

## Privacy

- Audio is captured only during a take and written to a local SQLite journal
  before transcription.
- No network request is made at runtime. The model is a local file.
- No telemetry, analytics or crash upload.
- History and settings live under `~/.local/share/sotto` and `~/.config/sotto`.
  Delete them whenever you like.

---

## What is inside

One model, one runtime, CPU only:

- **Model:** [`moondream/parakeet-redux`](https://huggingface.co/moondream/parakeet-redux),
  1.58-bit ternary, 178 MB, 25 European languages, CC-BY-4.0.
- **Runtime:** [`voxrt`](https://github.com/lmaksym/voxrt), Apache-2.0, vendored
  at a pinned commit.
- **No GPU path, no cloud fallback, no second model.** Those are deliberate
  exclusions: one code path that is tested end to end.

```
shortcut / tray
      │
      ▼
 pipeline (Rust)
 ├── audio capture ──► PCM journal on disk (before transcription)
 ├── sotto-stt sidecar ──► voxrt ──► parakeet-redux
 ├── dictionary (4 tiers)
 ├── SQLite history (raw and corrected text kept apart)
 └── delivery (clipboard + paste, with honest failure reporting)
```

The sidecar exists for a specific reason: `voxrt` has no cancellation token, so
the only way to stop inference is to end an owned process. It also keeps the
model resident, so the load is paid once, and isolates engine crashes from the
window.

### Measured performance

AMD Ryzen 7 7745HX, AVX-512 VNNI kernel:

| Clip | Decode | Real-time factor |
|---|---|---|
| 2.9 s | 88 ms | 33× |
| 9.7 s | 189 ms | 51× |
| 44.8 s | 872 ms | 51× |

Peak resident memory is about 950 MB with the model loaded (865 MB engine +
87 MB window), and returns to about 87 MB after idle unload. Disk footprint is
191 MB installed.

---

## Build from source

Requirements: Rust 1.89 or newer, GTK 3 and WebKitGTK 4.1 development headers,
and a C toolchain.

```bash
git clone https://github.com/MLuqmanBR/sotto.git
cd sotto
./scripts/fetch-model.sh            # downloads the 178 MB model into ./model
./scripts/build.sh                  # builds the engine first, then the app
./scripts/run.sh                    # runs from the build tree
```

To build the packages:

```bash
./scripts/package.sh                # .deb
./scripts/package-rpm.sh            # .rpm, from the same verified payload
```

The release artifacts were built from this tree with `voxrt` pinned at
`4fab626` (0.2.1). See `vendor/VOXRT-PIN.md`. The model is not committed because
of its size; `fetch-model.sh` retrieves it from Hugging Face. The installed
package always contains it, so end users never download anything.

### Tests

```bash
./scripts/verify-all.sh             # full gate
./scripts/verify-all.sh --quick     # skip the repeated-run stability pass
```

The gate runs, in order: release build with no warnings, every workspace test,
the command-surface and accent-palette checks, shortcut defaults and the
reserved-chord guard, the isolation suite, the real window driven by its own
buttons, X11 overlay placement on an offset monitor, the real tray over D-Bus,
package invariants, real install/uninstall of both formats, and three repeated
runs to catch flakiness.

Current results: **164 workspace tests, 51 isolation checks, 32 GUI checks, 10
X11 checks, 23 tray checks, 24 shortcut checks, 26 RPM install checks, 24 deb
install checks, 16 `.deb` + 16 `.rpm` package invariants, 3/3 stable runs.**

Some suites need `sudo` (package install), PipeWire/PulseAudio, `kdotool` and a
desktop session; they skip rather than pretend when the environment cannot
support them.

---

## Known limitations

- **A held chord's release edge on Wayland** is exercised by the shortcut
  decision tests and the chord registers with the compositor, but only a real
  held keypress proves KWin delivers the release. If a release is ever lost, the
  300-second ceiling ends the take; it cannot record forever.
- **Wayland multi-monitor placement** and a physically separate second display
  are not covered by the test machine. X11 placement is tested against a virtual
  monitor at a non-zero origin.
- **X11 was reached through Xwayland.** A native Xorg session may differ.
- **Wayland text insertion** needs `wtype` or `ydotool`; without one, delivery
  goes to the clipboard and the app says so.

---

## Licence

Sotto is MIT licensed. Speech recognition uses `voxrt` (Apache-2.0) with the
Parakeet Redux model by Moondream, derived from NVIDIA Parakeet TDT 0.6B v3
(CC-BY-4.0). The app's About page carries the model attribution the licence
requires. See [LICENSE](LICENSE).