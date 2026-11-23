//! Sotto — Tauri application entry point and command surface.
//!
//! Every command returns a typed result. Long operations publish progress through
//! events rather than blocking the webview (BUILD-PLAN §5.3).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// The pipeline lives in the library target so integration tests can drive it
// without opening a window.
#[cfg(feature = "e2e")]
mod e2e;

use sotto::pipeline;
use sotto::shortcuts::{decide, reserved_reason, to_accelerator, EdgeAction};
use pipeline::App;
use serde::{Deserialize, Serialize};
use sotto_core::Settings;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Emitter, Listener, Manager, State};

type AppState = Arc<App>;

#[derive(Debug, Serialize)]
struct Snapshot {
    version: String,
    model: pipeline::ModelStatus,
    settings: sotto_core::config::Settings,
    entries: usize,
    replacements: usize,
    history_count: usize,
    audio_bytes: u64,
    recording: bool,
    sidecar_running: bool,
    /// Words recognised so far in the current take (live overlay only).
    preview_text: String,
    /// Input level 0.0..=1.0 while recording, for the overlay meter.
    level: f32,
    /// Whether the autostart entry on disk actually matches the setting.
    autostart: AutostartView,
    /// Shortcuts that could not be registered, if any. Empty means all good.
    #[serde(default)]
    shortcut_problems: Vec<String>,
    /// Where Sotto keeps its files, shown on About so the user can find them.
    folders: Folders,
}

/// The directories Sotto owns, so About can name them instead of describing them.
#[derive(Debug, Serialize)]
struct Folders {
    config: String,
    data: String,
    history: String,
    audio: String,
    logs: String,
    model: String,
}

#[derive(Debug, Serialize)]
struct AutostartView {
    /// The user's preference.
    enabled: bool,
    /// An entry exists in ~/.config/autostart.
    entry_present: bool,
    /// The entry points at a file that still exists.
    target_exists: bool,
    target: Option<String>,
}

#[tauri::command]
fn get_snapshot(app: State<AppState>) -> Snapshot {
    let settings = app.settings.lock().map(|g| g.clone()).unwrap_or_default();
    let (entries, replacements) = app.dict.lock().map(|g| g.clone()).unwrap_or_default();
    let history_count = app.store.lock().ok().and_then(|s| s.list(1000, None).ok()).map(|v| v.len()).unwrap_or(0);
    let audio_bytes = app.store.lock().ok().and_then(|s| s.audio_bytes().ok()).unwrap_or(0);
    let (recording, level) = match app.recorder.lock() {
        Ok(g) => match g.as_ref() {
            Some(r) => (true, r.tap.level()),
            None => (false, 0.0),
        },
        Err(_) => (false, 0.0),
    };
    let sidecar_running = app.stt.lock().map(|g| g.is_some()).unwrap_or(false);
    let preview_text = app
        .preview_text
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    let st = sotto_core::autostart::status();
    let folders = Folders {
        config: app.config_path("").display().to_string(),
        data: app.root.display().to_string(),
        history: app.store.lock().map(|s| s.root().display().to_string()).unwrap_or_default(),
        audio: app.store.lock().map(|s| s.audio_dir().display().to_string()).unwrap_or_default(),
        logs: sotto_core::paths::log_dir().display().to_string(),
        model: app.model_status().directory,
    };
    Snapshot {
        version: sotto_core::APP_VERSION.to_string(),
        model: app.model_status(),
        autostart: AutostartView {
            enabled: settings.general.autostart,
            entry_present: st.entry_present,
            target_exists: st.target_exists,
            target: st.target.map(|p| p.display().to_string()),
        },
        settings,
        entries: entries.len(),
        replacements: replacements.len(),
        history_count,
        audio_bytes,
        recording,
        sidecar_running,
        preview_text,
        level,
        shortcut_problems: app
            .startup_shortcut_problems
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default(),
        folders,
    }
}

/// Apply the autostart preference to the session, and report what happened.
///
/// Called on every settings save so the desktop entry can never drift out of
/// step with the checkbox. Returns a human-readable problem when the write
/// failed, rather than silently claiming success.
fn apply_autostart(settings: &sotto_core::config::Settings) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find the app binary: {e}"))?;
    sotto_core::autostart::sync(settings.general.autostart, &exe)
        .map(|_| ())
        .map_err(|e| format!("could not update start-at-login: {e}"))
}

#[tauri::command]
fn list_inputs() -> Result<Vec<sotto_core::audio::InputDevice>, String> {
    sotto_core::audio::list_inputs().map_err(|e| e.to_string())
}

#[tauri::command]
fn save_settings(
    app: State<AppState>,
    handle: tauri::AppHandle,
    settings: sotto_core::config::Settings,
) -> Result<Snapshot, String> {
    settings.validate().map_err(|e| e.to_string())?;

    // Both of these touch the user's session (a file on disk, a shortcut grab),
    // so each only runs when the relevant setting actually changed. Running them
    // on every save made dragging a slider feel sluggish.
    let autostart_changed = apply_autostart_if_changed(&app, &settings)?;

    let mut cur = app.settings.lock().map_err(|e| e.to_string())?;
    let base = cur.revision;
    let mut next = settings;
    next.revision = base.wrapping_add(1);

    // Re-registering global shortcuts is not free: it talks to the desktop's
    // shortcut service and briefly tears down every grab. Doing that on each
    // keystroke of a slider caused visible lag, so it only happens when a chord
    // actually changed.
    let chords_changed = cur.dictation.hold_chord != next.dictation.hold_chord
        || cur.dictation.toggle_chord != next.dictation.toggle_chord
        || cur.dictation.cancel_chord != next.dictation.cancel_chord;

    *cur = next.clone();
    let path = app.config_path("settings.json");
    next.save(&path).map_err(|e| e.to_string())?;
    drop(cur);

    let _ = autostart_changed;
    let problems = if chords_changed {
        apply_shortcuts(&handle, &next)
    } else {
        Vec::new()
    };
    for p in &problems {
        eprintln!("shortcut: {p}");
    }
    let mut snap = get_snapshot(app);
    snap.shortcut_problems = problems;
    Ok(snap)
}

