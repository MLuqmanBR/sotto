//! App-level pipeline test: exercises the same `App::finish` path the GUI uses,
//! including dictionary correction, history storage and delivery reporting.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn setup() -> Option<(PathBuf, PathBuf)> {
    let bin = std::env::var("SOTTO_STT_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("target/release/sotto-stt"));
    let model = std::env::var("SOTTO_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("model"));
    if !bin.is_file() || !model.join("model.safetensors").is_file() {
        eprintln!("SKIP: sidecar or model not present");
        return None;
    }
    Some((bin, model))
}

#[test]
fn app_finish_produces_corrected_history_entry() {
    let Some((bin, model)) = setup() else { return };
    std::env::set_var("SOTTO_STT_BIN", &bin);
    std::env::set_var("SOTTO_MODEL_DIR", &model);

    let root = std::env::temp_dir().join(format!("sotto-app-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("config")).unwrap();

    // Seed a dictionary rule that is guaranteed to fire: an explicit
    // replacement. The clip used to make the model mishear "Mohammed", but a
    // regenerated clip now transcribes it correctly, so relying on that error
    // would make this test depend on the model's mood rather than on our code.
    let replacements = vec![sotto_dict::Replacement {
        id: "mohammed-full".into(),
        source: "Mohammed".into(),
        target: "Mohammed Al-Rashid".into(),
        boundary: sotto_dict::Boundary::Word,
        case_sensitive: false,
        enabled: true,
        priority: 0,
    }];
    sotto_core::save_dictionary(&root.join("config/dictionary.json"), &[], &replacements).unwrap();

    let mut settings = sotto_core::Settings::default();
    settings.dictation.auto_insert = false; // do not touch the real desktop
    settings.save(&root.join("config/settings.json")).unwrap();

    let app = sotto::pipeline::App::new(&root, model.clone());
    app.ensure_stt().expect("engine should start");

    // The real capture path writes a take's recording inside the store's managed
    // audio directory, so the test does the same rather than pointing the take at
    // a shared fixture. That keeps cleanup scoped to files Sotto owns.
    let fixture = repo_root().join("tests/fixtures/audio/medium.wav");
    let take_id = "t-e2e";
    let store_guard = app.store.lock().unwrap();
    store_guard
        .create_take(take_id, "2026-11-22T10:00:00Z")
        .unwrap();
    let wav = store_guard.audio_path_for(take_id);
    std::fs::create_dir_all(wav.parent().unwrap()).unwrap();
    std::fs::copy(&fixture, &wav).expect("copy fixture into the managed audio dir");
    drop(store_guard);

    let result = app.finish(take_id, &wav, 9_760).expect("finish");

    // Raw text is what the model said, untouched.
    assert!(
        result.raw_text.contains("Mohammed"),
        "the clip should be transcribed with the name, got: {}",
        result.raw_text
    );
    // Corrected text has the replacement applied.
    assert!(
        result.corrected_text.contains("Mohammed Al-Rashid"),
        "corrected should contain the replacement target, got: {}",
        result.corrected_text
    );
    assert_ne!(result.raw_text, result.corrected_text);
    assert!(!result.corrections.is_empty(), "a correction should be recorded");

    // History: both versions retrievable.
    let attempt = app
        .store
        .lock()
        .unwrap()
        .latest_attempt(take_id)
        .unwrap()
        .expect("attempt stored");
    assert_eq!(attempt.raw_text, result.raw_text);
    assert_eq!(attempt.corrected_text.as_deref(), Some(result.corrected_text.as_str()));
    assert!(
        attempt.corrections_json.contains("replacement"),
        "the tier should be recorded for explainability: {}",
        attempt.corrections_json
    );

    // Take marked transcribed with audio present.
    let take = app.store.lock().unwrap().get_take(take_id).unwrap().unwrap();
    assert_eq!(take.status, sotto_core::TakeStatus::Transcribed);
    assert!(take.audio_present);

    // Delivery was honest: auto_insert was off, so it must say copied, not inserted.
    assert!(
        result.delivery.contains("copied"),
        "delivery should report clipboard-only, got: {}",
        result.delivery
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn app_finish_enforces_retention_and_protects_starred_takes() {
    let Some((bin, model)) = setup() else { return };
    std::env::set_var("SOTTO_STT_BIN", &bin);
    std::env::set_var("SOTTO_MODEL_DIR", &model);

    let root = std::env::temp_dir().join(format!("sotto-retention-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("config")).unwrap();

    let mut settings = sotto_core::Settings::default();
    // "Delete after transcribing": the strongest form of the promise.
    settings.history.retention = sotto_core::config::Retention::None;
    settings.dictation.auto_insert = false;
    settings.save(&root.join("config/settings.json")).unwrap();

    let app = sotto::pipeline::App::new(&root, model.clone());
    app.ensure_stt().expect("engine should start");

    // As in the real capture path, the recording lives in the managed audio
    // directory, so cleanup is allowed to touch it.
    let fixture = repo_root().join("tests/fixtures/audio/medium.wav");
    let store = || app.store.lock().unwrap();

    // An older take the user starred, with audio on disk.
    store().create_take("starred", "2020-01-01T00:00:00Z").unwrap();
    let starred_audio = store().audio_path_for("starred");
    std::fs::write(&starred_audio, vec![0u8; 400]).unwrap();
    store()
        .finish_take(
            "starred",
            100,
            sotto_core::TakeStatus::Transcribed,
            Some(&starred_audio),
        )
        .unwrap();
    store().set_favourite("starred", true).unwrap();

    // An older take that was not starred.
    store().create_take("plain", "2020-01-01T00:00:01Z").unwrap();
    let plain_audio = store().audio_path_for("plain");
    std::fs::write(&plain_audio, vec![0u8; 400]).unwrap();
    store()
        .finish_take("plain", 100, sotto_core::TakeStatus::Transcribed, Some(&plain_audio))
        .unwrap();

    app.store
        .lock()
        .unwrap()
        .create_take("fresh", "2026-11-22T10:00:00Z")
        .unwrap();
    let fresh_audio = store().audio_path_for("fresh");
    std::fs::create_dir_all(fresh_audio.parent().unwrap()).unwrap();
    std::fs::copy(&fixture, &fresh_audio).expect("copy fixture into the managed audio dir");
    let result = app.finish("fresh", &fresh_audio, 9_760).expect("finish");
    assert!(!result.corrected_text.is_empty());

    // Retention ran as part of finishing a dictation, not on a page visit.
    let plain = store().get_take("plain").unwrap().unwrap();
    assert!(
        !plain.audio_present,
        "retention must delete the audio of an unstarred take"
    );
    assert!(
        store().latest_attempt("plain").unwrap().is_none(),
        "this take never had a transcript, so only its audio is affected"
    );

    // A starred take keeps its recording, which is what the star promises.
    let starred = store().get_take("starred").unwrap().unwrap();
    assert!(
        starred.audio_present,
        "a starred take must keep its audio through automatic cleanup"
    );
    assert!(starred_audio.exists());

    // "Delete after transcribing" means the take just transcribed loses its
    // audio too. Its transcript must survive, and that is the point of the
    // setting: the words stay, the recording goes.
    let fresh = store().get_take("fresh").unwrap().unwrap();
    assert!(
        !fresh.audio_present,
        "this setting deletes audio immediately after transcribing"
    );
    let attempt = store().latest_attempt("fresh").unwrap().expect("transcript kept");
    assert_eq!(attempt.corrected_text.as_deref(), Some(result.corrected_text.as_str()));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn app_refuses_when_model_missing() {
    let root = std::env::temp_dir().join(format!("sotto-nomodel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("config")).unwrap();

    let app = sotto::pipeline::App::new(&root, root.join("empty-model"));
    let err = app.ensure_stt().unwrap_err();
    assert!(
        err.contains("missing") || err.contains("model"),
        "should name the real cause, got: {err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn app_recovers_interrupted_takes_on_start() {
    let root = std::env::temp_dir().join(format!("sotto-recover-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("config")).unwrap();

    {
        let store = sotto_core::Store::open(root.join("history")).unwrap();
        store.create_take("crashy", "2026-11-22T10:00:00Z").unwrap();
        let audio = store.audio_path_for("crashy");
        std::fs::write(&audio, vec![0u8; 500]).unwrap();
        store
            .finish_take("crashy", 0, sotto_core::TakeStatus::Recording, Some(&audio))
            .unwrap();
    }

    // Re-opening the app must recover, not lose, that take.
    let app = sotto::pipeline::App::new(&root, root.join("model"));
    let t = app.store.lock().unwrap().get_take("crashy").unwrap().unwrap();
    assert_eq!(t.status, sotto_core::TakeStatus::Captured);
    let _ = std::fs::remove_dir_all(&root);
}
