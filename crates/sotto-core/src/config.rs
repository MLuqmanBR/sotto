//! Settings: schema, defaults, atomic persistence, and revision tracking.
//!
//! Every mutation bumps `revision` and is written atomically (temp file + rename),
//! so a crash mid-write can never leave a half-written config. The UI validates
//! a patch against a base revision before applying it (BUILD-PLAN §5.3), which
//! prevents a stale webview from silently clobbering a newer change.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("revision conflict: expected {expected}, found {found}")]
    RevisionConflict { expected: u64, found: u64 },
    #[error("invalid settings: {0}")]
    Invalid(String),
}

/// The blue the user has chosen. `None` means "not yet chosen" — the UI must ask
/// and must not substitute a default (decision D-2 in the plan).
pub type Palette = String;

/// The accent a fresh install starts on. Answer to D-2.
pub const DEFAULT_PALETTE: &str = "blue";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub revision: u64,

    pub general: General,
    pub audio: Audio,
    pub dictation: Dictation,
    pub dictionary: DictionarySettings,
    pub history: HistorySettings,
    pub delivery: DeliverySettings,
    pub model: ModelSettings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    /// Chosen accent colour id.
    ///
    /// D-2 is answered: a fresh install starts on **blue**, shown as selected in
    /// the picker. Before this the field was `None` while the stylesheet fell
    /// back to blue, so the app *looked* blue with no swatch marked — the picker
    /// and the app disagreed. `None` is still accepted from an older settings
    /// file and is read as blue, so an upgrade cannot land in that state.
    pub palette: Option<Palette>,
    pub theme: Theme,
    /// Show the floating pill overlay while recording.
    pub show_overlay: bool,
    pub overlay_position: OverlayPosition,
    /// What the overlay shows while you speak.
    pub overlay_style: OverlayStyle,
    /// Start the app when the desktop session starts.
    pub autostart: bool,
    /// What the window's close button does.
    pub close_action: CloseAction,
}

/// How much the recording overlay shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayStyle {
    /// Nothing at all.
    None,
    /// A compact pill with a level meter.
    Minimal,
    /// The pill plus words as they are recognised.
    Live,
}