/// Apply the autostart preference to the session, but only when it differs from
/// what the running settings already say.
fn apply_autostart_if_changed(
    app: &App,
    next: &sotto_core::config::Settings,
) -> Result<bool, String> {
    let current = app.settings.lock().map(|g| g.general.autostart).unwrap_or(false);
    if current == next.general.autostart {
        return Ok(false);
    }
    apply_autostart(next)?;
    Ok(true)
}

// ── Dictionary ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct DictView {
    entries: Vec<sotto_dict::Entry>,
    replacements: Vec<sotto_dict::Replacement>,
    duplicates: Vec<String>,
}

/// What an import actually did, so the UI can report it rather than guess.
#[derive(Debug, serde::Serialize)]
struct ImportReport {
    added: usize,
    skipped: usize,
    entries: usize,
}

#[tauri::command]
fn get_dictionary(app: State<AppState>) -> DictView {
    let (entries, replacements) = app.dict.lock().map(|g| g.clone()).unwrap_or_default();
    let d = app.current_dictionary();
    DictView {
        entries,
        replacements,
        duplicates: d.duplicate_terms(),
    }
}

#[tauri::command]
fn save_dictionary(
    app: State<AppState>,
    entries: Vec<sotto_dict::Entry>,
    replacements: Vec<sotto_dict::Replacement>,
) -> Result<DictView, String> {
    let probe = app.current_dictionary();
    let mut check = probe.clone();
    check.entries = entries.clone();
    check.replacements = replacements.clone();
    check.validate().map_err(|e| e.to_string())?;

    let path = app.config_path("dictionary.json");
    sotto_core::save_dictionary(&path, &entries, &replacements).map_err(|e| e.to_string())?;
    let mut g = app.dict.lock().map_err(|e| e.to_string())?;
    *g = (entries, replacements);
    drop(g);
    Ok(get_dictionary(app))
}

#[derive(Debug, Deserialize)]
struct TestRequest {
    text: String,
    fuzzy: bool,
    phonetic: bool,
    threshold: f64,
}

#[derive(Debug, Serialize)]
struct TestResponse {
    before: String,
    after: String,
    corrections: Vec<sotto_dict::Correction>,
}

#[tauri::command]
fn test_dictionary(app: State<AppState>, req: TestRequest) -> Result<TestResponse, String> {
    let (entries, replacements) = app.dict.lock().map(|g| g.clone()).unwrap_or_default();
    let dict = sotto_dict::Dictionary {
        entries,
        replacements,
        fuzzy_enabled: req.fuzzy,
        phonetic_enabled: req.phonetic,
        fuzzy_threshold: req.threshold,
    };
    let out = dict.correct(&req.text);
    Ok(TestResponse {
        before: req.text,
        after: out.text,
        corrections: out.corrections,
    })
}

/// Learn a rule from a user edit: propose the diff as an alias.
#[tauri::command]
fn suggest_alias(raw: String, corrected: String) -> Option<(String, String)> {
    // Token-level diff: find the first word that changed and its replacement.
    let r: Vec<&str> = raw.split_whitespace().collect();
    let c: Vec<&str> = corrected.split_whitespace().collect();
    if r.len() != c.len() || r.is_empty() {
        return None;
    }
    for (a, b) in r.iter().zip(c.iter()) {
        if a != b {
            return Some(((*a).to_string(), (*b).to_string()));
        }
    }
    None
}

/// Write the dictionary to a versioned JSON file and return its path.
///
/// The file is the same shape the app reads at startup, so an exported file can
/// also be dropped back in by hand — it is not a bespoke format for one button.
#[tauri::command]
fn export_dictionary(app: State<AppState>) -> Result<String, String> {
    let (entries, replacements) = app.dict.lock().map(|g| g.clone()).map_err(|e| e.to_string())?;
    if entries.is_empty() && replacements.is_empty() {
        return Err("the dictionary is empty, so there is nothing to export".into());
    }

    let dir = std::path::PathBuf::from(&app.root).join("backups");
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let stamp = pipeline::now_rfc3339().replace(':', "-");
    let path = dir.join(format!("sotto-dictionary-{stamp}.json"));
    sotto_core::save_dictionary(&path, &entries, &replacements)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(path.display().to_string())
}

