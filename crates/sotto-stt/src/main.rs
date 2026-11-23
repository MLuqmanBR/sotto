//! Sotto speech-to-text sidecar.
//!
//! A separate process that owns the speech model and speaks a private NDJSON
//! protocol on stdin/stdout. This exists for four reasons, all of them
//! consequences of the chosen runtime (`voxrt`) having no cancellation token and
//! no option surface:
//!
//! 1. **Cancellation becomes real.** The parent kills this process group, and the
//!    operating system guarantees the CPU work stops. Nothing can be "cancelled"
//!    inside an uninterruptible in-process call.
//! 2. **The model stays resident**, so the ~1.05 s load cost is paid once per
//!    session rather than once per dictation.
//! 3. **Crash isolation.** A fault in inference cannot take down the UI or lose
//!    the audio journal.
//! 4. **No ABI surface.** The parent talks JSON, not C.
//!
//! Protocol (one JSON object per line, `\n`-terminated, max 1 MiB per control
//! message, logs on stderr only):
//!
//! ```text
//! -> {"type":"hello","proto":1}
//! <- {"type":"ready","proto":1,"engine":"voxrt","version":"0.2.1",
//!     "model":"parakeet-redux","device":"cpu"}
//! -> {"type":"transcribe","id":"...","wav":"/abs/path.wav","timestamps":"segment"}
//! <- {"type":"result","id":"...","text":"...","segments":[...],
//!     "duration":3.42,"decode_ms":88}
//! <- {"type":"error","id":"...","code":"...","message":"..."}
//! -> {"type":"ping"}
//! <- {"type":"pong"}
//! -> {"type":"shutdown"}
//! ```
//!
//! `--selftest` runs a built-in check without any I/O and exits 0/1, so the
//! packaging and CI can verify a build without a microphone.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Instant;

const PROTO: u32 = 1;

/// Version of the vendored voxrt runtime, recorded at build time.
const VERSION: &str = "0.2.1";
const MAX_LINE: usize = 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
    Hello { #[serde(default)] proto: u32 },
    Transcribe {
        id: String,
        wav: String,
        #[serde(default)]
        timestamps: Option<String>,
    },
    Ping,
    Shutdown,
}

#[derive(Debug, Serialize)]
struct Word {
    start: f64,
    end: f64,
    word: String,
}

#[derive(Debug, Serialize)]
struct Segment {
    start: f64,
    end: f64,
    text: String,
    words: Vec<Word>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Response<'a> {
    Ready {
        proto: u32,
        engine: &'a str,
        version: &'a str,
        model: &'a str,
        device: &'a str,
        kernels: &'a str,
    },
    Result {
        id: String,
        text: String,
        segments: Vec<Segment>,
        duration: f64,
        decode_ms: u128,
        load_ms: u128,
    },
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        code: &'a str,
        message: String,
    },
    Pong,
    Bye,
}

