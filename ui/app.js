/* Sotto UI controller. Plain JS, no bundler. */

const T = window.__TAURI__;
const invoke = (cmd, args) => T.core.invoke(cmd, args);

/* Accent choices are distinct colours, not shades of one. Each id maps to a
   base accent; the weak/line variants are derived from it in CSS.
   Every id here must have a matching card in index.html, and every card must
   appear here — a missing entry renders the click as a no-op. */
const PALETTES = {
  blue: "#4C8DFF",
  green: "#2FB56B",
  orange: "#F08C2E",
  pink: "#E8558F",
  violet: "#9B6BFF",
  red: "#E5533D",
};

let state = {
  snapshot: null,
  settings: null,
  dict: { entries: [], replacements: [], duplicates: [] },
  history: [],
  recording: false,
};

/* ── helpers ─────────────────────────────────────────────────────────── */

const $ = (s) => document.querySelector(s);
const $$ = (s) => Array.from(document.querySelectorAll(s));

function toast(msg, bad = false) {
  const el = $("#toast");
  el.textContent = msg;
  el.classList.toggle("bad", bad);
  el.hidden = false;
  clearTimeout(toast._t);
  toast._t = setTimeout(() => (el.hidden = true), bad ? 5200 : 2600);
}

function fmtBytes(n) {
  if (!n) return "0 B";
  const u = ["B", "KB", "MB", "GB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(n < 10 && i > 0 ? 1 : 0)} ${u[i]}`;
}

function fmtDuration(ms) {
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  return `${Math.floor(s / 60)}m ${s % 60}s`;
}

function fmtDate(iso) {
  const d = new Date(iso);
  if (isNaN(d)) return iso;
  return d.toLocaleString(undefined, {
    month: "short", day: "numeric", hour: "2-digit", minute: "2-digit",
  });
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
  );
}

/* The accent colour.
   A settings file from before D-2 was answered has no palette; the stylesheet
   then drew blue while no swatch was marked, so the picker and the app disagreed.
   An absent or unrecognised value now resolves to blue, which is the documented
   default, so the swatch that is lit is always the colour on screen. */
const FALLBACK_PALETTE = "blue";

function applyPalette(name) {
  const resolved = PALETTES[name] ? name : FALLBACK_PALETTE;
  document.documentElement.style.setProperty("--accent", PALETTES[resolved]);
  $$(".swatch").forEach((b) =>
    b.classList.toggle("active", b.dataset.palette === resolved)
  );
}

function applyTheme(theme) {
  let resolved = theme;
  if (theme === "system") {
    resolved = matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
  }
  document.documentElement.dataset.theme = resolved;
}

/* ── navigation ──────────────────────────────────────────────────────── */

function go(page) {
  $$(".nav-item").forEach((b) => b.classList.toggle("active", b.dataset.page === page));
  $$(".page").forEach((p) => (p.hidden = p.id !== `page-${page}`));
  if (page === "history") loadHistory();
  if (page === "dictionary") renderDictionary();
  if (page === "about") renderAbout();
}

$$(".nav-item").forEach((b) => b.addEventListener("click", () => go(b.dataset.page)));

/* ── snapshot ────────────────────────────────────────────────────────── */

async function refresh() {
  const snap = await invoke("get_snapshot");
  state.snapshot = snap;
  state.settings = snap.settings;

  applyPalette(snap.settings.general.palette);

  applyTheme(snap.settings.general.theme);
  $("#theme").value = snap.settings.general.theme;

  // Overlay + startup controls
  const g = snap.settings.general;
  $$(".preview-card").forEach((c) => {
    const on = c.dataset.style === g.overlay_style;
    c.classList.toggle("active", on);
    c.setAttribute("aria-checked", on ? "true" : "false");
  });
  $("#overlay-position").value = g.overlay_position;
  $("#overlay-position-row").style.display = g.overlay_style === "none" ? "none" : "";
  $("#autostart").checked = g.autostart;
  $("#close-action").value = g.close_action;

  // Report the autostart entry honestly: the checkbox is the preference, the
  // foot line is what is actually on disk.
  const af = $("#autostart-foot");
  if (!g.autostart) {
    af.textContent = "Sotto will not start on its own.";
  } else if (!snap.autostart.entry_present) {
    af.textContent = "The login entry could not be created. Toggle this off and on to retry.";
  } else if (!snap.autostart.target_exists) {
    af.textContent = `The login entry points at a file that no longer exists${snap.autostart.target ? ` (${snap.autostart.target})` : ""}. Toggle this off and on to repair it.`;
  } else {
    af.textContent = `Starts automatically at login (${snap.autostart.target}).`;
  }

  // model row
  const m = snap.model;
  const badge = $("#model-badge");
  if (m.present) {
    badge.textContent = snap.sidecar_running ? "ready" : "loading";
    badge.className = "chip ok";
  } else {
    badge.textContent = "missing";
    badge.className = "chip bad";
  }
  const foot = $("#model-foot");
  foot.textContent = m.present
    ? `Ships with the app · ${m.engine || "voxrt"} · ${m.kernels || "cpu"} · ${m.device || "cpu"} · nothing is downloaded`
    : "The model files are missing from this installation. Reinstall Sotto to restore them.";
  syncIdleUnload(snap.settings.model.idle_unload_secs);  // status bar
  const dot = $("#engine-dot");
  const txt = $("#engine-text");
  if (snap.sidecar_running) {
    dot.className = "dot ok";
    txt.textContent = "Parakeet Redux";
  } else if (m.present) {
    dot.className = "dot";
    txt.textContent = "Parakeet Redux · loading";
  } else {
    dot.className = "dot bad";
    txt.textContent = "Model missing";
  }
  $("#status-right").textContent = `v${snap.version}`;
  renderShortcutProblems(snap.shortcut_problems);

  // audio
  $("#gain").value = snap.settings.audio.gain;
  $("#gain-val").textContent = `${Number(snap.settings.audio.gain).toFixed(2)}×`;
  $("#cues").checked = snap.settings.audio.cues;
  $("#mute-output").checked = snap.settings.audio.mute_output;

  // dictation
  $("#auto-insert").checked = snap.settings.dictation.auto_insert;
  $("#keep-clip").checked = snap.settings.dictation.keep_on_clipboard;
  $("#paste-delay").value = snap.settings.delivery.paste_delay_ms;
  $("#paste-delay-val").textContent = `${snap.settings.delivery.paste_delay_ms} ms`;

  // dictionary
  $("#dict-fuzzy").checked = snap.settings.dictionary.fuzzy_enabled;
  $("#dict-phonetic").checked = snap.settings.dictionary.phonetic_enabled;
  $("#dict-threshold").value = snap.settings.dictionary.fuzzy_threshold;
  $("#dict-threshold-val").textContent =
    Number(snap.settings.dictionary.fuzzy_threshold).toFixed(2);

  $("#hist-retention").value = snap.settings.history.retention;

  const d = snap.settings.dictation;
  $("#bind-hold").textContent = d.hold_chord || "Not set";
  $("#bind-toggle").textContent = d.toggle_chord || "Not set";
  $("#bind-cancel").textContent = d.cancel_chord || "Not set";

  // The snapshot is the truth about whether a take is running, not local state.
  //
  // `state.recording` used to be set only by the button's own click handler, so a
  // take started by the global shortcut (or by hold) left it false while the
  // button *label* was driven from the snapshot and read "Stop and transcribe".
  // Clicking it then tried to start a second take and failed with "already
  // recording" — the label promised stop and the button did not stop. Syncing
  // from the snapshot makes the button honest however the take began.
  state.recording = !!snap.recording;
  $("#btn-dictate").textContent = snap.recording ? "Stop and transcribe" : "Start dictation";
  document.body.classList.toggle("recording", snap.recording);
}

