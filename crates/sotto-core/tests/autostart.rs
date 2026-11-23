//! Integration tests for start-at-login: the entry is created, updated and
//! removed, and `status()` never lies about what is on disk.
//!
//! These run against a throwaway `XDG_CONFIG_HOME` so the developer's real
//! session is never touched.

use std::path::{Path, PathBuf};

/// Point autostart at a temp dir for the duration of a test.
///
/// `std::env::set_var` is process-global, so every test in this file must run
/// under the same mutex; the helper returns the guard to make that explicit.
fn isolate(tag: &str) -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = std::env::temp_dir().join(format!(
        "sotto-autostart-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", &root);
    (root, guard)
}

#[test]
fn enabling_creates_a_valid_entry() {
    let (root, _g) = isolate("create");
    // Use a file that genuinely exists, so `target_exists` means what it says.
    let exe_path = root.join("sotto");
    std::fs::write(&exe_path, b"#!/bin/sh\n").unwrap();
    let exe = exe_path.as_path();

    let st = sotto_core::autostart::sync(true, exe).expect("sync");
    assert!(st.entry_present, "entry should exist after enabling");
    assert!(st.target_exists, "the entry should point at a real file");

    let path = root.join("autostart/sotto.desktop");
    let text = std::fs::read_to_string(&path).expect("entry file");
    assert!(text.contains("[Desktop Entry]"));
    assert!(text.contains(&format!("Exec={}", exe.display())));
    assert!(text.contains("Type=Application"));

    // status() must agree with what sync() just reported.
    let again = sotto_core::autostart::status();
    assert_eq!(again.entry_present, st.entry_present);
    assert_eq!(again.target, st.target);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn disabling_removes_the_entry() {
    let (root, _g) = isolate("remove");
    let exe = Path::new("/usr/bin/sotto");

    sotto_core::autostart::sync(true, exe).unwrap();
    assert!(sotto_core::autostart::status().entry_present);

    let st = sotto_core::autostart::sync(false, exe).unwrap();
    assert!(!st.entry_present, "entry should be gone after disabling");
    assert!(!root.join("autostart/sotto.desktop").exists());

    // Disabling twice is not an error: the desired state is already true.
    let st = sotto_core::autostart::sync(false, exe).unwrap();
    assert!(!st.entry_present);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn broken_entry_is_reported_not_hidden() {
    let (root, _g) = isolate("broken");
    // Point at a binary that does not exist, as a stale install would.
    let ghost = Path::new("/nonexistent/sotto");
    sotto_core::autostart::sync(true, ghost).unwrap();

    let st = sotto_core::autostart::status();
    assert!(st.entry_present, "the entry is still on disk");
    assert!(!st.target_exists, "but its target is gone");
    assert!(st.broken, "so it must be reported as broken");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn path_with_spaces_survives_a_round_trip() {
    let (root, _g) = isolate("spaces");
    let exe = Path::new("/home/a user/apps/sotto");

    sotto_core::autostart::sync(true, exe).unwrap();
    let st = sotto_core::autostart::status();
    assert_eq!(
        st.target.as_deref(),
        Some(Path::new("/home/a user/apps/sotto")),
        "a quoted Exec must parse back to the full path"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rewriting_the_same_path_is_a_no_op() {
    let (root, _g) = isolate("idempotent");
    let exe = Path::new("/usr/bin/sotto");
    sotto_core::autostart::sync(true, exe).unwrap();
    let path = root.join("autostart/sotto.desktop");
    let first = std::fs::read_to_string(&path).unwrap();

    sotto_core::autostart::sync(true, exe).unwrap();
    let second = std::fs::read_to_string(&path).unwrap();
    assert_eq!(first, second, "content must not change");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn changing_the_executable_updates_the_entry() {
    let (root, _g) = isolate("update");
    sotto_core::autostart::sync(true, Path::new("/opt/sotto-old/sotto")).unwrap();
    sotto_core::autostart::sync(true, Path::new("/opt/sotto-new/sotto")).unwrap();

    let text = std::fs::read_to_string(root.join("autostart/sotto.desktop")).unwrap();
    assert!(text.contains("Exec=/opt/sotto-new/sotto"));
    assert!(!text.contains("sotto-old"));

    let _ = std::fs::remove_dir_all(&root);
}