fn emit(out: &mut impl Write, r: &Response) -> std::io::Result<()> {
    let line = serde_json::to_string(r).unwrap_or_else(|e| {
        format!(
            r#"{{"type":"error","code":"serialise","message":{}}}"#,
            serde_json::to_string(&e.to_string()).unwrap()
        )
    });
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

/// Human-readable description of the kernel voxrt selected, for the UI's
/// diagnostics panel. Derived from CPU features rather than guessed.
fn kernel_description() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512vnni") {
            return "avx512-vnni";
        }
        if is_x86_feature_detected!("avx2") {
            return "avx2";
        }
        return "scalar";
    }
    #[cfg(target_arch = "aarch64")]
    {
        if std::arch::is_aarch64_feature_detected!("dotprod") {
            return "neon-dotprod";
        }
        return "scalar";
    }
    #[allow(unreachable_code)]
    "scalar"
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--selftest") {
        std::process::exit(selftest(&args));
    }
    if args.iter().any(|a| a == "--version") {
        println!("sotto-stt {} (voxrt {})", env!("CARGO_PKG_VERSION"), VERSION);
        return;
    }

    let model_dir = args
        .iter()
        .position(|a| a == "--model")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .or_else(|| std::env::var("SOTTO_MODEL_DIR").ok())
        .or_else(|| std::env::var("VOXRT_MODEL_DIR").ok());

    let mut stdout = std::io::stdout().lock();
    let stdin = std::io::stdin().lock();

    let Some(model_dir) = model_dir else {
        let _ = emit(
            &mut stdout,
            &Response::Error {
                id: None,
                code: "no_model",
                message: "no model directory given (use --model or SOTTO_MODEL_DIR)".into(),
            },
        );
        std::process::exit(2);
    };

    if !Path::new(&model_dir).is_dir() {
        let _ = emit(
            &mut stdout,
            &Response::Error {
                id: None,
                code: "model_missing",
                message: format!("model directory does not exist: {model_dir}"),
            },
        );
        std::process::exit(2);
    }

    let mut transcriber: Option<voxrt::transcriber::Transcriber> = None;
    let mut load_ms: u128 = 0;

    for line in stdin.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > MAX_LINE {
            let _ = emit(
                &mut stdout,
                &Response::Error {
                    id: None,
                    code: "line_too_long",
                    message: format!("control message exceeded {MAX_LINE} bytes"),
                },
            );
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let _ = emit(
                    &mut stdout,
                    &Response::Error {
                        id: None,
                        code: "bad_json",
                        message: e.to_string(),
                    },
                );
                continue;
            }
        };

        match req {
            Request::Hello { proto } => {
                if proto != PROTO {
                    let _ = emit(
                        &mut stdout,
                        &Response::Error {
                            id: None,
                            code: "proto_mismatch",
                            message: format!("client proto {proto}, sidecar proto {PROTO}"),
                        },
                    );
                    let _ = stdout.flush();
                    std::process::exit(3);
                }
                // Load lazily but eagerly enough that the first dictation is warm.
                if transcriber.is_none() {
                    let t0 = Instant::now();
                    match voxrt::transcriber::Transcriber::load(&model_dir) {
                        Ok(t) => {
                            load_ms = t0.elapsed().as_millis();
                            transcriber = Some(t);
                        }
                        Err(e) => {
                            let _ = emit(
                                &mut stdout,
                                &Response::Error {
                                    id: None,
                                    code: "model_load_failed",
                                    message: e.to_string(),
                                },
                            );
                            let _ = stdout.flush();
                            std::process::exit(4);
                        }
                    }
                }
                let _ = emit(
                    &mut stdout,
                    &Response::Ready {
                        proto: PROTO,
                        engine: "voxrt",
                        version: VERSION,
                        model: "parakeet-redux",
                        device: "cpu",
                        kernels: kernel_description(),
                    },
                );
            }
            Request::Transcribe { id, wav, timestamps } => {
                let Some(t) = transcriber.as_ref() else {
                    let _ = emit(
                        &mut stdout,
                        &Response::Error {
                            id: Some(id),
                            code: "not_ready",
                            message: "hello must be sent before transcribe".into(),
                        },
                    );
                    continue;
                };
                if !Path::new(&wav).is_file() {
                    let _ = emit(
                        &mut stdout,
                        &Response::Error {
                            id: Some(id),
                            code: "audio_missing",
                            message: format!("no such file: {wav}"),
                        },
                    );
                    continue;
                }
                let t0 = Instant::now();
                match t.transcribe_wav(&wav) {
                    Ok(tr) => {
                        let decode_ms = t0.elapsed().as_millis();
                        // Word-level timings are only shipped when asked for: they
                        // roughly double the payload and most callers never need them.
                        let want_words = timestamps.as_deref() == Some("word");
                        let segments = tr
                            .segments
                            .iter()
                            .map(|s| Segment {
                                start: s.start,
                                end: s.end,
                                text: s.text.clone(),
                                words: if want_words {
                                    s.words
                                        .iter()
                                        .map(|w| Word {
                                            start: w.start,
                                            end: w.end,
                                            word: w.word.clone(),
                                        })
                                        .collect()
                                } else {
                                    Vec::new()
                                },
                            })
                            .collect();
                        let _ = emit(
                            &mut stdout,
                            &Response::Result {
                                id,
                                text: tr.text,
                                segments,
                                duration: tr
                                    .segments
                                    .last()
                                    .map(|s| s.end)
                                    .unwrap_or_default(),
                                decode_ms,
                                load_ms,
                            },
                        );
                    }
                    Err(e) => {
                        let _ = emit(
                            &mut stdout,
                            &Response::Error {
                                id: Some(id),
                                code: "transcribe_failed",
                                message: e.to_string(),
                            },
                        );
                    }
                }
            }
            Request::Ping => {
                let _ = emit(&mut stdout, &Response::Pong);
            }
            Request::Shutdown => {
                let _ = emit(&mut stdout, &Response::Bye);
                break;
            }
        }
    }
}

/// Verify the binary and its model without a microphone or a parent process.
fn selftest(args: &[String]) -> i32 {
    let model_dir = args
        .iter()
        .position(|a| a == "--model")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .or_else(|| std::env::var("SOTTO_MODEL_DIR").ok())
        .or_else(|| std::env::var("VOXRT_MODEL_DIR").ok());

    eprintln!("sotto-stt selftest");
    eprintln!("  version   : {} (voxrt {})", env!("CARGO_PKG_VERSION"), VERSION);
    eprintln!("  kernel    : {}", kernel_description());

    let Some(model_dir) = model_dir else {
        eprintln!("  model     : NOT PROVIDED (use --model or SOTTO_MODEL_DIR)");
        return 2;
    };
    eprintln!("  model dir : {model_dir}");
    let t0 = Instant::now();
    match voxrt::transcriber::Transcriber::load(&model_dir) {
        Ok(_) => {
            eprintln!("  load      : {} ms", t0.elapsed().as_millis());
            eprintln!("  result    : OK");
            0
        }
        Err(e) => {
            eprintln!("  load      : FAILED: {e}");
            1
        }
    }
}