/* ── settings persistence ────────────────────────────────────────────── */

let saveTimer = null;
function queueSave(mutator, immediate = false) {
  mutator(state.settings);
  if (saveTimer) clearTimeout(saveTimer);
  const doSave = async () => {
    try {
      const snap = await invoke("save_settings", { settings: state.settings });
      state.snapshot = snap;
      state.settings = snap.settings;
      applyPalette(snap.settings.general.palette);
      applyTheme(snap.settings.general.theme);
    } catch (e) {
      toast(`Could not save: ${e}`, true);
    }
  };
  if (immediate) doSave();
  else saveTimer = setTimeout(doSave, 250);
}

/* ── general ─────────────────────────────────────────────────────────── */

$$(".swatch").forEach((b) =>
  b.addEventListener("click", () => {
    const p = b.dataset.palette;
    applyPalette(p);
    queueSave((s) => (s.general.palette = p), true);
  })
);

$("#theme").addEventListener("change", (e) => {
  const v = e.target.value;
  applyTheme(v);
  queueSave((s) => (s.general.theme = v), true);
});

$$(".preview-card").forEach((card) =>
  card.addEventListener("click", () => {
    // Clicking the selected card deselects it, which means "no overlay". That
    // keeps the whole choice in two cards instead of adding a third for "None".
    const already = card.classList.contains("active");
    const v = already ? "none" : card.dataset.style;
    $$(".preview-card").forEach((c) => {
      const on = !already && c === card;
      c.classList.toggle("active", on);
      c.setAttribute("aria-checked", on ? "true" : "false");
    });
    $("#overlay-position-row").style.display = v === "none" ? "none" : "";
    queueSave((s) => {
      s.general.overlay_style = v;
      s.general.show_overlay = v !== "none";
    }, true);

    // Show the real overlay for a moment, so the choice is verified by sight
    // rather than by trusting a drawing.
    if (v !== "none") {
      invoke("preview_overlay", { style: v }).catch((e) => toast(String(e), true));
    }
  })
);

$("#overlay-position").addEventListener("change", (e) => {
  const v = e.target.value;
  queueSave((s) => (s.general.overlay_position = v), true);
});

$("#autostart").addEventListener("change", async (e) => {
  const on = e.target.checked;
  queueSave((s) => (s.general.autostart = on), true);
  // Re-read the snapshot so the foot line reports what actually happened on
  // disk, rather than echoing the checkbox back at the user.
  try {
    const snap = await invoke("get_snapshot");
    state.snapshot = snap;
    const af = $("#autostart-foot");
    if (on && !snap.autostart.entry_present) {
      af.textContent = "Saved, but the login entry could not be created. Check that ~/.config is writable.";
      toast("Could not create the login entry", true);
    } else if (on) {
      af.textContent = `Sotto will start automatically at login (${snap.autostart.target}).`;
    } else {
      af.textContent = "Sotto will not start on its own. You can launch it from the menu.";
    }
  } catch (err) {
    toast(`Could not save: ${err}`, true);
  }
});

$("#close-action").addEventListener("change", (e) => {
  const v = e.target.value;
  queueSave((s) => (s.general.close_action = v), true);
});

/* Idle unload is a toggle plus a duration: a slider for the common case, and a
   number field for anyone who wants an exact value. Both edit the same setting,
   so whichever the user reaches for stays in step with the other. */
function idleMinutes(secs) {
  return Math.max(1, Math.round(secs / 60));
}

function syncIdleUnload(secs) {
  const on = secs > 0;
  const minutes = idleMinutes(on ? secs : 300);

  $("#idle-unload").checked = on;
  $("#idle-unload-slider").value = String(minutes);
  $("#idle-unload-minutes").value = String(minutes);

  $("#idle-unload-slider").disabled = !on;
  $("#idle-unload-minutes").disabled = !on;
  $("#idle-unload-time-row").classList.toggle("muted", !on);

  $("#idle-unload-note").textContent = idleNote(on, minutes);
}

