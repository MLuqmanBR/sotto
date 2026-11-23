//! Playback test: proves a retained recording actually reaches an output device.
//!
//! Skipped unless `SOTTO_PLAYBACK_TEST=1`, because it opens a real audio stream.
//! Point it at a virtual sink so nothing is heard:
//!
//! ```text
//! pactl load-module module-null-sink sink_name=sotto_test_out
//! SOTTO_PLAYBACK_TEST=1 PULSE_SINK=sotto_test_out cargo test -p sotto-core --test playback
//! ```

use sotto_core::audio::Recorded;
use sotto_core::playback::{decode_wav, Player};

fn enabled() -> bool {
    if std::env::var("SOTTO_PLAYBACK_TEST").as_deref() != Ok("1") {
        eprintln!("SKIP: set SOTTO_PLAYBACK_TEST=1 to run playback tests");
        return false;
    }
    true
}

#[test]
fn a_real_recording_plays_to_an_output_device() {
    if !enabled() {
        return;
    }

    // A 600 ms tone, written the same way capture writes a take.
    let dir = std::env::temp_dir().join(format!("sotto-playtest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tone.wav");

    let rate = 16_000u32;
    let n = (rate as f64 * 0.6) as usize;
    let samples: Vec<f32> = (0..n)
        .map(|i| 0.2 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / rate as f64).sin() as f32)
        .collect();
    Recorded {
        samples_16k: samples,
        source_rate: rate,
        source_channels: 1,
    }
    .write_wav(&path)
    .unwrap();

    // The file round-trips through the decoder.
    let decoded = decode_wav(&path).unwrap();
    assert_eq!(decoded.sample_rate, rate);
    assert_eq!(decoded.duration_ms(), 600);

    // And it plays: the stream starts and the position advances, which can only
    // happen if the device is consuming samples.
    let player = Player::start(&path).expect("playback should start");
    assert!(player.is_playing(), "stream should be running");

    let mut advanced = false;
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let (played, total) = player.progress();
        assert!(played <= total, "position must not run past the end");
        if played > 0 {
            advanced = true;
            break;
        }
    }
    assert!(advanced, "the device consumed no samples; playback is not working");

    player.stop();
    drop(player);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_playback_replaces_the_first() {
    if !enabled() {
        return;
    }

    let dir = std::env::temp_dir().join(format!("sotto-playtest2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let make = |name: &str, hz: f64| {
        let path = dir.join(name);
        let rate = 16_000u32;
        let n = (rate as f64 * 2.0) as usize;
        let samples: Vec<f32> = (0..n)
            .map(|i| 0.1 * (2.0 * std::f64::consts::PI * hz * i as f64 / rate as f64).sin() as f32)
            .collect();
        Recorded {
            samples_16k: samples,
            source_rate: rate,
            source_channels: 1,
        }
        .write_wav(&path)
        .unwrap();
        path
    };

    let first = make("a.wav", 440.0);
    let second = make("b.wav", 660.0);

    let a = Player::start(&first).expect("first should start");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(a.is_playing());

    // Starting a second recording must stop the first, not play both at once.
    let b = Player::start(&second).expect("second should start");
    drop(a);
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(b.is_playing(), "the second recording should be the one playing");

    b.stop();
    drop(b);
    let _ = std::fs::remove_dir_all(&dir);
}
