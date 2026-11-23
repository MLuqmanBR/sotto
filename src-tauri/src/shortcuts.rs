//! Translating stored key chords into global-shortcut accelerators.
//!
//! The UI stores chords the way people write them (`Ctrl+Shift+Space`), because
//! that is what the user typed and what the settings file should contain. The
//! shortcut plugin wants X11 keysym names (`Control+Shift+Space`). This module is
//! the single place that translation happens, so a stored chord never has to
//! change if the plugin's vocabulary does.
//!
//! It lives in the library rather than the binary so it can be tested directly:
//! a chord that silently fails to register is invisible to the user, which makes
//! this exactly the kind of code that needs tests.

/// Modifier names the UI can produce, mapped to the plugin's spelling.
fn map_modifier(name: &str) -> Option<&'static str> {
    match name {
        "Ctrl" | "Control" => Some("Control"),
        "Alt" => Some("Alt"),
        "Shift" => Some("Shift"),
        "Super" | "Meta" | "Win" | "Command" | "Cmd" => Some("Super"),
        _ => None,
    }
}

fn is_modifier(part: &str) -> bool {
    map_modifier(part).is_some()
}

/// Chords this app must never register, with the reason to show a user.
///
/// `Ctrl+Alt+F1`–`F12` are the kernel's virtual-terminal switches. The kernel
/// consumes them before any application sees the key, so registering one does
/// not intercept it: the screen still switches away from the desktop. On this
/// project a test binding of `Ctrl+Alt+F9` left the whole machine's keyboard and
/// mouse unresponsive and required a hard reset. Refusing these is not
/// politeness, it is the only safe behaviour.
///
/// `Ctrl+Alt+Backspace` kills the display server, `Ctrl+Alt+Delete` restarts or
/// logs out on many systems, and `Alt+SysRq` is the kernel's debug facility.
/// None of them can do anything a dictation shortcut is for.
pub fn reserved_reason(chord: &str) -> Option<&'static str> {
    let parts: Vec<String> = chord
        .split('+')
        .map(|p| p.trim().to_ascii_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    let has = |name: &str| parts.iter().any(|p| p == name);
    let key = parts.last().map(String::as_str).unwrap_or("");
    let ctrl = has("ctrl") || has("control");
    let alt = has("alt");

    if ctrl && alt {
        // Every function key, not just F1–F12: kernels and keymaps can map the
        // higher ones to consoles too, and no dictation user needs this family.
        if let Some(rest) = key.strip_prefix('f') {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                return Some("the system uses it to switch virtual terminals");
            }
        }
        if key == "backspace" {
            return Some("it terminates the display server");
        }
        if key == "delete" {
            return Some("it restarts or logs out the machine on many systems");
        }
    }
    if alt && has("sysrq") {
        return Some("it is the kernel's debug facility");
    }
    None
}

