//! Integration tests for the core: config, storage, audio, dictionary wiring.

use sotto_core::config::{Retention, Settings, Theme};
use sotto_core::store::{Attempt, Store, TakeStatus};
use std::path::PathBuf;

fn tmpdir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "sotto-test-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

// ── config ──────────────────────────────────────────────────────────────────

#[test]
fn config_defaults_are_sane() {
    let s = Settings::default();
    assert_eq!(s.revision, 0);
    // D-2 is answered (2026-11-23): blue is the default and is shown as selected.
    // This used to assert `is_none()`, because choosing for the user was forbidden
    // while the question was open. It is now answered, so the assertion states the
    // answer instead of the absence of one.
    assert_eq!(
        s.general.palette.as_deref(),
        Some("blue"),
        "D-2: a fresh install starts on blue"
    );
    assert_eq!(s.general.theme, Theme::System);
    assert!(!s.dictionary.fuzzy_enabled, "tier 3 off by default");
    assert!(!s.dictionary.phonetic_enabled, "tier 4 off by default");
    assert_eq!(s.history.retention, Retention::ThreeDays);
    s.validate().expect("defaults must validate");
}

#[test]
fn a_fresh_install_starts_on_blue_and_says_so() {
    // D-2 is answered: blue is the default, and the picker marks it. Before this
    // the field was `None` while the stylesheet fell back to blue, so the app
    // looked blue with no swatch lit — the picker and the app disagreed.
    let s = Settings::default();
    assert_eq!(
        s.general.palette.as_deref(),
        Some(sotto_core::config::DEFAULT_PALETTE),
        "a fresh install should start on the documented default accent"
    );
    assert_eq!(sotto_core::config::DEFAULT_PALETTE, "blue");

    // It must survive a save/load cycle, or it would only be right on a machine
    // that never saved its settings.
    let dir = tmpdir("default-palette");
    s.save(&dir.join("settings.json")).unwrap();
    let back = Settings::load_or_default(&dir.join("settings.json")).unwrap();
    assert_eq!(back.general.palette.as_deref(), Some("blue"));

    // An older file with no palette must still load, not fail: an upgrade cannot
    // be allowed to break because a field was added. And it must resolve to blue,
    // not to "nothing selected while the app draws blue" — the inconsistency this
    // decision removed.
    std::fs::write(
        dir.join("old.json"),
        r#"{"revision":1,"general":{"theme":"system"}}"#,
    )
    .unwrap();
    let old = Settings::load_or_default(&dir.join("old.json")).unwrap();
    assert_eq!(
        old.general.palette.as_deref(),
        Some("blue"),
        "an upgraded install must not land in the no-swatch-but-blue state"
    );
}

#[test]
fn a_fresh_install_can_actually_dictate() {
    // The app shipped with no toggle chord, so the only way to dictate was to
    // click a button in the window — which types into Sotto itself, not into the
    // app the user is working in. A dictation app that does nothing when you
    // press a key is not usable, so both bindings ship set.
    let s = Settings::default();
    // Hold is the primary gesture: press to talk, release to stop.
    assert_eq!(
        s.dictation.hold_chord.as_deref(),
        Some("Ctrl+Space"),
        "a fresh install must have a working hold-to-talk shortcut"
    );
    // Toggle is the same chord with Shift, so pressing the hold chord can never
    // also toggle and the two bindings cannot race on one keypress.
    assert_eq!(
        s.dictation.toggle_chord.as_deref(),
        Some("Ctrl+Shift+Space"),
        "a fresh install must have a working toggle shortcut"
    );
    assert_ne!(
        s.dictation.hold_chord, s.dictation.toggle_chord,
        "the two defaults must be different chords"
    );
    assert_eq!(
        s.dictation.cancel_chord.as_deref(),
        Some("Escape"),
        "cancelling must always be reachable"
    );

    // And the defaults must survive the round trip through disk, or they would
    // only work on a machine that never saved its settings.
    let dir = tmpdir("fresh-install");
    s.save(&dir.join("settings.json")).unwrap();
    let back = Settings::load_or_default(&dir.join("settings.json")).unwrap();
    assert_eq!(back.dictation.hold_chord.as_deref(), Some("Ctrl+Space"));
    assert_eq!(
        back.dictation.toggle_chord.as_deref(),
        Some("Ctrl+Shift+Space")
    );
}

