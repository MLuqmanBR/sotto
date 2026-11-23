#!/usr/bin/env node
// Check the shortcut defaults and the reserved-chord guard against the code that
// actually ships.
//
// Two things must be true, and neither is visible to Rust's tests alone:
//
//  1. The UI's Reset buttons must restore the same defaults the backend ships.
//     If they drift, "Reset" puts back a chord the app no longer considers
//     default — a silent mismatch.
//  2. Ctrl+Alt+F1–F12 must be refused at the moment of capture. The backend
//     refuses them too, but the user needs the reason where they typed it.
//
// This extracts the real functions from ui/app.js and runs them, the same way
// check-palette.js does: a renamed function fails loudly instead of silently
// passing against nothing.

const fs = require("fs");
const path = require("path");

const ROOT = path.resolve(__dirname, "..");
const src = fs.readFileSync(path.join(ROOT, "ui/app.js"), "utf8");
const rustConfig = fs.readFileSync(
  path.join(ROOT, "crates/sotto-core/src/config.rs"),
  "utf8"
);

let failures = 0;
const ok = (name) => console.log(`  ok   ${name}`);
const bad = (name, detail) => {
  failures += 1;
  console.log(`  FAIL ${name}${detail ? ` — ${detail}` : ""}`);
};
const check = (name, cond, detail) => (cond ? ok(name) : bad(name, detail));

// ── extract the shipped code ──────────────────────────────────────────────

const pick = (re, what) => {
  const m = src.match(re);
  if (!m) {
    console.log(`  FAIL could not find ${what} in ui/app.js`);
    process.exit(1);
  }
  return m[0];
};

const mods = pick(/const MODS = \[[^\]]*\];/, "MODS");
const chordFromEvent = pick(
  /function chordFromEvent\(e\) \{[\s\S]*?\n\}/,
  "chordFromEvent"
);
const reservedReason = pick(
  /function reservedReason\(chord\) \{[\s\S]*?\n\}/,
  "reservedReason"
);
const defaultsSrc = pick(
  /const CHORD_DEFAULTS = \{[^}]*\};/,
  "CHORD_DEFAULTS"
);

const ui = new Function(
  `${mods}\n${chordFromEvent}\n${reservedReason}\n${defaultsSrc}\n` +
    "return { chordFromEvent, reservedReason, CHORD_DEFAULTS };"
)();

// ── 1. the defaults are the ones the user asked for ───────────────────────

check(
  "hold default is Ctrl+Space",
  ui.CHORD_DEFAULTS.hold_chord === "Ctrl+Space",
  `got ${JSON.stringify(ui.CHORD_DEFAULTS.hold_chord)}`
);
check(
  "toggle default is Ctrl+Shift+Space",
  ui.CHORD_DEFAULTS.toggle_chord === "Ctrl+Shift+Space",
  `got ${JSON.stringify(ui.CHORD_DEFAULTS.toggle_chord)}`
);
check(
  "cancel default is Escape",
  ui.CHORD_DEFAULTS.cancel_chord === "Escape",
  `got ${JSON.stringify(ui.CHORD_DEFAULTS.cancel_chord)}`
);
check(
  "hold and toggle defaults differ",
  ui.CHORD_DEFAULTS.hold_chord !== ui.CHORD_DEFAULTS.toggle_chord
);

// The UI's defaults must match the backend's shipped defaults, or Reset and a
// fresh install disagree.
const rustDefault = (field) => {
  const m = rustConfig.match(new RegExp(`${field}: Some\\("([^"]+)"\\.into\\(\\)\\)`));
  return m ? m[1] : null;
};
for (const field of ["hold_chord", "toggle_chord", "cancel_chord"]) {
  const rust = rustDefault(field);
  check(
    `${field} matches the backend default`,
    rust === ui.CHORD_DEFAULTS[field],
    `ui=${JSON.stringify(ui.CHORD_DEFAULTS[field])} rust=${JSON.stringify(rust)}`
  );
}

// ── 2. the UI can actually produce those chords ───────────────────────────

const event = (key, mods) => ({
  key,
  ctrlKey: !!mods.ctrl,
  shiftKey: !!mods.shift,
  altKey: !!mods.alt,
  metaKey: !!mods.meta,
});
check(
  "Ctrl+Space records as Ctrl+Space",
  ui.chordFromEvent(event(" ", { ctrl: true })) === "Ctrl+Space",
  `got ${JSON.stringify(ui.chordFromEvent(event(" ", { ctrl: true })))}`
);
check(
  "Ctrl+Shift+Space records as Ctrl+Shift+Space",
  ui.chordFromEvent(event(" ", { ctrl: true, shift: true })) ===
    "Ctrl+Shift+Space",
  `got ${JSON.stringify(
    ui.chordFromEvent(event(" ", { ctrl: true, shift: true }))
  )}`
);

// ── 3. the UI refuses exactly the kernel-owned family ─────────────────────

const refused = [
  "Ctrl+Alt+F1",
  "Ctrl+Alt+F9",
  "Ctrl+Alt+F12",
  "ctrl+alt+f4",
  "Control+Alt+F9",
  "Ctrl+Alt+Backspace",
  "Ctrl+Alt+Delete",
  "Alt+SysRq",
];
const allowed = [
  "Ctrl+Space",
  "Ctrl+Shift+Space",
  "Ctrl+Shift+F9",
  "Ctrl+Alt+C",
  "Alt+F9",
  "Ctrl+F5",
  "Escape",
];
for (const chord of refused) {
  check(
    `refuses ${chord}`,
    typeof ui.reservedReason(chord) === "string",
    "no reason returned"
  );
}
for (const chord of allowed) {
  check(
    `allows ${chord}`,
    ui.reservedReason(chord) === null,
    `refused with ${JSON.stringify(ui.reservedReason(chord))}`
  );
}

console.log(
  failures === 0
    ? "  All shortcut checks passed."
    : `  ${failures} shortcut check(s) failed.`
);
process.exit(failures === 0 ? 0 : 1);