/// Convert a stored chord to the accelerator syntax the plugin expects.
///
/// Returns `None` for anything that would be unsafe or meaningless to register:
///
/// - an empty chord;
/// - a chord made only of modifiers, which would swallow the whole keyboard;
/// - a modifier appearing anywhere but the end (the user meant a key);
/// - a chord with more than one non-modifier key;
/// - a chord the operating system claims before any application, such as the
///   virtual-terminal switches (see [`reserved_reason`]).
pub fn to_accelerator(chord: &str) -> Option<String> {
    let parts: Vec<&str> = chord
        .split('+')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }
    if reserved_reason(chord).is_some() {
        return None;
    }

    // Exactly one key, and it must be last. Two keys cannot be one shortcut.
    let key_positions: Vec<usize> = parts
        .iter()
        .enumerate()
        .filter(|(_, p)| !is_modifier(p))
        .map(|(i, _)| i)
        .collect();
    if key_positions.len() != 1 {
        return None;
    }
    let key_at = key_positions[0];
    if key_at != parts.len() - 1 {
        return None;
    }

    let mut out = Vec::with_capacity(parts.len());
    for (i, p) in parts.iter().enumerate() {
        if i == key_at {
            out.push((*p).to_string());
        } else {
            out.push(map_modifier(p)?.to_string());
        }
    }
    Some(out.join("+"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_modifier_chord_maps_to_plugin_names() {
        assert_eq!(to_accelerator("Ctrl+Shift+Space").as_deref(), Some("Control+Shift+Space"));
        assert_eq!(to_accelerator("Ctrl+D").as_deref(), Some("Control+D"));
        assert_eq!(to_accelerator("Alt+Space").as_deref(), Some("Alt+Space"));
        assert_eq!(to_accelerator("Super+D").as_deref(), Some("Super+D"));
    }

    #[test]
    fn meta_is_an_alias_for_super() {
        assert_eq!(to_accelerator("Meta+D").as_deref(), Some("Super+D"));
        assert_eq!(to_accelerator("Win+D").as_deref(), Some("Super+D"));
    }

    #[test]
    fn whitespace_around_parts_is_ignored() {
        assert_eq!(to_accelerator(" Ctrl + Alt + K ").as_deref(), Some("Control+Alt+K"));
    }

    #[test]
    fn modifiers_only_is_refused() {
        // Registering this would capture every keypress on the system.
        assert_eq!(to_accelerator("Ctrl"), None);
        assert_eq!(to_accelerator("Ctrl+Shift"), None);
        assert_eq!(to_accelerator("Ctrl+Alt+Shift+Super"), None);
    }

    #[test]
    fn empty_chord_is_refused() {
        assert_eq!(to_accelerator(""), None);
        assert_eq!(to_accelerator("   "), None);
        assert_eq!(to_accelerator("++"), None);
    }

    #[test]
    fn modifier_after_the_key_is_refused() {
        // "Ctrl+Space+Shift" is almost certainly a recording mistake; refusing it
        // is better than registering something the user did not ask for.
        assert_eq!(to_accelerator("Ctrl+Space+Shift"), None);
    }

    #[test]
    fn two_keys_are_refused() {
        assert_eq!(to_accelerator("Ctrl+A+B"), None);
    }

    #[test]
    fn single_key_without_modifier_is_allowed() {
        // Unusual but legal, and the user may genuinely want it.
        assert_eq!(to_accelerator("F9").as_deref(), Some("F9"));
        assert_eq!(to_accelerator("Pause").as_deref(), Some("Pause"));
    }

    #[test]
    fn unknown_modifier_name_is_refused() {
        assert_eq!(to_accelerator("Hyper+D"), None);
    }

    #[test]
    fn function_keys_and_named_keys_pass_through() {
        assert_eq!(to_accelerator("Ctrl+F5").as_deref(), Some("Control+F5"));
        assert_eq!(to_accelerator("Ctrl+Escape").as_deref(), Some("Control+Escape"));
    }

    // ── reserved chords (kernel-owned, never safe to bind) ────────────────

    #[test]
    fn virtual_terminal_chords_are_refused() {
        // Ctrl+Alt+F1–F12 switch virtual terminals in the kernel, which takes
        // them before any application can. Binding one does not intercept it; it
        // switches the screen away and, on this project, hung the whole machine.
        for n in 1..=12 {
            let chord = format!("Ctrl+Alt+F{n}");
            assert_eq!(
                to_accelerator(&chord),
                None,
                "{chord} must never reach the shortcut plugin"
            );
            assert!(
                reserved_reason(&chord).is_some(),
                "{chord} must explain why it was refused"
            );
        }
        // The exact chord that hung the keyboard, in the spellings a UI or a
        // hand-edited settings file can produce.
        assert!(reserved_reason("ctrl + alt + f9").is_some());
        assert!(reserved_reason("Control+Alt+F9").is_some());
        assert!(reserved_reason("CTRL+ALT+f1").is_some());
        // Higher function keys are also possible console switches.
        assert_eq!(to_accelerator("Ctrl+Alt+F13"), None);
    }

    #[test]
    fn near_misses_are_still_allowed() {
        // Only the kernel-owned combinations are refused. A Shift-added chord,
        // a letter, or a bare function key are all ordinary shortcuts.
        assert_eq!(
            to_accelerator("Ctrl+Shift+F9").as_deref(),
            Some("Control+Shift+F9")
        );
        assert_eq!(
            to_accelerator("Ctrl+Alt+C").as_deref(),
            Some("Control+Alt+C")
        );
        assert_eq!(to_accelerator("Alt+F9").as_deref(), Some("Alt+F9"));
        // Space plus the same modifiers is a key, not a VT chord: allowed.
        assert_eq!(
            to_accelerator("Ctrl+Alt+Space").as_deref(),
            Some("Control+Alt+Space")
        );
    }

    #[test]
    fn other_system_owned_chords_are_refused() {
        for chord in ["Ctrl+Alt+Backspace", "Ctrl+Alt+Delete", "Alt+SysRq"] {
            assert_eq!(
                to_accelerator(chord),
                None,
                "{chord} must never reach the shortcut plugin"
            );
            assert!(
                reserved_reason(chord).is_some(),
                "{chord} must explain why it was refused"
            );
        }
        // SysRq with a command key is a kernel combo too, not just bare SysRq.
        assert!(reserved_reason("Alt+SysRq+B").is_some());
    }

    // ── edge semantics (BUILD-PLAN §9, A05–A07) ──────────────────────────

    #[test]
    fn hold_starts_on_press_and_stops_on_release() {
        // The whole point of a hold binding: two edges, two effects.
        assert_eq!(decide("hold", true, false, false), EdgeAction::Start);
        assert_eq!(decide("hold", false, true, true), EdgeAction::Stop);
    }

    #[test]
    fn a_held_chord_does_not_open_a_second_take() {
        // Key repeat fires more presses while the key is down.
        assert_eq!(decide("hold", true, true, true), EdgeAction::Ignore);
    }

    #[test]
    fn releasing_the_hold_chord_leaves_a_toggle_take_running() {
        // Otherwise pressing hold, then toggle, then letting go of hold would
        // end the toggle's take.
        assert_eq!(decide("hold", false, true, false), EdgeAction::Ignore);
    }

    #[test]
    fn a_stale_hold_release_after_the_take_ended_does_nothing() {
        // The user stopped with the button, then released the chord.
        assert_eq!(decide("hold", false, false, true), EdgeAction::Ignore);
    }

    #[test]
    fn toggle_stops_on_the_second_press_regardless_of_duration() {
        assert_eq!(decide("toggle", true, false, false), EdgeAction::Start);
        assert_eq!(decide("toggle", true, true, false), EdgeAction::Stop);
        // A toggle take is not hold-owned, so its own release is meaningless.
        assert_eq!(decide("toggle", false, true, false), EdgeAction::Ignore);
    }

    #[test]
    fn cancel_only_cancels_and_never_starts() {
        assert_eq!(decide("cancel", true, true, true), EdgeAction::Cancel);
        assert_eq!(decide("cancel", true, false, false), EdgeAction::Ignore);
        assert_eq!(decide("cancel", false, true, true), EdgeAction::Ignore);
    }

    #[test]
    fn an_unknown_role_does_nothing() {
        assert_eq!(decide("nonsense", true, false, false), EdgeAction::Ignore);
    }
}


/// What one shortcut event should do.
///
/// This is the decision table for the three bindings, kept as a pure function so
/// the semantics can be tested without a compositor, a real chord, or a
/// microphone. It exists because "hold" is easy to implement as a toggle by
/// accident: acting on the press alone produces a binding that starts a take and
/// never ends it, which looks fine in code and is wrong in use.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum EdgeAction {
    /// Open a take.
    Start,
    /// End the running take and deliver it.
    Stop,
    /// Discard the running take.
    Cancel,
    /// Nothing to do for this edge.
    Ignore,
}

/// Decide what a shortcut event means.
///
/// * `role` — `"hold"`, `"toggle"` or `"cancel"`.
/// * `pressed` — the edge that fired.
/// * `recording` — whether a take is running.
/// * `hold_active` — whether the running take was started by the hold chord.
pub fn decide(role: &str, pressed: bool, recording: bool, hold_active: bool) -> EdgeAction {
    match (role, pressed) {
        // Press opens a take; a repeat press while one is open is ignored, which
        // is the repeat suppression the plan requires.
        ("hold", true) if !recording => EdgeAction::Start,
        ("hold", true) => EdgeAction::Ignore,
        // Release ends only a take the hold chord started. Releasing the chord
        // must never cut short a take the user began with the toggle chord.
        ("hold", false) if hold_active && recording => EdgeAction::Stop,
        ("hold", false) => EdgeAction::Ignore,
        // Toggle acts on the press alone; press duration is irrelevant.
        ("toggle", true) if recording => EdgeAction::Stop,
        ("toggle", true) => EdgeAction::Start,
        ("toggle", false) => EdgeAction::Ignore,
        // Cancel only ever cancels something.
        ("cancel", true) if recording => EdgeAction::Cancel,
        _ => EdgeAction::Ignore,
    }
}