#[test]
fn config_roundtrip_and_atomic_save() {
    let dir = tmpdir("cfg-rt");
    let path = dir.join("settings.json");
    let mut s = Settings::default();
    s.general.palette = Some("blue".into());
    s.dictionary.fuzzy_enabled = true;
    s.save(&path).unwrap();

    let loaded = Settings::load_or_default(&path).unwrap();
    assert_eq!(loaded.general.palette.as_deref(), Some("blue"));
    assert!(loaded.dictionary.fuzzy_enabled);

    // No temp file left behind.
    assert!(!path.with_extension("json.tmp").exists());
}

#[test]
fn config_revision_conflict_is_rejected() {
    let mut s = Settings::default();
    let base = s.revision;
    let mut patch = Settings::default();
    patch.general.palette = Some("green".into());
    s.apply(base, patch).unwrap();
    assert_eq!(s.revision, 1);

    // A stale webview tries again with the old revision.
    let stale = Settings::default();
    let err = s.apply(base, stale).unwrap_err();
    assert!(matches!(
        err,
        sotto_core::config::ConfigError::RevisionConflict { .. }
    ));
}

#[test]
fn config_rejects_invalid_values() {
    let mut s = Settings::default();
    s.audio.gain = 99.0;
    assert!(s.validate().is_err());

    let mut s = Settings::default();
    s.dictionary.fuzzy_threshold = 2.0;
    assert!(s.validate().is_err());

    let mut s = Settings::default();
    s.history.max_entries = 0;
    assert!(s.validate().is_err());

    let mut s = Settings::default();
    s.dictation.hold_chord = Some("Super+D".into());
    s.dictation.toggle_chord = Some("Super+D".into());
    assert!(s.validate().is_err(), "hold and toggle must differ");
}

#[test]
fn config_missing_file_yields_defaults() {
    let dir = tmpdir("cfg-missing");
    let s = Settings::load_or_default(&dir.join("nope.json")).unwrap();
    assert_eq!(s, Settings::default());
}

// ── store ───────────────────────────────────────────────────────────────────

#[test]
fn store_creates_schema_and_roundtrips_a_take() {
    let dir = tmpdir("store-1");
    let st = Store::open(&dir).unwrap();
    st.create_take("t1", "2026-11-22T10:00:00Z").unwrap();

    let t = st.get_take("t1").unwrap().unwrap();
    assert_eq!(t.status, TakeStatus::Recording);

    let audio = st.audio_path_for("t1");
    std::fs::write(&audio, b"RIFF....WAVE").unwrap();
    st.finish_take("t1", 1234, TakeStatus::Transcribed, Some(&audio))
        .unwrap();

    let t = st.get_take("t1").unwrap().unwrap();
    assert_eq!(t.duration_ms, 1234);
    assert_eq!(t.status, TakeStatus::Transcribed);
    assert!(t.audio_present);
}

#[test]
fn store_keeps_raw_and_corrected_separate() {
    let dir = tmpdir("store-2");
    let st = Store::open(&dir).unwrap();
    st.create_take("t1", "2026-11-22T10:00:00Z").unwrap();
    st.finish_take("t1", 900, TakeStatus::Transcribed, None).unwrap();

    let a = Attempt {
        id: "a1".into(),
        take_id: "t1".into(),
        raw_text: "ask Lukman".into(),
        corrected_text: Some("ask Luqman".into()),
        model: "parakeet-redux".into(),
        decode_ms: 90,
        corrections_json: "[]".into(),
        created_at: "2026-11-22T10:00:01Z".into(),
    };
    st.add_attempt(&a).unwrap();

    let got = st.latest_attempt("t1").unwrap().unwrap();
    assert_eq!(got.raw_text, "ask Lukman", "raw must never be rewritten");
    assert_eq!(got.corrected_text.as_deref(), Some("ask Luqman"));

    // Editing the correction must not touch the raw text.
    st.set_corrected("a1", "ask Luqman Ahmed").unwrap();
    let got = st.latest_attempt("t1").unwrap().unwrap();
    assert_eq!(got.raw_text, "ask Lukman");
    assert_eq!(got.corrected_text.as_deref(), Some("ask Luqman Ahmed"));
}