/* One short line. The previous copy ran to three clauses and wrapped mid-word,
   which is what made the row look broken. */
function idleNote(on, minutes) {
  return on
    ? `Frees about 700 MB. The next dictation takes a moment to start.`
    : `Keeps about 700 MB in use, so dictation always starts instantly.`;
}

/* "5 minutes", "1 hour", "1 hour 30 minutes" — a duration people read at a
   glance instead of a number of minutes they have to convert. */
function fmtMinutes(m) {
  if (m < 60) return `${m} minute${m === 1 ? "" : "s"}`;
  const h = Math.floor(m / 60);
  const r = m % 60;
  const hours = `${h} hour${h === 1 ? "" : "s"}`;
  return r ? `${hours} ${r} minute${r === 1 ? "" : "s"}` : hours;
}

/* Clamp to what the backend validates (1 minute … 24 hours) and write it back to
   both controls, so an empty or out-of-range value can never be saved. */
function commitIdleMinutes(raw) {
  let minutes = Math.round(Number(raw));
  if (!Number.isFinite(minutes) || minutes < 1) minutes = 1;
  if (minutes > 1440) minutes = 1440;
  syncIdleUnload(minutes * 60);
  queueSave((s) => (s.model.idle_unload_secs = minutes * 60), true);
}

$("#idle-unload").addEventListener("change", (e) => {
  const on = e.target.checked;
  if (on) {
    // Restore whatever the user last chose, defaulting to five minutes.
    const minutes = Number($("#idle-unload-minutes").value) || 5;
    syncIdleUnload(minutes * 60);
    queueSave((s) => (s.model.idle_unload_secs = minutes * 60), true);
  } else {
    syncIdleUnload(0);
    queueSave((s) => (s.model.idle_unload_secs = 0), true);
  }
});

// Dragging the slider updates the number as it moves, so the two never disagree.
$("#idle-unload-slider").addEventListener("input", (e) => {
  const minutes = Number(e.target.value);
  $("#idle-unload-minutes").value = String(minutes);
  $("#idle-unload-note").textContent = idleNote(true, minutes);
});
// Committing on release (change) is what actually saves, so a drag is one save
// rather than one per pixel.
$("#idle-unload-slider").addEventListener("change", (e) => {
  commitIdleMinutes(e.target.value);
});

$("#idle-unload-minutes").addEventListener("change", (e) => {
  commitIdleMinutes(e.target.value);
});
$("#idle-unload-minutes").addEventListener("keydown", (e) => {
  if (e.key === "Enter") e.target.blur();
});

$("#gain").addEventListener("input", (e) => {
  const v = Number(e.target.value);
  $("#gain-val").textContent = `${v.toFixed(2)}×`;
  queueSave((s) => (s.audio.gain = v));
});

$("#cues").addEventListener("change", (e) =>
  queueSave((s) => (s.audio.cues = e.target.checked), true)
);

$("#mute-output").addEventListener("change", (e) =>
  queueSave((s) => (s.audio.mute_output = e.target.checked), true)
);

$("#device").addEventListener("change", (e) => {
  const v = e.target.value;
  queueSave((s) => (s.audio.input_device = v === "__default" ? null : v), true);
});

$("#btn-test-mic").addEventListener("click", loadDevices);

async function loadDevices() {
  try {
    const devs = await invoke("list_inputs");
    const sel = $("#device");
    sel.innerHTML = `<option value="__default">System default</option>`;
    for (const d of devs) {
      const o = document.createElement("option");
      o.value = d.name;
      o.textContent = `${d.name}${d.is_default ? " (default)" : ""}`;
      sel.appendChild(o);
    }
    sel.value = state.settings?.audio?.input_device || "__default";
    if (!devs.length) toast("No microphone found", true);
  } catch (e) {
    toast(`Could not list microphones: ${e}`, true);
  }
}

/* ── dictation ───────────────────────────────────────────────────────── */

$("#btn-dictate").addEventListener("click", async () => {
  const btn = $("#btn-dictate");
  if (state.recording) {
    btn.disabled = true;
    btn.textContent = "Transcribing…";
    try {
      const r = await invoke("stop_dictation");
      state.recording = false;
      showResult(r);
      await refresh();
    } catch (e) {
      state.recording = false;
      toast(String(e), true);
      await refresh();
    } finally {
      btn.disabled = false;
    }
  } else {
    try {
      await invoke("start_dictation");
      state.recording = true;
      $("#hero-title").textContent = "Listening…";
      $("#hero-sub").textContent = "Speak now. Press stop when you're done.";
      $("#hero-result").hidden = true;
      await refresh();
    } catch (e) {
      toast(String(e), true);
    }
  }
});

function showResult(r) {
  const box = $("#hero-result");
  box.hidden = false;
  const changed = r.raw_text !== r.corrected_text;
  box.innerHTML = `
    <div>${escapeHtml(r.corrected_text)}</div>
    ${changed ? `<div class="d">Model heard: “${escapeHtml(r.raw_text)}”</div>` : ""}
    <div class="d">${fmtDuration(r.duration_ms)} audio · ${r.decode_ms} ms · ${escapeHtml(r.delivery)}</div>
  `;
  $("#hero-title").textContent = "Done";
  // Say only what actually happened. "Inserted" means a paste was sent; anything
  // else means the words are on the clipboard and the user has to place them
  // themselves — claiming otherwise would be the exact dishonesty BUILD-PLAN §10
  // forbids.
  //
  // Matching is on the prefix "copied", not one exact string: there are two ways
  // to end up on the clipboard ("copied only: …" when pasting failed or was not
  // asked for, and plain "copied to clipboard" when auto-insert is off), and the
  // first version only recognised one of them. A user who had turned auto-insert
  // off was told "Not delivered" while their words were sitting on the clipboard.
  const d = String(r.delivery || "");
  if (d.startsWith("inserted")) {
    $("#hero-sub").textContent = "Your words are in the app you were using.";
  } else if (d.includes("focused window changed")) {
    $("#hero-sub").textContent =
      "Copied to the clipboard — the window you were in changed, so nothing was pasted.";
  } else if (d.startsWith("copied")) {
    $("#hero-sub").textContent = "Copied to the clipboard. Press Ctrl+V where you want it.";
  } else {
    $("#hero-sub").textContent = "Not delivered. The text is saved in History.";
  }
}

