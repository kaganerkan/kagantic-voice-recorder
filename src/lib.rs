//! Crate root: the recording pipeline lives here, exposed to the binary.

pub mod audio;
pub mod devices;
pub mod encoder;
pub mod ogg;
pub mod output;
pub mod session;
pub mod writers;

pub use output::OutputFormat;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::sync::{mpsc, Notify};

use crate::audio::{discard_partial_blocking, CaptureFormat};
use crate::session::default_path_for;
use crate::writers::{drain_capture, open_sink};

/// User-facing control commands sent into the recording loop.
#[derive(Debug, Clone, Copy)]
pub enum Control {
    Pause,
    Resume,
    Stop,
}

/// Outcome of a recording run.
pub struct RecordedFile {
    pub path: PathBuf,
    pub frames_encoded: u64,
    pub duration_ms: u64,
}

/// Caller-supplied configuration for a recording run.
pub struct RunConfig {
    /// Explicit output path. When `None`, `resolve_output` allocates a
    /// collision-free default.
    pub output: Option<PathBuf>,
    pub bitrate_bps: i32,
    pub force_channels: Option<u8>,
    pub sample_rate: Option<u32>,
    pub format: OutputFormat,
    /// Optional explicit file extension. When `None`, the format's default
    /// extension is used.
    pub extension: Option<String>,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            output: None,
            bitrate_bps: 96_000,
            force_channels: None,
            sample_rate: None,
            format: OutputFormat::Opus,
            extension: None,
        }
    }
}

impl RunConfig {
    /// Extension the recorder should actually use, honouring an explicit
    /// override first and falling back to the format's default.
    pub fn effective_ext(&self) -> &str {
        self.extension
            .as_deref()
            .unwrap_or_else(|| self.format.default_ext())
    }
}

/// Resolve one collision-safe destination; explicit filenames keep their
/// extension unless overridden. Pass this path back in RunConfig to the worker.
pub fn resolve_output(cfg: &RunConfig, default_dir: &Path) -> PathBuf {
    match cfg.output.clone() {
        Some(mut path) => {
            if let Some(extension) = &cfg.extension {
                path.set_extension(extension);
            }
            if path.exists() {
                if let Some(stem) = path.file_stem() {
                    return crate::session::next_available_path(
                        path.parent().unwrap_or_else(|| Path::new(".")),
                        &stem.to_string_lossy(),
                        path.extension().and_then(|ext| ext.to_str()).unwrap_or(""),
                    );
                }
            }
            path
        }
        None => default_path_for(default_dir, cfg.effective_ext()),
    }
}

/// Spawn the capture stream, write frames to a container sink, and respond
/// to `Control` messages until `Stop` is received.
pub async fn run_recorder(
    cfg: RunConfig,
    mut controls: mpsc::Receiver<Control>,
    stop_notify: Arc<Notify>,
    mut progress: impl FnMut(&Progress) + Send + 'static,
) -> Result<RecordedFile> {
    // Resolve the final output path now so create_dir_all / File::create
    // operate on a single canonical location, regardless of the caller's
    // `RunConfig.output` field.
    let output = cfg
        .output
        .clone()
        .unwrap_or_else(|| default_path_for(Path::new("."), cfg.effective_ext()));
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).context("create output dir")?;

    let (capture, accum) = audio::start_capture(cfg.force_channels.unwrap_or(0))?;
    let fmt = capture.format.clone();
    if cfg.sample_rate.is_some_and(|rate| rate != fmt.sample_rate) {
        anyhow::bail!("requested sample rate {:?} differs from supported capture rate {} Hz; omit --sample-rate to use the device rate", cfg.sample_rate, fmt.sample_rate);
    }
    let diagnostics = capture.diagnostics.clone();
    let mut capture = Some(capture);
    let mut sink = open_sink(&output, &fmt, cfg.format, cfg.bitrate_bps)?;
    let mut state = Progress::new(fmt.clone(), cfg.format.to_string());
    let mut buffer = Vec::with_capacity(fmt.sample_rate as usize / 100 * usize::from(fmt.channels));
    let mut samples = 0u64;
    let mut backend_error = None;
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    loop {
        let mut stopping = false;
        tokio::select! {
            biased;
            cmd = controls.recv() => match cmd {
                Some(Control::Pause) => {
                    discard_partial_blocking(&accum);
                    state.paused = true;
                }
                Some(Control::Resume) => {
                    discard_partial_blocking(&accum);
                    state.paused = false;
                }
                Some(Control::Stop) | None => stopping = true,
            },
            _ = stop_notify.notified() => stopping = true,
            _ = tick.tick() => {},
        }
        if let Some(error) = diagnostics.take_error() {
            backend_error = Some(error);
            stopping = true;
        }
        if stopping {
            drop(capture.take());
        }
        if state.paused {
            discard_partial_blocking(&accum);
        } else {
            let (count, _) = drain_capture(&accum, sink.as_mut(), &fmt, stopping, &mut buffer)?;
            samples += count;
        }
        state.frames_encoded = samples / u64::from(fmt.channels) * 50 / u64::from(fmt.sample_rate);
        state.granule = samples / u64::from(fmt.channels);
        progress(&state);
        if stopping {
            sink.finalize().context("finalize recording")?;
            if let Some(error) = backend_error.or_else(|| diagnostics.take_error()) {
                anyhow::bail!(
                    "capture backend failed: {error}; partial recording finalized at {}",
                    output.display()
                );
            }
            break;
        }
    }
    progress(&state);
    Ok(RecordedFile {
        path: output,
        frames_encoded: state.frames_encoded,
        duration_ms: samples * 1000 / u64::from(fmt.channels) / u64::from(fmt.sample_rate),
    })
}

pub struct Progress {
    pub format: CaptureFormat,
    pub frames_encoded: u64,
    pub granule: u64,
    pub paused: bool,
    pub started_unix_ms: i64,
    /// Lowercase display name of the negotiated `OutputFormat`.
    pub format_name: String,
}

impl Progress {
    fn new(format: CaptureFormat, format_name: String) -> Self {
        Self {
            format,
            frames_encoded: 0,
            granule: 0,
            paused: false,
            started_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            format_name,
        }
    }
}

#[cfg(test)]
mod output_path_tests {
    use super::*;

    #[test]
    fn resolved_cli_destination_honors_directory_and_preserves_existing_explicit_takes() {
        let directory = tempfile::tempdir().unwrap();
        let cfg = RunConfig {
            format: OutputFormat::WavPcm16le,
            ..Default::default()
        };
        let default = resolve_output(&cfg, directory.path());
        assert_eq!(default.parent(), Some(directory.path()));
        assert_eq!(default.extension().unwrap(), "wav");
        for name in ["take.wav", "take"] {
            let existing = directory.path().join(name);
            std::fs::write(&existing, b"original").unwrap();
            let cfg = RunConfig {
                output: Some(existing.clone()),
                ..Default::default()
            };
            let resolved = resolve_output(&cfg, directory.path());
            assert_ne!(resolved, existing);
            let expected = if name == "take.wav" {
                "take-1.wav"
            } else {
                "take-1"
            };
            assert_eq!(resolved.file_name().unwrap(), expected);
            let mut sink = open_sink(
                &resolved,
                &CaptureFormat {
                    sample_rate: 44_100,
                    channels: 1,
                },
                OutputFormat::WavPcm16le,
                96_000,
            )
            .unwrap();
            sink.finalize().unwrap();
            assert_eq!(std::fs::read(existing).unwrap(), b"original");
            assert_eq!(std::fs::metadata(resolved).unwrap().len(), 44);
        }
    }
}
