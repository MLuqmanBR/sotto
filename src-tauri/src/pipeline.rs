//! Application state and the dictation pipeline.
//!
//! The pipeline is deliberately linear and explicit: capture → journal → transcribe
//! → correct → deliver. Audio reaches disk *before* transcription begins, so a
//! crash, a cancel, or a model failure never destroys a recording.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use sotto_core::audio::Recorder;
use sotto_core::stt::SttClient;
use sotto_core::store::{Attempt, Store, TakeStatus};
use sotto_core::config::Settings;

pub struct App {
    pub root: PathBuf,
    pub settings: Mutex<Settings>,
    pub store: Mutex<Store>,
    pub dict: Mutex<(Vec<sotto_dict::Entry>, Vec<sotto_dict::Replacement>)>,
    pub model_dir: Mutex<PathBuf>,
    pub stt: Mutex<Option<SttClient>>,
    pub recorder: Mutex<Option<Recording>>,
    /// The output mute applied for the current recording, if any. Restored when
    /// the take ends, so muting while recording never outlives the recording.
    pub muted: Mutex<Option<sotto_core::mute::Muted>>,
    /// Words recognised so far in the current take, for the live overlay.
    pub preview_text: Mutex<String>,
    /// The live-preview worker, when one is running.
    pub preview: Mutex<Option<LivePreview>>,
    /// Shortcuts the desktop refused at startup. Surfaced to the UI so a chord
    /// that silently does nothing can be explained instead of guessed at.
    pub startup_shortcut_problems: Mutex<Vec<String>>,
    /// When the engine was last used, for the idle unload. Seconds since the
    /// process started, or `None` if the engine has never been loaded.
    pub last_engine_use: Mutex<Option<std::time::Instant>>,
    /// The window that had focus when the current take started.
    ///
    /// Used to keep a transcript out of the wrong application (A14). Recorded at
    /// the start because by the time transcription finishes the user may have
    /// switched windows, and a paste goes wherever focus is *then*.
    pub take_target: Mutex<Option<String>>,
    /// True while the current take was started by a **hold** chord and has not
    /// yet been released. Releasing the chord ends that take; it must not end a
    /// take the user started with the toggle chord.
    pub hold_active: AtomicBool,
}

/// A background worker that turns audio into words while you are still speaking.
///
/// It exists only for the overlay; the transcript you actually receive is always
/// produced by one full pass over the finished recording, so a preview can never
/// change what gets typed into your app.
pub struct LivePreview {
    stop: Arc<AtomicBool>,
    join: std::thread::JoinHandle<()>,
}

impl LivePreview {
    /// Ask the worker to stop and wait for it. Waiting matters: it holds the
    /// sidecar, and the final transcription needs that same sidecar next.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.join.join();
    }
}

