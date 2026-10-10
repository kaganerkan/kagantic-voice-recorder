//! cpal audio capture + frame accumulator.
//!
//! Prefer a supported 48 kHz input configuration; retain the actual device
//! rate otherwise. Conversion to Opus's 48 kHz clock happens in the sink.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate as CpalSampleRate, StreamConfig};
use parking_lot::Mutex;

use crate::encoder::OpusStreamEncoder;

/// Cumulative interleaved callback counters, independent of the worker draining.
#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureSnapshot {
    pub callbacks: u64,
    pub samples: u64,
    pub nonzero_samples: u64,
}

/// Shared callback diagnostics; no logging subscriber is needed to retain errors.
#[derive(Clone, Default)]
pub struct CaptureDiagnostics {
    inner: Arc<CaptureDiagnosticsInner>,
}

#[derive(Default)]
struct CaptureDiagnosticsInner {
    callbacks: AtomicU64,
    samples: AtomicU64,
    nonzero_samples: AtomicU64,
    error: Mutex<Option<String>>,
}

impl CaptureDiagnostics {
    pub fn record_samples(&self, samples: &[f32]) {
        self.record_counts(samples.len(), samples.iter().filter(|&&s| s != 0.0).count());
    }

    fn record_counts(&self, samples: usize, nonzero: usize) {
        self.inner
            .samples
            .fetch_add(samples as u64, Ordering::Relaxed);
        self.inner
            .nonzero_samples
            .fetch_add(nonzero as u64, Ordering::Relaxed);
        self.inner.callbacks.fetch_add(1, Ordering::Release);
    }

    pub fn report_error(&self, error: String) {
        tracing::error!("capture backend: {error}");
        // Preserve the first failure rather than replacing it with cascading errors.
        self.inner.error.lock().get_or_insert(error);
    }

    pub fn snapshot(&self) -> CaptureSnapshot {
        CaptureSnapshot {
            callbacks: self.inner.callbacks.load(Ordering::Acquire),
            samples: self.inner.samples.load(Ordering::Relaxed),
            nonzero_samples: self.inner.nonzero_samples.load(Ordering::Relaxed),
        }
    }

    pub fn take_error(&self) -> Option<String> {
        self.inner.error.lock().take()
    }
}

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
    pub diagnostics: CaptureDiagnostics,
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
    let cfg = negotiate_config(&device, channels_hint)?;
    let sample_format = cfg.sample_format();
    let channels = cfg.channels();
    let sample_rate = cfg.sample_rate().0;
    let stream_config: StreamConfig = cfg.into();

    let accum: Accum = Arc::new(Mutex::new(Vec::with_capacity(
        sample_rate as usize * channels as usize / 50, // ~20ms of audio
    )));

    let diagnostics = CaptureDiagnostics::default();
    let errors = diagnostics.clone();
    let err_fn = move |e: cpal::StreamError| errors.report_error(e.to_string());

    let stream = match sample_format {
        SampleFormat::F32 => {
            let accum_cb = accum.clone();
            let diagnostics = diagnostics.clone();
            device
                .build_input_stream(
                    &stream_config,
                    move |data: &[f32], _| {
                        let mut buf = accum_cb.lock();
                        buf.extend_from_slice(data);
                        diagnostics.record_samples(data);
                    },
                    err_fn,
                    None,
                )
                .context("build f32 input stream")?
        }
        SampleFormat::I16 => {
            let accum_cb = accum.clone();
            let diagnostics = diagnostics.clone();
            device
                .build_input_stream(
                    &stream_config,
                    move |data: &[i16], _| {
                        let mut buf = accum_cb.lock();
                        for &s in data {
                            buf.push(s as f32 / 32768.0);
                        }
                        diagnostics
                            .record_counts(data.len(), data.iter().filter(|&&s| s != 0).count());
                    },
                    err_fn,
                    None,
                )
                .context("build i16 input stream")?
        }
        SampleFormat::U16 => {
            let accum_cb = accum.clone();
            let diagnostics = diagnostics.clone();
            device
                .build_input_stream(
                    &stream_config,
                    move |data: &[u16], _| {
                        let mut buf = accum_cb.lock();
                        for &s in data {
                            buf.push((s as f32 - 32768.0) / 32768.0);
                        }
                        diagnostics.record_counts(
                            data.len(),
                            data.iter().filter(|&&s| s != 32768).count(),
                        );
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
                channels: channels as u8,
            },
            diagnostics,
        },
        accum,
    ))
}

