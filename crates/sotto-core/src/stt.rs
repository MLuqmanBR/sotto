//! Client for the `sotto-stt` sidecar.
//!
//! Owns the child process, speaks NDJSON to it, and — critically — can *actually*
//! stop it. Because the sidecar is a separate process group, cancellation is a
//! signal the kernel enforces, not a request the model politely ignores.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SttError {
    #[error("sidecar spawn failed: {0}")]
    Spawn(String),
    #[error("sidecar io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sidecar protocol: {0}")]
    Protocol(String),
    #[error("{code}: {message}")]
    Remote { code: String, message: String },
    #[error("sidecar is not ready")]
    NotReady,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub start: f64,
    pub end: f64,
    pub word: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub text: String,
    #[serde(default)]
    pub words: Vec<Word>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub text: String,
    #[serde(default)]
    pub segments: Vec<Segment>,
    pub duration: f64,
    pub decode_ms: u64,
    pub load_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ready {
    pub engine: String,
    pub version: String,
    pub model: String,
    pub device: String,
    pub kernels: String,
}

pub struct SttClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    ready: Ready,
}

impl SttClient {
    /// Spawn the sidecar and complete the `hello` handshake.
    pub fn start(binary: &Path, model_dir: &Path) -> Result<Self, SttError> {
        let mut child = Command::new(binary)
            .arg("--model")
            .arg(model_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| SttError::Spawn(format!("{}: {e}", binary.display())))?;

        let stdin = child.stdin.take().ok_or_else(|| SttError::Spawn("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| SttError::Spawn("no stdout".into()))?;

        let mut c = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            ready: Ready {
                engine: String::new(),
                version: String::new(),
                model: String::new(),
                device: String::new(),
                kernels: String::new(),
            },
        };
        c.send(&serde_json::json!({"type":"hello","proto":1}))?;
        let v = c.read_json()?;
        match v.get("type").and_then(|t| t.as_str()) {
            Some("ready") => {
                c.ready = serde_json::from_value(v).map_err(|e| SttError::Protocol(e.to_string()))?;
                Ok(c)
            }
            _ => Err(SttError::Protocol(format!("expected ready, got {v}"))),
        }
    }

    pub fn ready(&self) -> &Ready {
        &self.ready
    }

    fn send(&mut self, v: &serde_json::Value) -> Result<(), SttError> {
        let line = serde_json::to_string(v).map_err(|e| SttError::Protocol(e.to_string()))?;
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_json(&mut self) -> Result<serde_json::Value, SttError> {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line)?;
        if n == 0 {
            return Err(SttError::Protocol("sidecar closed the stream".into()));
        }
        serde_json::from_str(&line).map_err(|e| SttError::Protocol(e.to_string()))
    }

    pub fn transcribe(&mut self, wav: &Path, want_words: bool) -> Result<Transcript, SttError> {
        let id = uuid::Uuid::new_v4().to_string();
        self.send(&serde_json::json!({
            "type": "transcribe",
            "id": id,
            "wav": wav.to_string_lossy(),
            "timestamps": if want_words { "word" } else { "segment" },
        }))?;
        let v = self.read_json()?;
        match v.get("type").and_then(|t| t.as_str()) {
            Some("result") => {
                serde_json::from_value(v).map_err(|e| SttError::Protocol(e.to_string()))
            }
            Some("error") => Err(SttError::Remote {
                code: v
                    .get("code")
                    .and_then(|c| c.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                message: v
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string(),
            }),
            _ => Err(SttError::Protocol(format!("unexpected reply: {v}"))),
        }
    }

    pub fn ping(&mut self) -> Result<(), SttError> {
        self.send(&serde_json::json!({"type":"ping"}))?;
        let v = self.read_json()?;
        if v.get("type").and_then(|t| t.as_str()) == Some("pong") {
            Ok(())
        } else {
            Err(SttError::Protocol(format!("expected pong, got {v}")))
        }
    }

    /// Stop the sidecar. Sends a polite shutdown, then guarantees termination.
    /// The sidecar's process id.
    ///
    /// Used to prove cancellation by process death: a test can watch `/proc` for
    /// the pid and see it go, which is evidence, whereas "we sent a message" is
    /// not.
    pub fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }

    /// Whether the sidecar is still running.
    ///
    /// A crash is not a theoretical concern: the engine is a separate process
    /// doing heavy numeric work, and if it dies the client must notice. Without
    /// this check a single crash left the app holding a dead handle, so **every
    /// later dictation failed** until it was restarted — the worst kind of bug,
    /// because it looks like the app broke for no reason.
    ///
    /// `try_wait` reaps the child if it exited, so a dead engine is not left as a
    /// zombie.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn shutdown(&mut self) {
        let _ = self.send(&serde_json::json!({"type":"shutdown"}));
        let _ = self.child.wait();
    }

    /// Forcefully stop the sidecar. Used for cancellation: the process dies, so
    /// the CPU work provably stops.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for SttClient {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Locate the sidecar binary next to the running executable, or via env.
pub fn find_binary() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SOTTO_STT_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    for cand in ["sotto-stt", "../sotto-stt", "sotto-stt.exe"] {
        let p = dir.join(cand);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}
