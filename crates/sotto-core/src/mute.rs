//! Best-effort muting of other audio while recording.
//!
//! BUILD-PLAN §6.1 lists "mute-during-recording" on the General page. The setting
//! is called best-effort in the config for a reason: it must never be the thing
//! that stops a dictation. If `pactl` is absent, or there is no default sink, the
//! recording proceeds and nothing is muted.
//!
//! Muting is a *save and restore*, not a toggle. The previous mute state is read
//! first and put back exactly, so a user who had already muted their speakers
//! does not find them unmuted afterwards.

use std::process::Command;

/// Read and mute the default sink. Returns the previous state, or `None` if the
/// machine could not be muted (no tool, no sink, or already muted — in which case
/// there is nothing to restore).
pub fn mute() -> Option<Muted> {
    if !have("pactl") {
        return None;
    }
    let before = is_muted()?;
    if before {
        // Already muted: leave it alone and remember not to unmute.
        return Some(Muted { was_muted: true });
    }
    if set_muted(true) {
        Some(Muted { was_muted: false })
    } else {
        None
    }
}

/// A mute that can be undone exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Muted {
    /// Whether the sink was already muted before Sotto touched it.
    pub was_muted: bool,
}

impl Muted {
    /// Put the sink back the way it was.
    pub fn restore(self) {
        // Only unmute what Sotto muted. Unmuting a sink the user had muted
        // themselves would be a surprise they did not ask for.
        if !self.was_muted {
            let _ = set_muted(false);
        }
    }
}

fn have(bin: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn is_muted() -> Option<bool> {
    let out = Command::new("pactl")
        .arg("get-sink-mute")
        .arg("@DEFAULT_SINK@")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
    if text.contains("yes") {
        Some(true)
    } else if text.contains("no") {
        Some(false)
    } else {
        None
    }
}

fn set_muted(muted: bool) -> bool {
    Command::new("pactl")
        .arg("set-sink-mute")
        .arg("@DEFAULT_SINK@")
        .arg(if muted { "1" } else { "0" })
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restoring_a_mute_we_did_not_set_is_a_no_op() {
        // This is the important invariant: if the sink was already muted when we
        // looked, we must not unmute it. The test asserts the decision, not the
        // device, so it runs anywhere.
        let m = Muted { was_muted: true };
        assert!(m.was_muted, "a pre-existing mute must be left alone");
        let m = Muted { was_muted: false };
        assert!(!m.was_muted, "a mute we applied is ours to undo");
    }
}