/// Select an actual rate/channel/format tuple, not a fabricated StreamConfig.
fn negotiate_config(
    device: &cpal::Device,
    channels_hint: u8,
) -> Result<cpal::SupportedStreamConfig> {
    select_config(
        device
            .supported_input_configs()
            .context("query supported input configs")?,
        channels_hint,
    )
}

fn select_config(
    supported: impl IntoIterator<Item = cpal::SupportedStreamConfigRange>,
    channels_hint: u8,
) -> Result<cpal::SupportedStreamConfig> {
    let selected = supported
        .into_iter()
        .filter_map(|range| {
            let channels = range.channels();
            if !(1..=2).contains(&channels)
                || (channels_hint != 0 && channels != u16::from(channels_hint))
                || range.min_sample_rate().0 == 0
                || range.min_sample_rate() > range.max_sample_rate()
            {
                return None;
            }
            let format_score = match range.sample_format() {
                SampleFormat::F32 => 0,
                SampleFormat::I16 => 1,
                SampleFormat::U16 => 2,
                _ => return None,
            };
            let rate = PREFERRED_RATE.clamp(range.min_sample_rate().0, range.max_sample_rate().0);
            let score = (rate.abs_diff(PREFERRED_RATE), channels, format_score);
            Some((score, range, rate))
        })
        .min_by_key(|(score, _, _)| *score);
    let (_, range, rate) = selected.ok_or_else(|| anyhow!(
        "no supported mono/stereo F32, I16 or U16 input configuration matching requested channels ({channels_hint}; 0 = automatic)"
    ))?;
    range
        .try_with_sample_rate(CpalSampleRate(rate))
        .ok_or_else(|| anyhow!("input sample rate {rate} is outside the advertised range"))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn range(
        channels: u16,
        min: u32,
        max: u32,
        format: SampleFormat,
    ) -> cpal::SupportedStreamConfigRange {
        cpal::SupportedStreamConfigRange::new(
            channels,
            CpalSampleRate(min),
            CpalSampleRate(max),
            cpal::SupportedBufferSize::Unknown,
            format,
        )
    }

    #[test]
    fn only_44100_stereo_is_selected_without_fabricating_mono_or_48000() {
        let config = select_config([range(2, 44_100, 44_100, SampleFormat::I16)], 0).unwrap();
        assert_eq!(config.channels(), 2);
        assert_eq!(config.sample_rate().0, 44_100);
        assert_eq!(config.sample_format(), SampleFormat::I16);
        assert!(select_config([range(2, 44_100, 44_100, SampleFormat::I16)], 1).is_err());
    }

    #[test]
    fn prefers_supported_48000_and_filters_unsupported_formats_and_channels() {
        let config = select_config(
            [
                range(1, 44_100, 44_100, SampleFormat::F32),
                range(2, 44_100, 96_000, SampleFormat::U16),
                range(1, 48_000, 48_000, SampleFormat::I32),
                range(256, 48_000, 48_000, SampleFormat::F32),
            ],
            0,
        )
        .unwrap();
        assert_eq!(config.sample_rate().0, 48_000);
        assert_eq!(config.channels(), 2);
        assert_eq!(config.sample_format(), SampleFormat::U16);
        assert!(select_config([], 0).is_err());
        assert!(select_config([range(1, 0, 0, SampleFormat::F32)], 0).is_err());
        assert!(select_config([range(1, 48_000, 48_000, SampleFormat::I32)], 0).is_err());
    }

    #[test]
    fn capture_diagnostics_distinguish_absent_zero_signal_and_retained_failure() {
        let diagnostics = CaptureDiagnostics::default();
        assert_eq!(diagnostics.snapshot().callbacks, 0);
        diagnostics.record_samples(&[0.0, 0.0]);
        assert_eq!(diagnostics.snapshot().callbacks, 1);
        assert_eq!(diagnostics.snapshot().samples, 2);
        assert_eq!(diagnostics.snapshot().nonzero_samples, 0);
        diagnostics.record_samples(&[0.25, -0.25, 0.0]);
        assert_eq!(diagnostics.snapshot().nonzero_samples, 2);
        diagnostics.report_error("device disconnected".into());
        diagnostics.report_error("secondary error".into());
        assert_eq!(
            diagnostics.take_error().as_deref(),
            Some("device disconnected")
        );
        assert_eq!(diagnostics.take_error(), None);
    }
}