/// What happens when the user closes the main window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseAction {
    /// Keep running in the tray; dictation keeps working.
    Tray,
    /// Quit the app entirely.
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayPosition {
    /// Bottom centre. `bottom_right` is accepted from settings written before
    /// that option was removed and is treated as bottom centre, so upgrading
    /// never loses a config or leaves the overlay in a place the UI cannot show.
    #[serde(alias = "bottom_right")]
    BottomCentre,
    TopCentre,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Audio {
    /// ALSA/PipeWire device name; `None` means system default.
    pub input_device: Option<String>,
    /// Input gain multiplier applied before the model sees the audio.
    pub gain: f32,
    /// Mute other audio while recording. Best-effort.
    pub mute_output: bool,
    /// Play a short tone when recording starts and stops.
    pub cues: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Dictation {
    /// Hold-to-talk chord, e.g. "Super+D". `None` = unbound.
    pub hold_chord: Option<String>,
    /// Toggle chord. `None` = unbound.
    pub toggle_chord: Option<String>,
    /// Cancel chord, valid while recording.
    pub cancel_chord: Option<String>,
    /// Insert automatically when the transcript is ready.
    pub auto_insert: bool,
    /// Keep the transcript on the clipboard after inserting.
    pub keep_on_clipboard: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DictionarySettings {
    /// Tier 3: fuzzy matching. Off by default (plan §5.3, D-9).
    pub fuzzy_enabled: bool,
    /// Tier 4: phonetic matching. Off by default (plan §5.3, D-9).
    pub phonetic_enabled: bool,
    pub fuzzy_threshold: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistorySettings {
    pub retention: Retention,
    /// Hard cap on stored recordings, as a second line of defence.
    pub max_entries: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    /// Keep the transcript but delete the audio immediately.
    None,
    ThreeDays,
    ThirtyDays,
    Forever,
}

impl Retention {
    pub fn days(self) -> Option<u32> {
        match self {
            Retention::None => Some(0),
            Retention::ThreeDays => Some(3),
            Retention::ThirtyDays => Some(30),
            Retention::Forever => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeliverySettings {
    /// Prefer clipboard+paste over synthetic typing. Paste is faster and safer
    /// for long text; typing is the fallback when no clipboard tool exists.
    pub prefer_paste: bool,
    /// Restore the previous clipboard contents after pasting.
    pub restore_clipboard: bool,
    /// Delay before pasting, to let a hotkey modifier be released.
    pub paste_delay_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelSettings {
    /// Directory holding config.json, model.safetensors, ternary.json,
    /// tokenizer.json. `None` = default app data location.
    pub directory: Option<PathBuf>,
    /// Verify the model files on start.
    pub verify_on_start: bool,
    /// Unload the speech engine after this many seconds of no dictation.
    ///
    /// The engine holds ~700 MB resident, so leaving it loaded forever is the
    /// difference between a background app and a memory leak with a window.
    /// `0` keeps it loaded always, for someone who dictates constantly and
    /// prefers a fast first word over the memory.
    pub idle_unload_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            revision: 0,
            general: General::default(),
            audio: Audio::default(),
            dictation: Dictation::default(),
            dictionary: DictionarySettings::default(),
            history: HistorySettings::default(),
            delivery: DeliverySettings::default(),
            model: ModelSettings::default(),
        }
    }
}

impl Default for General {
    fn default() -> Self {
        Self {
            palette: Some(DEFAULT_PALETTE.to_string()),
            theme: Theme::System,
            show_overlay: true,
            overlay_position: OverlayPosition::BottomCentre,
            overlay_style: OverlayStyle::Minimal,
            autostart: false,
            close_action: CloseAction::Tray,
        }
    }
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            input_device: None,
            gain: 1.0,
            mute_output: false,
            cues: true,
        }
    }
}

impl Default for Dictation {
    fn default() -> Self {
        Self {
            // Both bindings ship set, because a dictation app that does nothing
            // when you press a key is not usable: the window's own button only
            // types into Sotto itself.
            //
            // Ctrl+Space is the chord people already expect for hold-to-talk, so
            // it is the hold binding. Toggle needs its own chord, distinct from
            // hold while using the same key, so it is Ctrl+Space with Shift
            // added: Ctrl+Shift+Space. Pressing Ctrl+Space cannot fire the
            // toggle, and the two never race.
            //
            // A default *can* collide with another application. That is handled
            // honestly rather than avoided: registration failure is reported in
            // the UI with the chord named, so a collision reads as "another app
            // owns this" instead of "this does nothing".
            hold_chord: Some("Ctrl+Space".into()),
            toggle_chord: Some("Ctrl+Shift+Space".into()),
            cancel_chord: Some("Escape".into()),
            auto_insert: true,
            keep_on_clipboard: false,
        }
    }
}

impl Default for DictionarySettings {
    fn default() -> Self {
        Self {
            fuzzy_enabled: false,
            phonetic_enabled: false,
            fuzzy_threshold: sotto_dict::DEFAULT_FUZZY_THRESHOLD,
        }
    }
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            retention: Retention::ThreeDays,
            max_entries: 5_000,
        }
    }
}

impl Default for DeliverySettings {
    fn default() -> Self {
        Self {
            prefer_paste: true,
            restore_clipboard: true,
            paste_delay_ms: 120,
        }
    }
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self {
            directory: None,
            verify_on_start: true,
            // Five minutes: long enough that a burst of dictation never pays the
            // reload cost twice, short enough that a laptop left alone overnight
            // is not holding most of a gigabyte.
            idle_unload_secs: 300,
        }
    }
}

impl Settings {
    /// Reject values that would break the app at runtime.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(0.0..=4.0).contains(&self.audio.gain) {
            return Err(ConfigError::Invalid(format!(
                "audio.gain must be within 0.0..=4.0, got {}",
                self.audio.gain
            )));
        }
        if !(0.0..=1.0).contains(&self.dictionary.fuzzy_threshold) {
            return Err(ConfigError::Invalid(format!(
                "dictionary.fuzzy_threshold must be within 0.0..=1.0, got {}",
                self.dictionary.fuzzy_threshold
            )));
        }
        if self.history.max_entries == 0 {
            return Err(ConfigError::Invalid(
                "history.max_entries must be at least 1".into(),
            ));
        }
        if self.delivery.paste_delay_ms > 2_000 {
            return Err(ConfigError::Invalid(
                "delivery.paste_delay_ms must be at most 2000".into(),
            ));
        }
        if self.model.idle_unload_secs > 86_400 {
            return Err(ConfigError::Invalid(
                "model.idle_unload_secs must be at most 86400".into(),
            ));
        }
        if let (Some(h), Some(t)) = (&self.dictation.hold_chord, &self.dictation.toggle_chord) {
            if h == t {
                return Err(ConfigError::Invalid(
                    "hold and toggle chords must differ".into(),
                ));
            }
        }
        if let Some(p) = &self.general.palette {
            const KNOWN: [&str; 6] = ["blue", "green", "orange", "pink", "violet", "red"];
            if !KNOWN.contains(&p.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "general.palette must be one of {KNOWN:?}, got {p:?}"
                )));
            }
        }
        Ok(())
    }

    /// Load from `path`, or return defaults if the file does not exist.
    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomically persist. Writes a sibling temp file, fsyncs, then renames.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)?;
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Apply a patch only if the caller's base revision matches, then bump.
    pub fn apply(&mut self, base_revision: u64, patch: Settings) -> Result<(), ConfigError> {
        if base_revision != self.revision {
            return Err(ConfigError::RevisionConflict {
                expected: base_revision,
                found: self.revision,
            });
        }
        let revision = self.revision.wrapping_add(1);
        let mut next = patch;
        next.revision = revision;
        next.validate()?;
        *self = next;
        Ok(())
    }
}

