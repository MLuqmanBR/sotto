//! The engine dying must not break the app permanently.
//!
//! The speech engine is a separate process doing heavy numeric work. If it dies —
//! a bug in the runtime, the OOM killer, a corrupted page of weights — the client
//! must notice. It did not: `ensure_stt` saw a handle, assumed it was good, and
//! handed a dead process to every later transcription, so **one crash made every
//! subsequent dictation fail** until the app was restarted. From the user's side
//! the app simply stopped working, with no reason given.
//!
//! This test kills the engine for real and then transcribes again, which is the
//! only way to know the recovery path works.
//!
//! Opt-in, because it starts a real engine:
//!
//! ```text
//! cargo test -p sotto --test engine_recovery
//! ```

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
    std::env::set_var("SOTTO_STT_BIN", &bin);
    std::env::set_var("SOTTO_MODEL_DIR", &model);
    Some((bin, model))
}

fn app(model: PathBuf, name: &str) -> (std::sync::Arc<sotto::pipeline::App>, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "sotto-engine-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::fs::create_dir_all(root.join("history")).unwrap();
    let mut s = sotto_core::Settings::default();
    s.dictation.auto_insert = false;
    s.save(&root.join("config/settings.json")).unwrap();
    (std::sync::Arc::new(sotto::pipeline::App::new(&root, model)), root)
}

#[test]
fn a_crashed_engine_is_replaced_instead_of_reused() {
    let Some((_bin, model)) = setup() else { return };
    let clip = repo_root().join("tests/fixtures/audio/short.wav");
    if !clip.is_file() {
        eprintln!("SKIP: test clip missing");
        return;
    }

    let (app, root) = app(model, "crash");

    // 1. A working engine.
    app.ensure_stt().expect("the engine should start");
    let first_pid = app.stt_pid().expect("a pid");
    assert!(
        std::path::Path::new(&format!("/proc/{first_pid}")).exists(),
        "the engine should be running"
    );

    // 2. Kill it the way a crash would: from outside, with no message.
    let _ = std::process::Command::new("kill")
        .args(["-9", &first_pid.to_string()])
        .status();
    // Give the kernel a moment to reap it.
    for _ in 0..40 {
        if !std::path::Path::new(&format!("/proc/{first_pid}")).exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // 3. The app must notice and start a new one, not reuse the dead handle.
    app.ensure_stt()
        .expect("a crashed engine should be replaced, not reused");
    let second_pid = app.stt_pid().expect("a new pid");
    assert_ne!(
        first_pid, second_pid,
        "the dead engine was reused instead of replaced"
    );
    assert!(
        std::path::Path::new(&format!("/proc/{second_pid}")).exists(),
        "the replacement engine should be running"
    );

    // 4. And it must actually work: a real transcription after the crash.
    //
    // Deliberately no take is opened here. `abort_take` kills the engine by
    // design (cancellation must stop CPU work), so opening and cancelling one
    // would remove the very engine this step is checking.
    let tr = {
        let mut g = app.stt.lock().unwrap();
        let c = g.as_mut().expect("engine");
        c.transcribe(&clip, false).expect("transcription after recovery")
    };
    assert!(
        !tr.text.trim().is_empty(),
        "the replacement engine produced no text"
    );
    eprintln!("after a crash: {:?}", tr.text);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_model_is_reported_not_guessed() {
    let Some((_bin, _model)) = setup() else { return };

    // A root whose model directory has nothing in it. The app must say so plainly
    // rather than starting an engine that cannot work.
    let root = std::env::temp_dir().join(format!("sotto-nomodel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::fs::create_dir_all(root.join("history")).unwrap();
    let empty_model = root.join("empty-model");
    std::fs::create_dir_all(&empty_model).unwrap();

    let app = sotto::pipeline::App::new(&root, empty_model);
    let err = app
        .ensure_stt()
        .expect_err("an empty model directory must not start an engine");
    assert!(
        err.contains("missing"),
        "the error should say the model is missing, got: {err}"
    );
    assert!(
        app.stt_pid().is_none(),
        "no engine should be left running"
    );

    let _ = std::fs::remove_dir_all(&root);
}
