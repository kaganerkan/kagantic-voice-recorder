//! cpal audio capture + frame accumulator.
//!
//! Design choices:
//! - The cpal callback runs on the real-time audio thread; it must never
//!   block. We use a `parking_lot::Mutex<Vec<f32>>` (sync, non-async) so
//!   pushes are O(1) without ever awaiting a future.
//! - Opus always operates at 48 kHz internally. We negotiate the capture
//!   device to 48 kHz when it supports that rate, falling back to the
//!   device's default only as a last resort. When the rate differs, the
//!   encoder's `sample_rate_from_u32` falls back to the closest supported
//!   rate and we resample.
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate as CpalSampleRate, StreamConfig};
use parking_lot::Mutex;

use crate::encoder::OpusStreamEncoder;

/// Negotiated capture format we expose to the rest of the app.
#[derive(Debug, Clone)]
pub struct CaptureFormat {
    pub sample_rate: u32,
    pub channels: u8,
}

/// Live capture stream + negotiated format.
pub struct Capture {
    pub stream: cpal::Stream,
    pub format: CaptureFormat,
}

/// The accumulator shared between the cpal real-time thread (lock + extend) and
/// the encoder worker thread (lock + drain). `parking_lot::Mutex` is sync
/// and never parks: the audio thread pushes in O(1) and never blocks.
pub type Accum = Arc<Mutex<Vec<f32>>>;

const PREFERRED_RATE: u32 = 48_000;

/// Pick a default input device and start capturing into a shared ring buffer.
pub fn start_capture(channels_hint: u8) -> Result<(Capture, Accum)> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no default input device"))?;
    start_capture_with_device(device, channels_hint)
}

/// Start capturing from an exact CPAL device name, as returned in the first
/// element of [`crate::devices::input_devices`], or the host default when absent
/// or empty. Human-facing labels must not be passed as selection identifiers.
pub fn start_capture_named(
    device_name: Option<&str>,
    channels_hint: u8,
) -> Result<(Capture, Accum)> {
    let host = cpal::default_host();
    let device = if let Some(name) = device_name.filter(|s| !s.is_empty()) {
        host.input_devices()
            .context("enumerate input devices for capture")?
            .find(|d| d.name().map(|n| n == name).unwrap_or(false))
            .ok_or_else(|| anyhow!("input device is unavailable: {name}"))?
    } else {
        host.default_input_device()
            .ok_or_else(|| anyhow!("no default input device"))?
    };
    start_capture_with_device(device, channels_hint)
}

