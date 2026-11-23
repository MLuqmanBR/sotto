//! Test-only surface for driving the real window from a script.
//!
//! **Not compiled into a release build.** It is behind the non-default `e2e`
//! feature, and `scripts/gui-e2e.sh` is the only thing that turns it on.
//!
//! Why it exists: the app's most important path — press Start, speak, press Stop,
//! see the words — lives behind the webview. Under Wayland on this machine
//! synthetic mouse clicks do not register (`ydotoold` has no socket), so there is
//! no way to press the button from outside. Everything else was covered by driving
//! library functions, which proved the pipeline but not that the *button* is
//! wired to it. This closes that gap without a permanent debug backdoor in the
//! shipped binary.
//!
//! Two pieces:
//!
//! - [`probe_write`], a command the injected script calls to report what it sees;
//! - [`install`], which evaluates a script from a file once the window is up.
//!
//! Both read their paths from the environment, so the harness decides where the
//! output goes and nothing is hardcoded.

use std::path::PathBuf;
use tauri::Manager;

/// Where the injected script writes its findings.
///
/// Defaults to a file in the temp directory rather than requiring the variable, so
/// a manual run does something predictable.
fn out_path() -> PathBuf {
    std::env::var_os("SOTTO_E2E_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("sotto-e2e.txt"))
}

/// Report what the page observed.
///
/// Deliberately appends rather than overwrites: the script reports progress as it
/// goes, and a step that hangs should still leave the steps before it on disk.
/// That is the difference between "the test failed at step 3" and "the test
/// produced nothing".
#[tauri::command]
pub fn probe_write(text: String) -> Result<(), String> {
    use std::io::Write;
    let path = out_path();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("could not open {}: {e}", path.display()))?;
    writeln!(f, "{text}").map_err(|e| e.to_string())?;
    f.flush().map_err(|e| e.to_string())
}

/// Evaluate the script named by `SOTTO_E2E_SCRIPT`, once the window is ready.
///
/// Waits for the page to finish its first load before injecting, because a script
/// that runs too early finds no buttons and reports a failure that says nothing
/// about the app.
pub fn install(handle: &tauri::AppHandle) {
    let Some(script) = std::env::var_os("SOTTO_E2E_SCRIPT").map(PathBuf::from) else {
        return;
    };
    let Ok(source) = std::fs::read_to_string(&script) else {
        eprintln!("e2e: could not read {}", script.display());
        return;
    };
    let delay_ms: u64 = std::env::var("SOTTO_E2E_DELAY_MS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(6000);

    let h = handle.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        let Some(win) = h.get_webview_window("main") else {
            eprintln!("e2e: no main window");
            return;
        };
        if let Err(e) = win.eval(&source) {
            eprintln!("e2e: eval failed: {e}");
        }
    });
}