#[cfg(test)]
mod idle_unload_tests {
    use super::*;

    #[test]
    fn idle_unload_defaults_to_five_minutes() {
        // Long enough that a burst of dictation never pays a reload twice, short
        // enough that a laptop left overnight is not holding most of a gigabyte.
        let s = Settings::default();
        assert_eq!(s.model.idle_unload_secs, 300);
        s.validate().expect("default must validate");
    }

    #[test]
    fn zero_means_keep_the_engine_loaded() {
        // Zero is the user explicitly turning the feature off, so it must be a
        // valid value rather than something validation rejects.
        let mut s = Settings::default();
        s.model.idle_unload_secs = 0;
        s.validate().expect("0 means never unload, which is valid");
    }

    #[test]
    fn a_day_is_the_ceiling() {
        let mut s = Settings::default();
        s.model.idle_unload_secs = 86_400;
        s.validate().expect("24 hours is allowed");

        s.model.idle_unload_secs = 86_401;
        assert!(
            s.validate().is_err(),
            "more than a day is rejected rather than silently clamped"
        );
    }

    #[test]
    fn the_setting_survives_a_save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("sotto-idle-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        let mut s = Settings::default();
        s.model.idle_unload_secs = 45 * 60;
        s.save(&path).unwrap();

        let back = Settings::load_or_default(&path).unwrap();
        assert_eq!(back.model.idle_unload_secs, 2700, "45 minutes");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_older_settings_file_without_the_field_still_loads() {
        // A settings file written before this setting existed must not fail to
        // load: it gets the default, so an upgrade never resets someone's setup.
        let dir = std::env::temp_dir().join(format!("sotto-idle-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"revision":3,"model":{"directory":null,"verify_on_start":true}}"#,
        )
        .unwrap();

        let s = Settings::load_or_default(&path).unwrap();
        assert_eq!(s.revision, 3, "the rest of the file is honoured");
        assert_eq!(s.model.idle_unload_secs, 300, "the new field gets its default");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod position_tests {
    use super::*;

    #[test]
    fn a_removed_position_loads_as_bottom_centre() {
        // The UI now offers only bottom centre and top centre. A settings file
        // written while "bottom right" existed must still load: a parse error
        // would drop every other setting along with it.
        let p: OverlayPosition = serde_json::from_str("\"bottom_right\"").unwrap();
        assert_eq!(p, OverlayPosition::BottomCentre);

        let p: OverlayPosition = serde_json::from_str("\"top_centre\"").unwrap();
        assert_eq!(p, OverlayPosition::TopCentre);
    }
}