#[test]
fn store_lists_newest_first_with_pagination() {
    let dir = tmpdir("store-3");
    let st = Store::open(&dir).unwrap();
    for i in 0..5 {
        let id = format!("t{i}");
        st.create_take(&id, &format!("2026-11-22T10:00:0{i}Z")).unwrap();
        st.finish_take(&id, 100, TakeStatus::Transcribed, None).unwrap();
    }
    let page1 = st.list(3, None).unwrap();
    assert_eq!(page1.len(), 3);
    assert_eq!(page1[0].take.id, "t4");

    let last = &page1[2];
    let page2 = st
        .list(3, Some((&last.take.created_at, &last.take.id)))
        .unwrap();
    assert_eq!(page2.len(), 2);
    assert_eq!(page2[0].take.id, "t1");
}

#[test]
fn store_cascades_delete_to_attempts() {
    let dir = tmpdir("store-4");
    let st = Store::open(&dir).unwrap();
    st.create_take("t1", "2026-11-22T10:00:00Z").unwrap();
    st.add_attempt(&Attempt {
        id: "a1".into(),
        take_id: "t1".into(),
        raw_text: "x".into(),
        corrected_text: None,
        model: "m".into(),
        decode_ms: 1,
        corrections_json: "[]".into(),
        created_at: "2026-11-22T10:00:01Z".into(),
    })
    .unwrap();

    let n = st.delete(&["t1".to_string()], true).unwrap();
    assert_eq!(n, 1);
    assert!(st.latest_attempt("t1").unwrap().is_none());
}

#[test]
fn store_recovers_interrupted_recording() {
    let dir = tmpdir("store-5");
    let st = Store::open(&dir).unwrap();
    st.create_take("t1", "2026-11-22T10:00:00Z").unwrap();

    // Simulate a crash: status still 'recording', but audio is on disk.
    let audio = st.audio_path_for("t1");
    std::fs::write(&audio, vec![0u8; 1000]).unwrap();
    st.finish_take("t1", 0, TakeStatus::Recording, Some(&audio))
        .unwrap();

    let n = st.recover_interrupted().unwrap();
    assert_eq!(n, 1);
    let t = st.get_take("t1").unwrap().unwrap();
    assert_eq!(
        t.status,
        TakeStatus::Captured,
        "a take with audio must be recoverable, not lost"
    );
}

#[test]
fn store_retention_none_deletes_audio_but_keeps_text() {
    let dir = tmpdir("store-6");
    let st = Store::open(&dir).unwrap();
    st.create_take("t1", "2020-01-01T00:00:00Z").unwrap();
    let audio = st.audio_path_for("t1");
    std::fs::write(&audio, b"x").unwrap();
    st.finish_take("t1", 10, TakeStatus::Transcribed, Some(&audio))
        .unwrap();
    st.add_attempt(&Attempt {
        id: "a1".into(),
        take_id: "t1".into(),
        raw_text: "kept".into(),
        corrected_text: None,
        model: "m".into(),
        decode_ms: 1,
        corrections_json: "[]".into(),
        created_at: "2020-01-01T00:00:01Z".into(),
    })
    .unwrap();

    let removed = st.enforce_retention(Some(0)).unwrap();
    assert_eq!(removed, 1);
    assert!(!audio.exists(), "audio must be gone");
    let t = st.get_take("t1").unwrap().unwrap();
    assert!(!t.audio_present);
    assert!(
        st.latest_attempt("t1").unwrap().is_some(),
        "transcript must survive audio expiry"
    );
}