/* ── keybindings ─────────────────────────────────────────────────────── */

const MODS = ["Control", "Shift", "Alt", "Meta", "Super"];

function chordFromEvent(e) {
  if (MODS.includes(e.key)) return null;
  const parts = [];
  if (e.ctrlKey) parts.push("Ctrl");
  if (e.altKey) parts.push("Alt");
  if (e.shiftKey) parts.push("Shift");
  if (e.metaKey) parts.push("Super");
  let k = e.key;
  if (k === " ") k = "Space";
  else if (k.length === 1) k = k.toUpperCase();
  if (!parts.length) return null;
  parts.push(k);
  return parts.join("+");
}

/* Chords the kernel or desktop claims before any application sees them.
   Mirrors reserved_reason in src-tauri/src/shortcuts.rs; keep the two in step.
   Ctrl+Alt+F1–F12 are virtual-terminal switches — binding one does not intercept
   it, it switches the screen away, and it once hung this machine hard enough to
   need a reset. */
function reservedReason(chord) {
  const parts = chord.toLowerCase().split("+").map((p) => p.trim());
  const has = (n) => parts.includes(n);
  const key = parts[parts.length - 1];
  if ((has("ctrl") || has("control")) && has("alt")) {
    if (/^f\d+$/.test(key)) return "the system uses it to switch virtual terminals";
    if (key === "backspace") return "it terminates the display server";
    if (key === "delete") return "it restarts or logs out the machine";
  }
  if (has("alt") && has("sysrq")) return "it is the kernel's debug facility";
  return null;
}

function bindChord(btn, field) {
  btn.addEventListener("click", () => {
    btn.classList.add("capturing");
    const prev = btn.textContent;
    btn.textContent = "Press keys…";
    const onKey = (e) => {
      e.preventDefault();
      e.stopPropagation();
      if (e.key === "Escape") { cleanup(); btn.textContent = prev; return; }
      const chord = chordFromEvent(e);
      if (!chord) return;
      // Refuse a system-owned chord here, before it is saved: the backend
      // refuses it too, but a toast at the moment of capture is the only place
      // the user can see why and try another one immediately.
      const reserved = reservedReason(chord);
      if (reserved) {
        cleanup();
        btn.classList.remove("capturing");
        btn.textContent = prev;
        toast(`${chord} is reserved — ${reserved}`);
        return;
      }
      cleanup();
      btn.classList.remove("capturing");
      btn.textContent = chord;
      queueSave((s) => (s.dictation[field] = chord), true);
    };
    const cleanup = () => window.removeEventListener("keydown", onKey, true);
    window.addEventListener("keydown", onKey, true);
  });
}

bindChord($("#bind-hold"), "hold_chord");
bindChord($("#bind-toggle"), "toggle_chord");
bindChord($("#bind-cancel"), "cancel_chord");

/* Reset puts a chord back to its shipped default.
   Hold is Ctrl+Space (press to talk, release to stop) and toggle is the same
   chord plus Shift, so the two cannot fire on one keypress; cancel is Escape. */
const CHORD_DEFAULTS = { hold_chord: "Ctrl+Space", toggle_chord: "Ctrl+Shift+Space", cancel_chord: "Escape" };
$$("[data-reset]").forEach((btn) =>
  btn.addEventListener("click", () => {
    const field = btn.dataset.reset;
    const value = CHORD_DEFAULTS[field] ?? null;
    queueSave((s) => (s.dictation[field] = value), true);
  })
);

/* Shortcut problems are the desktop refusing a chord because another application
   already owns it. Saying so is the difference between "this does nothing" and a
   user knowing why. */
function renderShortcutProblems(problems) {
  const box = $("#bind-problems");
  if (!box) return;
  const list = problems || [];
  if (!list.length) {
    box.hidden = true;
    box.textContent = "";
    return;
  }
  box.hidden = false;
  box.textContent = list.join(" ");
}

$("#auto-insert").addEventListener("change", (e) =>
  queueSave((s) => (s.dictation.auto_insert = e.target.checked), true)
);
$("#keep-clip").addEventListener("change", (e) =>
  queueSave((s) => (s.dictation.keep_on_clipboard = e.target.checked), true)
);
$("#paste-delay").addEventListener("input", (e) => {
  const v = Number(e.target.value);
  $("#paste-delay-val").textContent = `${v} ms`;
  queueSave((s) => (s.delivery.paste_delay_ms = v));
});

/* ── history ─────────────────────────────────────────────────────────── */

$("#hist-retention").addEventListener("change", (e) => {
  queueSave((s) => (s.history.retention = e.target.value), true);
});

$("#hist-search").addEventListener("input", renderHistory);

/* Deleting every recording cannot be undone, so it asks first.
   The confirmation is inline rather than a native dialog: WebKitGTK in a Tauri
   window does not reliably provide window.confirm, and a silent no-op dialog is
   worse than no dialog. First click arms the button and names the count; a second
   click within four seconds does it; anything else disarms. */
let clearArmed = null;
function disarmClear() {
  const b = $("#btn-clear-audio");
  b.classList.remove("danger");
  b.textContent = "Delete all audio";
  if (clearArmed) clearTimeout(clearArmed);
  clearArmed = null;
}