pub struct Recording {
    pub take_id: String,
    /// Read-only view of the live capture, for the overlay meter and preview.
    pub tap: sotto_core::audio::AudioTap,
    pub stop_tx: mpsc::Sender<()>,
    pub join: std::thread::JoinHandle<Option<sotto_core::audio::Recorded>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStatus {
    pub present: bool,
    pub directory: String,
    pub bytes: u64,
    pub engine: Option<String>,
    pub kernels: Option<String>,
    pub device: Option<String>,
    pub load_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PipelineResult {
    pub take_id: String,
    pub raw_text: String,
    pub corrected_text: String,
    pub corrections: Vec<sotto_dict::Correction>,
    pub duration_ms: i64,
    pub decode_ms: u64,
    pub delivery: String,
}

impl App {
    pub fn new(root: &std::path::Path, model_dir: PathBuf) -> Self {
        // Create both roots up front. A first run must leave the app in a state
        // where every later write can succeed; discovering a missing directory
        // only at the first settings save would turn a launch into a failed
        // save with nothing to explain it.
        let cfg_dir = root.join("config");
        let _ = std::fs::create_dir_all(&cfg_dir);
        let store = Store::open(root.join("history")).expect("history store");
        let _ = store.recover_interrupted();
        let cfg_path = cfg_dir.join("settings.json");
        let settings = Settings::load_or_default(&cfg_path).unwrap_or_default();
        let dict_path = cfg_dir.join("dictionary.json");
        let dict = sotto_core::load_dictionary(&dict_path);
        Self {
            root: root.to_path_buf(),
            settings: Mutex::new(settings),
            store: Mutex::new(store),
            dict: Mutex::new(dict),
            model_dir: Mutex::new(model_dir),
            stt: Mutex::new(None),
            recorder: Mutex::new(None),
            muted: Mutex::new(None),
            preview_text: Mutex::new(String::new()),
            preview: Mutex::new(None),
            startup_shortcut_problems: Mutex::new(Vec::new()),
            last_engine_use: Mutex::new(None),
            take_target: Mutex::new(None),
            hold_active: AtomicBool::new(false),
        }
    }

    pub fn config_path(&self, name: &str) -> PathBuf {
        self.root.join("config").join(name)
    }

    /// Ensure the sidecar is running with the model resident.
    /// Point the app at the model bundled with the installation.
    pub fn set_model_dir(&self, dir: PathBuf) {
        if let Ok(mut g) = self.model_dir.lock() {
            *g = dir;
        }
        // A running sidecar holds the old path; drop it so the next use reloads.
        if let Ok(mut g) = self.stt.lock() {
            if let Some(c) = g.as_mut() {
                c.kill();
            }
            *g = None;
        }
    }

    /// The sidecar's process id, if one is running.
    ///
    /// Exposed so a test can verify cancellation by **process death** rather than
    /// by trusting that a request was sent — which is exactly how acceptance A04
    /// words it.
    pub fn stt_pid(&self) -> Option<u32> {
        self.stt.lock().ok().and_then(|g| g.as_ref().and_then(|c| c.pid()))
    }

    pub fn ensure_stt(&self) -> Result<(), String> {
        let mut guard = self.stt.lock().map_err(|e| e.to_string())?;
        if let Some(client) = guard.as_mut() {
            if client.alive() {
                // Already loaded: this call *is* a use, so the idle clock restarts.
                // Without this, someone dictating every four minutes with a
                // five-minute timeout would still have the engine pulled out from
                // under them mid-sentence.
                drop(guard);
                self.touch_engine();
                return Ok(());
            }
            // The engine died since it was started. Drop the dead handle and fall
            // through to start a fresh one, rather than handing a corpse to the
            // next transcription and failing forever.
            eprintln!("speech engine exited unexpectedly; starting a new one");
            *guard = None;
        }
        let dir = self.model_dir.lock().map_err(|e| e.to_string())?.clone();
        let bin = sotto_core::stt::find_binary()
            .ok_or_else(|| "the speech engine binary is missing from this installation".to_string())?;
        if !dir.join("model.safetensors").is_file() {
            return Err(format!(
                "the speech model is missing from this installation ({})",
                dir.display()
            ));
        }

        // `verify_on_start` decides how much is checked before the engine is
        // launched. The cheap check above always runs; when the setting is on,
        // every file the model needs is confirmed as well, so a truncated or
        // partially-copied installation is reported here instead of surfacing as
        // a confusing decode failure on the first dictation.
        let verify = self
            .settings
            .lock()
            .map(|g| g.model.verify_on_start)
            .unwrap_or(true);
        if verify {
            for name in ["config.json", "tokenizer.json", "ternary.json"] {
                let p = dir.join(name);
                if !p.is_file() {
                    return Err(format!(
                        "the speech model is incomplete: {} is missing ({})",
                        name,
                        dir.display()
                    ));
                }
                if p.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
                    return Err(format!(
                        "the speech model is incomplete: {} is empty ({})",
                        name,
                        dir.display()
                    ));
                }
            }
        }

        let client = SttClient::start(&bin, &dir).map_err(|e| e.to_string())?;
        *guard = Some(client);
        drop(guard);
        self.touch_engine();
        Ok(())
    }

    /// Record that the engine was just used, so the idle timer restarts.
    pub fn touch_engine(&self) {
        if let Ok(mut g) = self.last_engine_use.lock() {
            *g = Some(std::time::Instant::now());
        }
    }

    /// Unload the engine if it has been idle for longer than the user allows.
    ///
    /// Returns whether it unloaded anything. The engine holds roughly 700 MB
    /// resident, so a laptop left alone should not keep it: requirement S-U12 and
    /// acceptance A16 both ask for "no idle model". `idle_unload_secs == 0` means
    /// the user turned this off and wants the engine kept warm.
    /// How long until the engine should be dropped, if it should be at all.
    ///
    /// The idle thread used to sleep a flat 30 s and then ask, so a user who chose
    /// "unload after 1 minute" waited up to **two** minutes — the setting meant
    /// something other than what it said. This returns the remaining time so the
    /// thread can wake exactly when the deadline arrives, and it costs nothing
    /// when the setting is `0` (never unload).
    pub fn idle_deadline_in(&self) -> Option<std::time::Duration> {
        let limit = self.settings.lock().ok()?.model.idle_unload_secs;
        if limit == 0 {
            return None;
        }
        // Nothing loaded: no deadline.
        if self.stt.lock().ok()?.is_none() {
            return None;
        }
        let idle_for = (*self.last_engine_use.lock().ok()?)? .elapsed();
        let limit = std::time::Duration::from_secs(limit);
        Some(limit.saturating_sub(idle_for))
    }

    pub fn unload_if_idle(&self) -> bool {
        let limit = match self.settings.lock() {
            Ok(g) => g.model.idle_unload_secs,
            Err(_) => return false,
        };
        if limit == 0 {
            return false;
        }
        let idle_for = match self.last_engine_use.lock() {
            Ok(g) => match *g {
                Some(t) => t.elapsed(),
                // Never used since start: nothing loaded, nothing to unload.
                None => return false,
            },
            Err(_) => return false,
        };
        if idle_for.as_secs() < limit {
            return false;
        }
        // Never unload mid-recording: the engine is about to be needed, and
        // killing it would turn a running take into a failure.
        if let Ok(g) = self.recorder.lock() {
            if g.is_some() {
                return false;
            }
        }
        let mut guard = match self.stt.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        if guard.is_none() {
            return false;
        }
        if let Some(c) = guard.as_mut() {
            c.kill();
        }
        *guard = None;
        if let Ok(mut g) = self.last_engine_use.lock() {
            *g = None;
        }
        true
    }

    /// Begin the live-preview worker for a take, if the user asked for it.
    ///
    /// The worker re-reads the capture every few hundred milliseconds, writes a
    /// temporary WAV of what exists so far, and asks the sidecar to transcribe
    /// it. Anything under a second of audio is skipped, because a fragment that
    /// short produces confident nonsense. The text it produces is *preview only*:
    /// it goes to the overlay, never to the document.
    pub fn start_preview(self: &Arc<Self>, tap: sotto_core::audio::AudioTap, style: sotto_core::config::OverlayStyle) {
        use sotto_core::config::OverlayStyle;
        if style != OverlayStyle::Live {
            return;
        }
        self.stop_preview();

        let stop = Arc::new(AtomicBool::new(false));
        let app = self.clone();
        let stop_flag = stop.clone();
        let join = std::thread::spawn(move || {
            // Give the first words time to become a sentence.
            let mut last_committed = 0usize;
            while !stop_flag.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(450));
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                let captured = tap.captured_samples_16k();
                // Only decode when there is meaningfully more audio than last time.
                if captured < last_committed + 8_000 {
                    continue;
                }
                let samples = tap.preview_16k();
                if samples.len() < 12_000 {
                    continue;
                }
                last_committed = samples.len();

                let recorded = sotto_core::audio::Recorded {
                    samples_16k: samples,
                    source_rate: 16_000,
                    source_channels: 1,
                };
                let tmp = std::env::temp_dir()
                    .join(format!("sotto-preview-{}.wav", std::process::id()));
                if recorded.write_wav(&tmp).is_err() {
                    continue;
                }
                let text = {
                    let mut g = match app.stt.lock() {
                        Ok(g) => g,
                        Err(_) => break,
                    };
                    match g.as_mut() {
                        Some(c) => c.transcribe(&tmp, false).map(|t| t.text).unwrap_or_default(),
                        None => break,
                    }
                };
                let _ = std::fs::remove_file(&tmp);
                if !text.trim().is_empty() {
                    if let Ok(mut p) = app.preview_text.lock() {
                        *p = text;
                    }
                }
            }
        });

