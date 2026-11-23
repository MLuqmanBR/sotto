//! Playback through the app's own host thread and command surface.
//!
//! The History page plays a recording by calling `play_recording` with a take
//! id; that command resolves the id through the store and hands the path to
//! `playback_host`. This test drives exactly that path — a real store, a real
//! recording on disk, the real playback thread — and asserts the position
//! advances, which can only happen if an output device is consuming samples.
//!
//! Skipped unless `SOTTO_PLAYBACK_TEST=1`, because it opens a real stream. Point
//! it at a virtual sink so nothing is heard:
//!
//! ```text
//! pactl load-module module-null-sink sink_name=sotto_test_out
//! SOTTO_PLAYBACK_TEST=1 PULSE_SINK=sotto_test_out cargo test -p sotto --test playback_host
//! ```

use sotto_core::audio::Recorded;
use sotto_core::store::{Attempt, Store, TakeStatus};
use std::path::PathBuf;

fn enabled() -> bool {
    if std::env::var("SOTTO_PLAYBACK_TEST").as_deref() != Ok("1") {
        eprintln!("SKIP: set SOTTO_PLAYBACK_TEST=1 to run playback tests");
        return false;
    }
    true
}

/// A take with a real recording, stored exactly as the capture path stores one.
fn seeded(root: &std::path::Path) -> PathBuf {
    let store = Store::open(root.join("history")).unwrap();
    store.create_take("t-play", "2026-11-23T01:00:00Z").unwrap();
    let path = store.audio_path_for("t-play");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    // 1.2 s of a quiet tone: long enough that the position moves measurably.
    let rate = 16_000u32;
    let n = (rate as f64 * 1.2) as usize;
    let samples: Vec<f32> = (0..n)
        .map(|i| 0.15 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / rate as f64).sin() as f32)
        .collect();
    Recorded {
        samples_16k: samples,
        source_rate: rate,
        source_channels: 1,
    }
    .write_wav(&path)
    .unwrap();

    store
        .finish_take("t-play", 1200, TakeStatus::Transcribed, Some(&path))
        .unwrap();
    store
        .add_attempt(&Attempt {
            id: "a-play".into(),
            take_id: "t-play".into(),
            raw_text: "playback".into(),
            corrected_text: None,
            model: "parakeet-redux".into(),
            decode_ms: 10,
            corrections_json: "[]".into(),
            created_at: "2026-11-23T01:00:00Z".into(),
        })
        .unwrap();
    path
}

#[test]
fn a_stored_recording_plays_through_the_host() {
    if !enabled() {
        return;
    }
    let root = std::env::temp_dir().join(format!("sotto-hostplay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let path = seeded(&root);

    // The command resolves the id to a path, then hands it to the host thread.
    sotto::playback_host::play(path).expect("playback should start");

    let mut advanced = false;
    let mut saw_total = 0u64;
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(40));
        let s = sotto::playback_host::status();
        if s.duration_ms > 0 {
            saw_total = s.duration_ms;
        }
        if s.position_ms > 0 {
            advanced = true;
            break;
        }
    }
    sotto::playback_host::stop();

    assert!(advanced, "the host reported no progress; playback is not working");
    assert!(saw_total > 0, "the host should report the recording's length");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn stop_ends_playback_and_the_host_reports_idle() {
    if !enabled() {
        return;
    }
    let root = std::env::temp_dir().join(format!("sotto-hoststop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let path = seeded(&root);

    sotto::playback_host::play(path).expect("playback should start");
    std::thread::sleep(std::time::Duration::from_millis(150));
    sotto::playback_host::stop();

    // The host clears its player on stop, so status must settle to idle.
    let mut idle = false;
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(40));
        if !sotto::playback_host::status().playing {
            idle = true;
            break;
        }
    }
    assert!(idle, "playback should report not-playing after stop");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_recording_is_refused_not_played() {
    if !enabled() {
        return;
    }
    // The command refuses a path that is not there before touching a device.
    let err = sotto::playback_host::play(std::path::PathBuf::from("/nonexistent/x.wav"))
        .expect_err("a missing file must not start playback");
    let msg = err.to_string();
    assert!(
        msg.contains("not found") || msg.contains("missing") || msg.contains("could not read"),
        "the error should say the recording is gone, got: {msg}"
    );
}