$("#btn-clear-audio").addEventListener("click", async () => {
  const btn = $("#btn-clear-audio");
  const count = state.history.filter((h) => h.take.audio_present).length;
  if (!count) return;

  if (!clearArmed) {
    btn.classList.add("danger");
    btn.textContent = `Delete ${count} recording${count === 1 ? "" : "s"}?`;
    clearArmed = setTimeout(disarmClear, 4000);
    return;
  }

  disarmClear();
  try {
    // Deletes the audio only. The transcripts stay, which is what this page
    // promises; deleting the words is a per-item action.
    await invoke("delete_all_audio");
    await loadHistory();
    await refresh();
  } catch (e) {
    toast(String(e), true);
  }
});

// Leaving the page, or starting anything else, disarms a pending confirmation so
// a stale armed button cannot be clicked later by accident.
$("#hist-search").addEventListener("focus", disarmClear);
$$(".nav-item").forEach((b) => b.addEventListener("click", disarmClear));

/* Only one recording plays at a time, so at most one row shows as playing. */
let playingRow = null;
let playPoll = null;

function setPlaying(el) {
  if (playingRow && playingRow !== el) {
    playingRow.classList.remove("playing");
    const b = playingRow.querySelector(".play-btn");
    if (b) b.textContent = "▶";
    const f = playingRow.querySelector(".play-fill");
    if (f) f.style.width = "0%";
  }
  playingRow = el;

  if (playPoll) {
    clearInterval(playPoll);
    playPoll = null;
  }

  if (el) {
    el.classList.add("playing");
    const b = el.querySelector(".play-btn");
    if (b) b.textContent = "■";
    // Poll the app for position. Playback runs natively, so the page cannot
    // learn about it from a media event.
    playPoll = setInterval(async () => {
      try {
        const s = await invoke("playback_status");
        if (!s.playing) {
          setPlaying(null);
          return;
        }
        const fill = el.querySelector(".play-fill");
        if (fill && s.duration_ms > 0) {
          fill.style.width = `${Math.min(100, (s.position_ms / s.duration_ms) * 100)}%`;
        }
      } catch {
        setPlaying(null);
      }
    }, 200);
  }
}

$("#btn-export-history").addEventListener("click", async () => {
  try {
    await invoke("export_history");
  } catch (e) {
    toast(String(e), true);
  }
});

async function loadHistory() {
  try {
    state.history = await invoke("list_history", { limit: 200, before: null });
    renderHistory();
  } catch (e) {
    toast(`Could not load history: ${e}`, true);
  }
}

function renderHistory() {
  const list = $("#hist-list");
  const clear = $("#btn-clear-audio");
  const audioCount = state.history.filter((h) => h.take.audio_present).length;
  clear.disabled = audioCount === 0;
  clear.title = audioCount === 0 ? "No recordings to delete" : "";
  const q = ($("#hist-search").value || "").toLowerCase().trim();
  const items = state.history.filter((h) => {
    if (!q) return true;
    const t = (h.attempt?.corrected_text || h.attempt?.raw_text || "").toLowerCase();
    return t.includes(q);
  });

  if (!items.length) {
    list.innerHTML = `<p class="empty">${
      state.history.length ? "Nothing matches that search." : "No dictations yet."
    }</p>`;
    return;
  }

  list.innerHTML = "";
  for (const h of items) {
    const t = h.take;
    const a = h.attempt;
    const text = a?.corrected_text || a?.raw_text || "";
    const changed = a && a.corrected_text && a.corrected_text !== a.raw_text;
    const el = document.createElement("div");
    el.className = "hist-item";
    el.innerHTML = `
      <div class="meta-line">
        <span>${fmtDate(t.created_at)}</span>
        <span>${fmtDuration(t.duration_ms)}</span>
        <span>${t.audio_present ? "audio kept" : "audio deleted"}</span>
        ${a ? `<span>${a.decode_ms} ms</span>` : ""}
      </div>
      ${
        changed
          ? `<div class="views" role="tablist">
               <button class="view-tab active" data-view="corrected" role="tab" aria-selected="true">Corrected</button>
               <button class="view-tab" data-view="raw" role="tab" aria-selected="false">Raw</button>
             </div>`
          : ""
      }
      <div class="body">${escapeHtml(text) || "<em>(empty)</em>"}</div>
      ${
        t.audio_present
          ? `<div class="player" data-take="${escapeHtml(t.id)}">
               <button class="play-btn" data-act="play" title="Play this recording">▶</button>
               <div class="play-bar"><div class="play-fill"></div></div>
               <span class="play-time">${fmtDuration(t.duration_ms)}</span>
             </div>`
          : `<div class="audio-gone">Recording no longer on disk</div>`
      }
      <div class="acts">
        <button data-act="copy">Copy</button>
        <button data-act="star" class="${t.favourite ? "on" : ""}" aria-pressed="${
          t.favourite ? "true" : "false"
        }" title="${
          t.favourite
            ? "Starred: protected from automatic cleanup"
            : "Star to protect this from automatic cleanup"
        }">${t.favourite ? "★ Starred" : "☆ Star"}</button>
        <button data-act="edit">Edit text</button>
        ${changed ? `<button data-act="learn">Add correction to dictionary</button>` : ""}
        ${
          t.audio_present
            ? `<button data-act="retry" title="Transcribe this recording again">Transcribe again</button>`
            : ""
        }
        <button data-act="delete">Delete</button>
      </div>
    `;

    // Raw and Corrected are separate views of the same dictation. The raw text is
    // what the model said; the corrected text is what the dictionary made of it.
    // Showing them apart is what makes a correction auditable.
    if (changed) {
      const body = el.querySelector(".body");
      el.querySelectorAll(".view-tab").forEach((tab) =>
        tab.addEventListener("click", () => {
          const raw = tab.dataset.view === "raw";
          el.querySelectorAll(".view-tab").forEach((x) => {
            const on = x === tab;
            x.classList.toggle("active", on);
            x.setAttribute("aria-selected", on ? "true" : "false");
          });
          body.textContent = raw ? a.raw_text : text;
          body.classList.toggle("is-raw", raw);
        })
      );
    }

    el.querySelector('[data-act="copy"]').addEventListener("click", async () => {
      try {
        await invoke("copy_text", { text });
        toast("Copied");
      } catch (e) {
        toast(String(e), true);
      }
    });
    const learn = el.querySelector('[data-act="learn"]');
    if (learn) {
      learn.addEventListener("click", async () => {
        const sug = await invoke("suggest_alias", {
          raw: a.raw_text,
          corrected: a.corrected_text,
        });
        if (!sug) return toast("Couldn't isolate a single-word correction", true);
        const [heard, right] = sug;
        await addEntry(right, [heard]);
      });
    }

    // Starring protects a take from automatic cleanup, so it has to be
    // reachable from the list — otherwise the retention rules I enforce after
    // each dictation could remove a recording the user meant to keep.
    el.querySelector('[data-act="star"]').addEventListener("click", async () => {
      try {
        await invoke("set_favourite", { id: t.id, favourite: !t.favourite });
        await loadHistory();
      } catch (e) {
        toast(String(e), true);
      }
    });

    // Editing rewrites the corrected text only. The model's raw output stays
    // untouched, so the provenance History shows remains true.
    el.querySelector('[data-act="edit"]').addEventListener("click", () => {
      if (!a) return toast("This dictation has no transcript to edit", true);
      const body = el.querySelector(".body");
      const input = document.createElement("textarea");
      input.className = "edit-box";
      input.rows = 3;
      input.value = text;
      body.replaceWith(input);
      input.focus();
      input.setSelectionRange(input.value.length, input.value.length);

      const finish = async (save) => {
        if (!save) return loadHistory();
        const next = input.value.trim();
        try {
          await invoke("edit_attempt", { attemptId: a.id, text: next });
          await loadHistory();
          await refresh();
        } catch (e) {
          toast(String(e), true);
        }
      };
      input.addEventListener("keydown", (ev) => {
        if (ev.key === "Escape") finish(false);
        if (ev.key === "Enter" && (ev.ctrlKey || ev.metaKey)) finish(true);
      });
      input.addEventListener("blur", () => finish(true));
    });
    // Playback runs in the app, not in the webview: WebKitGTK refuses to decode
    // audio on this stack, so the page asks the app to play the file instead.
    const playBtn = el.querySelector('[data-act="play"]');
    if (playBtn) {
      playBtn.addEventListener("click", async () => {
        // The `playing` class lives on the row, not on this button. Checking the
        // button made Stop fall through to the play branch, which restarted the
        // recording — it looked like pause-then-resume. The row is the state.
        if (el.classList.contains("playing")) {
          try {
            await invoke("stop_playback");
          } catch (e) {
            toast(String(e), true);
          }
          setPlaying(null);
          return;
        }
        try {
          await invoke("play_recording", { id: t.id });
          setPlaying(el);
        } catch (e) {
          toast(String(e), true);
          setPlaying(null);
        }
      });
    }

    const retry = el.querySelector('[data-act="retry"]');
    if (retry) {
      retry.addEventListener("click", async () => {
        retry.disabled = true;
        const label = retry.textContent;
        retry.textContent = "Transcribing…";
        try {
          await invoke("retry_transcription", { id: t.id });
          await loadHistory();
        } catch (e) {
          toast(String(e), true);
        } finally {
          retry.disabled = false;
          retry.textContent = label;
        }
      });
    }

    el.querySelector('[data-act="delete"]').addEventListener("click", async () => {
      try {
        await invoke("delete_history", { ids: [t.id], audio: true });
        await loadHistory();
        await refresh();
      } catch (e) {
        toast(String(e), true);
      }
    });
    list.appendChild(el);
  }
}

