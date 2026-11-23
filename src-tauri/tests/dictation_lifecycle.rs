//! The dictation lifecycle, driven the way the window drives it.
//!
//! Until now the whole start→stop→transcribe→deliver path lived in the binary,
//! so the only way to reach it was to click a button — which cannot be scripted
//! under Wayland here. That made the app's single most important behaviour the
//! one thing no test touched: every suite entered the pipeline *after* the audio
//! existed, or exercised pieces of it, but nothing ran `begin_take` → `end_take`.
//!
//! Those two methods are now library functions and this test calls them directly,
//! so the path under test is the same code the buttons call, with a real
//! microphone device, a real sidecar, a real model, a real store and real
//! delivery bookkeeping.
//!
//! Opt-in, because it opens an audio device:
//!
//! ```text
//! SOTTO_TEST_CAPTURE_DEVICE=<virtual source> SOTTO_TEST_PLAYBACK_SINK=<sink> \
//!   cargo test -p sotto --test dictation_lifecycle
//! ```

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

/// Both tests open the same capture device and play audio into the same sink.
/// Run in parallel they would record each other's clip, so one lock serialises
/// them. This is about the shared device, not about the product.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn setup() -> Option<(String, String, PathBuf, PathBuf)> {
    let device = std::env::var("SOTTO_TEST_CAPTURE_DEVICE").ok()?;
    let sink = std::env::var("SOTTO_TEST_PLAYBACK_SINK").ok()?;
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
    std::env::set_var("SOTTO_STT_BIN", &bin);
    std::env::set_var("SOTTO_MODEL_DIR", &model);
    Some((device, sink, bin, model))
}

fn app(model: PathBuf, name: &str) -> (Arc<sotto::pipeline::App>, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "sotto-lifecycle-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::fs::create_dir_all(root.join("history")).unwrap();

    let mut settings = sotto_core::Settings::default();
    // The test must not type into the real desktop.
    settings.dictation.auto_insert = false;
    settings.audio.input_device = Some(std::env::var("SOTTO_TEST_CAPTURE_DEVICE").unwrap());
    settings.save(&root.join("config/settings.json")).unwrap();

    (Arc::new(sotto::pipeline::App::new(&root, model)), root)
}

/// Play the test clip into the sink the capture device is monitoring.
fn speak(sink: &str, clip: &std::path::Path) -> std::process::Child {
    std::process::Command::new("paplay")
        .arg("-d")
        .arg(sink)
        .arg(clip)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("paplay should be available")
}