/// Add terms from one-term-per-line text.
///
/// The line format is `correct spelling, other spelling, another`. Blank lines and
/// lines starting with `#` are ignored, so a list can be annotated. A term that is
/// already in the dictionary is skipped rather than duplicated, and the counts are
/// reported so the user can see what happened instead of guessing.
#[tauri::command]
fn import_dictionary_lines(
    app: State<AppState>,
    text: String,
) -> Result<ImportReport, String> {
    let (mut entries, replacements) = app.dict.lock().map(|g| g.clone()).map_err(|e| e.to_string())?;
    let mut added = 0usize;
    let mut skipped = 0usize;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split(',').map(str::trim).filter(|s| !s.is_empty());
        let Some(preferred) = parts.next() else { continue };
        let aliases: Vec<String> = parts.map(str::to_string).collect();
        let aliases = if aliases.is_empty() {
            vec![preferred.to_string()]
        } else {
            aliases
        };

        // A term already present is merged rather than added twice: the same
        // spelling twice would make the winner depend on order.
        if let Some(existing) = entries
            .iter_mut()
            .find(|e| e.preferred.to_lowercase() == preferred.to_lowercase())
        {
            let before = existing.aliases.len();
            for a in aliases {
                if !existing.aliases.iter().any(|x| x.to_lowercase() == a.to_lowercase()) {
                    existing.aliases.push(a);
                }
            }
            if existing.aliases.len() == before {
                skipped += 1;
            } else {
                added += 1;
            }
            continue;
        }

        entries.push(sotto_dict::Entry {
            id: format!(
                "{}-{}",
                preferred.to_lowercase().replace(|c: char| !c.is_alphanumeric(), "-"),
                added + skipped
            ),
            preferred: preferred.to_string(),
            aliases,
            language: None,
            case_policy: sotto_dict::CasePolicy::Exact,
            enabled: true,
        });
        added += 1;
    }

    if added == 0 {
        return Ok(ImportReport {
            added: 0,
            skipped,
            entries: entries.len(),
        });
    }

    // Validate before writing, so a bad line cannot leave the file half-updated.
    let mut check = app.current_dictionary();
    check.entries = entries.clone();
    check.validate().map_err(|e| e.to_string())?;

    let path = app.config_path("dictionary.json");
    sotto_core::save_dictionary(&path, &entries, &replacements).map_err(|e| e.to_string())?;
    let n = entries.len();
    if let Ok(mut g) = app.dict.lock() {
        *g = (entries, replacements);
    }
    Ok(ImportReport { added, skipped, entries: n })
}

// ── History ─────────────────────────────────────────────────────────────────