/* ── dictionary ──────────────────────────────────────────────────────── */

async function loadDictionary() {
  state.dict = await invoke("get_dictionary");
  renderDictionary();
}

function renderDictionary() {
  const { entries, replacements, duplicates } = state.dict;

  // Conflicts are reported, not hidden: two entries claiming the same spelling
  // make the winner depend on order, which is exactly the kind of thing a user
  // cannot debug from the transcript alone.
  const dup = $("#dict-dup");
  if (duplicates && duplicates.length) {
    dup.hidden = false;
    dup.textContent = `Same spelling more than once: ${duplicates.join(", ")}. Only one will apply.`;
    dup.classList.add("warn");
  } else {
    dup.hidden = true;
    dup.textContent = "";
    dup.classList.remove("warn");
  }

  const list = $("#dict-entries");
  if (!entries.length) {
    list.innerHTML = `<p class="empty">No words yet. Start with your own name — it's the one the model is most likely to get wrong.</p>`;
  } else {
    list.innerHTML = "";
    entries.forEach((e, i) => {
      const el = document.createElement("div");
      el.className = "entry" + (e.enabled === false ? " disabled" : "");
      el.innerHTML = `
        <input type="checkbox" class="mini-switch" ${e.enabled === false ? "" : "checked"}
               aria-label="Enable ${escapeHtml(e.preferred)}" title="On or off" />
        <span class="pref">${escapeHtml(e.preferred)}</span>
        <span class="aliases">${e.aliases.length ? "also: " + e.aliases.map(escapeHtml).join(", ") : ""}</span>
        <span class="spacer"></span>
        <select class="mini-select" aria-label="Case policy for ${escapeHtml(e.preferred)}" title="How casing is handled">
          <option value="exact"${e.case_policy !== "preserve" ? " selected" : ""}>As written</option>
          <option value="preserve"${e.case_policy === "preserve" ? " selected" : ""}>Follow input</option>
        </select>
        <button title="Remove">Remove</button>
      `;
      el.querySelector(".mini-switch").addEventListener("change", (ev) => {
        const next = entries.map((x, j) => (j === i ? { ...x, enabled: ev.target.checked } : x));
        saveEntries(next, replacements);
      });
      el.querySelector(".mini-select").addEventListener("change", (ev) => {
        const next = entries.map((x, j) =>
          j === i ? { ...x, case_policy: ev.target.value } : x
        );
        saveEntries(next, replacements);
      });
      el.querySelector("button").addEventListener("click", () => {
        saveEntries(entries.filter((_, j) => j !== i), replacements);
      });
      list.appendChild(el);
    });
  }

  const rlist = $("#dict-replacements");
  if (!replacements.length) {
    rlist.innerHTML = `<p class="empty">No replacements yet.</p>`;
  } else {
    rlist.innerHTML = "";
    replacements.forEach((r, i) => {
      const el = document.createElement("div");
      el.className = "entry" + (r.enabled === false ? " disabled" : "");
      el.innerHTML = `
        <input type="checkbox" class="mini-switch" ${r.enabled === false ? "" : "checked"}
               aria-label="Enable replacement" title="On or off" />
        <span class="pref">${escapeHtml(r.source)}</span>
        <span class="aliases">→ ${escapeHtml(r.target)}</span>
        <span class="spacer"></span>
        <button title="Remove">Remove</button>
      `;
      el.querySelector(".mini-switch").addEventListener("change", (ev) => {
        const next = replacements.map((x, j) => (j === i ? { ...x, enabled: ev.target.checked } : x));
        saveEntries(entries, next);
      });
      el.querySelector("button").addEventListener("click", () => {
        saveEntries(entries, replacements.filter((_, j) => j !== i));
      });
      rlist.appendChild(el);
    });
  }
}

