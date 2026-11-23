//! End-to-end pipeline test: real sidecar, real model, real audio, real store.
//!
//! Skips (with a clear message) when the model or the sidecar binary is absent,
//! so `cargo test` works on a machine that has not downloaded the 178 MB model.

use sotto_core::config::Settings;
use sotto_core::store::{Attempt, Store, TakeStatus};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The model is ~887 MB resident per sidecar. Running several e2e tests in
/// parallel thrashes the CPU and makes latency assertions meaningless, so the
/// tests that start a sidecar take this lock in turn.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn model_dir() -> PathBuf {
    std::env::var("SOTTO_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("model"))
}

fn sidecar() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SOTTO_STT_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    for cand in ["target/release/sotto-stt", "target/debug/sotto-stt"] {
        let p = repo_root().join(cand);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn audio(name: &str) -> PathBuf {
    repo_root().join("tests/fixtures/audio").join(name)
}

fn skip(reason: &str) -> bool {
    eprintln!("SKIP: {reason}");
    true
}

#[test]
fn e2e_transcribe_correct_and_store() {
    let _guard = serial();
    let Some(bin) = sidecar() else {
        skip("sotto-stt binary not built (cargo build -p sotto-stt --release)");
        return;
    };
    let model = model_dir();
    if !model.join("model.safetensors").is_file() {
        skip("model not downloaded");
        return;
    }
    let wav = audio("medium.wav");
    if !wav.is_file() {
        skip("test audio missing");
        return;
    }

    // 1. Start the sidecar and confirm the handshake.
    let mut client = sotto_core::stt::SttClient::start(&bin, &model).expect("sidecar start");
    assert_eq!(client.ready().engine, "voxrt");
    assert_eq!(client.ready().model, "parakeet-redux");
    assert_eq!(client.ready().device, "cpu");
    assert!(
        client.ready().kernels.contains("avx512") || client.ready().kernels.contains("avx2"),
        "expected a SIMD kernel, got {}",
        client.ready().kernels
    );

    // 2. Transcribe.
    let tr = client.transcribe(&wav, true).expect("transcribe");
    assert!(!tr.text.is_empty(), "transcript must not be empty");
    assert!(tr.duration > 9.0 && tr.duration < 11.0, "duration {}", tr.duration);
    assert!(!tr.segments.is_empty());
    assert!(
        !tr.segments[0].words.is_empty(),
        "word timings were requested"
    );
    assert!(
        tr.text.to_lowercase().contains("meeting"),
        "expected the known content, got: {}",
        tr.text
    );

    // 3. Dictionary: apply a rule that is guaranteed to fire.
    //
    // This used to rely on the model mishearing "Mohammed". A regenerated clip
    // now transcribes the name correctly, so depending on that error would make
    // the test depend on the model's mood rather than on the dictionary. An
    // explicit replacement always matches, which is what this step is testing.
    let replacements = vec![sotto_dict::Replacement {
        id: "mohammed-full".into(),
        source: "Mohammed".into(),
        target: "Mohammed Al-Rashid".into(),
        boundary: sotto_dict::Boundary::Word,
        case_sensitive: false,
        enabled: true,
        priority: 0,
    }];
    let settings = Settings {
        dictionary: sotto_core::config::DictionarySettings {
            fuzzy_enabled: true,
            phonetic_enabled: true,
            fuzzy_threshold: sotto_dict::DEFAULT_FUZZY_THRESHOLD,
        },
        ..Default::default()
    };
    let dict = sotto_core::dictionary_from(&settings, vec![], replacements);
    let corrected = dict.correct(&tr.text);
    assert!(
        corrected.text.contains("Mohammed Al-Rashid"),
        "dictionary should have applied the replacement; got: {}",
        corrected.text
    );

    // 4. Store: raw and corrected kept apart, audio referenced.
    let dir = std::env::temp_dir().join(format!("sotto-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let st = Store::open(&dir).unwrap();
    st.create_take("t1", "2026-11-22T10:00:00Z").unwrap();
    st.finish_take("t1", tr.duration as i64 * 1000, TakeStatus::Transcribed, Some(&wav))
        .unwrap();
    st.add_attempt(&Attempt {
        id: "a1".into(),
        take_id: "t1".into(),
        raw_text: tr.text.clone(),
        corrected_text: Some(corrected.text.clone()),
        model: client.ready().model.clone(),
        decode_ms: tr.decode_ms as i64,
        corrections_json: serde_json::to_string(&corrected.corrections).unwrap(),
        created_at: "2026-11-22T10:00:01Z".into(),
    })
    .unwrap();

    let got = st.latest_attempt("t1").unwrap().unwrap();
    assert_eq!(got.raw_text, tr.text, "raw preserved verbatim");
    assert_ne!(got.corrected_text.as_deref(), Some(tr.text.as_str()));

    // 5. Performance budget: decode must be far faster than real time.
    let rtf = tr.duration / (tr.decode_ms as f64 / 1000.0);
    eprintln!(
        "e2e: {} chars, {:.2}s audio, {} ms decode ({:.1}x real time), kernel {}",
        tr.text.len(),
        tr.duration,
        tr.decode_ms,
        rtf,
        client.ready().kernels
    );
    assert!(rtf > 10.0, "expected >10x real time, got {rtf:.1}x");

    // 6. Clean shutdown, then a second client can start (no port/lock leaks).
    client.shutdown();
    drop(client);
    let mut c2 = sotto_core::stt::SttClient::start(&bin, &model).expect("restart");
    c2.ping().expect("ping");
    c2.shutdown();

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn e2e_short_clip_latency_budget() {
    let _guard = serial();
    let Some(bin) = sidecar() else {
        skip("sotto-stt binary not built");
        return;
    };
    let model = model_dir();
    let wav = audio("short.wav");
    if !model.join("model.safetensors").is_file() || !wav.is_file() {
        skip("model or audio missing");
        return;
    }
    let mut c = sotto_core::stt::SttClient::start(&bin, &model).expect("start");
    let tr = c.transcribe(&wav, false).expect("transcribe");
    // A ~3 s dictation must come back in well under a second once resident.
    eprintln!("short clip: {} ms decode for {:.2}s audio", tr.decode_ms, tr.duration);
    assert!(
        tr.decode_ms < 500,
        "3 s dictation took {} ms, expected < 500 ms",
        tr.decode_ms
    );
    c.shutdown();
}

#[test]
fn e2e_sidecar_rejects_missing_audio() {
    let _guard = serial();
    let Some(bin) = sidecar() else {
        skip("sotto-stt binary not built");
        return;
    };
    let model = model_dir();
    if !model.join("model.safetensors").is_file() {
        skip("model missing");
        return;
    }
    let mut c = sotto_core::stt::SttClient::start(&bin, &model).expect("start");
    let err = c
        .transcribe(Path::new("/nonexistent/audio.wav"), false)
        .unwrap_err();
    match err {
        sotto_core::stt::SttError::Remote { code, .. } => {
            assert_eq!(code, "audio_missing", "must fail cleanly, not crash");
        }
        other => panic!("expected a remote error, got {other:?}"),
    }
    c.shutdown();
}

#[test]
fn e2e_cancellation_actually_stops_the_process() {
    let _guard = serial();
    let Some(bin) = sidecar() else {
        skip("sotto-stt binary not built");
        return;
    };
    let model = model_dir();
    if !model.join("model.safetensors").is_file() {
        skip("model missing");
        return;
    }
    let mut c = sotto_core::stt::SttClient::start(&bin, &model).expect("start");
    // Killing must terminate the child; the Drop impl does this too.
    c.kill();
    // A killed client cannot answer a ping: proves the process is gone.
    assert!(c.ping().is_err(), "killed sidecar must not respond");
}