#[test]
fn a_full_dictation_runs_through_the_same_path_the_button_uses() {
    let _guard = serial();
    let Some((_device, sink, _bin, model)) = setup() else { return };

    let clip = repo_root().join("tests/fixtures/audio/medium.wav");
    if !clip.is_file() {
        eprintln!("SKIP: test clip missing (run scripts/make-test-audio.sh)");
        return;
    }

    let (app, root) = app(model, "full");

    // Start, exactly as the button does.
    let take_id = app.begin_take().expect("a take should open");

    // A second start must be refused, not silently replace the running take.
    let err = app
        .begin_take()
        .expect_err("a second take must be refused");
    assert!(
        err.contains("already recording"),
        "the refusal should say why, got: {err}"
    );

    let mut player = speak(&sink, &clip);
    let _ = player.wait();
    // A small tail so the final word is not clipped.
    std::thread::sleep(std::time::Duration::from_millis(400));

    // Stop, exactly as the button does.
    let result = app.end_take().expect("the take should transcribe");
    assert!(
        !result.corrected_text.trim().is_empty(),
        "the take produced no text"
    );
    assert!(result.decode_ms > 0, "decode time should be reported");

    // Acceptance A16: no microphone is left open once a take ends. An open input
    // device is both a privacy problem and a reason the machine cannot suspend.
    assert!(
        app.recorder.lock().unwrap().is_none(),
        "the microphone is still open after the take ended"
    );

    // The take is closed and the audio survived on disk, which is the promise
    // the journal exists to keep.
    let store = sotto_core::Store::open(root.join("history")).expect("store reopens");
    let entries = store.list(50, None).expect("history list");
    assert_eq!(entries.len(), 1, "exactly one take should be recorded");
    assert_eq!(entries[0].take.id, take_id);
    assert!(
        entries[0].take.duration_ms > 1000,
        "the take should record real duration, got {}",
        entries[0].take.duration_ms
    );
    assert!(
        entries[0].take.audio_present,
        "the recording must still be on disk after the take"
    );

    let attempts = store.attempts_for(&take_id).expect("attempts");
    assert_eq!(attempts.len(), 1, "one attempt should be stored");
    assert_eq!(
        attempts[0].raw_text, result.raw_text,
        "the stored raw text must be what the engine produced"
    );
    assert!(
        attempts[0].model.contains("parakeet"),
        "the attempt should name the model, got {}",
        attempts[0].model
    );

    // No take is left mid-flight: recovery must find nothing to repair, or the
    // app would reopen a take that actually finished.
    assert_eq!(
        store.recover_interrupted().unwrap_or(0),
        0,
        "a finished take must not be left in the recording state"
    );

    eprintln!("transcribed: {:?}", result.corrected_text);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cancelling_drops_the_take_and_leaves_no_pending_row() {
    let _guard = serial();
    let Some((_device, sink, _bin, model)) = setup() else { return };
    let (app, root) = app(model, "cancel");

    let take_id = app.begin_take().expect("a take should open");

    // Make sure an engine is actually running, so the cancel has something to
    // kill and the check is meaningful.
    app.ensure_stt().expect("engine should start");
    let pid = app.stt_pid().expect("the engine should have a process");

    let mut player = speak(&sink, &repo_root().join("tests/fixtures/audio/short.wav"));
    let _ = player.wait();
    std::thread::sleep(std::time::Duration::from_millis(200));

    app.abort_take().expect("cancel should succeed");

    // Acceptance A04: cancellation must stop CPU work, verified by **process
    // death**, not by a dropped request. A decode already running would otherwise
    // keep burning cores with nobody waiting for the answer.
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "the engine process {pid} survived a cancel; CPU work would continue"
    );
    assert!(
        app.stt_pid().is_none(),
        "the app still holds an engine after a cancel"
    );

    // Cancelled, not pending: a cancel is a decision, so recovery must not
    // resurrect it on the next start.
    let store = sotto_core::Store::open(root.join("history")).expect("store reopens");
    assert_eq!(
        store.recover_interrupted().unwrap_or(0),
        0,
        "a cancelled take must not be left in the recording state"
    );
    let entries = store.list(50, None).unwrap();
    let take = entries.iter().find(|e| e.take.id == take_id);
    if let Some(e) = take {
        assert!(
            format!("{:?}", e.take.status).to_lowercase().contains("cancel"),
            "the take should be cancelled, got {:?}",
            e.take.status
        );
    }

    // And the recorder is free again, so the next take can start.
    let again = app.begin_take().expect("a new take should open after a cancel");
    app.abort_take().unwrap();
    assert!(!again.is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn quitting_during_a_take_removes_the_empty_row() {
    let _guard = serial();
    let Some((_device, _sink, _bin, model)) = setup() else { return };
    let (app, root) = app(model, "quit-mid-take");

    let take_id = app.begin_take().expect("a take should open");
    std::thread::sleep(std::time::Duration::from_millis(300));

    // This is the shutdown path. A quit is not a dictation, so unlike Cancel it
    // must not leave a zero-length row for History to show on the next launch.
    app.discard_take_on_exit();

    let store = sotto_core::Store::open(root.join("history")).expect("store reopens");
    let entries = store.list(50, None).unwrap();
    assert!(
        entries.is_empty(),
        "a quit during a take left {} row(s) in History",
        entries.len()
    );
    let audio = store.audio_path_for(&take_id);
    assert!(
        !audio.exists(),
        "the discarded recording is still on disk at {}",
        audio.display()
    );
    assert_eq!(
        store.recover_interrupted().unwrap_or(0),
        0,
        "nothing should be left for crash recovery to repair"
    );
    assert!(
        app.stt_pid().is_none(),
        "the engine must not outlive the app"
    );

    let _ = std::fs::remove_dir_all(&root);
}