fn start_capture_with_device(device: cpal::Device, channels_hint: u8) -> Result<(Capture, Accum)> {
    // Negotiate: prefer 48 kHz (matches libopus internal rate), fall back to
    // the device default if 48 kHz isn't in the supported config list.
    let (cfg, channels, sample_rate) = negotiate_config(&device, channels_hint)?;
    let sample_format = cfg.sample_format();

    let stream_config = StreamConfig {
        channels,
        sample_rate: CpalSampleRate(sample_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let accum: Accum = Arc::new(Mutex::new(Vec::with_capacity(
        sample_rate as usize * channels as usize / 50, // ~20ms of audio
    )));

    let err_fn = |e| tracing::error!("cpal stream error: {e}");

    let stream = match sample_format {
        SampleFormat::F32 => {
            let accum_cb = accum.clone();
            device
                .build_input_stream(
                    &stream_config,
                    move |data: &[f32], _| {
                        let mut buf = accum_cb.lock();
                        buf.extend_from_slice(data);
                    },
                    err_fn,
                    None,
                )
                .context("build f32 input stream")?
        }
        SampleFormat::I16 => {
            let accum_cb = accum.clone();
            device
                .build_input_stream(
                    &stream_config,
                    move |data: &[i16], _| {
                        let mut buf = accum_cb.lock();
                        for &s in data {
                            buf.push(s as f32 / 32768.0);
                        }
                    },
                    err_fn,
                    None,
                )
                .context("build i16 input stream")?
        }
        SampleFormat::U16 => {
            let accum_cb = accum.clone();
            device
                .build_input_stream(
                    &stream_config,
                    move |data: &[u16], _| {
                        let mut buf = accum_cb.lock();
                        for &s in data {
                            buf.push((s as f32 - 32768.0) / 32768.0);
                        }
                    },
                    err_fn,
                    None,
                )
                .context("build u16 input stream")?
        }
        f => return Err(anyhow!("unsupported sample format: {f:?}")),
    };

    stream.play().context("start cpal stream")?;
    tracing::info!(
        "capture started: {} ch @ {} Hz ({:?})",
        channels,
        sample_rate,
        sample_format
    );

    Ok((
        Capture {
            stream,
            format: CaptureFormat {
                sample_rate,
                channels: channels.try_into().expect("device channels fit in u8"),
            },
        },
        accum,
    ))
}

/// Negotiate a stream config: prefer 48 kHz, mono (unless the user pinned a
/// channel count), 16-bit or f32. Returns (supported_config, channels, rate).
fn negotiate_config(
    device: &cpal::Device,
    channels_hint: u8,
) -> Result<(cpal::SupportedStreamConfig, u16, u32)> {
    // First, walk the supported config range list and prefer 48 kHz.
    let supported = device
        .supported_input_configs()
        .context("query supported input configs")?;

    // Filter to configs whose max sample rate ≥ 48000 and min ≤ 48000, with
    // at least 1 channel. Also require f32 or i16 (drop u16 because it's
    // not commonly supported and we already had bugs in that path).
    let mut candidates: Vec<cpal::SupportedStreamConfigRange> = supported.collect();
    // Prefer matching sample format first (f32 > i16 > u16), then 48 kHz.
    candidates.sort_by_key(|r| {
        let rate_score =
            if r.min_sample_rate().0 <= PREFERRED_RATE && r.max_sample_rate().0 >= PREFERRED_RATE {
                0
            } else if r.max_sample_rate().0 >= PREFERRED_RATE {
                1
            } else {
                2
            };
        let fmt_score = match r.sample_format() {
            SampleFormat::F32 => 0,
            SampleFormat::I16 => 1,
            _ => 2,
        };
        let chan_score = if channels_hint == 0 {
            if r.channels() == 1 {
                0
            } else {
                1
            }
        } else if r.channels() == u16::from(channels_hint) {
            0
        } else {
            1
        };
        (rate_score, fmt_score, chan_score)
    });

    let range = candidates
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no supported input configs"))?;

    let channels = if channels_hint == 0 {
        range.channels()
    } else {
        // Use the user's hint if it's within what the range supports.
        range.channels().min(channels_hint.into())
    };

    let cfg = range.with_sample_rate(CpalSampleRate(PREFERRED_RATE));
    Ok((cfg, channels, PREFERRED_RATE))
}

/// Drain exactly one Opus frame from the accumulator. Sync; never blocks.
pub fn try_take_frame_blocking(accum: &Accum, enc: &OpusStreamEncoder) -> Option<Vec<f32>> {
    let need = enc.frame_size() * enc.channels() as usize;
    let mut buf = accum.lock();
    if buf.len() < need {
        return None;
    }
    Some(buf.drain(..need).collect())
}

/// Discard any accumulated samples (called on Pause to drop the partial frame).
pub fn discard_partial_blocking(accum: &Accum) {
    accum.lock().clear();
}

/// RMS of the most recent samples, scaled to 0..=1. Cheap; runs on the worker.
pub fn current_level_blocking(accum: &Accum) -> f32 {
    let buf = accum.lock();
    if buf.is_empty() {
        return 0.0;
    }
    // Use the last 1024 samples (or fewer) for a smooth meter.
    let start = buf.len().saturating_sub(1024);
    let mut sum = 0.0f32;
    for &s in &buf[start..] {
        sum += s * s;
    }
    let rms = (sum / (buf.len() - start) as f32).sqrt();
    (rms.min(1.0) * 2.0).min(1.0)
}

// --- Async wrappers kept for the existing CLI / run_recorder pipeline. ---

/// Async wrapper around `try_take_frame_blocking` for the CLI's `tokio::select!`.
pub async fn try_take_frame(accum: &Accum, enc: &OpusStreamEncoder) -> Option<Vec<f32>> {
    try_take_frame_blocking(accum, enc)
}

/// Async wrapper around `discard_partial_blocking`.
pub async fn discard_partial(accum: &Accum) {
    discard_partial_blocking(accum);
}