#[tauri::command]
fn list_history(
    app: State<AppState>,
    limit: Option<u32>,
    before: Option<(String, String)>,
) -> Result<Vec<sotto_core::HistoryEntry>, String> {
    let b = before.as_ref().map(|(a, c)| (a.as_str(), c.as_str()));
    app.store
        .lock()
        .map_err(|e| e.to_string())?
        .list(limit.unwrap_or(100), b)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_history(app: State<AppState>, ids: Vec<String>, audio: bool) -> Result<usize, String> {
    app.store.lock().map_err(|e| e.to_string())?.delete(&ids, audio).map_err(|e| e.to_string())
}

/// Delete every recording on disk but keep the transcripts.
///
/// This is what History's "Delete all audio" button means. The page says
/// "Transcripts are kept until you delete them. This setting only controls the
/// audio", so deleting the takes as well would break that promise — and the
/// button is not the one that deletes history.
#[tauri::command]
fn delete_all_audio(app: State<AppState>) -> Result<usize, String> {
    app.store
        .lock()
        .map_err(|e| e.to_string())?
        .delete_all_audio()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_favourite(app: State<AppState>, id: String, favourite: bool) -> Result<(), String> {
    app.store
        .lock()
        .map_err(|e| e.to_string())?
        .set_favourite(&id, favourite)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn edit_attempt(app: State<AppState>, attempt_id: String, text: String) -> Result<(), String> {
    app.store.lock().map_err(|e| e.to_string())?.set_corrected(&attempt_id, &text).map_err(|e| e.to_string())
}

#[tauri::command]
fn copy_text(text: String) -> Result<String, String> {
    sotto_core::delivery::copy(&text).map_err(|e| e.to_string())
}

/// Re-transcribe a retained recording, appending a new attempt.
///
/// Retry never overwrites: the previous attempt stays in the database, so the
/// History page can still show what the first run produced (BUILD-PLAN A24).
/// The result is **not** delivered anywhere — a retry must not paste into
/// whatever window happens to be focused. The user copies it deliberately.
#[tauri::command]
fn retry_transcription(
    app: State<AppState>,
    id: String,
) -> Result<pipeline::PipelineResult, String> {
    let take = app
        .store
        .lock()
        .map_err(|e| e.to_string())?
        .get_take(&id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no such recording".to_string())?;

    let wav = take
        .audio_path
        .filter(|p| p.is_file())
        .ok_or_else(|| "this recording is no longer on disk, so it cannot be transcribed again".to_string())?;

    // Delivery is suppressed for the duration: a retry produces text for the
    // History page, not a keystroke in someone's editor.
    let previous = app
        .settings
        .lock()
        .map(|g| g.dictation.auto_insert)
        .unwrap_or(true);
    if let Ok(mut g) = app.settings.lock() {
        g.dictation.auto_insert = false;
    }
    let out = app.finish(&id, &wav, take.duration_ms);
    if let Ok(mut g) = app.settings.lock() {
        g.dictation.auto_insert = previous;
    }
    out
}

/// Write every transcript to a plain text file and return its path.
///
/// Newest first, one block per dictation, with the date, the corrected text and —
/// when the dictionary changed something — what the model originally heard. Plain
/// text so the file is useful in any editor and needs no importer.
#[tauri::command]
fn export_history(app: State<AppState>) -> Result<String, String> {
    let entries = app
        .store
        .lock()
        .map_err(|e| e.to_string())?
        .list(10_000, None)
        .map_err(|e| e.to_string())?;

    if entries.is_empty() {
        return Err("there is nothing to export yet".into());
    }

    let mut out = String::new();
    out.push_str("Sotto transcripts\n");
    out.push_str(&format!("Exported {}\n\n", pipeline::now_rfc3339()));
    for e in &entries {
        let a = match e.attempt.as_ref() {
            Some(a) => a,
            None => continue,
        };
        let text = a.corrected_text.as_deref().unwrap_or(&a.raw_text);
        out.push_str(&format!("{}  ({} ms)\n", e.take.created_at, e.take.duration_ms));
        out.push_str(text);
        out.push('\n');
        if a.corrected_text.is_some() && a.corrected_text.as_deref() != Some(a.raw_text.as_str()) {
            out.push_str(&format!("Model heard: {}\n", a.raw_text));
        }
        out.push('\n');
    }

    let dir = std::path::PathBuf::from(&app.root).join("exports");
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let stamp = pipeline::now_rfc3339().replace(':', "-");
    let path = dir.join(format!("sotto-transcripts-{stamp}.txt"));
    std::fs::write(&path, out).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(path.display().to_string())
}

// ── Playback ────────────────────────────────────────────────────────────────

/// Play a retained recording.
///
/// The id is resolved through the database, so the page can never name a file of
/// its own choosing (BUILD-PLAN §15.2). Playback runs natively rather than in the
/// webview, because WebKitGTK refuses to decode audio on this stack — see
/// `sotto_core::playback` for the evidence.
#[tauri::command]
fn play_recording(app: State<AppState>, id: String) -> Result<(), String> {
    let path = app
        .store
        .lock()
        .map_err(|e| e.to_string())?
        .get_take(&id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no such recording".to_string())?
        .audio_path
        .ok_or_else(|| "this recording is no longer on disk".to_string())?;

    if !path.is_file() {
        return Err("this recording is no longer on disk".into());
    }
    sotto::playback_host::play(path).map_err(|e| e.to_string())
}

#[tauri::command]
fn stop_playback() -> Result<(), String> {
    sotto::playback_host::stop();
    Ok(())
}

#[tauri::command]
fn playback_status() -> sotto::playback_host::Status {
    sotto::playback_host::status()
}

// ── Backup and diagnostics ──────────────────────────────────────────────────

/// Write a settings backup and return where it went.
///
/// The destination is chosen by the app rather than a file dialog: a dialog adds
/// a permission surface and a dependency for one button, and a predictable path
/// the UI can name is easier to explain. The file is plain JSON the user can read
/// and edit.
#[tauri::command]
fn export_settings(app: State<AppState>) -> Result<String, String> {
    #[derive(serde::Serialize)]
    struct Backup<'a> {
        app: &'a str,
        version: &'a str,
        exported_at: String,
        settings: &'a sotto_core::config::Settings,
        dictionary: DictionaryExport<'a>,
    }
    #[derive(serde::Serialize)]
    struct DictionaryExport<'a> {
        entries: &'a [sotto_dict::Entry],
        replacements: &'a [sotto_dict::Replacement],
    }

    let settings = app.settings.lock().map(|g| g.clone()).map_err(|e| e.to_string())?;
    let (entries, replacements) = app.dict.lock().map(|g| g.clone()).map_err(|e| e.to_string())?;

    let backup = Backup {
        app: "sotto",
        version: sotto_core::APP_VERSION,
        exported_at: pipeline::now_rfc3339(),
        settings: &settings,
        dictionary: DictionaryExport {
            entries: &entries,
            replacements: &replacements,
        },
    };

    let dir = std::path::PathBuf::from(&app.root).join("backups");
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let stamp = pipeline::now_rfc3339().replace(':', "-");
    let path = dir.join(format!("sotto-settings-{stamp}.json"));
    let body = serde_json::to_vec_pretty(&backup).map_err(|e| e.to_string())?;
    std::fs::write(&path, body).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(path.display().to_string())
}

/// Read a settings backup back in. Returns what was restored.
#[tauri::command]
fn import_settings(app: State<AppState>, path: String) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct Backup {
        #[serde(default)]
        app: String,
        settings: sotto_core::config::Settings,
        #[serde(default)]
        dictionary: Option<DictionaryExport>,
    }
    #[derive(serde::Deserialize)]
    struct DictionaryExport {
        #[serde(default)]
        entries: Vec<sotto_dict::Entry>,
        #[serde(default)]
        replacements: Vec<sotto_dict::Replacement>,
    }

    let bytes = std::fs::read(&path).map_err(|e| format!("could not read {path}: {e}"))?;
    let backup: Backup =
        serde_json::from_slice(&bytes).map_err(|e| format!("that file is not a Sotto backup: {e}"))?;

    // A JSON file of some other shape is refused rather than half-applied.
    if !backup.app.is_empty() && backup.app != "sotto" {
        return Err(format!("that backup belongs to “{}”, not Sotto", backup.app));
    }
    backup.settings.validate().map_err(|e| e.to_string())?;

    let path_settings = app.config_path("settings.json");
    let mut settings = backup.settings;
    settings.revision = app.settings.lock().map(|g| g.revision).unwrap_or(0).wrapping_add(1);
    settings.save(&path_settings).map_err(|e| e.to_string())?;
    if let Ok(mut g) = app.settings.lock() {
        *g = settings.clone();
    }

    let mut restored = String::from("settings");
    if let Some(d) = backup.dictionary {
        let dict_path = app.config_path("dictionary.json");
        sotto_core::save_dictionary(&dict_path, &d.entries, &d.replacements)
            .map_err(|e| e.to_string())?;
        if let Ok(mut g) = app.dict.lock() {
            *g = (d.entries, d.replacements);
        }
        restored = "settings and dictionary".into();
    }
    Ok(restored)
}

/// Write a diagnostics bundle and return its path.
///
/// The report itself is built by [`pipeline::diagnostics_report`], which is
/// tested for the properties that matter: no transcript text, no dictionary
/// contents, and no absolute paths (acceptance A17). This function only writes it.
#[tauri::command]
fn save_diagnostics(app: State<AppState>) -> Result<String, String> {
    let report = pipeline::diagnostics_report(app.inner());
    let dir = std::path::PathBuf::from(&app.root).join("diagnostics");
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let stamp = pipeline::now_rfc3339().replace(':', "-");
    let path = dir.join(format!("sotto-diagnostics-{stamp}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(path.display().to_string())
}

/// Open a folder in the desktop's file manager.
#[tauri::command]
fn open_folder(path: String) -> Result<(), String> {
    let p = std::path::PathBuf::from(&path);
    if !p.is_dir() {
        return Err(format!("{path} is not a folder"));
    }
    std::process::Command::new("xdg-open")
        .arg(&p)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open {path}: {e}"))
}

// ── Dictation ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct StartResponse {
    take_id: String,
}

#[tauri::command]
fn start_dictation(app: State<AppState>, handle: tauri::AppHandle) -> Result<StartResponse, String> {
    let settings = app.settings.lock().map(|g| g.clone()).unwrap_or_default();

    // The take itself is opened by the pipeline, so the same code path a test
    // drives is the one the button drives.
    let take_id = app.begin_take()?;

    // Live preview needs the sidecar; start it before the worker so the first
    // partial has an engine to talk to. Failure here must not block dictation.
    if settings.general.overlay_style == sotto_core::config::OverlayStyle::Live {
        if app.ensure_stt().is_ok() {
            if let Some(rec) = app.recorder.lock().ok().and_then(|g| g.as_ref().map(|r| r.tap.clone())) {
                app.start_preview(rec, settings.general.overlay_style);
            }
        }
    }

    show_overlay(app.inner(), &handle, &settings, None);

    // Mute other audio while recording, if asked. Best-effort: if it fails the
    // dictation carries on, because a mute is a convenience and a recording is
    // the point.
    if settings.audio.mute_output {
        if let Some(m) = sotto_core::mute::mute() {
            if let Ok(mut g) = app.muted.lock() {
                *g = Some(m);
            }
        }
    }

    // Start cue: a short rising tone, so the user knows capture is live without
    // looking at the screen. Best-effort — a missing output device must never
    // stop a dictation.
    if settings.audio.cues {
        play_cue(settings.audio.gain, true);
    }

    // Safety ceiling (BUILD-PLAN §9): a lost release edge must not leave an
    // unlimited recording. The watchdog stops **this** take if it outlives the
    // ceiling, and never a later one.
    {
        let arc = app.inner().clone();
        let h = handle.clone();
        arc.watch_take(take_id.clone(), move || {
            let _ = stop_dictation(h.state::<AppState>(), h.clone());
        });
    }

    // Tell the window, whoever asked. The shortcut handler used to be the only
    // caller that emitted this, which meant any other caller — a future tray
    // action, an IPC client, a test — left the page showing "Start dictation"
    // while a take was running. Emitting from the command itself keeps the window
    // correct for every caller instead of for one.
    let _ = handle.emit("dictation-changed", ());

    Ok(StartResponse { take_id })
}

/// Play the start or stop cue, if the machine has an output device.
///
/// Runs on its own thread so a slow or absent audio device cannot delay capture
/// or delivery. The gain setting scales the cue too, so a quiet microphone setup
/// does not come with a startling beep.
fn play_cue(gain: f32, start: bool) {
    std::thread::spawn(move || {
        let amp = (0.14 * gain.clamp(0.5, 3.0)).clamp(0.05, 0.35);
        // Rising for start, falling for stop: the direction alone says which.
        let (hz, ms) = if start { (660.0, 80) } else { (440.0, 90) };
        let samples = sotto_core::playback::tone(hz, ms, amp, 16_000);
        let _ = sotto_core::playback::Player::play_mono(samples, 16_000);
    });
}

/// Put the output mute back the way it was before recording.
///
/// Safe to call at any time and idempotent: it takes the stored mute, so a second
/// call finds nothing to restore. Called from every path that ends a recording —
/// stop, cancel, and an error during transcription — so a muted sink can never
/// outlive the take that muted it.
fn restore_mute(app: &App) {
    let m = app.muted.lock().ok().and_then(|mut g| g.take());
    if let Some(m) = m {
        m.restore();
    }
}

#[tauri::command]
fn stop_dictation(app: State<AppState>, handle: tauri::AppHandle) -> Result<pipeline::PipelineResult, String> {
    hide_overlay(&handle);

    // Stop cue and unmute happen before transcription, not after: the user
    // pressed stop, so the acknowledgement and their sound should come back now
    // rather than in a second's time.
    if let Ok(s) = app.settings.lock() {
        if s.audio.cues {
            play_cue(s.audio.gain, false);
        }
    }
    restore_mute(app.inner());

    let result = app.end_take();
    let _ = handle.emit("dictation-changed", ());
    result
}

fn show_overlay(
    app: &Arc<App>,
    handle: &tauri::AppHandle,
    settings: &sotto_core::config::Settings,
    preview: Option<sotto::overlay::Source>,
) {
    use sotto_core::config::OverlayStyle;
    if !settings.general.show_overlay || settings.general.overlay_style == OverlayStyle::None {
        hide_overlay(handle);
        return;
    }

    let style = settings.general.overlay_style;
    let palette = settings.general.palette.clone();
    let placement = sotto::overlay::Placement::from_config(settings.general.overlay_position);

    // The preview supplies its own synthetic sweep; a real recording reads the
    // live capture through the app, so the meter always reflects what the
    // microphone is hearing right now.
    let source = preview.unwrap_or_else(|| {
        let app = app.clone();
        let level_app = app.clone();
        sotto::overlay::Source::live(
            Arc::new(move || {
                level_app
                    .recorder
                    .lock()
                    .ok()
                    .and_then(|g| g.as_ref().map(|r| r.tap.level()))
                    .unwrap_or(0.0)
            }),
            Arc::new(move || {
                app.preview_text
                    .lock()
                    .map(|g| g.clone())
                    .unwrap_or_default()
            }),
        )
    });

    let _ = handle.run_on_main_thread(move || {
        sotto::overlay::show(style, palette.as_deref(), placement, source);
    });
}

/// Hide the overlay, if it is showing.
fn hide_overlay(handle: &tauri::AppHandle) {
    let _ = handle.run_on_main_thread(sotto::overlay::hide);
}

/// Show the overlay for a few seconds so the user can see the real thing.
///
/// The settings page shows miniature drawings; this is the actual window, at the
/// actual size, in the actual position. It uses a synthetic level sweep so it
/// can be demonstrated without opening the microphone, and it never starts a
/// recording.
#[tauri::command]
fn preview_overlay(
    app: State<AppState>,
    handle: tauri::AppHandle,
    style: String,
) -> Result<(), String> {
    use sotto_core::config::OverlayStyle;
    let style = match style.as_str() {
        "live" => OverlayStyle::Live,
        "minimal" => OverlayStyle::Minimal,
        other => return Err(format!("unknown overlay style: {other}")),
    };
    let mut settings = app.settings.lock().map(|g| g.clone()).unwrap_or_default();
    settings.general.overlay_style = style;
    settings.general.show_overlay = true;
    let source = sotto::overlay::Source::synthetic();
    show_overlay(&app.inner().clone(), &handle, &settings, Some(source));

    // Hide it again on our own, so a preview can never be left on screen.
    let h = handle.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(3_600));
        hide_overlay(&h);
    });
    Ok(())
}

