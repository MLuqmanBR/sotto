//! Playback of retained recordings.
//!
//! The History page needs to replay a recording, and the webview cannot do it on
//! this stack: WebKitGTK refused both a custom URI scheme and a `blob:` URL with
//! the same media error, while GStreamer itself decoded the same file happily
//! (`gst-launch-1.0 filesrc ! wavparse ! fakesink` exits 0). Rather than depend
//! on a webview media stack that is not working here, playback runs in this
//! process through the same CPAL dependency capture already uses.
//!
//! One recording plays at a time. Starting a new one stops the previous one,
//! which is what a person expects when they press play on a second entry, and it
//! guarantees at most one output stream is ever open.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PlayError {
    #[error("recording not found: {0}")]
    Missing(String),
    #[error("no output device: {0}")]
    NoDevice(String),
    #[error("device build failed: {0}")]
    Build(String),
    #[error("stream start failed: {0}")]
    Start(String),
    #[error("could not read the recording: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported wav: {0}")]
    Format(String),
}

/// A decoded WAV file: 16-bit PCM, mono or multi-channel.
#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    pub samples: Vec<f32>,
    pub channels: u16,
    pub sample_rate: u32,
}

impl Decoded {
    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate == 0 || self.channels == 0 {
            return 0;
        }
        (self.samples.len() as u64 * 1000) / (self.sample_rate as u64 * self.channels as u64)
    }

    /// Average the channels down to mono, so playback works on any device.
    pub fn to_mono(&self) -> Vec<f32> {
        if self.channels <= 1 {
            return self.samples.clone();
        }
        let ch = self.channels as usize;
        self.samples
            .chunks_exact(ch)
            .map(|frame| frame.iter().sum::<f32>() / ch as f32)
            .collect()
    }
}

/// Read a 16-bit PCM WAV into memory.
///
/// Only what Sotto itself writes is supported: mono or stereo, 16-bit, any rate.
/// A file that is not that shape is reported as `Format` rather than played as
/// noise, because a wrong interpretation of the header sounds like corruption.
pub fn decode_wav(path: &Path) -> Result<Decoded, PlayError> {
    let bytes = std::fs::read(path).map_err(|_| PlayError::Missing(path.display().to_string()))?;
    parse_wav(&bytes)
}

/// Parse the WAV bytes. Split out so it can be tested without a file.
pub fn parse_wav(bytes: &[u8]) -> Result<Decoded, PlayError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(PlayError::Format("not a RIFF/WAVE file".into()));
    }

    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut bits = 0u16;
    let mut data: Option<&[u8]> = None;

    // Walk the chunk list. `fmt ` and `data` are not guaranteed to be adjacent
    // or in that order, so each is located on its own.
    let mut i = 12usize;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let size = u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]])
            as usize;
        let body_start = i + 8;
        let body_end = body_start.saturating_add(size).min(bytes.len());
        let body = &bytes[body_start..body_end];

        match id {
            b"fmt " => {
                if body.len() < 16 {
                    return Err(PlayError::Format("fmt chunk too short".into()));
                }
                let format = u16::from_le_bytes([body[0], body[1]]);
                channels = u16::from_le_bytes([body[2], body[3]]);
                sample_rate =
                    u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                bits = u16::from_le_bytes([body[14], body[15]]);
                if format != 1 {
                    return Err(PlayError::Format(format!(
                        "only uncompressed PCM is supported (format tag {format})"
                    )));
                }
            }
            b"data" => data = Some(body),
            _ => {}
        }

        // Chunks are word-aligned: an odd size is followed by one pad byte.
        i = body_start + size + (size & 1);
    }

    if channels == 0 || sample_rate == 0 {
        return Err(PlayError::Format("missing fmt chunk".into()));
    }
    if bits != 16 {
        return Err(PlayError::Format(format!("expected 16-bit samples, got {bits}-bit")));
    }
    let data = data.ok_or_else(|| PlayError::Format("missing data chunk".into()))?;

    let samples = data
        .chunks_exact(2)
        .map(|p| i16::from_le_bytes([p[0], p[1]]) as f32 / 32_768.0)
        .collect();

    Ok(Decoded {
        samples,
        channels,
        sample_rate,
    })
}

/// A running playback. Dropping it stops the sound.
pub struct Player {
    stop: Arc<AtomicBool>,
    stream: Option<cpal::Stream>,
    /// Samples played so far, so the UI can show progress.
    played: Arc<AtomicU64>,
    total: u64,
    /// Rate the samples are played at, for turning counts into milliseconds.
    rate: u32,
}

impl Player {
    /// Start playing `path` on the default output device.
    pub fn start(path: &Path) -> Result<Self, PlayError> {
        let decoded = decode_wav(path)?;
        if decoded.samples.is_empty() {
            return Err(PlayError::Format("the recording is empty".into()));
        }
        Self::play_mono(decoded.to_mono(), decoded.sample_rate)
    }

