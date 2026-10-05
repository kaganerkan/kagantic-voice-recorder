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
use crate::writers::{FrameSink, OggOpusSink, RawF32Sink, WavSink};

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

/// Resolve the output path for a recording run: caller-supplied path wins,
/// otherwise a collision-free default inside `default_dir`.
pub fn resolve_output(cfg: &RunConfig, default_dir: &Path) -> PathBuf {
    match cfg.output.clone() {
        Some(p) => p,
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

    // Start the cpal capture stream first to discover the real format.
    let (capture, accum) = audio::start_capture(cfg.force_channels.unwrap_or(0))?;
    let _stream = capture.stream;

    // The encoder is bound to the *negotiated* sample rate. cpal gives us what
    // the device supports; if the user forced a different rate we'd need to
    // resample — for the supported command set we use the device's rate.
    let mut fmt = capture.format.clone();
    if let Some(r) = cfg.sample_rate {
        fmt.sample_rate = r;
    }

    let format_name = cfg.format.to_string();
    let mut progress_state = Progress::new(fmt.clone(), format_name);

    let mut frames_encoded: u64 = 0;

    match cfg.format {
        OutputFormat::Opus => {
            // Construct the Opus sink with the negotiated format. The sink
            // owns the encoder, but we need the encoder's frame size to drain
            // a complete frame from the accumulator before handing it back to
            // the sink for encoding + writing.
            let mut sink = OggOpusSink::new_file(&output, &fmt, cfg.bitrate_bps)
                .context("create Opus sink")?;
            let opus_frame_size = sink.encoder.frame_size() * fmt.channels as usize;
            sink.write_header(&fmt)?;

            loop {
                tokio::select! {
                    biased;

                    cmd = controls.recv() => {
                        match cmd {
                            Some(Control::Pause) => {
                                discard_partial_blocking(&accum);
                                progress_state.paused = true;
                                progress(&progress_state);
                            }
                            Some(Control::Resume) => {
                                progress_state.paused = false;
                                progress(&progress_state);
                            }
                            Some(Control::Stop) | None => break,
                        }
                    }

                    _ = stop_notify.notified() => break,

                    // Service the capture ring once per 5ms while recording.
                    _ = tokio::time::sleep(Duration::from_millis(5)), if !progress_state.paused => {
                        loop {
                            let frame = {
                                let mut buf = accum.lock();
                                if buf.len() < opus_frame_size { None } else {
                                    Some(buf.drain(..opus_frame_size).collect::<Vec<f32>>())
                                }
                            };
                            let Some(frame) = frame else { break };
                            sink.write_frames(&frame, &fmt)?;
                            frames_encoded += 1;
                            progress_state.frames_encoded = frames_encoded;
                            progress_state.granule = frames_encoded
                                * sink.encoder.frame_size() as u64;
                            progress(&progress_state);
                        }
                    }
                }
            }

            sink.finalize()?;
        }
        OutputFormat::WavPcm16le | OutputFormat::RawF32le => {
            let mut sink: Box<dyn FrameSink> = match cfg.format {
                OutputFormat::WavPcm16le => {
                    Box::new(WavSink::new_file(&output, &fmt).context("create WAV sink")?)
                }
                OutputFormat::RawF32le => {
                    Box::new(RawF32Sink::new_file(&output, &fmt).context("create raw sink")?)
                }
                _ => unreachable!(),
            };
            sink.write_header(&fmt)?;

            // PCM sinks consume frames in 480-sample (per-channel) blocks at
            // 48 kHz. The accumulator holds interleaved samples at the
            // negotiated capture rate; pull enough interleaved samples and
            // resample when the device rate differs from 48 kHz.
            let pcm_block = 480usize;
            let channel_count = fmt.channels as usize;

            loop {
                tokio::select! {
                    biased;

                    cmd = controls.recv() => {
                        match cmd {
                            Some(Control::Pause) => {
                                discard_partial_blocking(&accum);
                                progress_state.paused = true;
                                progress(&progress_state);
                            }
                            Some(Control::Resume) => {
                                progress_state.paused = false;
                                progress(&progress_state);
                            }
                            Some(Control::Stop) | None => break,
                        }
                    }

                    _ = stop_notify.notified() => break,

                    _ = tokio::time::sleep(Duration::from_millis(5)), if !progress_state.paused => {
                        let need = pcm_block * channel_count;
                        let frame = {
                            let mut buf = accum.lock();
                            if buf.len() < need { None }
                            else { Some(buf.drain(..need).collect::<Vec<f32>>()) }
                        };
                        let Some(frame) = frame else { continue };
                        let frame = if fmt.sample_rate == 48_000 {
                            frame
                        } else {
                            crate::output::resample_linear(&frame, fmt.sample_rate, 48_000)
                        };
                        sink.write_frames(&frame, &fmt)?;
                        frames_encoded += 1;
                        progress_state.frames_encoded = frames_encoded;
                        progress_state.granule =
                            (frames_encoded * pcm_block as u64) * fmt.sample_rate as u64 / 48_000;
                        progress(&progress_state);
                    }
                }
            }

            sink.finalize()?;
        }
    }

    // Persist a final progress callback so the formatter sees the closing
    // `frames_encoded` before the user-facing REPL moves on.
    progress(&progress_state);

    let duration_ms = if cfg.format == OutputFormat::Opus {
        // Opus: 960 samples per frame at 48 kHz = 20 ms.
        frames_encoded * 20
    } else {
        // PCM: 480 samples per block × 1000 / 48_000 = 10 ms per block.
        frames_encoded * 10
    };

    Ok(RecordedFile {
        path: output,
        frames_encoded,
        duration_ms,
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