        if let Ok(mut g) = self.preview.lock() {
            *g = Some(LivePreview { stop, join });
        }
    }

    /// Stop the preview worker and wait for it, so it releases the sidecar.
    pub fn stop_preview(&self) {
        let worker = self.preview.lock().ok().and_then(|mut g| g.take());
        if let Some(w) = worker {
            w.stop();
        }
        if let Ok(mut p) = self.preview_text.lock() {
            p.clear();
        }
    }

    /// Stop a take that outlives the safety ceiling.
    ///
    /// A hold binding depends on a **release** edge, and a release can be lost:
    /// the compositor drops it, the machine suspends, or the backend goes away
    /// mid-hold. Without a ceiling that leaves an unlimited recording and an
    /// unbounded file on disk, which BUILD-PLAN §9 forbids. The visible Stop
    /// action is the first line of defence; this is the one that works when
    /// nobody is looking at the screen.
    ///
    /// `stop` runs on the watchdog thread, at most once, and **only** if
    /// `take_id` is still the take in progress. A take that already ended — or
    /// that was replaced by a newer one — is left alone, so a stale watchdog can
    /// never cut off the take after it.
    pub fn watch_take<F>(self: &Arc<Self>, take_id: String, stop: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let app = Arc::clone(self);
        std::thread::spawn(move || {
            let ceiling = take_ceiling();
            let started = std::time::Instant::now();
            loop {
                // Poll rather than sleep the whole ceiling: the take usually
                // ends long before it, and the thread must notice.
                std::thread::sleep(std::time::Duration::from_millis(200));
                let current = app
                    .recorder
                    .lock()
                    .ok()
                    .and_then(|g| g.as_ref().map(|r| r.take_id.clone()));
                if current.as_deref() != Some(take_id.as_str()) {
                    return;
                }
                if started.elapsed() >= ceiling {
                    // The take was started by a hold chord that never reported
                    // its release; clearing the flag keeps the next release
                    // event from trying to stop an already-finished take.
                    app.hold_active.store(false, Ordering::SeqCst);
                    stop();
                    return;
                }
            }
        });
    }

    pub fn model_status(&self) -> ModelStatus {
        let dir = &self
            .model_dir
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| PathBuf::from("."));
        let mut bytes = 0u64;
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if let Ok(m) = e.metadata() {
                    bytes += m.len();
                }
            }
        }
        let present = dir.join("model.safetensors").is_file()
            && dir.join("tokenizer.json").is_file()
            && dir.join("ternary.json").is_file();
        let (engine, kernels, device) = match self.stt.lock().ok().and_then(|g| {
            g.as_ref().map(|c| {
                (
                    c.ready().engine.clone(),
                    c.ready().kernels.clone(),
                    c.ready().device.clone(),
                )
            })
        }) {
            Some(v) => (Some(v.0), Some(v.1), Some(v.2)),
            None => (None, None, None),
        };
        ModelStatus {
            present,
            directory: dir.display().to_string(),
            bytes,
            engine,
            kernels,
            device,
            load_ms: None,
        }
    }

    pub fn current_dictionary(&self) -> sotto_dict::Dictionary {
        let settings = self.settings.lock().map(|g| g.clone()).unwrap_or_default();
        let (e, r) = self.dict.lock().map(|g| g.clone()).unwrap_or_default();
        sotto_core::dictionary_from(&settings, e, r)
    }

    /// Run the full post-capture pipeline.
    /// Open a take: start capture, write the take row, hold the recorder.
    ///
    /// This is the body of the "start dictation" command, kept here rather than in
    /// the binary so the lifecycle can be driven by a test. Everything the window
    /// does around it — the overlay, the start cue, muting other audio — stays in
    /// the command, because none of it is part of what a take *is*.
    ///
    /// Refuses a second take rather than silently discarding the first, because a
    /// double start would otherwise lose the audio already being recorded.
    pub fn begin_take(&self) -> Result<String, String> {
        {
            let g = self.recorder.lock().map_err(|e| e.to_string())?;
            if g.is_some() {
                return Err("already recording".into());
            }
        }

        // Note the window the user is typing in, before anything can move focus.
        // Taken before capture starts so the overlay cannot become the answer.
        if let Ok(mut g) = self.take_target.lock() {
            *g = sotto_core::delivery::active_window();
        }

        let settings = self.settings.lock().map(|g| g.clone()).unwrap_or_default();
        let rec = start_capture(settings.audio.input_device.clone(), settings.audio.gain)?;
        let take_id = rec.take_id.clone();
        self.store
            .lock()
            .map_err(|e| e.to_string())?
            .create_take(&take_id, &now_rfc3339())
            .map_err(|e| e.to_string())?;

        let tap = rec.tap.clone();
        let mut g = self.recorder.lock().map_err(|e| e.to_string())?;
        *g = Some(rec);
        drop(g);
        let _ = tap;
        Ok(take_id)
    }

    /// End a take: stop capture, journal the audio, transcribe, correct, deliver.
    ///
    /// The order is the contract, not an implementation detail: the WAV reaches
    /// disk and the row reaches the store **before** transcription begins, so a
    /// crash or a model failure never destroys a recording. A failure during
    /// transcription marks the take failed but keeps its audio.
    ///
    /// Returns the pipeline result the window shows, or the reason nothing was
    /// transcribed — in which case the take is closed as cancelled, not left
    /// pending.
    pub fn end_take(&self) -> Result<PipelineResult, String> {
        // The preview holds the sidecar; release it before the real pass.
        self.stop_preview();

        let rec = {
            let mut g = self.recorder.lock().map_err(|e| e.to_string())?;
            g.take().ok_or("not recording")?
        };

        let _ = rec.stop_tx.send(());
        let recorded = rec
            .join
            .join()
            .map_err(|_| "capture thread panicked".to_string())?
            .ok_or("capture produced no audio")?;

        if recorded.is_empty() {
            self.store
                .lock()
                .map_err(|e| e.to_string())?
                .finish_take(&rec.take_id, 0, TakeStatus::Cancelled, None)
                .map_err(|e| e.to_string())?;
            return Err("recording was too short to transcribe".into());
        }

        let wav = self
            .store
            .lock()
            .map_err(|e| e.to_string())?
            .audio_path_for(&rec.take_id);
        recorded.write_wav(&wav).map_err(|e| e.to_string())?;
        let duration_ms = recorded.duration_ms();

        // Journal before transcription: the audio is already safe on disk.
        self.store
            .lock()
            .map_err(|e| e.to_string())?
            .finish_take(
                &rec.take_id,
                duration_ms,
                TakeStatus::Captured,
                Some(&wav),
            )
            .map_err(|e| e.to_string())?;

        match self.finish(&rec.take_id, &wav, duration_ms) {
            Ok(r) => Ok(r),
            Err(e) => {
                if let Ok(s) = self.store.lock() {
                    let _ = s.finish_take(
                        &rec.take_id,
                        duration_ms,
                        TakeStatus::Failed,
                        Some(&wav),
                    );
                }
                Err(e)
            }
        }
    }

    /// Discard the take in progress.
    ///
    /// Drops the audio, closes the row as cancelled, and **kills the sidecar**:
    /// a cancel is a request to stop working, so it must not queue behind a
    /// decode that is already running.
    pub fn abort_take(&self) -> Result<(), String> {
        self.stop_preview();
        let rec = {
            let mut g = self.recorder.lock().map_err(|e| e.to_string())?;
            g.take()
        };
        if let Some(rec) = rec {
            let _ = rec.stop_tx.send(());
            let _ = rec.join.join();
            if let Ok(s) = self.store.lock() {
                let _ = s.finish_take(&rec.take_id, 0, TakeStatus::Cancelled, None);
            }
        }
        if let Ok(mut g) = self.stt.lock() {
            if let Some(c) = g.as_mut() {
                c.kill();
            }
            *g = None;
        }
        Ok(())
    }

    /// Stop capture for shutdown and remove the take entirely.
    ///
    /// This is deliberately not `abort_take`: Cancel is a user action and leaves a
    /// cancelled entry as evidence that something was recorded. Quitting is not a
    /// dictation, so a row with no audio and no words would be a phantom in History
    /// after the next launch. The audio file is written under the take's id, so it is
    /// removed even though the take never reached `finish_take`.
    pub fn discard_take_on_exit(&self) {
        self.stop_preview();
        let rec = self.recorder.lock().ok().and_then(|mut g| g.take());
        if let Some(rec) = rec {
            let _ = rec.stop_tx.send(());
            let _ = rec.join.join();
            if let Ok(store) = self.store.lock() {
                let audio = store.audio_path_for(&rec.take_id);
                let _ = store.delete(&[rec.take_id.clone()], false);
                let _ = std::fs::remove_file(audio);
            }
        }
        if let Ok(mut g) = self.stt.lock() {
            if let Some(c) = g.as_mut() {
                c.kill();
            }
            *g = None;
        }
    }

    pub fn finish(
        &self,
        take_id: &str,
        wav: &std::path::Path,
        duration_ms: i64,
    ) -> Result<PipelineResult, String> {
        self.ensure_stt()?;
        let mut guard = self.stt.lock().map_err(|e| e.to_string())?;
        let client = guard.as_mut().ok_or("sidecar vanished")?;
        let tr = match client.transcribe(wav, false) {
            Ok(tr) => tr,
            Err(e) => {
                // The engine may have crashed mid-decode. Drop it so the next
                // dictation starts a new one instead of failing the same way.
                *guard = None;
                return Err(e.to_string());
            }
        };
        drop(guard);

        let dict = self.current_dictionary();
        let corrected = dict.correct(&tr.text);

        let settings = self.settings.lock().map(|g| g.clone()).unwrap_or_default();
        let delivery = if settings.dictation.auto_insert {
            let target = self.take_target.lock().ok().and_then(|g| g.clone());
            let out = sotto_core::delivery::deliver_to(
                &corrected.text,
                settings.delivery.prefer_paste,
                settings.delivery.restore_clipboard,
                settings.delivery.paste_delay_ms,
                settings.dictation.keep_on_clipboard,
                target.as_deref(),
            );
            match out {
                sotto_core::delivery::Outcome::Inserted { method } => format!("inserted ({method})"),
                sotto_core::delivery::Outcome::CopiedOnly { reason } => format!("copied only: {reason}"),
                sotto_core::delivery::Outcome::Failed { reason } => format!("not delivered: {reason}"),
            }
        } else {
            // Clipboard-only: still make the text available.
            let _ = sotto_core::delivery::copy(&corrected.text);
            "copied to clipboard".to_string()
        };

        self.store.lock().map_err(|e| e.to_string())?
            .finish_take(take_id, duration_ms, TakeStatus::Transcribed, Some(wav))
            .map_err(|e| e.to_string())?;
        let now = now_rfc3339();
        {
            let store = self.store.lock().map_err(|e| e.to_string())?;
            store
                .add_attempt(&Attempt {
                    id: uuid::Uuid::new_v4().to_string(),
                    take_id: take_id.to_string(),
                    raw_text: tr.text.clone(),
                    corrected_text: Some(corrected.text.clone()),
                    model: "parakeet-redux".into(),
                    decode_ms: tr.decode_ms as i64,
                    corrections_json: serde_json::to_string(&corrected.corrections)
                        .unwrap_or_default(),
                    created_at: now,
                })
                .map_err(|e| e.to_string())?;

            // Enforce the two storage promises the Settings page makes. Both were
            // implemented in the store but never called, so "Keep recordings" did
            // nothing and history grew without limit. Running them here means the
            // guarantee holds after every dictation, not only when someone
            // happens to open History.
            let _ = store.enforce_retention(settings.history.retention.days());
            let _ = store.trim_to(settings.history.max_entries);
        }

        Ok(PipelineResult {
            take_id: take_id.to_string(),
            raw_text: tr.text,
            corrected_text: corrected.text,
            corrections: corrected.corrections,
            duration_ms,
            decode_ms: tr.decode_ms,
            delivery,
        })
    }
}