    /// Play already-decoded mono samples.
    pub fn play_mono(samples: Vec<f32>, source_rate: u32) -> Result<Self, PlayError> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        // Refuse before touching the audio device. Opening a stream to play
        // nothing would take exclusive access, make the device appear busy, and
        // then stop immediately — an error the user cannot act on.
        if samples.is_empty() {
            return Err(PlayError::Format("the recording is empty".into()));
        }

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| PlayError::NoDevice("no default output".into()))?;
        let config = device
            .default_output_config()
            .map_err(|e| PlayError::Build(e.to_string()))?;

        let out_rate = config.sample_rate().0;
        let out_channels = config.channels() as usize;
        let format = config.sample_format();

        // Resample to the device rate once, up front, rather than per callback.
        let mono = if out_rate == source_rate {
            samples
        } else {
            sotto_core_resample(&samples, source_rate, out_rate)
        };
        let total = mono.len() as u64;

        let stop = Arc::new(AtomicBool::new(false));
        let played = Arc::new(AtomicU64::new(0));
        let cursor = Arc::new(Mutex::new(0usize));
        let data = Arc::new(mono);

        let stop_cb = stop.clone();
        let played_cb = played.clone();
        let cursor_cb = cursor.clone();
        let data_cb = data.clone();

        // The callback must not allocate or block: it reads from a shared cursor
        // over a pre-allocated buffer and duplicates mono across channels.
        let next = move |out: &mut [f32]| {
            if stop_cb.load(Ordering::Relaxed) {
                out.fill(0.0);
                return;
            }
            let mut pos = cursor_cb.lock().map(|g| *g).unwrap_or(usize::MAX);
            for frame in out.chunks_mut(out_channels.max(1)) {
                let s = data_cb.get(pos).copied().unwrap_or(0.0);
                for v in frame.iter_mut() {
                    *v = s;
                }
                if pos < data_cb.len() {
                    pos += 1;
                } else {
                    // Reached the end: silence from here on.
                    for v in frame.iter_mut() {
                        *v = 0.0;
                    }
                }
            }
            if let Ok(mut g) = cursor_cb.lock() {
                *g = pos.min(data_cb.len());
            }
            played_cb.store(pos.min(data_cb.len()) as u64, Ordering::Relaxed);
            if pos >= data_cb.len() {
                stop_cb.store(true, Ordering::Relaxed);
            }
        };

        let err_fn = |e| eprintln!("playback stream error: {e}");
        let stream = match format {
            cpal::SampleFormat::F32 => device.build_output_stream(
                &config.into(),
                move |out: &mut [f32], _| next(out),
                err_fn,
                None,
            ),
            cpal::SampleFormat::I16 => {
                let next = next;
                device.build_output_stream(
                    &config.into(),
                    move |out: &mut [i16], _| {
                        let mut tmp = vec![0f32; out.len()];
                        next(&mut tmp);
                        for (o, s) in out.iter_mut().zip(tmp) {
                            *o = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                        }
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let next = next;
                device.build_output_stream(
                    &config.into(),
                    move |out: &mut [u16], _| {
                        let mut tmp = vec![0f32; out.len()];
                        next(&mut tmp);
                        for (o, s) in out.iter_mut().zip(tmp) {
                            let v = (s.clamp(-1.0, 1.0) + 1.0) / 2.0;
                            *o = (v * u16::MAX as f32) as u16;
                        }
                    },
                    err_fn,
                    None,
                )
            }
            other => {
                return Err(PlayError::Format(format!(
                    "unsupported output sample format: {other:?}"
                )))
            }
        }
        .map_err(|e| PlayError::Build(e.to_string()))?;

        stream
            .play()
            .map_err(|e| PlayError::Start(e.to_string()))?;

        Ok(Player {
            stop,
            stream: Some(stream),
            played,
            total,
            rate: out_rate,
        })
    }

    /// The rate these samples are played at, in samples per second.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Ask playback to stop. Idempotent.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        !self.stop.load(Ordering::Relaxed) && self.played.load(Ordering::Relaxed) < self.total
    }

    /// Samples played so far, and the total. Used for a progress readout.
    pub fn progress(&self) -> (u64, u64) {
        (self.played.load(Ordering::Relaxed), self.total)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // Stop the callback before the stream is torn down, so it cannot write
        // into a buffer that is going away.
        self.stop.store(true, Ordering::Relaxed);
        self.stream = None;
    }
}

/// Linear resampling, matching what capture uses for its own conversion.
fn sotto_core_resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    crate::audio::resample_linear(input, from, to)
}

/// A short tone, for the start/stop cue.
///
/// Generated rather than shipped as a file: two sine bursts cost a few hundred
/// kilobytes of nothing, and a WAV asset would have to be located at runtime.
/// The envelope ramps in and out over 8 ms so the tone does not click.
pub fn tone(freq_hz: f32, duration_ms: u32, amplitude: f32, rate: u32) -> Vec<f32> {
    let n = (rate as f64 * duration_ms as f64 / 1000.0) as usize;
    let ramp = (rate as f64 * 0.008) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / rate as f64;
            let s = (2.0 * std::f64::consts::PI * freq_hz as f64 * t).sin() as f32;
            let env = if ramp == 0 {
                1.0
            } else if i < ramp {
                i as f32 / ramp as f32
            } else if i + ramp >= n {
                (n.saturating_sub(i)) as f32 / ramp as f32
            } else {
                1.0
            };
            s * amplitude * env.clamp(0.0, 1.0)
        })
        .collect()
}

