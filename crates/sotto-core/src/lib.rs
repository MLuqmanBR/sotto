//! Sotto core: everything the UI needs, with no UI dependency.
//!
//! Layers, from the bottom:
//! - [`config`] — validated, atomically-persisted settings with revisions.
//! - [`store`] — SQLite history; raw and corrected text kept apart.
//! - [`audio`] — capture to 16 kHz mono, WAV writing.
//! - [`stt`] — client for the `sotto-stt` sidecar (the only speech engine).
//! - [`delivery`] — honest insertion into the focused app.
//! - [`paths`] — where things live.
//!
//! The dictionary itself lives in the `sotto-dict` crate.

pub mod audio;
pub mod autostart;
pub mod config;
pub mod delivery;
pub mod paths;
pub mod playback;
pub mod mute;
pub mod stt;
pub mod store;

pub use config::Settings;
pub use store::{Attempt, HistoryEntry, Store, Take, TakeStatus};

/// The product name, used in window titles and about screens.
pub const APP_NAME: &str = "Sotto";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build the dictionary from settings plus the user's stored entries.
pub fn dictionary_from(
    settings: &Settings,
    entries: Vec<sotto_dict::Entry>,
    replacements: Vec<sotto_dict::Replacement>,
) -> sotto_dict::Dictionary {
    sotto_dict::Dictionary {
        entries,
        replacements,
        fuzzy_enabled: settings.dictionary.fuzzy_enabled,
        phonetic_enabled: settings.dictionary.phonetic_enabled,
        fuzzy_threshold: settings.dictionary.fuzzy_threshold,
    }
}

/// Load dictionary entries from `dictionary.json` next to the settings file.
pub fn load_dictionary(path: &std::path::Path) -> (Vec<sotto_dict::Entry>, Vec<sotto_dict::Replacement>) {
    #[derive(serde::Deserialize, Default)]
    struct File {
        #[serde(default)]
        entries: Vec<sotto_dict::Entry>,
        #[serde(default)]
        replacements: Vec<sotto_dict::Replacement>,
    }
    match std::fs::read(path) {
        Ok(b) => match serde_json::from_slice::<File>(&b) {
            Ok(f) => (f.entries, f.replacements),
            Err(e) => {
                eprintln!("dictionary.json is invalid ({e}); starting empty");
                (Vec::new(), Vec::new())
            }
        },
        Err(_) => (Vec::new(), Vec::new()),
    }
}

/// Atomically save dictionary entries.
pub fn save_dictionary(
    path: &std::path::Path,
    entries: &[sotto_dict::Entry],
    replacements: &[sotto_dict::Replacement],
) -> std::io::Result<()> {
    #[derive(serde::Serialize)]
    struct File<'a> {
        version: u32,
        entries: &'a [sotto_dict::Entry],
        replacements: &'a [sotto_dict::Replacement],
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(&File {
        version: 1,
        entries,
        replacements,
    })?;
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
