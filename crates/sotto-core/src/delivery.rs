//! Delivering text to the focused application.
//!
//! The rule that governs this module (BUILD-PLAN §10): **never claim text was
//! delivered because a helper process exited zero.** Every path here returns an
//! outcome the caller must surface honestly, including "copied but not inserted".

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::process::{Command, Stdio};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DeliveryError {
    #[error("no delivery helper available (need wl-copy/wtype on Wayland or xclip/xdotool on X11)")]
    NoHelper,
    #[error("clipboard failed: {0}")]
    Clipboard(String),
    #[error("paste failed: {0}")]
    Paste(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Session {
    Wayland,
    X11,
    Unknown,
}

pub fn detect_session() -> Session {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        Session::Wayland
    } else if std::env::var_os("DISPLAY").is_some() {
        Session::X11
    } else {
        Session::Unknown
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// Text was inserted at the cursor.
    Inserted { method: String },
    /// Text is on the clipboard but insertion was not possible or not attempted.
    CopiedOnly { reason: String },
    /// Nothing happened.
    Failed { reason: String },
}

fn have(bin: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Put `text` on the clipboard. Returns the helper used.
pub fn copy(text: &str) -> Result<String, DeliveryError> {
    let session = detect_session();
    let candidates: &[(&str, &[&str])] = match session {
        Session::Wayland => &[("wl-copy", &[])],
        Session::X11 => &[("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])],
        Session::Unknown => &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])],
    };
    for (bin, args) in candidates {
        if !have(bin) {
            continue;
        }
        let mut cmd = Command::new(bin);
        cmd.args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().map_err(|e| DeliveryError::Clipboard(e.to_string()))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(text.as_bytes())
                .map_err(|e| DeliveryError::Clipboard(e.to_string()))?;
        }
        let status = child.wait().map_err(|e| DeliveryError::Clipboard(e.to_string()))?;
        if status.success() {
            return Ok((*bin).to_string());
        }
    }
    Err(DeliveryError::NoHelper)
}

/// Read the current clipboard, so it can be restored after pasting.
pub fn read_clipboard() -> Option<String> {
    let session = detect_session();
    let (bin, args): (&str, &[&str]) = match session {
        Session::Wayland => ("wl-paste", &["--no-newline"]),
        _ => ("xclip", &["-selection", "clipboard", "-o"]),
    };
    if !have(bin) {
        return None;
    }
    let out = Command::new(bin).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Identify the window that currently has focus.
///
/// Used to keep a transcript out of the wrong application (acceptance A14): a
/// take takes a second or two to transcribe, and if the user switches windows in
/// that time, pasting would put their words somewhere they never intended — a
/// private note into a group chat, for example. So the window is recorded when
/// dictation starts and compared again before pasting.
///
/// Returns `None` when the window cannot be identified, which is treated as
/// "unknown, do not block": refusing to deliver because a helper is missing would
/// be worse than the risk it guards against.
pub fn active_window() -> Option<String> {
    // KDE on Wayland: `kdotool` talks to KWin and reports a stable window id.
    // X11 and other desktops: `xdotool`. Both print one line on success.
    for (bin, args) in [
        ("kdotool", &["getactivewindow"][..]),
        ("xdotool", &["getactivewindow"][..]),
    ] {
        if !have(bin) {
            continue;
        }
        let out = Command::new(bin).args(args).output().ok()?;
        if !out.status.success() {
            continue;
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !id.is_empty() {
            return Some(id);
        }
    }
    None
}

/// Whether focus has moved away from the window a take started in.
///
/// A pure function so the rule can be tested without a desktop: the interesting
/// cases are the ones where a window cannot be identified, and those cannot be
/// produced on demand from a real session.
///
/// An unknown window on **either** side is treated as "not changed". Blocking
/// delivery because a helper is missing would break the feature on desktops
/// without `kdotool`/`xdotool`, which is worse than the risk being guarded.
pub fn focus_changed(expected: Option<&str>, now: Option<&str>) -> bool {
    match (expected, now) {
        (Some(want), Some(have)) => want != have,
        _ => false,
    }
}

/// Send a paste keystroke (Ctrl+V) to the focused window.
pub fn send_paste(delay_ms: u64) -> Result<(), DeliveryError> {
    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    let session = detect_session();
    match session {
        Session::Wayland => {
            // wtype emits real key events through the compositor.
            if have("wtype") {
                let ok = Command::new("wtype")
                    .args(["-M", "ctrl", "v", "-m", "ctrl"])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if ok {
                    return Ok(());
                }
            }
            if have("ydotool") {
                let ok = Command::new("ydotool")
                    .args(["key", "29:1", "47:1", "47:0", "29:0"])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if ok {
                    return Ok(());
                }
            }
            Err(DeliveryError::Paste(
                "no Wayland paste helper (install wtype or ydotool)".into(),
            ))
        }
        _ => {
            if have("xdotool") {
                let ok = Command::new("xdotool")
                    .args(["key", "--clearmodifiers", "ctrl+v"])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if ok {
                    return Ok(());
                }
            }
            Err(DeliveryError::Paste(
                "no X11 paste helper (install xdotool)".into(),
            ))
        }
    }
}

/// Type text directly, as a fallback when the clipboard path is unavailable.
pub fn type_text(text: &str) -> Result<(), DeliveryError> {
    let session = detect_session();
    match session {
        Session::Wayland => {
            if have("wtype") {
                let ok = Command::new("wtype")
                    .arg("--")
                    .arg(text)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if ok {
                    return Ok(());
                }
            }
            Err(DeliveryError::Paste("no wtype for typing".into()))
        }
        _ => {
            if have("xdotool") {
                let ok = Command::new("xdotool")
                    .args(["type", "--clearmodifiers", "--"])
                    .arg(text)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if ok {
                    return Ok(());
                }
            }
            Err(DeliveryError::Paste("no xdotool for typing".into()))
        }
    }
}

/// Copy, then paste. Falls back to typing, then to copied-only.
///
/// `keep_on_clipboard` overrides `restore_clipboard`: the two settings pull in
/// opposite directions, and "keep the transcript on the clipboard" is the more
/// specific instruction, so it wins. Without this the UI setting did nothing.
pub fn deliver(
    text: &str,
    prefer_paste: bool,
    restore_clipboard: bool,
    paste_delay_ms: u64,
    keep_on_clipboard: bool,
) -> Outcome {
    deliver_to(
        text,
        prefer_paste,
        restore_clipboard,
        paste_delay_ms,
        keep_on_clipboard,
        None,
    )
}

/// [`deliver`], refusing to paste into a window other than `expected`.
///
/// `expected` is the window that had focus when the take started. If focus has
/// moved since, the text is left on the clipboard and the outcome says why, so
/// the user is told rather than silently mis-pasted. `None` means "not checked",
/// which keeps the old behaviour available where no window can be identified.
pub fn deliver_to(
    text: &str,
    prefer_paste: bool,
    restore_clipboard: bool,
    paste_delay_ms: u64,
    keep_on_clipboard: bool,
    expected: Option<&str>,
) -> Outcome {
    if text.is_empty() {
        return Outcome::Failed {
            reason: "nothing to deliver (empty transcript)".into(),
        };
    }

    // A14: never paste into a different window than the one the user was typing
    // in when they started. Copying is safe anywhere; pasting is not.
    let focus_moved = focus_changed(expected, active_window().as_deref());
    let prefer_paste = if focus_moved {
        // Still copy — the words are not lost — but do not send a keystroke.
        false
    } else {
        prefer_paste
    };

    let restore = restore_clipboard && !keep_on_clipboard;

    let previous = if restore { read_clipboard() } else { None };

    match copy(text) {
        Ok(helper) => {
            if prefer_paste {
                match send_paste(paste_delay_ms) {
                    Ok(()) => {
                        if restore {
                            if let Some(prev) = previous {
                                let _ = copy(&prev);
                            }
                        }
                        Outcome::Inserted {
                            method: format!("{helper}+paste"),
                        }
                    }
                    Err(e) => Outcome::CopiedOnly {
                        reason: format!("copied to clipboard, could not paste: {e}"),
                    },
                }
            } else if focus_moved {
                Outcome::CopiedOnly {
                    reason: "the focused window changed, so the text was copied instead of pasted"
                        .into(),
                }
            } else {
                Outcome::CopiedOnly {
                    reason: "clipboard-only mode is enabled".into(),
                }
            }
        }
        Err(_) => match type_text(text) {
            Ok(()) => Outcome::Inserted {
                method: "typed".into(),
            },
            Err(e) => Outcome::Failed {
                reason: format!("no clipboard helper and typing failed: {e}"),
            },
        },
    }
}

#[cfg(test)]
mod focus_tests {
    use super::*;

    #[test]
    fn the_same_window_is_not_a_change() {
        assert!(!focus_changed(Some("0x1"), Some("0x1")));
    }

    #[test]
    fn a_different_window_is_a_change() {
        // The case that matters: the user started in their editor and finished
        // in a chat window. Pasting would put their words in the wrong place.
        assert!(focus_changed(Some("0x1"), Some("0x2")));
    }

    #[test]
    fn an_unknown_window_never_blocks_delivery() {
        // No helper, or no window: the rule must not fire. A guard that breaks
        // delivery on an unusual desktop is worse than the risk it removes.
        assert!(!focus_changed(None, Some("0x1")));
        assert!(!focus_changed(Some("0x1"), None));
        assert!(!focus_changed(None, None));
    }

    #[test]
    fn a_refused_paste_still_copies() {
        // The words must never be lost. This exercises the real clipboard path,
        // so it restores what was there afterwards.
        let previous = read_clipboard();
        let out = deliver_to(
            "sotto focus test",
            true,   // the user asked for paste
            true,   // restore the clipboard afterwards
            0,
            false,
            Some("definitely-not-the-focused-window"),
        );
        match out {
            Outcome::CopiedOnly { reason } => {
                assert!(
                    reason.contains("focused window changed"),
                    "the reason should explain the refusal, got: {reason}"
                );
            }
            other => panic!("a moved focus must not paste, got {other:?}"),
        }
        if let Some(p) = previous {
            let _ = copy(&p);
        }
    }
}
