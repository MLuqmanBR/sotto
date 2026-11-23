//! The safety ceiling on a single take.
//!
//! A hold binding depends on a release edge. If the compositor drops that
//! release — or the machine suspends mid-hold, or the shortcut backend goes away
//! — a take would otherwise run forever, recording the room and growing a file
//! with no upper bound. BUILD-PLAN §9 requires a bounded maximum; this proves
//! the bound actually fires and actually stops the right take.
//!
//! Opt-in, like the capture test, because it opens a real input device:
//!
//! ```text
//! SOTTO_TEST_CAPTURE_DEVICE=<virtual source> SOTTO_MAX_TAKE_SECS=1 \
//!   cargo test -p sotto --test take_ceiling
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// These tests share two process-wide things: the ceiling environment variable
/// and the capture device. The test harness runs test functions in parallel, so
/// without this lock one test's `remove_var` could land between another's `set_var`
/// and its watchdog check — a failure that says nothing about the product. One
/// lock, held for the whole body, makes the run deterministic.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn device() -> Option<String> {
    match std::env::var("SOTTO_TEST_CAPTURE_DEVICE") {
        Ok(d) if !d.trim().is_empty() => Some(d),
        _ => {
            eprintln!("SKIP: SOTTO_TEST_CAPTURE_DEVICE not set (no virtual device)");
            None
        }
    }
}

fn app() -> (Arc<sotto::pipeline::App>, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "sotto-ceiling-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::fs::create_dir_all(root.join("history")).unwrap();
    let a = sotto::pipeline::App::new(&root, root.join("model"));
    (Arc::new(a), root)
}

#[test]
fn a_take_that_outlives_the_ceiling_is_stopped() {
    let _guard = serial();
    let Some(dev) = device() else { return };
    // A one-second ceiling keeps the test quick; the real value is five minutes.
    std::env::set_var("SOTTO_MAX_TAKE_SECS", "1");

    let (app, root) = app();
    let rec = sotto::pipeline::start_capture(Some(dev), 1.0).expect("capture should start");
    let take_id = rec.take_id.clone();
    *app.recorder.lock().unwrap() = Some(rec);

    let fired = Arc::new(AtomicBool::new(false));
    let f = Arc::clone(&fired);
    app.watch_take(take_id.clone(), move || {
        f.store(true, Ordering::SeqCst);
    });

    // Well past the ceiling plus the watchdog's poll interval.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    while std::time::Instant::now() < deadline && !fired.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        fired.load(Ordering::SeqCst),
        "the ceiling did not fire; a lost release edge would record forever"
    );

    // Clean up the live stream the way the app would.
    let rec = app.recorder.lock().unwrap().take();
    if let Some(rec) = rec {
        let _ = rec.stop_tx.send(());
        let _ = rec.join.join();
    }
    std::env::remove_var("SOTTO_MAX_TAKE_SECS");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_watchdog_does_not_stop_a_take_that_already_ended() {
    let _guard = serial();
    let Some(dev) = device() else { return };
    std::env::set_var("SOTTO_MAX_TAKE_SECS", "1");

    let (app, root) = app();
    let rec = sotto::pipeline::start_capture(Some(dev), 1.0).expect("capture should start");
    let take_id = rec.take_id.clone();
    *app.recorder.lock().unwrap() = Some(rec);

    let fired = Arc::new(AtomicBool::new(false));
    let f = Arc::clone(&fired);
    app.watch_take(take_id.clone(), move || {
        f.store(true, Ordering::SeqCst);
    });

    // The user stops normally, before the ceiling. The watchdog must notice the
    // take is gone and do nothing — otherwise a late stop would cut off whatever
    // the user started next.
    std::thread::sleep(std::time::Duration::from_millis(150));
    let rec = app.recorder.lock().unwrap().take();
    if let Some(rec) = rec {
        let _ = rec.stop_tx.send(());
        let _ = rec.join.join();
    }

    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(
        !fired.load(Ordering::SeqCst),
        "a stale watchdog fired on a take that had already ended"
    );

    std::env::remove_var("SOTTO_MAX_TAKE_SECS");
    let _ = std::fs::remove_dir_all(&root);
}
