//! Filesystem locations, following the XDG base directory spec.
//!
//! Everything Sotto owns lives under one of two roots, so uninstall can be
//! complete and honest (acceptance A18).
//!
//! `SOTTO_ROOT` overrides both roots at once. It exists so a test or a portable
//! install can be pointed at a single directory, and every helper here honours it
//! — a helper that ignored it would report a path outside the sandbox, which is
//! both confusing and a hole in the isolation guarantee.

use std::path::PathBuf;

/// The override root, when one is set.
fn override_root() -> Option<PathBuf> {
    std::env::var_os("SOTTO_ROOT").map(PathBuf::from)
}

/// Config, settings, dictionary.
pub fn config_dir() -> PathBuf {
    if let Some(r) = override_root() {
        return r.join("config");
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".config"))
        .join("sotto")
}

/// History database, audio, downloaded model.
pub fn data_dir() -> PathBuf {
    if let Some(r) = override_root() {
        return r;
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/share"))
        .join("sotto")
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn dictionary_path() -> PathBuf {
    config_dir().join("dictionary.json")
}

pub fn history_dir() -> PathBuf {
    data_dir().join("history")
}

/// Default model directory: `~/.local/share/sotto/models/parakeet-redux`.
pub fn default_model_dir() -> PathBuf {
    data_dir().join("models").join("parakeet-redux")
}

pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

/// Override root for tests and portable installs.
pub fn set_root(root: &std::path::Path) {
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    std::env::set_var("XDG_DATA_HOME", root.join("data"));
}