#[test]
fn store_delete_all_audio_keeps_transcripts() {
    let dir = tmpdir("store-8");
    let st = Store::open(&dir).unwrap();
    st.create_take("t1", "2026-11-22T10:00:00Z").unwrap();
    let audio = st.audio_path_for("t1");
    std::fs::write(&audio, b"x").unwrap();
    st.finish_take("t1", 10, TakeStatus::Transcribed, Some(&audio))
        .unwrap();
    st.add_attempt(&Attempt {
        id: "a1".into(),
        take_id: "t1".into(),
        raw_text: "words survive".into(),
        corrected_text: None,
        model: "m".into(),
        decode_ms: 1,
        corrections_json: "[]".into(),
        created_at: "2026-11-22T10:00:01Z".into(),
    })
    .unwrap();
    // An orphan the database does not reference must also go: a crash can leave
    // one behind, and the button says the audio is gone.
    let orphan = st.audio_dir().join("orphan.wav");
    std::fs::write(&orphan, b"y").unwrap();

    let removed = st.delete_all_audio().unwrap();
    assert_eq!(removed, 2, "both the referenced and the orphan file");
    assert!(!audio.exists());
    assert!(!orphan.exists());
    assert_eq!(st.audio_bytes().unwrap(), 0);

    let all = st.list(10, None).unwrap();
    assert_eq!(all.len(), 1, "the take itself must survive");
    assert!(!all[0].take.audio_present);
    assert_eq!(
        all[0].attempt.as_ref().unwrap().raw_text,
        "words survive",
        "the transcript must survive audio deletion"
    );
}

#[test]
fn store_trim_enforces_max_entries() {
    let dir = tmpdir("store-7");
    let st = Store::open(&dir).unwrap();
    for i in 0..6 {
        let id = format!("t{i}");
        st.create_take(&id, &format!("2026-11-22T10:00:0{i}Z")).unwrap();
    }
    let n = st.trim_to(4).unwrap();
    assert_eq!(n, 2);
    let all = st.list(100, None).unwrap();
    assert_eq!(all.len(), 4);
    assert_eq!(all[0].take.id, "t5", "newest kept");
}

// ── audio ───────────────────────────────────────────────────────────────────

#[test]
fn audio_downmix_averages_channels() {
    use sotto_core::audio::downmix;
    let stereo = vec![1.0f32, 0.0, 0.5, 0.5, -1.0, -1.0];
    let mono = downmix(&stereo, 2);
    assert_eq!(mono, vec![0.5, 0.5, -1.0]);
}

#[test]
fn audio_resample_preserves_duration() {
    use sotto_core::audio::resample_linear;
    let input: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.001).sin()).collect();
    let out = resample_linear(&input, 48_000, 16_000);
    assert!(
        (out.len() as i64 - 16_000).abs() <= 2,
        "1 s at 48 kHz must become ~1 s at 16 kHz, got {}",
        out.len()
    );
}

#[test]
fn audio_wav_header_is_valid() {
    use sotto_core::audio::Recorded;
    let dir = tmpdir("wav");
    let path = dir.join("a.wav");
    let rec = Recorded {
        samples_16k: vec![0.0f32; 16_000],
        source_rate: 16_000,
        source_channels: 1,
    };
    rec.write_wav(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    assert_eq!(&bytes[36..40], b"data");
    // 44-byte header + 16000 samples * 2 bytes
    assert_eq!(bytes.len(), 44 + 32_000);
    assert_eq!(rec.duration_ms(), 1000);
}

// ── dictionary wiring ───────────────────────────────────────────────────────

#[test]
fn dictionary_honours_settings_switches() {
    use sotto_core::{dictionary_from, load_dictionary, save_dictionary};
    use sotto_dict::{CasePolicy, Entry};

    let dir = tmpdir("dict-wire");
    let path = dir.join("dictionary.json");
    let entries = vec![Entry {
        id: "luqman".into(),
        preferred: "Luqman".into(),
        aliases: vec!["Luqman".into()],
        language: None,
        case_policy: CasePolicy::Exact,
        enabled: true,
    }];
    save_dictionary(&path, &entries, &[]).unwrap();
    let (e, r) = load_dictionary(&path);
    assert_eq!(e.len(), 1);
    assert!(r.is_empty());

    let mut settings = Settings::default();
    // Off by default: "Lukman" must survive untouched.
    let d = dictionary_from(&settings, e.clone(), r.clone());
    assert_eq!(d.correct("Lukman").text, "Lukman");

    // Switch fuzzy on: it should repair.
    settings.dictionary.fuzzy_enabled = true;
    let d = dictionary_from(&settings, e, r);
    assert_eq!(d.correct("Lukman").text, "Luqman");
}

#[test]
fn dictionary_file_survives_corruption() {
    let dir = tmpdir("dict-corrupt");
    let path = dir.join("dictionary.json");
    std::fs::write(&path, b"{not json").unwrap();
    let (e, r) = sotto_core::load_dictionary(&path);
    assert!(e.is_empty() && r.is_empty(), "must degrade, not panic");
}
