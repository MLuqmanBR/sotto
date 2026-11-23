//! Real capture test, using a virtual audio device.
//!
//! This exercises the code the "Start dictation" button calls: open the input
//! device through CPAL, record, resample to 16 kHz mono, write a WAV, and hand
//! it to the sidecar. It exists because the rest of the suite enters the
//! pipeline *after* the WAV exists, which left the capture path — the part most
//! likely to differ between machines — unverified.
//!
//! It is deliberately opt-in: it only runs when `SOTTO_TEST_CAPTURE_DEVICE` names
//! an input device, because opening an arbitrary microphone is not something a
//! test should do on its own. The isolation harness sets that variable to a
//! virtual sink, so no real microphone is ever opened.

use std::path::PathBuf;
use std::sync::mpsc;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Play a clip into the named sink, so the device under test receives audio.
fn play_into_sink(sink: &str, wav: &std::path::Path) -> std::process::Child {
    std::process::Command::new("paplay")
        .arg("-d")
        .arg(sink)
        .arg(wav)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("paplay should be available for this test")
}

#[test]
fn capture_records_and_transcribes_real_audio() {
    let Some(device) = std::env::var("SOTTO_TEST_CAPTURE_DEVICE").ok() else {
        eprintln!("SKIP: SOTTO_TEST_CAPTURE_DEVICE not set (no virtual device)");
        return;
    };
    let Some(sink) = std::env::var("SOTTO_TEST_PLAYBACK_SINK").ok() else {
        eprintln!("SKIP: SOTTO_TEST_PLAYBACK_SINK not set");
        return;
    };

    let bin = std::env::var("SOTTO_STT_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("target/release/sotto-stt"));
    let model = std::env::var("SOTTO_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("model"));
    if !bin.is_file() || !model.join("model.safetensors").is_file() {
        eprintln!("SKIP: sidecar or model not present");
        return;
    }

    let clip = repo_root().join("tests/fixtures/audio/medium.wav");
    if !clip.is_file() {
        eprintln!("SKIP: test clip missing");
        return;
    }

    // Start capture exactly the way the app does, on a worker thread.
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let dev = device.clone();
    let handle = std::thread::spawn(move || {
        let rec = match sotto_core::audio::Recorder::start(Some(&dev), 1.0) {
            Ok(r) => {
                let _ = ready_tx.send(Ok(()));
                r
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e.to_string()));
                return None;
            }
        };
        let _ = stop_rx.recv();
        Some(rec.stop())
    });

    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("could not open capture device {device}: {e}"),
        Err(_) => panic!("capture thread died before starting"),
    }

    // Feed the device, then stop.
    let mut player = play_into_sink(&sink, &clip);
    let played = player.wait().is_ok();
    assert!(played, "playback helper failed");
    // A small tail so the last word is not clipped.
    std::thread::sleep(std::time::Duration::from_millis(400));
    let _ = stop_tx.send(());

    let recorded = handle
        .join()
        .expect("capture thread panicked")
        .expect("capture produced nothing");

    // The capture must contain a plausible amount of audio.
    let ms = recorded.duration_ms();
    assert!(
        ms > 1500,
        "captured only {ms} ms; the device did not deliver audio"
    );

    // It must survive the round trip to disk as a valid WAV.
    let out_dir = std::env::temp_dir().join(format!("sotto-capture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();
    let wav = out_dir.join("capture.wav");
    recorded.write_wav(&wav).expect("write wav");
    let bytes = std::fs::metadata(&wav).unwrap().len();
    assert!(bytes > 44 + 32_000, "wav is implausibly small: {bytes} bytes");
    assert_eq!(&std::fs::read(&wav).unwrap()[..4], b"RIFF");

    // And the sidecar must turn it into words.
    let mut client = sotto_core::stt::SttClient::start(&bin, &model).expect("sidecar start");
    let tr = client.transcribe(&wav, false).expect("transcribe");
    assert!(
        !tr.text.trim().is_empty(),
        "the captured audio produced no transcript"
    );
    eprintln!("captured {ms} ms -> {:?}", tr.text);
    client.shutdown();

    let _ = std::fs::remove_dir_all(&out_dir);
}

#[test]
fn capture_reports_a_missing_device_instead_of_hanging() {
    // A device that cannot exist must fail fast with a message naming it, rather
    // than starting a recording that silently captures nothing.
    let t0 = std::time::Instant::now();
    // `Recorder` is not `Debug` (it owns a live audio stream), so match rather
    // than `expect_err`.
    let msg = match sotto_core::audio::Recorder::start(Some("definitely-not-a-real-device"), 1.0) {
        Ok(_) => panic!("a nonexistent device must not open"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("definitely-not-a-real-device") || msg.to_lowercase().contains("not found"),
        "the error should name the device, got: {msg}"
    );
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(5),
        "failing to open a device should be immediate"
    );
}