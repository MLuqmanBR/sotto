#!/usr/bin/env node
// Check the accent-colour rule against the code that actually ships.
//
// Why this exists: the app rendered blue while the picker marked nothing, because
// no palette was stored and the stylesheet had a hardcoded blue fallback. The two
// disagreed, and nothing caught it — a visual mismatch that no Rust test can see
// and no screenshot reliably shows.
//
// The rule is now: the colour drawn is always the swatch that is lit. This runs the
// real `PALETTES` and `applyPalette` from `ui/app.js` against a stub DOM, so it
// checks the shipped function rather than a re-implementation of it. Extracting by
// text means the check fails loudly if the code is renamed instead of silently
// passing against nothing.

const fs = require("fs");
const path = require("path");

const ROOT = path.resolve(__dirname, "..");
const src = fs.readFileSync(path.join(ROOT, "ui/app.js"), "utf8");
const html = fs.readFileSync(path.join(ROOT, "ui/index.html"), "utf8");

const palettes = src.match(/const PALETTES = \{[\s\S]*?\};/);
const fallback = src.match(/const FALLBACK_PALETTE = "[^"]+";/);
const fn = src.match(/function applyPalette\(name\) \{[\s\S]*?\n\}/);
if (!palettes || !fallback || !fn) {
  console.log("  FAIL could not find PALETTES, FALLBACK_PALETTE or applyPalette in ui/app.js");
  process.exit(1);
}

// The colours the markup offers. The two lists must agree, or a swatch exists that
// applyPalette cannot resolve.
const declared = [...html.matchAll(/data-palette="([a-z]+)"/g)].map((m) => m[1]);
if (!declared.length) {
  console.log("  FAIL no swatches found in ui/index.html");
  process.exit(1);
}

const swatches = declared.map((p) => ({
  dataset: { palette: p },
  classList: {
    _on: false,
    toggle(cls, on) {
      if (cls === "active") this._on = !!on;
    },
    remove(cls) {
      if (cls === "active") this._on = false;
    },
  },
}));

const root = {
  style: {
    _v: {},
    setProperty(k, v) {
      this._v[k] = v;
    },
  },
};

const code = `
${palettes[0]}
${fallback[0]}
const document = { documentElement: __root };
const $$ = () => __swatches;
${fn[0]}
return applyPalette;
`;
const applyPalette = new Function("__root", "__swatches", code)(root, swatches);

const active = () => swatches.filter((s) => s.classList._on).map((s) => s.dataset.palette);

let failures = 0;
function check(name, cond, detail = "") {
  if (cond) {
    console.log(`  PASS ${name}`);
  } else {
    console.log(`  FAIL ${name}${detail ? " — " + detail : ""}`);
    failures++;
  }
}

// 1. Exactly one swatch is lit, always. Zero means the picker and the app disagree;
//    two means the picker cannot say which colour is in force.
function exactlyOne(name) {
  const a = active();
  check(`${name} lights exactly one swatch`, a.length === 1, JSON.stringify(a));
  return a[0];
}

// 2. The default. D-2 is answered as blue, so this is the state a new user sees.
applyPalette("blue");
check("blue is the swatch that is lit", exactlyOne("blue") === "blue");
check("blue sets --accent", root.style._v["--accent"] === "#4C8DFF", root.style._v["--accent"]);

// 3. The exact bug: no stored palette. It must resolve to the default and light it,
//    not draw blue with an empty picker.
applyPalette(null);
check("an absent palette still lights a swatch", exactlyOne("null") === "blue");
check("an absent palette still draws blue", root.style._v["--accent"] === "#4C8DFF", root.style._v["--accent"]);

applyPalette(undefined);
check("an undefined palette still lights a swatch", exactlyOne("undefined") === "blue");

// 4. A value from a future version, or a corrupted file, must not leave the picker
//    empty either.
applyPalette("chartreuse");
check("an unknown palette falls back and lights it", exactlyOne("unknown") === "blue");

// 5. Every colour the markup offers must be selectable, and exclusive.
for (const name of declared) {
  applyPalette(name);
  check(`${name} is exclusive`, exactlyOne(name) === name);
}

// 6. The markup and the palette table must describe the same set.
const tableKeys = [...palettes[0].matchAll(/^\s*([a-z]+):\s*"#[0-9A-Fa-f]{6}"/gm)].map((m) => m[1]);
check(
  "the markup and the palette table offer the same colours",
  JSON.stringify([...declared].sort()) === JSON.stringify([...tableKeys].sort()),
  `markup=${declared.join(",")} table=${tableKeys.join(",")}`
);

// 7. Six genuinely different colours, not shades of one. The user asked for this
//    explicitly: distinct hues, not variations on a blue.
const values = [...palettes[0].matchAll(/"#([0-9A-Fa-f]{6})"/g)].map((m) => m[1].toLowerCase());
const unique = new Set(values);
check("every accent is a distinct colour", unique.size === values.length, values.join(","));

function hue(hex) {
  const r = parseInt(hex.slice(0, 2), 16) / 255;
  const g = parseInt(hex.slice(2, 4), 16) / 255;
  const b = parseInt(hex.slice(4, 6), 16) / 255;
  const max = Math.max(r, g, b);
  const min = Math.min(r, g, b);
  const d = max - min;
  if (d === 0) return 0;
  let h;
  if (max === r) h = ((g - b) / d) % 6;
  else if (max === g) h = (b - r) / d + 2;
  else h = (r - g) / d + 4;
  return ((h * 60) + 360) % 360;
}

const hues = values.map(hue).sort((a, b) => a - b);
let minGap = 360;
for (let i = 0; i < hues.length; i++) {
  const next = i === hues.length - 1 ? hues[0] + 360 : hues[i + 1];
  minGap = Math.min(minGap, next - hues[i]);
}
check(
  "the colours are spread around the wheel, not shades of one",
  minGap >= 20,
  `closest pair is ${minGap.toFixed(0)}° apart: ${hues.map((h) => h.toFixed(0)).join(", ")}`
);

if (failures) {
  console.log(`\n  ${failures} check(s) failed`);
  process.exit(1);
}
console.log(`\n  ${declared.length} accents, all distinct and all selectable`);
