//! Audio capture and WAV writing.
//!
//! The capture callback does no allocation, no I/O and no locking beyond a
//! bounded push into a shared buffer, per BUILD-PLAN §5.2. Samples are converted
//! to 16 kHz mono f32 lazily when the recording is finalised, so a device that
//! only offers 48 kHz still works without resampling in the callback.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("no input device: {0}")]
    NoDevice(String),
    #[error("device build failed: {0}")]
    Build(String),
    #[error("stream start failed: {0}")]
    Start(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InputDevice {
    pub name: String,
    pub is_default: bool,
    pub channels: u16,
    pub sample_rate: u32,
}

/// List capture-capable devices.
pub fn list_inputs() -> Result<Vec<InputDevice>, AudioError> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let default_name = host
        .default_input_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_default();
    let mut out = Vec::new();
    let devices = host.input_devices().map_err(|e| AudioError::NoDevice(e.to_string()))?;
    for d in devices {
        let name = d.name().unwrap_or_else(|_| "unknown".into());
        let cfg = d.default_input_config().ok();
        out.push(InputDevice {
            is_default: name == default_name,
            name,
            channels: cfg.as_ref().map(|c| c.channels()).unwrap_or(1),
            sample_rate: cfg.as_ref().map(|c| c.sample_rate().0).unwrap_or(16_000),
        });
    }
    Ok(out)
}

struct Shared {
    buf: Mutex<Vec<f32>>,
    peak: AtomicU32,
    frames: AtomicU32,
    overflow: AtomicBool,
}

impl Shared {
    /// Mono 16 kHz copy of everything captured so far.
    ///
    /// Used by the live overlay to show words as they are recognised. It copies
    /// rather than drains, so the final transcript still sees the whole take.
    pub fn snapshot_16k(&self, channels: u16, rate: u32) -> Vec<f32> {
        let buf = match self.buf.try_lock() {
            Ok(g) => g.clone(),
            Err(_) => return Vec::new(),
        };
        let mono = downmix(&buf, channels);
        if rate == 16_000 {
            mono
        } else {
            resample_linear(&mono, rate, 16_000)
        }
    }

    /// Total frames pushed by the capture callback, at the source rate.
    pub fn frames(&self) -> u32 {
        self.frames.load(Ordering::Relaxed)
    }
}

/// A read-only handle on a live capture.
///
/// The audio stream itself never leaves the thread that opened it, so this is
/// how anything else — the level meter, the live-preview worker — observes the
/// capture: it clones a small set of atomics plus a shared buffer, not the
/// stream. Cheap to clone and safe to read from another thread.
#[derive(Clone)]
pub struct AudioTap {
    shared: Arc<Shared>,
    source_rate: u32,
    source_channels: u16,
}

impl AudioTap {
    /// Current input level, 0.0..=1.0, for the overlay meter.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.shared.peak.load(Ordering::Relaxed))
    }

    /// Mono 16 kHz copy of everything captured so far.
    ///
    /// Used by the live overlay to show words as they are recognised. It copies
    /// rather than drains, so the final transcript still sees the whole take.
    pub fn preview_16k(&self) -> Vec<f32> {
        self.shared
            .snapshot_16k(self.source_channels, self.source_rate)
    }

    /// How many samples (mono, 16 kHz) have been captured so far.
    pub fn captured_samples_16k(&self) -> usize {
        let frames = self.shared.frames() as u64;
        let ch = self.source_channels.max(1) as u64;
        let mono = frames / ch;
        if self.source_rate == 16_000 {
            mono as usize
        } else {
            (mono as u128 * 16_000 / self.source_rate as u128) as usize
        }
    }
}

/// A live recording. Dropping it stops capture.
pub struct Recorder {
    stream: Option<cpal::Stream>,
    shared: Arc<Shared>,
    source_rate: u32,
    source_channels: u16,
}

/// Cap on buffered samples: ~10 minutes at 48 kHz mono. Beyond this the take is
/// flagged rather than growing without bound.
const MAX_SAMPLES: usize = 48_000 * 60 * 10;

impl Recorder {
    /// Start capturing from `device_name`, or the system default when `None`.
    pub fn start(device_name: Option<&str>, gain: f32) -> Result<Self, AudioError> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        let host = cpal::default_host();
        let device = match device_name {
            Some(n) => host
                .input_devices()
                .map_err(|e| AudioError::NoDevice(e.to_string()))?
                .find(|d| d.name().map(|x| x == n).unwrap_or(false))
                .ok_or_else(|| AudioError::NoDevice(format!("device `{n}` not found")))?,
            None => host
                .default_input_device()
                .ok_or_else(|| AudioError::NoDevice("no default input device".into()))?,
        };

        let supported = device
            .default_input_config()
            .map_err(|e| AudioError::Build(e.to_string()))?;
        let source_rate = supported.sample_rate().0;
        let source_channels = supported.channels();
        let config: cpal::StreamConfig = supported.clone().into();