async function saveEntries(entries, replacements) {
  try {
    state.dict = await invoke("save_dictionary", { entries, replacements });
    renderDictionary();
    await refresh();
  } catch (e) {
    toast(String(e), true);
  }
}

async function addEntry(preferred, aliases) {
  const existing = state.dict.entries.find(
    (e) => e.preferred.toLowerCase() === preferred.toLowerCase()
  );
  let entries;
  if (existing) {
    const merged = Array.from(new Set([...existing.aliases, ...aliases]));
    entries = state.dict.entries.map((e) => (e === existing ? { ...e, aliases: merged } : e));
  } else {
    entries = [
      ...state.dict.entries,
      {
        id: preferred.toLowerCase().replace(/\s+/g, "-") + "-" + Date.now().toString(36),
        preferred,
        aliases,
        language: null,
        case_policy: "exact",
        enabled: true,
      },
    ];
  }
  await saveEntries(entries, state.dict.replacements);
}

async function commitNewEntry() {
  const pref = $("#new-preferred").value.trim();
  const aliasesRaw = $("#new-aliases").value.trim();
  if (!pref) return toast("Enter the correct spelling first", true);
  const aliases = aliasesRaw
    ? aliasesRaw.split(",").map((s) => s.trim()).filter(Boolean)
    : [pref];
  await addEntry(pref, aliases);
  $("#new-preferred").value = "";
  $("#new-aliases").value = "";
}

$("#btn-add-inline").addEventListener("click", commitNewEntry);
$("#btn-add-entry").addEventListener("click", () => {
  $("#new-preferred").focus();
});

/* ── replacement rules ───────────────────────────────────────────────── */

async function commitNewReplacement() {
  const from = $("#new-replace-from").value.trim();
  const to = $("#new-replace-to").value.trim();
  if (!from) return toast("Enter the text to replace", true);
  if (!to) return toast("Enter what it should read as", true);

  // Replacing a phrase with itself would fire forever on every match, so it is
  // refused here rather than saved as a rule that does nothing.
  if (from.toLowerCase() === to.toLowerCase()) {
    return toast("Those are the same, so the rule would do nothing", true);
  }

  const replacement = {
    id: `r-${from.toLowerCase().replace(/\s+/g, "-")}-${Date.now().toString(36)}`,
    source: from,
    target: to,
    boundary: "word",
    case_sensitive: false,
    enabled: true,
    priority: 0,
  };
  await saveEntries(state.dict.entries, [...state.dict.replacements, replacement]);
  $("#new-replace-from").value = "";
  $("#new-replace-to").value = "";
}

$("#btn-add-replacement").addEventListener("click", commitNewReplacement);
["#new-replace-from", "#new-replace-to"].forEach((sel) =>
  $(sel).addEventListener("keydown", (e) => {
    if (e.key === "Enter") commitNewReplacement();
  })
);

/* ── dictionary backup ───────────────────────────────────────────────── */

$("#btn-dict-export").addEventListener("click", async () => {
  try {
    const path = await invoke("export_dictionary");
    $("#dict-backup-note").textContent = `Saved to ${path}`;
    $("#dict-backup-note").classList.add("path");
  } catch (e) {
    toast(String(e), true);
  }
});

$("#btn-dict-import").addEventListener("click", async () => {
  const raw = prompt(
    "Paste one term per line.\n\nEach line is a correct spelling. Add other spellings after a comma:\nMohammed, Mohamed, Muhammad"
  );
  if (!raw || !raw.trim()) return;
  try {
    const r = await invoke("import_dictionary_lines", { text: raw });
    await loadDictionary();
    $("#dict-backup-note").textContent = `Added ${r.added}, skipped ${r.skipped} already there.`;
    $("#dict-backup-note").classList.remove("path");
  } catch (e) {
    toast(String(e), true);
  }
});

["#dict-fuzzy", "#dict-phonetic"].forEach((sel) => {
  $(sel).addEventListener("change", () => {
    const f = $("#dict-fuzzy").checked;
    const p = $("#dict-phonetic").checked;
    queueSave((s) => {
      s.dictionary.fuzzy_enabled = f;
      s.dictionary.phonetic_enabled = p;
    }, true);
    runTest();
  });
});

$("#dict-threshold").addEventListener("input", (e) => {
  const v = Number(e.target.value);
  $("#dict-threshold-val").textContent = v.toFixed(2);
  queueSave((s) => (s.dictionary.fuzzy_threshold = v));
  runTest();
});