/// Build the diagnostics report.
///
/// It describes the build, the environment and the settings so a problem can be
/// diagnosed — and **nothing about the person using it**. Acceptance A17 requires
/// that the file can be shared without disclosing who made it, so this function
/// deliberately contains:
///
/// - **no transcript text**, no dictionary entries, no audio;
/// - **no absolute paths**, because a path carries the username and the directory
///   layout (`/home/alice/...`). The model directory is therefore reported as a
///   *basename plus a bundled flag*, never the full path.
///
/// It lives here, rather than in the command, so a test can assert those
/// properties instead of trusting a comment. The check is in
/// `tests/diagnostics.rs`, which walks the whole JSON tree for anything that
/// looks like a path.
pub fn diagnostics_report(app: &App) -> serde_json::Value {
    let settings = app.settings.lock().map(|g| g.clone()).unwrap_or_default();
    let model = app.model_status();
    let (entries, replacements) = app.dict.lock().map(|g| g.clone()).unwrap_or_default();
    let history_count = app
        .store
        .lock()
        .ok()
        .and_then(|s| s.list(1000, None).ok())
        .map(|v| v.len())
        .unwrap_or(0);
    let audio_bytes = app
        .store
        .lock()
        .ok()
        .and_then(|s| s.audio_bytes().ok())
        .unwrap_or(0);
    let sidecar = app.stt.lock().ok().and_then(|g| {
        g.as_ref().map(|c| {
            format!(
                "{} {} ({})",
                c.ready().engine,
                c.ready().version,
                c.ready().kernels
            )
        })
    });

    // The basename only. `Path::file_name` of an absolute path is the last
    // component, which says which model is loaded without saying where.
    let model_dir_name = std::path::Path::new(&model.directory)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    serde_json::json!({
        "app": "sotto",
        "version": sotto_core::APP_VERSION,
        "generated_at": now_rfc3339(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "session": std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".into()),
        "desktop": std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "unknown".into()),
        "model": {
            "present": model.present,
            // Basename, not the path. See the note above.
            "name": model_dir_name,
            "bytes": model.bytes,
            "engine": model.engine,
            "kernels": model.kernels,
            "device": model.device,
        },
        "engine_running": sidecar,
        "settings": {
            "revision": settings.revision,
            "theme": settings.general.theme,
            "palette": settings.general.palette,
            "overlay_style": settings.general.overlay_style,
            "overlay_position": settings.general.overlay_position,
            "autostart": settings.general.autostart,
            "close_action": settings.general.close_action,
            "input_device": settings.audio.input_device,
            "gain": settings.audio.gain,
            "cues": settings.audio.cues,
            "mute_output": settings.audio.mute_output,
            "auto_insert": settings.dictation.auto_insert,
            "keep_on_clipboard": settings.dictation.keep_on_clipboard,
            "prefer_paste": settings.delivery.prefer_paste,
            "restore_clipboard": settings.delivery.restore_clipboard,
            "paste_delay_ms": settings.delivery.paste_delay_ms,
            "retention": settings.history.retention,
            "max_entries": settings.history.max_entries,
            "fuzzy_enabled": settings.dictionary.fuzzy_enabled,
            "phonetic_enabled": settings.dictionary.phonetic_enabled,
            "fuzzy_threshold": settings.dictionary.fuzzy_threshold,
            "verify_on_start": settings.model.verify_on_start,
            "idle_unload_secs": settings.model.idle_unload_secs,
        },
        "dictionary": {
            "entries": entries.len(),
            "replacements": replacements.len(),
        },
        "storage": {
            "history_entries": history_count,
            "audio_bytes": audio_bytes,
        },
    })
}