        let shared = Arc::new(Shared {
            buf: Mutex::new(Vec::with_capacity(source_rate as usize * 4)),
            peak: AtomicU32::new(0),
            frames: AtomicU32::new(0),
            overflow: AtomicBool::new(false),
        });
        let s = shared.clone();

        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device.build_input_stream(
                &config,
                move |data: &[f32], _| push(&s, data, gain, 1.0),
                err_cb,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_input_stream(
                &config,
                move |data: &[i16], _| {
                    let tmp: Vec<f32> = data.iter().map(|v| *v as f32 / 32768.0).collect();
                    push(&s, &tmp, gain, 1.0)
                },
                err_cb,
                None,
            ),
            cpal::SampleFormat::U16 => device.build_input_stream(
                &config,
                move |data: &[u16], _| {
                    let tmp: Vec<f32> = data
                        .iter()
                        .map(|v| (*v as f32 - 32768.0) / 32768.0)
                        .collect();
                    push(&s, &tmp, gain, 1.0)
                },
                err_cb,
                None,
            ),
            other => {
                return Err(AudioError::Build(format!(
                    "unsupported sample format {other:?}"
                )))
            }
        }
        .map_err(|e| AudioError::Build(e.to_string()))?;

        stream.play().map_err(|e| AudioError::Start(e.to_string()))?;
        Ok(Self {
            stream: Some(stream),
            shared,
            source_rate,
            source_channels,
        })
    }

    /// Current input level, 0.0..=1.0, for the overlay meter.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.shared.peak.load(Ordering::Relaxed))
    }

    /// A cheap, cloneable read-only handle on this capture.
    pub fn tap(&self) -> AudioTap {
        AudioTap {
            shared: self.shared.clone(),
            source_rate: self.source_rate,
            source_channels: self.source_channels,
        }
    }

    pub fn source_rate(&self) -> u32 {
        self.source_rate
    }

    pub fn overflowed(&self) -> bool {
        self.shared.overflow.load(Ordering::Relaxed)
    }

    pub fn stop(mut self) -> Recorded {
        self.stream = None;
        let buf = self.shared.buf.lock().map(|g| g.clone()).unwrap_or_default();
        let mono = downmix(&buf, self.source_channels);
        let resampled = resample_linear(&mono, self.source_rate, 16_000);
        Recorded {
            samples_16k: resampled,
            source_rate: self.source_rate,
            source_channels: self.source_channels,
        }
    }
}

fn err_cb(e: cpal::StreamError) {
    eprintln!("audio stream error: {e}");
}

fn push(shared: &Arc<Shared>, data: &[f32], gain: f32, _scale: f32) {
    let mut peak = 0f32;
    for &v in data {
        let a = v.abs();
        if a > peak {
            peak = a;
        }
    }
    shared.peak.store(peak.min(1.0).to_bits(), Ordering::Relaxed);
    shared
        .frames
        .fetch_add(data.len() as u32, Ordering::Relaxed);
    if let Ok(mut g) = shared.buf.try_lock() {
        if g.len() + data.len() > MAX_SAMPLES {
            shared.overflow.store(true, Ordering::Relaxed);
            return;
        }
        if (gain - 1.0).abs() < f32::EPSILON {
            g.extend_from_slice(data);
        } else {
            g.extend(data.iter().map(|v| (v * gain).clamp(-1.0, 1.0)));
        }
    }
}

/// Average channels down to mono.
pub fn downmix(interleaved: &[f32], channels: u16) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    if ch == 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks(ch)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

/// Linear resampler. Adequate for speech at 16 kHz; the model's own front end
/// does the real spectral work.
pub fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let out_len = ((input.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 * ratio;
        let i0 = src.floor() as usize;
        let i1 = (i0 + 1).min(input.len() - 1);
        let frac = (src - i0 as f64) as f32;
        out.push(input[i0] * (1.0 - frac) + input[i1] * frac);
    }
    out
}

/// A finished recording, already at the model's expected format.
pub struct Recorded {
    pub samples_16k: Vec<f32>,
    pub source_rate: u32,
    pub source_channels: u16,
}

impl Recorded {
    pub fn duration_ms(&self) -> i64 {
        (self.samples_16k.len() as f64 / 16.0) as i64
    }

    pub fn is_empty(&self) -> bool {
        self.samples_16k.len() < 1_600 // under 100 ms
    }

    /// Write a 16-bit PCM mono 16 kHz WAV.
    pub fn write_wav(&self, path: &Path) -> Result<(), AudioError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = Vec::with_capacity(44 + self.samples_16k.len() * 2);
        let data_len = (self.samples_16k.len() * 2) as u32;
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&16_000u32.to_le_bytes());
        out.extend_from_slice(&(16_000u32 * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for s in &self.samples_16k {
            let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            out.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(path, out)?;
        Ok(())
    }
}