let testTimer = null;
$("#dict-test").addEventListener("input", () => {
  if (testTimer) clearTimeout(testTimer);
  testTimer = setTimeout(runTest, 200);
});

async function runTest() {
  const text = $("#dict-test").value;
  const out = $("#dict-test-out");
  if (!text.trim()) { out.innerHTML = ""; return; }
  try {
    const r = await invoke("test_dictionary", {
      req: {
        text,
        fuzzy: $("#dict-fuzzy").checked,
        phonetic: $("#dict-phonetic").checked,
        threshold: Number($("#dict-threshold").value),
      },
    });
    let html = escapeHtml(r.after);
    for (const c of r.corrections) {
      html = html.replace(escapeHtml(c.replacement), `<mark>${escapeHtml(c.replacement)}</mark>`);
    }
    out.innerHTML = `<div class="after">${html}</div><div class="fired">${
      r.corrections.length
        ? r.corrections
            .map((c) => `${c.tier}: “${escapeHtml(c.original)}” → “${escapeHtml(c.replacement)}”`)
            .join("<br>")
        : "No changes."
    }</div>`;
  } catch (e) {
    out.innerHTML = `<div class="fired">${escapeHtml(e)}</div>`;
  }
}

/* ── about ───────────────────────────────────────────────────────────── */

function renderAbout() {
  const s = state.snapshot;
  if (!s) return;
  const rows = [
    ["Version", s.version],
    ["Engine", s.model.engine || "voxrt"],
    ["Kernel", s.model.kernels || "—"],
    ["Device", s.model.device || "cpu"],
    ["Model", "parakeet-redux"],
    ["Model size", fmtBytes(s.model.bytes)],
    ["Dictionary", `${s.entries} words · ${s.replacements} replacements`],
  ];
  $("#about-rows").innerHTML = rows
    .map(([k, v]) => `<div class="kv-row"><span class="k">${k}</span><span class="v">${escapeHtml(v)}</span></div>`)
    .join("");

  const drows = [
    ["Dictations", String(s.history_count)],
    ["Audio on disk", fmtBytes(s.audio_bytes)],
    ["Recordings kept", $("#hist-retention").selectedOptions[0]?.textContent || "—"],
  ];
  $("#data-rows").innerHTML = drows
    .map(([k, v]) => `<div class="kv-row"><span class="k">${k}</span><span class="v">${escapeHtml(v)}</span></div>`)
    .join("");

  // Folders, each with a button that opens it. Naming the real path is more
  // useful than describing where data lives.
  const f = s.folders || {};
  const folders = [
    ["Settings", f.config],
    ["Data", f.data],
    ["History", f.history],
    ["Recordings", f.audio],
    ["Logs", f.logs],
    ["Model", f.model],
  ];
  $("#folder-rows").innerHTML = folders
    .filter(([, p]) => p)
    .map(
      ([k, p]) => `<div class="kv-row"><span class="k">${k}</span>
        <span class="v path">${breakablePath(p)}</span>
        <button class="mini" data-open="${escapeHtml(p)}" title="Open ${escapeHtml(k)}">Open</button>
      </div>`
    )
    .join("");
  $$("#folder-rows button[data-open]").forEach((b) =>
    b.addEventListener("click", async () => {
      try {
        await invoke("open_folder", { path: b.dataset.open });
      } catch (e) {
        toast(String(e), true);
      }
    })
  );
}

/* ── events + boot ───────────────────────────────────────────────────── */

// Event names use kebab-case, not dots. Tauri validates the name when the page
// subscribes and rejects a dot outright ("Event name must include only
// alphanumeric characters, `-`, `/`, `:` and `_`"), so `engine.ready` could never
// be listened to. The backend emitted it into the void: the status bar sat on
// "loading" until something else happened to call refresh().
T.event.listen("engine-ready", () => refresh());

/* A take can start or end without this page being touched — the global shortcut,
   the hold chord, the safety ceiling, or a tray action. The backend emits
   `dictation.changed` for exactly that reason, but nothing was listening, so the
   window kept showing stale state until something else refreshed it. */
T.event.listen("dictation-changed", () => refresh());

/* A path that wraps where a reader expects it to.
   `word-break: break-all` split "history" into "h / istory" across lines, which
   reads as a rendering bug. A <wbr> after each slash lets the line break at a
   directory boundary and nowhere else, so the path stays legible however narrow
   the column gets. The text itself is escaped, so the only markup added is <wbr>. */
function breakablePath(p) {
  return escapeHtml(p).replaceAll("/", "/<wbr>");
}

/* ── backup and diagnostics ──────────────────────────────────────────── */

$("#btn-export-settings").addEventListener("click", async () => {
  try {
    const path = await invoke("export_settings");
    $("#backup-note").textContent = `Saved to ${path}`;
    $("#backup-note").classList.add("path");
  } catch (e) {
    toast(String(e), true);
  }
});

$("#btn-import-settings").addEventListener("click", async () => {
  // The path is typed rather than picked: a native file dialog needs an extra
  // plugin and permission for one button, and the export path is shown above so
  // the user already knows what to paste.
  const path = prompt("Path to a Sotto settings backup (.json):");
  if (!path) return;
  try {
    const what = await invoke("import_settings", { path: path.trim() });
    await refresh();
    await loadDictionary();
    renderAbout();
    $("#backup-note").textContent = `Restored ${what}`;
    $("#backup-note").classList.remove("path");
  } catch (e) {
    toast(String(e), true);
  }
});

$("#btn-diagnostics").addEventListener("click", async () => {
  try {
    const path = await invoke("save_diagnostics");
    $("#backup-note").textContent = `Saved to ${path}`;
    $("#backup-note").classList.add("path");
  } catch (e) {
    toast(String(e), true);
  }
});

(async function boot() {
  try {
    await refresh();
    await loadDevices();
    await loadDictionary();
    go("general");
  } catch (e) {
    toast(`Startup problem: ${e}`, true);
    console.error(e);
  }
})();