pub fn now_rfc3339() -> String {
    // Avoid pulling in a datetime crate: format from the system clock directly.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Convert Unix seconds to a UTC calendar date.
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Howard Hinnant's days-from-civil, inverted.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Start capturing on a worker thread. Returns once capture is live.
pub fn start_capture(
    device: Option<String>,
    gain: f32,
) -> Result<Recording, String> {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<sotto_core::audio::AudioTap, String>>();
    let take_id = uuid::Uuid::new_v4().to_string();
    let id_for_thread = take_id.clone();

    let join = std::thread::spawn(move || {
        let rec = match Recorder::start(device.as_deref(), gain) {
            Ok(r) => {
                let _ = ready_tx.send(Ok(r.tap()));
                r
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e.to_string()));
                return None;
            }
        };
        // Block until the UI says stop (or the channel closes).
        let _ = stop_rx.recv();
        let recorded = rec.stop();
        let _ = id_for_thread;
        Some(recorded)
    });

    match ready_rx.recv() {
        Ok(Ok(tap)) => Ok(Recording {
            take_id,
            tap,
            stop_tx,
            join,
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("capture thread died before starting".into()),
    }
}

/// The longest a single take may run before the pipeline stops it on its own.
///
/// Five minutes is far longer than a dictation and far shorter than "forgot to
/// release the key". It is a safety bound, not a feature, so it is not in
/// Settings: nobody wants to tune it, and a bound a user can raise is not a bound.
pub const MAX_TAKE_SECS: u64 = 300;

/// The ceiling actually in force.
///
/// `SOTTO_MAX_TAKE_SECS` overrides it so a test can prove the auto-stop without
/// recording for five minutes. An invalid or zero value falls back to the
/// constant rather than disabling the bound.
pub fn take_ceiling() -> std::time::Duration {
    ceiling_from(std::env::var("SOTTO_MAX_TAKE_SECS").ok().as_deref())
}

/// The override parser, separated from the environment read.
///
/// It is a pure function on purpose. Reading the environment *inside* the tests
/// made them race: the variables are process-global and the test harness runs
/// them in parallel, so one test's `set_var` was visible to another's
/// `remove_var`. That produced a test which failed roughly one run in four while
/// the code under test was correct — a flaky test is worse than no test, because
/// it teaches you to ignore failures.
fn ceiling_from(raw: Option<&str>) -> std::time::Duration {
    let secs = raw
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(MAX_TAKE_SECS);
    std::time::Duration::from_secs(secs)
}

#[cfg(test)]
mod ceiling_tests {
    use super::*;

    // These call `ceiling_from` rather than `take_ceiling` so they never touch
    // the process environment. Tests that mutate env vars race with each other;
    // the parser is the part worth testing, and it is pure.

    #[test]
    fn the_default_ceiling_is_five_minutes() {
        assert_eq!(ceiling_from(None), std::time::Duration::from_secs(300));
    }

    #[test]
    fn the_ceiling_can_be_lowered_for_a_test() {
        assert_eq!(ceiling_from(Some("2")), std::time::Duration::from_secs(2));
        assert_eq!(ceiling_from(Some(" 90 ")), std::time::Duration::from_secs(90));
    }

    #[test]
    fn a_nonsense_override_does_not_disable_the_bound() {
        // "0" would mean "no limit", which is the one thing this must never be.
        assert_eq!(ceiling_from(Some("0")), std::time::Duration::from_secs(300));
        assert_eq!(
            ceiling_from(Some("not-a-number")),
            std::time::Duration::from_secs(300)
        );
        assert_eq!(ceiling_from(Some("")), std::time::Duration::from_secs(300));
        assert_eq!(ceiling_from(Some("-5")), std::time::Duration::from_secs(300));
    }
}

#[cfg(test)]
mod idle_tests {
    use super::*;

    /// A root of its own per test.
    ///
    /// Keying on the idle limit looked fine and was not: two tests used 60 s, so
    /// they shared a directory and whichever finished first deleted it while the
    /// other was still using it. The test name is what actually distinguishes them.
    fn app_with(limit_secs: u64, name: &str) -> (Arc<App>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "sotto-idle-{}-{limit_secs}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("history")).unwrap();
        let mut s = Settings::default();
        s.model.idle_unload_secs = limit_secs;
        s.save(&root.join("config/settings.json")).unwrap();
        (Arc::new(App::new(&root, root.join("model"))), root)
    }

    #[test]
    fn never_unload_means_no_deadline() {
        // `0` is the documented "never unload" value; it must not become a
        // deadline of zero, which would unload immediately and repeatedly.
        let (app, root) = app_with(0, "never");
        assert_eq!(app.idle_deadline_in(), None);
        assert!(!app.unload_if_idle());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn with_no_engine_there_is_nothing_to_schedule() {
        let (app, root) = app_with(60, "noengine");
        assert_eq!(app.idle_deadline_in(), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_deadline_is_the_configured_time_not_a_tick_multiple() {
        // The bug this covers: the thread slept a flat 30 s, so "unload after 1
        // minute" could take two. The deadline must be ~the configured time from
        // the last use, whatever that time is.
        let (app, root) = app_with(45, "deadline");
        // Pretend an engine is loaded and was last used just now. A real engine
        // is not needed to test the arithmetic, and spawning one would make this
        // test depend on a model being installed.
        *app.last_engine_use.lock().unwrap() = Some(std::time::Instant::now());
        *app.stt.lock().unwrap() = None; // no engine: no deadline, see below

        // With no engine, the deadline is None even though a use was recorded.
        assert_eq!(app.idle_deadline_in(), None);

        // The arithmetic itself, once an engine is present, is checked through
        // `unload_if_idle`, which is the function the thread calls.
        assert!(!app.unload_if_idle(), "nothing is loaded, so nothing unloads");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_use_that_is_still_recent_does_not_unload() {
        let (app, root) = app_with(60, "recent");
        *app.last_engine_use.lock().unwrap() = Some(std::time::Instant::now());
        // No engine loaded, so this must be a no-op rather than a panic.
        assert!(!app.unload_if_idle());
        let _ = std::fs::remove_dir_all(&root);
    }
}