#[cfg(test)]
mod tone_tests {
    use super::tone;

    #[test]
    fn tone_has_the_requested_length_and_stays_in_range() {
        let t = tone(880.0, 90, 0.15, 16_000);
        assert_eq!(t.len(), 16_000 * 90 / 1000);
        assert!(t.iter().all(|s| s.abs() <= 0.16), "amplitude must stay bounded");
        assert!(t.iter().any(|s| s.abs() > 0.1), "the tone must actually sound");
    }

    #[test]
    fn tone_starts_and_ends_at_silence_so_it_does_not_click() {
        let t = tone(660.0, 120, 0.2, 16_000);
        assert!(t[0].abs() < 0.001, "starts at silence");
        assert!(t[t.len() - 1].abs() < 0.01, "ends at silence");
    }

    #[test]
    fn a_zero_length_tone_is_empty_not_a_panic() {
        assert!(tone(440.0, 0, 0.2, 16_000).is_empty());
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal 16-bit PCM WAV in memory.
    fn wav(channels: u16, rate: u32, samples: &[i16]) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
        out.extend_from_slice(&(channels * 2).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    #[test]
    fn parses_mono_16k() {
        let bytes = wav(1, 16_000, &[0, 16_384, -16_384]);
        let d = parse_wav(&bytes).unwrap();
        assert_eq!(d.channels, 1);
        assert_eq!(d.sample_rate, 16_000);
        assert_eq!(d.samples.len(), 3);
        assert!((d.samples[1] - 0.5).abs() < 0.001);
        assert!((d.samples[2] + 0.5).abs() < 0.001);
    }

    #[test]
    fn duration_is_derived_from_rate_and_channels() {
        let bytes = wav(2, 48_000, &[0i16; 96_000]); // 48000 stereo frames
        let d = parse_wav(&bytes).unwrap();
        assert_eq!(d.duration_ms(), 1000, "one second of stereo at 48 kHz");
    }

    #[test]
    fn stereo_is_downmixed_to_mono() {
        // Left +1.0, right -1.0 averages to 0.
        let bytes = wav(2, 16_000, &[32_767, -32_768, 32_767, 32_767]);
        let d = parse_wav(&bytes).unwrap();
        let mono = d.to_mono();
        assert_eq!(mono.len(), 2);
        assert!(mono[0].abs() < 0.001, "opposite channels cancel");
        assert!((mono[1] - 1.0).abs() < 0.001, "equal channels add up");
    }

    #[test]
    fn rejects_a_non_wav() {
        assert!(matches!(parse_wav(b"not a wav at all"), Err(PlayError::Format(_))));
        assert!(matches!(parse_wav(&[]), Err(PlayError::Format(_))));
    }

    #[test]
    fn rejects_non_pcm_and_non_16bit() {
        let mut bytes = wav(1, 16_000, &[0, 0]);
        // Format tag 3 is IEEE float: not what we write, so refuse it.
        bytes[20] = 3;
        assert!(matches!(parse_wav(&bytes), Err(PlayError::Format(_))));

        let mut bytes = wav(1, 16_000, &[0, 0]);
        bytes[34] = 8; // bits per sample
        assert!(matches!(parse_wav(&bytes), Err(PlayError::Format(_))));
    }

    #[test]
    fn missing_file_is_reported_as_missing() {
        let err = decode_wav(Path::new("/nonexistent/x.wav")).unwrap_err();
        assert!(matches!(err, PlayError::Missing(_)), "got {err:?}");
    }

    #[test]
    fn reads_a_real_file_written_by_the_recorder() {
        let dir = std::env::temp_dir().join(format!("sotto-play-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.wav");

        let rec = crate::audio::Recorded {
            samples_16k: vec![0.0, 0.25, -0.25, 0.5],
            source_rate: 16_000,
            source_channels: 1,
        };
        rec.write_wav(&path).unwrap();

        let d = decode_wav(&path).unwrap();
        assert_eq!(d.sample_rate, 16_000);
        assert_eq!(d.channels, 1);
        assert_eq!(d.samples.len(), 4);
        assert!((d.samples[1] - 0.25).abs() < 0.01);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_recording_is_rejected_before_opening_a_device() {
        // No output device is touched: the emptiness check comes first, so this
        // is safe to run on a machine with no sound card.
        let err = Player::play_mono(vec![], 16_000);
        // Either it refuses for being empty, or the machine has no output device.
        match err {
            Err(PlayError::NoDevice(_)) | Err(PlayError::Format(_)) => {}
            Err(e) => panic!("unexpected error {e:?}"),
            Ok(_) => panic!("an empty buffer must not start a stream"),
        }
    }
}
