//! One playback thread, so a CPAL stream is only ever touched from the thread
//! that created it.
//!
//! `cpal::Stream` is not `Send` on ALSA, and the Tauri command surface runs on
//! arbitrary threads. Owning the player on a dedicated thread and talking to it
//! over a channel is the smallest arrangement that keeps the stream inside one
//! thread while still letting any command start or stop playback.
//!
//! Only one recording plays at a time: a `Play` stops whatever was playing,
//! which is what a person expects and keeps exactly one output stream open.

use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use sotto_core::playback::{PlayError, Player};

/// What the UI needs to know about playback.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct Status {
    pub playing: bool,
    /// Milliseconds played so far.
    pub position_ms: u64,
    /// Total length of the recording in milliseconds.
    pub duration_ms: u64,
}

enum Msg {
    Play {
        path: PathBuf,
        reply: mpsc::Sender<Result<(), PlayError>>,
    },
    Stop,
    Status(mpsc::Sender<Status>),
}

fn tx() -> &'static Mutex<Sender<Msg>> {
    static TX: OnceLock<Mutex<Sender<Msg>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Msg>();
        std::thread::Builder::new()
            .name("sotto-playback".into())
            .spawn(move || {
                // The player lives entirely on this thread for its whole life.
                let mut player: Option<Player> = None;
                while let Ok(msg) = rx.recv() {
                    match msg {
                        Msg::Play { path, reply } => {
                            if let Some(p) = player.take() {
                                p.stop();
                            }
                            match Player::start(&path) {
                                Ok(p) => {
                                    player = Some(p);
                                    let _ = reply.send(Ok(()));
                                }
                                Err(e) => {
                                    let _ = reply.send(Err(e));
                                }
                            }
                        }
                        Msg::Stop => {
                            if let Some(p) = player.take() {
                                p.stop();
                            }
                        }
                        Msg::Status(reply) => {
                            let status = match player.as_ref() {
                                Some(p) if p.is_playing() => {
                                    let (played, total) = p.progress();
                                    let rate = p.rate();
                                    Status {
                                        playing: true,
                                        position_ms: ms(played, rate),
                                        duration_ms: ms(total, rate),
                                    }
                                }
                                _ => Status::default(),
                            };
                            // Drop a finished player so its stream is released.
                            if player.as_ref().map(|p| !p.is_playing()).unwrap_or(false) {
                                player = None;
                            }
                            let _ = reply.send(status);
                        }
                    }
                }
            })
            .expect("playback thread");
        Mutex::new(tx)
    })
}

/// Send a message to the playback thread.
fn send(msg: Msg) -> bool {
    match tx().lock() {
        Ok(tx) => tx.send(msg).is_ok(),
        Err(e) => e.into_inner().send(msg).is_ok(),
    }
}

fn ms(samples: u64, rate: u32) -> u64 {
    if rate == 0 {
        0
    } else {
        samples * 1000 / rate as u64
    }
}

/// Start playing `path`. Any previous playback stops first.
pub fn play(path: PathBuf) -> Result<(), PlayError> {
    let (reply, wait) = mpsc::channel();
    if !send(Msg::Play { path, reply }) {
        return Err(PlayError::Format("playback thread stopped".into()));
    }
    wait.recv()
        .map_err(|_| PlayError::Format("playback thread stopped".into()))?
}

/// Stop whatever is playing, if anything.
pub fn stop() {
    send(Msg::Stop);
}

/// Current playback state.
pub fn status() -> Status {
    let (reply, wait) = mpsc::channel();
    if !send(Msg::Status(reply)) {
        return Status::default();
    }
    wait.recv_timeout(std::time::Duration::from_millis(500))
        .unwrap_or_default()
}