#[tauri::command]
fn cancel_dictation(app: State<AppState>, handle: tauri::AppHandle) -> Result<(), String> {
    hide_overlay(&handle);
    restore_mute(app.inner());
    let result = app.abort_take();
    let _ = handle.emit("dictation-changed", ());
    result
}

fn main() {
    let root = std::env::var_os("SOTTO_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    dirs_home().join(".local").join("share")
                })
                .join("sotto")
        });
    let model_dir = std::env::var_os("SOTTO_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // An explicit directory in settings wins, so a user who keeps the
            // model elsewhere is honoured. It is only a fallback chain: the
            // environment, then settings, then the bundled default.
            let from_settings = Settings::load_or_default(
                &root.join("config").join("settings.json"),
            )
            .ok()
            .and_then(|s| s.model.directory);
            from_settings.unwrap_or_else(|| root.join("models").join("parakeet-redux"))
        });

    let app = Arc::new(App::new(&root, model_dir));

    tauri::Builder::default()
        // One instance only. A second copy would register the same global chords,
        // lose the race, and appear broken: the hotkey would silently do nothing
        // because the first instance owns it. Instead the running window is
        // raised and the new process exits.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let _ = show_window(app.clone());
        }))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(app.clone())
        .setup(move |handle| {
            // The model ships inside the package. Resolve it from the resource
            // directory so the user never downloads anything.
            if let Ok(res) = handle.path().resource_dir() {
                let bundled = res.join("model");
                if bundled.join("model.safetensors").is_file() {
                    app.set_model_dir(bundled);
                }
                let sidecar = res.join("sotto-stt");
                if sidecar.is_file() {
                    std::env::set_var("SOTTO_STT_BIN", &sidecar);
                }
            }
            // Pre-warm the engine in the background so the first dictation is fast.
            let a = app.clone();
            let h = handle.handle().clone();
            std::thread::spawn(move || {
                if let Err(e) = a.ensure_stt() {
                    eprintln!("warm-up skipped: {e}");
                }
                let _ = h.emit("engine-ready", a.model_status());
            });

            // Idle unload. A tick every 30 s is cheap and makes the setting feel
            // immediate without waking the process constantly. The engine is only
            // dropped after the user's configured idle time, and never during a
            // recording.
            let idle_app = app.clone();
            std::thread::Builder::new()
                .name("sotto-idle".into())
                .spawn(move || loop {
                    // Wake when the deadline actually arrives, not on a fixed
                    // tick. A flat 30 s sleep made "unload after 1 minute" take
                    // up to two, so the number in the settings was not the truth.
                    // A short cap keeps the thread responsive to a changed
                    // setting and to a take starting.
                    let wait = match idle_app.idle_deadline_in() {
                        Some(remaining) => remaining.min(std::time::Duration::from_secs(5)),
                        // Nothing to unload: check back occasionally rather than
                        // spinning, in case a setting changes.
                        None => std::time::Duration::from_secs(15),
                    };
                    std::thread::sleep(wait);
                    if idle_app.unload_if_idle() {
                        eprintln!("engine unloaded after idling");
                    }
                })
                .ok();

            build_tray(&handle.handle().clone())?;

            // Keep the tray menu's verb in step with the take. The event is
            // emitted by start/stop/cancel themselves, so the tray, the window,
            // a shortcut and the watchdog all update the same way.
            let tray_handle = handle.handle().clone();
            handle.listen("dictation-changed", move |_| refresh_tray(&tray_handle));

            // Test-only: evaluate a script from a file once the window is up.
            // Compiled out unless the `e2e` feature is on (see `src/e2e.rs`).
            #[cfg(feature = "e2e")]
            e2e::install(handle.handle());
            // Give the main window its icon explicitly. The tray icon and the
            // window icon are separate in GTK, so setting one does not set the
            // other; without this the window and the taskbar entry show a
            // generic placeholder even though the tray is correct.
            if let Some(win) = handle.get_webview_window("main") {
                if let Ok(img) =
                    tauri::image::Image::from_bytes(include_bytes!("../icons/512x512.png"))
                {
                    let _ = win.set_icon(img);
                }
            }

            // Register the user's shortcuts now, so they work before the window
            // is ever opened.
            let handle_ref = handle.handle().clone();
            let s = app.settings.lock().map(|g| g.clone()).unwrap_or_default();
            let problems = apply_shortcuts(&handle_ref, &s);
            for p in &problems {
                eprintln!("shortcut: {p}");
            }
            // Keep them so the Keybindings page can say why a chord does nothing.
            if let Ok(mut g) = app.startup_shortcut_problems.lock() {
                *g = problems;
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let app = window.state::<AppState>();
                let action = app
                    .settings
                    .lock()
                    .map(|g| g.general.close_action)
                    .unwrap_or(sotto_core::config::CloseAction::Tray);
                match action {
                    // Hide rather than destroy: the tray icon keeps the app
                    // reachable, and dictation keeps working while the window is
                    // out of the way.
                    sotto_core::config::CloseAction::Tray => {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                    sotto_core::config::CloseAction::Quit => {
                        // Let the default close path run; exit is handled in
                        // RunEvent::Exit below so the sidecar is always reaped.
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            list_inputs,
            save_settings,
            get_dictionary,
            save_dictionary,
            test_dictionary,
            export_dictionary,
            import_dictionary_lines,
            suggest_alias,
            list_history,
            delete_history,
            delete_all_audio,
            set_favourite,
            edit_attempt,
            copy_text,
            #[cfg(feature = "e2e")]
            e2e::probe_write,
            retry_transcription,
            export_history,
            play_recording,
            stop_playback,
            playback_status,
            export_settings,
            import_settings,
            save_diagnostics,
            open_folder,
            start_dictation,
            stop_dictation,
            cancel_dictation,
            show_window,
            quit_app,
            preview_overlay,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Sotto")
        .run(|handle, event| {
            if let tauri::RunEvent::Exit = event {
                // Reap the sidecar and any in-progress take on every exit path,
                // including closing the window when the app is set to quit.
                if let Some(app) = handle.try_state::<AppState>() {
                    restore_mute(&app);
                    app.discard_take_on_exit();
                }
            }
        });
}

/// Register (or re-register) every configured shortcut with the desktop.
///
/// Called at startup and after each settings save. Registration can fail when
/// another application already owns a chord; that is reported to the caller so
/// the UI can say so instead of leaving the user with a shortcut that silently
/// does nothing.
fn apply_shortcuts(app: &tauri::AppHandle, settings: &sotto_core::config::Settings) -> Vec<String> {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};

    // Log the outcome of every binding. "My shortcut does nothing" is the hardest
    // support question to answer from the outside, and the answer is usually
    // visible here: either the desktop refused the chord because another
    // application owns it, or it was never set. Both are now in the log as well
    // as in the window.

    let gs = app.global_shortcut();
    // Start from a clean slate so a removed binding stops firing.
    let _ = gs.unregister_all();

    let mut problems = Vec::new();
    let wanted = [
        ("hold", settings.dictation.hold_chord.as_deref()),
        ("toggle", settings.dictation.toggle_chord.as_deref()),
        ("cancel", settings.dictation.cancel_chord.as_deref()),
    ];
    for (action, chord) in wanted {
        let Some(chord) = chord else {
            eprintln!("shortcut {action}: not set");
            continue;
        };
        // Checked before translation so the user gets the actual reason instead
        // of a generic "not usable": a kernel-owned chord is a different problem
        // from a typo, and telling them which one it is is the difference
        // between a fixable mistake and a mystery.
        if let Some(reason) = reserved_reason(chord) {
            problems.push(format!("{chord} is reserved: {reason}"));
            eprintln!("shortcut {action}: {chord} refused (reserved: {reason})");
            continue;
        }
        let Some(accel) = to_accelerator(chord) else {
            problems.push(format!("{chord} is not a usable shortcut"));
            continue;
        };
        let Ok(shortcut) = accel.parse::<Shortcut>() else {
            problems.push(format!("{chord} is not a shortcut this system understands"));
            continue;
        };
        let a = app.clone();
        let act = action.to_string();
        if let Err(e) = gs.on_shortcut(shortcut, move |_h, _s, event| {
            // A hold binding needs **both** edges: press starts the take and
            // release ends it. Acting on the press alone would make "hold" a
            // toggle wearing a different name, which is what BUILD-PLAN §9
            // forbids. Toggle and cancel act on the press only.
            let pressed = event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed;
            let app = a.clone();
            let act = act.clone();
            std::thread::spawn(move || {
                let core = app.state::<AppState>().inner().clone();
                let recording = core.recorder.lock().map(|g| g.is_some()).unwrap_or(false);
                let hold_active = core
                    .hold_active
                    .load(std::sync::atomic::Ordering::SeqCst);
                let handle = app.clone();
                // The decision table lives in `shortcuts` so it can be tested
                // without a compositor; the handler only carries it out.
                let r = match decide(&act, pressed, recording, hold_active) {
                    EdgeAction::Start => {
                        core.hold_active
                            .store(act == "hold", std::sync::atomic::Ordering::SeqCst);
                        let r = start_dictation(app.state::<AppState>(), handle).map(|_| ());
                        if r.is_err() {
                            core.hold_active
                                .store(false, std::sync::atomic::Ordering::SeqCst);
                        }
                        r
                    }
                    EdgeAction::Stop => {
                        core.hold_active
                            .store(false, std::sync::atomic::Ordering::SeqCst);
                        stop_dictation(app.state::<AppState>(), handle).map(|_| ())
                    }
                    EdgeAction::Cancel => {
                        core.hold_active
                            .store(false, std::sync::atomic::Ordering::SeqCst);
                        cancel_dictation(app.state::<AppState>(), handle)
                    }
                    EdgeAction::Ignore => Ok(()),
                };
                if let Err(e) = r {
                    eprintln!("shortcut {act}: {e}");
                }
                // No emit here: start/stop/cancel announce for themselves, so the
                // event fires for every caller rather than only this one.
            });
        }) {
            problems.push(format!("{chord} could not be registered: {e}"));
        } else {
            eprintln!("shortcut {action}: {chord} registered");
        }
    }
    problems
}
#[tauri::command]
fn show_window(app: tauri::AppHandle) -> Result<(), String> {
    let w = app
        .get_webview_window("main")
        .ok_or_else(|| "the settings window is not available".to_string())?;
    let _ = w.unminimize();
    w.show().map_err(|e| e.to_string())?;
    w.set_focus().map_err(|e| e.to_string())?;
    Ok(())
}

/// Quit for real, including from the tray when the window is hidden.
#[tauri::command]
fn quit_app(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(state) = app.try_state::<AppState>() {
        // Quitting is not a dictation: drop the in-progress take instead of
        // leaving a zero-length "failed" row for the next launch to explain.
        restore_mute(&state);
        state.discard_take_on_exit();
    }
    app.exit(0);
    Ok(())
}

/// Build the tray menu for the current dictation state.
///
/// The dictation entry names what clicking it will do. A static "Start dictation"
/// label was wrong half the time: while recording, the entry stops the take, and
/// a menu that says Start when it means Stop is a trap.
fn tray_menu(
    handle: &tauri::AppHandle,
    recording: bool,
) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};

    let open = MenuItem::with_id(handle, "open", "Open Sotto", true, None::<&str>)?;
    let dictate = MenuItem::with_id(
        handle,
        "dictate",
        if recording { "Stop and transcribe" } else { "Start dictation" },
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(handle, "quit", "Quit Sotto", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(handle)?;
    Menu::with_items(handle, &[&open, &dictate, &sep, &quit])
}

/// The tray icon and its menu.
///
/// Every entry maps to something the user can verify: opening the window, toggling
/// dictation, or quitting.
///
/// Linux tray icons are AppIndicators, and an AppIndicator has no primary-click
/// action. Tauri therefore does not deliver `TrayIconEvent` on this platform.
/// The first version disabled the menu on left click and opened the window from a
/// click handler — so on KDE a left click did nothing at all. The menu now opens on
/// left click, and "Open Sotto" is its first entry.
fn build_tray(handle: &tauri::AppHandle) -> tauri::Result<()> {
    use tauri::tray::TrayIconBuilder;

    let menu = tray_menu(handle, false)?;
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/128x128.png"))?;

    TrayIconBuilder::with_id("sotto-tray")
        .icon(icon)
        .icon_as_template(false)
        .tooltip("Sotto — press your shortcut to dictate")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => {
                let _ = show_window(app.clone());
            }
            "dictate" => {
                // Run the same path the button uses, off the menu's thread.
                let app = app.clone();
                std::thread::spawn(move || {
                    let state = app.state::<AppState>();
                    let handle = app.clone();
                    let recording = state.recorder.lock().map(|g| g.is_some()).unwrap_or(false);
                    let r = if recording {
                        stop_dictation(state, handle).map(|_| ())
                    } else {
                        start_dictation(state, handle).map(|_| ())
                    };
                    if let Err(e) = r {
                        eprintln!("tray dictation: {e}");
                    }
                    // No emit here either; the commands announce for themselves.
                });
            }
            "quit" => {
                let _ = quit_app(app.clone());
            }
            _ => {}
        })
        .build(handle)?;
    Ok(())
}

/// Redraw the tray menu after a take starts or ends.
///
/// Called from the `dictation-changed` listener, so every caller — window button,
/// shortcut, tray itself, watchdog — leaves the menu showing the right verb.
fn refresh_tray(handle: &tauri::AppHandle) {
    let Some(tray) = handle.tray_by_id("sotto-tray") else {
        return;
    };
    let recording = handle
        .try_state::<AppState>()
        .and_then(|s| s.recorder.lock().map(|g| g.is_some()).ok())
        .unwrap_or(false);
    if let Ok(menu) = tray_menu(handle, recording) {
        let _ = tray.set_menu(Some(menu));
    }
    let _ = tray.set_tooltip(Some(if recording {
        "Sotto — recording"
    } else {
        "Sotto — press your shortcut to dictate"
    }));
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}
