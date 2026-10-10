#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! Native audio recorder built with eframe/egui and the glow renderer.
//! Audio capture and encoding run on a worker thread; closing the window
//! stops and joins that worker after the current take has been finalized.
//! Hardware-free worker regression tests decode synthetic output with ffmpeg.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui::{self, Color32, FontId, RichText, Stroke};
use kvr_recorder::audio::{
    discard_partial_blocking, start_capture_named, Accum, CaptureDiagnostics, CaptureFormat,
    CaptureSnapshot,
};
use kvr_recorder::devices::input_devices;
use kvr_recorder::output::OutputFormat;
use kvr_recorder::session::resolve_recording_path;
use kvr_recorder::writers::{drain_capture, open_sink};

const NAVY: Color32 = Color32::from_rgb(26, 39, 68);
const BLUE: Color32 = Color32::from_rgb(74, 111, 165);
const CREAM: Color32 = Color32::from_rgb(214, 208, 202);
const SAND: Color32 = Color32::from_rgb(245, 243, 240);
const WHITE: Color32 = Color32::WHITE;
const HISTORY_SAMPLES: usize = 96;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Starting,
    Recording,
    Paused,
    Stopping,
    Stopped,
}

impl State {
    fn is_busy(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Recording | Self::Paused | Self::Stopping
        )
    }

    fn badge(self) -> (&'static str, Color32) {
        match self {
            Self::Idle => ("READY", NAVY),
            Self::Starting => ("CONNECTING", BLUE),
            Self::Recording => ("RECORDING", NAVY),
            Self::Paused => ("PAUSED", BLUE),
            Self::Stopping => ("SAVING", BLUE),
            Self::Stopped => ("SAVED", NAVY),
        }
    }
}

#[derive(Debug)]
enum RecorderMsg {
    Ready,
    Progress(ProgressUpdate),
    /// Emitted by the worker thread as soon as the output file has been
    /// resolved and the file handle opened. The GUI updates the visible path
    /// field so the user sees the actual filename the worker is writing to
    /// (which may differ from the placeholder stem the GUI pre-filled).
    Started {
        path: PathBuf,
    },
    Finished {
        path: PathBuf,
        bytes: u64,
    },
    Error(String),
    Diagnostic(String),
}

#[derive(Clone, Copy, Debug, Default)]
struct ProgressUpdate {
    elapsed_ms: u64,
    bytes: u64,
    level: f32,
    paused: bool,
}

#[derive(Debug)]
enum WorkerCmd {
    Pause,
    Resume,
    Stop,
}

struct AppConfig {
    selected_device: Option<String>,
    output_path: String,
    bitrate_kbps: i32,
    format: OutputFormat,
    extension: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        let placeholder = default_recording_directory()
            .map(|dir| dir.join("recording.opus").to_string_lossy().into_owned())
            .unwrap_or_default();
        Self {
            selected_device: None,
            output_path: placeholder,
            bitrate_kbps: 96,
            format: OutputFormat::Opus,
            extension: None,
        }
    }
}

impl AppConfig {
    /// Effective file extension, honouring an explicit override and falling
    /// back to the format's default.
    fn effective_ext(&self) -> &str {
        match &self.extension {
            Some(ext) if !ext.is_empty() => ext.as_str(),
            _ => self.output_format().default_ext(),
        }
    }

    /// Format implied by the current path or override. Used when the user
    /// types a filename with a known extension.
    fn output_format(&self) -> OutputFormat {
        if let Some(ext) = self.extension.as_deref().filter(|s| !s.is_empty()) {
            if let Ok(f) = OutputFormat::from_str(ext) {
                return f;
            }
        }
        if let Some(ext) = std::path::Path::new(&self.output_path)
            .extension()
            .and_then(|e| e.to_str())
        {
            if let Ok(f) = OutputFormat::from_str(ext) {
                return f;
            }
        }
        self.format
    }

    fn select_format(&mut self, format: OutputFormat) {
        self.format = format;
        if self
            .extension
            .as_deref()
            .is_some_and(|ext| ext.parse::<OutputFormat>().is_ok())
        {
            self.extension = None;
        }
        let mut path = PathBuf::from(&self.output_path);
        if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.parse::<OutputFormat>().is_ok())
        {
            path.set_extension(format.default_ext());
            self.output_path = path.to_string_lossy().into_owned();
        }
    }

    fn edit_extension(&mut self, text: String, commit: bool) {
        self.extension = if commit {
            let text = text.trim().to_owned();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        } else {
            // Keep even the empty in-progress buffer across repaint frames.
            Some(text)
        };
    }
}

fn choose_writable_directory(candidates: &[PathBuf]) -> Result<PathBuf> {
    let mut errors = Vec::new();
    for directory in candidates.iter().filter(|path| path.is_absolute()) {
        let attempt = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(directory)?;
            let probe = directory.join(format!(
                ".kvr-write-probe-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            let file = std::fs::File::options()
                .write(true)
                .create_new(true)
                .open(&probe)?;
            drop(file);
            std::fs::remove_file(probe)
        })();
        match attempt {
            Ok(()) => return Ok(directory.clone()),
            Err(error) => errors.push(format!("{}: {error}", directory.display())),
        }
    }
    anyhow::bail!(
        "no writable user recording directory; choose an output path. {}",
        errors.join("; ")
    )
}

fn default_recording_directory() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(user) = directories::UserDirs::new() {
        if let Some(audio) = user.audio_dir() {
            candidates.push(audio.join("Kagantic Voice Recorder"));
        }
    }
    if let Some(base) = directories::BaseDirs::new() {
        candidates.push(
            base.data_local_dir()
                .join("Kagantic Voice Recorder")
                .join("Recordings"),
        );
        candidates.push(
            base.home_dir()
                .join("Kagantic Voice Recorder")
                .join("Recordings"),
        );
    }
    // Last resort is explicit and independent of the installation/cwd.
    candidates.push(std::env::temp_dir().join(format!("kvr-recordings-{}", std::process::id())));
    choose_writable_directory(&candidates)
}

fn initialize_logging() -> Result<PathBuf> {
    let directory = directories::BaseDirs::new()
        .map(|base| {
            base.data_local_dir()
                .join("Kagantic Voice Recorder")
                .join("Logs")
        })
        .unwrap_or_else(|| std::env::temp_dir().join(format!("kvr-logs-{}", std::process::id())));
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("gui.log");
    let file = std::fs::File::options()
        .create(true)
        .append(true)
        .open(&path)?;
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::sync::Mutex::new(file))
        .try_init()
        .map_err(|error| anyhow::anyhow!("initialize GUI log: {error}"))?;
    Ok(path)
}

fn capture_diagnostic(current: CaptureSnapshot, previous: CaptureSnapshot, paused: bool) -> String {
    let input = if current.callbacks == previous.callbacks || current.samples == previous.samples {
        "Waiting for input callbacks / no new samples received."
    } else if current.nonzero_samples == previous.nonzero_samples {
        "Input callbacks active / all-zero samples (silence is valid)."
    } else {
        "Input callbacks active / nonzero samples received."
    };
    if paused {
        format!("{input} Paused / captured input is discarded.")
    } else {
        input.into()
    }
}

struct App {
    state: State,
    cfg: AppConfig,
    last_progress: ProgressUpdate,
    level_history: VecDeque<f32>,
    status: String,
    error: Option<String>,
    device_error: Option<String>,
    capture_note: String,
    log_note: String,
    saved_path: Option<PathBuf>,
    /// Filename the worker actually opened for the current take. Populated
    /// from the `Started { path }` message and shown in the status panel.
    /// Kept separate from `cfg.output_path` so the field remains the
    /// placeholder (`recording.<ext>`) for the next RECORD click.
    last_resolved: Option<PathBuf>,
    input_devices: Vec<(String, String)>,
    cmd_tx: Option<Sender<WorkerCmd>>,
    msg_rx: Option<Receiver<RecorderMsg>>,
    worker: Option<JoinHandle<()>>,
    closing: bool,
    startup_repaints: u8,
    logo: egui::TextureHandle,
    microphone_icon: egui::TextureHandle,
    folder_icon: egui::TextureHandle,
}

fn display_font(size: f32) -> FontId {
    FontId::new(size, egui::FontFamily::Name("Silkscreen".into()))
}

fn configure_style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, bytes) in [
        (
            "Silkscreen",
            include_bytes!("../../assets/fonts/Silkscreen-Regular.ttf").as_slice(),
        ),
        (
            "VT323",
            include_bytes!("../../assets/fonts/VT323-Regular.ttf").as_slice(),
        ),
        (
            "DotGothic16",
            include_bytes!("../../assets/fonts/DotGothic16-Regular.ttf").as_slice(),
        ),
    ] {
        fonts
            .font_data
            .insert(name.into(), egui::FontData::from_static(bytes).into());
        fonts
            .families
            .insert(egui::FontFamily::Name(name.into()), vec![name.into()]);
    }
    fonts
        .families
        .get_mut(&egui::FontFamily::Proportional)
        .unwrap()
        .insert(0, "VT323".into());
    fonts
        .families
        .get_mut(&egui::FontFamily::Monospace)
        .unwrap()
        .insert(0, "DotGothic16".into());
    ctx.set_fonts(fonts);
    ctx.set_visuals(egui::Visuals::light());
    let mut style = (*ctx.style()).clone();
    style.animation_time = 0.0;
    style.spacing.item_spacing = egui::vec2(12.0, 12.0);
    style.spacing.button_padding = egui::vec2(12.0, 12.0);
    style.spacing.interact_size.y = 40.0;
    style
        .text_styles
        .insert(egui::TextStyle::Heading, display_font(24.0));
    style
        .text_styles
        .insert(egui::TextStyle::Body, FontId::proportional(22.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, display_font(11.0));
    style
        .text_styles
        .insert(egui::TextStyle::Small, FontId::proportional(20.0));
    style
        .text_styles
        .insert(egui::TextStyle::Monospace, FontId::monospace(16.0));
    style.visuals.override_text_color = Some(NAVY);
    style.visuals.weak_text_color = Some(NAVY);
    style.visuals.panel_fill = SAND;
    style.visuals.window_fill = WHITE;
    style.visuals.extreme_bg_color = SAND;
    style.visuals.faint_bg_color = SAND;
    style.visuals.window_stroke = Stroke::new(3.0_f32, NAVY);
    style.visuals.window_corner_radius = egui::CornerRadius::ZERO;
    style.visuals.window_shadow = egui::epaint::Shadow {
        offset: [4, 4],
        blur: 0,
        spread: 0,
        color: NAVY,
    };
    style.visuals.popup_shadow = style.visuals.window_shadow;
    style.visuals.selection.bg_fill = BLUE;
    style.visuals.selection.stroke = Stroke::new(3.0_f32, WHITE);
    style.visuals.text_cursor.blink = false;
    style.visuals.handle_shape = egui::style::HandleShape::Rect { aspect_ratio: 0.6 };
    for widget in [
        &mut style.visuals.widgets.noninteractive,
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::ZERO;
        widget.fg_stroke = Stroke::new(2.0_f32, NAVY);
        widget.bg_stroke = Stroke::new(2.0_f32, NAVY);
        widget.bg_fill = SAND;
        widget.weak_bg_fill = SAND;
    }
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(2.0_f32, CREAM);
    for widget in [
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        widget.bg_fill = BLUE;
        widget.weak_bg_fill = BLUE;
        widget.fg_stroke = Stroke::new(2.0_f32, WHITE);
        widget.bg_stroke = Stroke::new(3.0_f32, NAVY);
    }
    ctx.set_style(style);
}

fn load_logo(ctx: &egui::Context) -> egui::TextureHandle {
    load_asset(
        ctx,
        "kaganerkan-logo",
        include_bytes!("../../assets/pixel-art-logo.png"),
        egui::TextureOptions::NEAREST,
    )
}

fn load_asset(
    ctx: &egui::Context,
    name: &str,
    bytes: &[u8],
    options: egui::TextureOptions,
) -> egui::TextureHandle {
    let image = image::load_from_memory(bytes)
        .expect("bundled artwork must be a valid PNG")
        .to_rgba8();
    ctx.load_texture(
        name,
        egui::ColorImage::from_rgba_unmultiplied(
            [image.width() as usize, image.height() as usize],
            image.as_raw(),
        ),
        options,
    )
}

fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(WHITE)
        .stroke(Stroke::new(3.0_f32, NAVY))
        .corner_radius(0)
        .shadow(egui::epaint::Shadow {
            offset: [4, 4],
            blur: 0,
            spread: 0,
            color: CREAM,
        })
        .inner_margin(16)
}

fn section_title(ui: &mut egui::Ui, title: &str, description: &str) {
    ui.label(RichText::new(title).font(display_font(13.0)));
    ui.label(description);
    ui.add_space(4.0);
}

fn icon_section_title(
    ui: &mut egui::Ui,
    title: &str,
    description: &str,
    icon: &egui::TextureHandle,
) {
    ui.horizontal(|ui| {
        let (tile, _) = ui.allocate_exact_size(egui::vec2(48.0, 48.0), egui::Sense::hover());
        ui.painter().rect_filled(tile, 0, NAVY);
        ui.painter().image(
            icon.id(),
            tile.shrink(8.0),
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            WHITE,
        );
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.label(RichText::new(title).font(display_font(13.0)));
            ui.label(description);
        });
    });
    ui.add_space(4.0);
}

fn action_button(
    ui: &mut egui::Ui,
    enabled: bool,
    label: &str,
    width: f32,
    primary: bool,
) -> egui::Response {
    let shadow = ui.painter().add(egui::Shape::Noop);
    let response = ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(label).color(if primary && enabled {
            WHITE
        } else {
            NAVY
        }))
        .fill(if !enabled {
            CREAM
        } else if primary {
            NAVY
        } else {
            SAND
        })
        .min_size(egui::vec2(width, 48.0)),
    );
    let pressed = enabled && response.is_pointer_button_down_on();
    if !pressed {
        ui.painter().set(
            shadow,
            egui::Shape::rect_filled(
                response.rect.translate(egui::vec2(3.0, 3.0)),
                0,
                if enabled { NAVY } else { CREAM },
            ),
        );
    }
    if enabled && (response.hovered() || response.has_focus()) {
        ui.painter().rect_stroke(
            response.rect.expand(3.0),
            0,
            Stroke::new(3.0_f32, BLUE),
            egui::StrokeKind::Outside,
        );
    }
    response
}

fn display_level(level: f32) -> f32 {
    if level > 0.0 {
        ((20.0 * level.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn format_label(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Opus => "Opus (.opus)",
        OutputFormat::WavPcm16le => "WAV (PCM s16le)",
        OutputFormat::RawF32le => "Raw f32le",
    }
}

fn format_name(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Opus => "OPUS",
        OutputFormat::WavPcm16le => "WAV",
        OutputFormat::RawF32le => "RAW",
    }
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_style(&cc.egui_ctx);
        let logo = load_logo(&cc.egui_ctx);
        let mut app = Self {
            state: State::Idle,
            cfg: AppConfig::default(),
            last_progress: ProgressUpdate::default(),
            level_history: VecDeque::with_capacity(HISTORY_SAMPLES),
            status: "Choose an input and press Record to begin.".into(),
            error: None,
            device_error: None,
            capture_note: String::new(),
            log_note: String::new(),
            saved_path: None,
            last_resolved: None,
            input_devices: Vec::new(),
            cmd_tx: None,
            msg_rx: None,
            worker: None,
            closing: false,
            startup_repaints: 3,
            logo,
            microphone_icon: load_asset(
                &cc.egui_ctx,
                "microphone",
                include_bytes!("../../assets/icons/microphone.png"),
                egui::TextureOptions::LINEAR,
            ),
            folder_icon: load_asset(
                &cc.egui_ctx,
                "folder",
                include_bytes!("../../assets/icons/folder.png"),
                egui::TextureOptions::LINEAR,
            ),
        };
        app.refresh_devices();
        app
    }

    fn refresh_devices(&mut self) {
        match input_devices() {
            Ok(devices) => {
                if let Some(selected) = &self.cfg.selected_device {
                    if !devices.iter().any(|(id, _)| id == selected) {
                        self.cfg.selected_device = None;
                        self.status = "The selected microphone is no longer available. Using the system default.".into();
                    }
                }
                self.input_devices = devices;
                self.device_error = None;
            }
            Err(error) => {
                self.device_error = Some(format!("Could not list microphones: {error:#}"));
            }
        }
    }

    fn selected_device_label(&self) -> &str {
        match self.cfg.selected_device.as_ref() {
            None => "System default microphone",
            Some(selected) => self
                .input_devices
                .iter()
                .find(|(id, _)| id == selected)
                .map(|(_, label)| label.as_str())
                .unwrap_or("Selected microphone unavailable"),
        }
    }

    fn start_recording(&mut self, ctx: &egui::Context) {
        if self.state.is_busy() || self.worker.is_some() || self.cfg.output_path.trim().is_empty() {
            return;
        }
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (msg_tx, msg_rx) = mpsc::channel();
        let device_name = self.cfg.selected_device.clone();
        // Resolve the actual filename at the moment the user presses RECORD.
        // The field shown in the UI is a *placeholder stem* (default
        // `recording.<ext>`); we substitute a fresh `YYYYMMDD-HHMMSS` stamp
        // every time the user keeps the placeholder, so two presses inside
        // the same second still produce two unique takes. User-typed stems
        // are preserved verbatim (with `-2`, `-3`, … appended on collision).
        let typed = PathBuf::from(&self.cfg.output_path);
        let typed = if typed.is_absolute() {
            typed
        } else {
            match std::env::current_dir() {
                Ok(cwd) => cwd.join(typed),
                Err(error) => {
                    self.error = Some(format!("Resolve output directory: {error}"));
                    return;
                }
            }
        };
        let ext = self.cfg.effective_ext().to_string();
        // Resolve the actual filename at the moment the user presses RECORD.
        // The field shown in the UI is a *placeholder stem* (default
        // `recording.<ext>`); the resolver substitutes a fresh
        // `YYYYMMDD-HHMMSS` stamp every time the user keeps the placeholder,
        // so two presses inside the same second still produce two unique
        // takes. User-typed stems (`cat`, `interview`, …) are preserved
        // verbatim, with `-1`, `-2`, … appended on collision.
        //
        // IMPORTANT: we never mutate `self.cfg.output_path` here — that
        // would cause the next click to see the previous resolved timestamp
        // as user input and start suffixing instead of producing a fresh
        // timestamp.
        let output = resolve_recording_path(&typed, &ext);
        if let Some(parent) = output.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                self.error = Some(format!(
                    "Create output directory {}: {error}",
                    parent.display()
                ));
                return;
            }
        }
        let bitrate = self.cfg.bitrate_kbps * 1000;
        let format = self.cfg.output_format();
        let extension = self.cfg.extension.clone();
        let repaint = ctx.clone();
        match std::thread::Builder::new()
            .name("kvr-gui".into())
            .spawn(move || {
                if let Err(error) = run_worker(
                    device_name,
                    output,
                    bitrate,
                    format,
                    extension,
                    cmd_rx,
                    &msg_tx,
                ) {
                    tracing::error!("recording failed: {error:#}");
                    let _ = msg_tx.send(RecorderMsg::Error(format!("{error:#}")));
                }
                repaint.request_repaint();
            }) {
            Ok(worker) => {
                self.worker = Some(worker);
                self.cmd_tx = Some(cmd_tx);
                self.msg_rx = Some(msg_rx);
                self.state = State::Starting;
                self.last_progress = ProgressUpdate::default();
                self.level_history.clear();
                self.saved_path = None;
                self.last_resolved = None;
                self.error = None;
                self.capture_note.clear();
                self.status = "Connecting to the microphone...".into();
            }
            Err(error) => {
                self.error = Some(format!("Could not start the recording worker: {error}"));
            }
        }
    }

    fn send_command(&mut self, command: WorkerCmd, next_state: State, status: &str) {
        if self
            .cmd_tx
            .as_ref()
            .is_some_and(|tx| tx.send(command).is_ok())
        {
            self.state = next_state;
            self.status = status.into();
        } else {
            // The worker's terminal message (or channel disconnection) is
            // handled on the next update. Keep settings locked until it joins.
            self.state = State::Stopping;
            self.status = "Waiting for the recording worker...".into();
        }
    }

    fn stop_recording(&mut self) {
        if matches!(
            self.state,
            State::Starting | State::Recording | State::Paused
        ) {
            self.send_command(
                WorkerCmd::Stop,
                State::Stopping,
                "Finalizing and saving your recording...",
            );
            self.last_progress.level = 0.0;
        }
    }

    fn join_worker(&mut self) -> bool {
        self.cmd_tx = None;
        self.msg_rx = None;
        self.worker
            .take()
            .is_none_or(|worker| worker.join().is_ok())
    }

    fn receive_messages(&mut self) {
        while let Some(rx) = &self.msg_rx {
            match rx.try_recv() {
                Ok(RecorderMsg::Ready) => {
                    if self.state == State::Starting {
                        self.state = State::Recording;
                        self.status =
                            "Recording. Pause leaves silence out of the saved take.".into();
                    }
                }
                Ok(RecorderMsg::Started { path }) => {
                    // Show the resolved timestamped filename in the status
                    // line so the user can see what file is actually being
                    // written. We deliberately do NOT mutate
                    // `self.cfg.output_path` here — if we did, the next
                    // RECORD click would see the previous timestamp as the
                    // user's input and start suffixing (`-2`, `-3`, …)
                    // instead of generating a fresh `YYYYMMDD-HHMMSS`.
                    self.last_resolved = Some(path);
                    self.status = match &self.last_resolved {
                        Some(p) => format!(
                            "Recording to {}",
                            p.file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        ),
                        None => "Recording started.".into(),
                    };
                }
                Ok(RecorderMsg::Progress(progress)) => {
                    self.last_progress = progress;
                    if self.state == State::Recording && !progress.paused {
                        if self.level_history.len() == HISTORY_SAMPLES {
                            self.level_history.pop_front();
                        }
                        self.level_history.push_back(progress.level);
                    }
                }
                Ok(RecorderMsg::Diagnostic(note)) => self.capture_note = note,
                Ok(RecorderMsg::Finished { path, bytes }) => {
                    if self.join_worker() {
                        self.state = State::Stopped;
                        self.last_progress.level = 0.0;
                        self.last_progress.bytes = bytes;
                        self.saved_path = Some(path);
                        self.status = "Recording saved. Your file is ready.".into();
                    } else {
                        self.state = State::Idle;
                        self.error =
                            Some("The recording worker exited unexpectedly while saving.".into());
                    }
                    break;
                }
                Ok(RecorderMsg::Error(error)) => {
                    self.join_worker();
                    self.state = State::Idle;
                    self.last_progress.level = 0.0;
                    self.error = Some(error);
                    self.status =
                        "Recording could not be completed. Check the input and output settings."
                            .into();
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.join_worker();
                    self.state = State::Idle;
                    self.last_progress.level = 0.0;
                    self.error =
                        Some("The recording worker exited without completing the take.".into());
                    break;
                }
            }
        }
    }

    fn header(&self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 112.0),
            egui::Sense::hover(),
        );
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0, NAVY);
        let grid = Stroke::new(1.0_f32, BLUE.gamma_multiply(0.25));
        let mut x = rect.left();
        while x < rect.right() {
            painter.line_segment(
                [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                grid,
            );
            x += 32.0;
        }
        let mut y = rect.top();
        while y < rect.bottom() {
            painter.line_segment(
                [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                grid,
            );
            y += 32.0;
        }
        let tile =
            egui::Rect::from_min_size(rect.min + egui::vec2(16.0, 16.0), egui::vec2(80.0, 80.0));
        painter.rect_filled(tile.translate(egui::vec2(4.0, 4.0)), 0, BLUE);
        painter.rect_filled(tile, 0, SAND);
        painter.rect_stroke(
            tile,
            0,
            Stroke::new(4.0_f32, CREAM),
            egui::StrokeKind::Inside,
        );
        painter.image(
            self.logo.id(),
            tile.shrink(8.0),
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            WHITE,
        );
        let text_rect = egui::Rect::from_min_max(
            egui::pos2(tile.right() + 16.0, rect.top() + 16.0),
            rect.max - egui::vec2(16.0, 16.0),
        );
        painter.rect_filled(text_rect, 0, NAVY);
        ui.scope_builder(egui::UiBuilder::new().max_rect(text_rect), |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.label(
                RichText::new("KAGANTIC VOICE RECORDER")
                    .font(display_font(if rect.width() < 480.0 { 17.0 } else { 24.0 }))
                    .color(WHITE),
            );
            ui.label(
                RichText::new("kaganerkan / audio tools")
                    .font(display_font(10.0))
                    .color(CREAM),
            );
            if rect.width() >= 480.0 {
                ui.label(
                    RichText::new("Capture clearly. Keep every take lightweight.").color(WHITE),
                );
            }
        });
    }

    fn recorder_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let (badge, color) = self.state.badge();
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("01 / CAPTURE").font(display_font(12.0)));
                egui::Frame::new()
                    .fill(color)
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .show(ui, |ui| {
                        ui.label(RichText::new(badge).font(display_font(11.0)).color(WHITE));
                    });
            });
            ui.add_space(8.0);
            let elapsed = self.last_progress.elapsed_ms;
            let timer = format!(
                "{:02}:{:02}:{:02}.{}",
                elapsed / 3_600_000,
                (elapsed / 60_000) % 60,
                (elapsed / 1000) % 60,
                (elapsed / 100) % 10
            );
            let timer_size = (ui.available_width() / (timer.len() as f32 * 0.85)).clamp(16.0, 40.0);
            ui.label(
                RichText::new(timer)
                    .font(display_font(timer_size))
                    .color(NAVY),
            );
            ui.label("Recorded duration / pauses excluded");
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(format!(
                    "{:.1} KiB file size / includes container headers",
                    self.last_progress.bytes as f64 / 1024.0
                ));
                ui.label(
                    RichText::new(format!(
                        "{}/{} KBPS",
                        format_name(self.cfg.output_format()),
                        self.cfg.bitrate_kbps
                    ))
                    .font(display_font(10.0))
                    .color(BLUE),
                );
            });
            ui.add_space(12.0);
            self.controls(ui, ctx);
            ui.add_space(14.0);
            ui.separator();
            ui.add_space(4.0);
            self.input_meter(ui);
        });
    }

    fn controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let narrow = ui.available_width() < 420.0;
        let gap = ui.spacing().item_spacing.x;
        let width = ui.available_width();
        let record_width = if narrow {
            width
        } else {
            (width - gap * 2.0) * 0.42
        };
        let secondary_width = if narrow {
            (width - gap) / 2.0
        } else {
            (width - record_width - gap * 2.0) / 2.0
        };
        let can_record =
            !self.state.is_busy() && !self.closing && !self.cfg.output_path.trim().is_empty();
        if narrow {
            if action_button(ui, can_record, "RECORD", record_width, true).clicked() {
                self.start_recording(ctx);
            }
            ui.horizontal(|ui| self.secondary_controls(ui, secondary_width));
        } else {
            ui.horizontal(|ui| {
                if action_button(ui, can_record, "RECORD", record_width, true).clicked() {
                    self.start_recording(ctx);
                }
                self.secondary_controls(ui, secondary_width);
            });
        }
    }

    fn secondary_controls(&mut self, ui: &mut egui::Ui, width: f32) {
        let paused = self.state == State::Paused;
        let label = if paused { "RESUME" } else { "PAUSE" };
        if action_button(
            ui,
            matches!(self.state, State::Recording | State::Paused),
            label,
            width,
            false,
        )
        .clicked()
        {
            if paused {
                self.send_command(WorkerCmd::Resume, State::Recording, "Recording resumed.");
            } else {
                self.send_command(
                    WorkerCmd::Pause,
                    State::Paused,
                    "Paused. Press Resume to continue the same take.",
                );
                self.last_progress.level = 0.0;
            }
        }
        if action_button(
            ui,
            matches!(
                self.state,
                State::Starting | State::Recording | State::Paused
            ),
            "STOP",
            width,
            false,
        )
        .clicked()
        {
            self.stop_recording();
        }
    }

    fn input_meter(&self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("INPUT LEVEL").font(display_font(16.0)));
            let caption = match self.state {
                State::Recording => "Live microphone / RMS",
                State::Paused => "Paused / input excluded",
                State::Stopping => "Finalizing take",
                _ => "Monitoring starts when recording",
            };
            ui.label(RichText::new(caption).font(FontId::proportional(18.0)));
        });
        let level = if self.state == State::Recording {
            self.last_progress.level
        } else {
            0.0
        };
        let db = if level > 0.0 {
            format!("{:.0} dBFS", 20.0 * level.log10())
        } else {
            "No signal".into()
        };
        ui.label(RichText::new(db).monospace());
        let (meter, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 20.0), egui::Sense::hover());
        ui.painter().rect_stroke(
            meter,
            0,
            Stroke::new(2.0_f32, NAVY),
            egui::StrokeKind::Inside,
        );
        let inner = meter.shrink(4.0);
        let step = inner.width() / 32.0;
        let lit = (display_level(level) * 32.0).ceil() as usize;
        for index in 0..32 {
            let segment = egui::Rect::from_min_size(
                inner.min + egui::vec2(index as f32 * step, 0.0),
                egui::vec2((step - 2.0).max(1.0), inner.height()),
            );
            ui.painter()
                .rect_filled(segment, 0, if index < lit { BLUE } else { CREAM });
        }
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 86.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0, NAVY);
        let plot = rect.shrink2(egui::vec2(12.0, 12.0));
        for fraction in [0.25, 0.5, 0.75] {
            let y = plot.bottom() - plot.height() * fraction;
            painter.line_segment(
                [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
                Stroke::new(1.0_f32, BLUE),
            );
        }
        let step = plot.width() / HISTORY_SAMPLES as f32;
        let offset = HISTORY_SAMPLES - self.level_history.len();
        for (index, &sample) in self.level_history.iter().enumerate() {
            let height = display_level(sample) * plot.height();
            if height > 0.0 {
                let x = plot.left() + (offset + index) as f32 * step;
                let bar = egui::Rect::from_min_max(
                    egui::pos2(x, plot.bottom() - height),
                    egui::pos2(x + step * 0.7, plot.bottom()),
                );
                painter.rect_filled(bar, 0, WHITE);
            }
        }
        if self.level_history.is_empty() {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Waiting for input",
                FontId::proportional(20.0),
                CREAM,
            );
        }
        ui.label(RichText::new("Recent input / last 8 seconds / -60 to 0 dBFS").small());
    }

    fn input_settings(&mut self, ui: &mut egui::Ui) {
        card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            icon_section_title(
                ui,
                "02 / MICROPHONE",
                "Select the input for your next take.",
                &self.microphone_icon,
            );
            ui.add_enabled_ui(!self.state.is_busy(), |ui| {
                egui::ComboBox::from_id_salt("input_device")
                    .width(ui.available_width())
                    .wrap()
                    .selected_text(self.selected_device_label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.cfg.selected_device,
                            None,
                            "System default microphone",
                        );
                        for (id, label) in &self.input_devices {
                            ui.selectable_value(
                                &mut self.cfg.selected_device,
                                Some(id.clone()),
                                label,
                            );
                        }
                    });
                if ui.button("Refresh inputs").clicked() {
                    self.refresh_devices();
                }
            });
            if let Some(error) = &self.device_error {
                ui.add(egui::Label::new(RichText::new(format!("INPUT ERROR / {error}"))).wrap());
            } else if self.input_devices.is_empty() {
                ui.add(
                    egui::Label::new(
                        "No named inputs found. The system default will be used if available.",
                    )
                    .wrap(),
                );
            }
        });
    }

    fn quality_settings(&mut self, ui: &mut egui::Ui) {
        card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            section_title(ui, "03 / QUALITY", "Efficient, high-quality audio.");
            ui.add_enabled_ui(!self.state.is_busy(), |ui| {
                ui.label(
                    RichText::new(format!("{} KBPS", self.cfg.bitrate_kbps))
                        .font(display_font(18.0))
                        .color(BLUE),
                );
                ui.spacing_mut().slider_width = (ui.available_width() - 8.0).max(80.0);
                ui.add(
                    egui::Slider::new(&mut self.cfg.bitrate_kbps, 16..=256)
                        .step_by(8.0)
                        .show_value(false),
                );
                ui.label(RichText::new("16 kbps compact / 256 kbps detailed").small());
            });
        });
    }

    fn output_settings(&mut self, ui: &mut egui::Ui) {
        card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            icon_section_title(
                ui,
                "04 / OUTPUT",
                "Stop finalizes and saves this take.",
                &self.folder_icon,
            );
            ui.add_enabled_ui(!self.state.is_busy(), |ui| {
                let mut selected_format = self.cfg.output_format();
                ui.horizontal(|ui| {
                    ui.label("FORMAT");
                    egui::ComboBox::from_id_salt("output_format")
                        .selected_text(format_label(selected_format))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut selected_format,
                                OutputFormat::Opus,
                                format_label(OutputFormat::Opus),
                            );
                            ui.selectable_value(
                                &mut selected_format,
                                OutputFormat::WavPcm16le,
                                format_label(OutputFormat::WavPcm16le),
                            );
                            ui.selectable_value(
                                &mut selected_format,
                                OutputFormat::RawF32le,
                                format_label(OutputFormat::RawF32le),
                            );
                        });
                });

                if selected_format != self.cfg.output_format() {
                    self.cfg.select_format(selected_format);
                }
                let mut ext_buffer = self.cfg.extension.clone()
                    .unwrap_or_else(|| self.cfg.effective_ext().to_string());
                ui.horizontal(|ui| {
                    ui.label("EXTENSION");
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut ext_buffer)
                            .id(egui::Id::new("extension_override"))
                            .font(egui::TextStyle::Monospace)
                            .desired_width(120.0),
                    );
                    if response.changed() || response.lost_focus() {
                        self.cfg.edit_extension(ext_buffer, response.lost_focus());
                    }
                });

                if ui.available_width() < 440.0 {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cfg.output_path)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(ui.available_width()),
                    );
                    self.browse_button(ui);
                } else {
                    ui.horizontal(|ui| {
                        let width = (ui.available_width() - 115.0).max(100.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.cfg.output_path)
                                .font(egui::TextStyle::Monospace)
                                .desired_width(width),
                        );
                        self.browse_button(ui);
                    });
                }
            });
            if self.cfg.output_path.trim().is_empty() {
                ui.label("OUTPUT REQUIRED / Choose a path before recording.");
            } else {
                ui.label(
                    RichText::new(
                        "Leave the placeholder as-is and the recorder names each take `YYYYMMDD-HHMMSS.<ext>` (timestamped at the moment you start). Custom stems get a `-1`, `-2`, … suffix if they collide.",
                    )
                    .small(),
                );
            }
        });
    }

    fn browse_button(&mut self, ui: &mut egui::Ui) {
        if ui.button("Browse...").clicked() {
            let mut dialog = rfd::FileDialog::new()
                .add_filter("Opus", &["opus"])
                .add_filter("WAV", &["wav"])
                .add_filter("Raw f32le", &["f32le"]);
            let current = PathBuf::from(&self.cfg.output_path);
            if let Some(parent) = current
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                dialog = dialog.set_directory(parent);
            }
            let ext = self.cfg.effective_ext().to_string();
            if let Some(name) = current.file_name() {
                let name = name.to_string_lossy();
                // Only set the seed filename when the user hasn't already
                // typed something; otherwise let the OS keep the user's text.
                if !name.is_empty() {
                    dialog = dialog.set_file_name(name.as_ref());
                }
            } else {
                dialog = dialog.set_file_name(format!("recording.{ext}"));
            }
            if let Some(path) = dialog.save_file() {
                self.cfg.output_path = path.to_string_lossy().into_owned();
                if let Some(ext) = std::path::Path::new(&self.cfg.output_path)
                    .extension()
                    .and_then(|e| e.to_str())
                {
                    // Derive the format from the user's chosen extension so
                    // the dropdown reflects what they're about to save.
                    if let Ok(f) = OutputFormat::from_str(ext) {
                        self.cfg.format = f;
                    }
                }
            }
        }
    }

    fn status_panel(&self, ui: &mut egui::Ui) {
        card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(if self.error.is_some() {
                    "05 / RECORDING ERROR"
                } else {
                    "05 / SESSION"
                })
                .font(display_font(12.0)),
            );
            ui.add(egui::Label::new(&self.status).wrap());
            if let Some(error) = &self.error {
                ui.add(egui::Label::new(RichText::new(error).color(NAVY)).wrap());
            }
            if !self.capture_note.is_empty() {
                ui.add(egui::Label::new(&self.capture_note).wrap());
            }
            if !self.log_note.is_empty() {
                ui.add(egui::Label::new(&self.log_note).wrap());
            }
            if let Some(path) = &self.last_resolved {
                ui.label(
                    RichText::new("DESTINATION")
                        .font(display_font(11.0))
                        .color(BLUE),
                );
                ui.add(egui::Label::new(RichText::new(path.to_string_lossy()).monospace()).wrap());
            }
            if let Some(path) = &self.saved_path {
                ui.label(
                    RichText::new("SAVED RECORDING")
                        .font(display_font(11.0))
                        .color(BLUE),
                );
                let path = path.to_string_lossy();
                ui.add(egui::Label::new(RichText::new(path.as_ref()).monospace()).wrap())
                    .on_hover_text(path.as_ref());
                if ui.small_button("Copy saved path").clicked() {
                    ui.ctx().copy_text(path.into_owned());
                }
            }
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive_messages();
        if ctx.input(|input| input.viewport().close_requested()) && self.worker.is_some() {
            self.closing = true;
            self.stop_recording();
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if self.closing && self.worker.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(SAND).inner_margin(16))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let padding = ((ui.available_width() - 880.0) / 2.0).max(0.0);
                        let content_width = ui.available_width().min(880.0) - 4.0;
                        ui.horizontal(|ui| {
                            ui.add_space(padding);
                            ui.vertical(|ui| {
                                ui.set_width(content_width);
                                self.header(ui);
                                ui.add_space(4.0);
                                self.recorder_panel(ui, ctx);
                                ui.add_space(8.0);
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(
                                        RichText::new("TAKE SETTINGS").font(display_font(12.0)),
                                    );
                                    if self.state.is_busy() {
                                        ui.label("Locked during this take");
                                    }
                                });
                                if ui.available_width() >= 680.0 {
                                    ui.columns(2, |columns| {
                                        self.input_settings(&mut columns[0]);
                                        self.quality_settings(&mut columns[1]);
                                    });
                                } else {
                                    self.input_settings(ui);
                                    self.quality_settings(ui);
                                }
                                self.output_settings(ui);
                                self.status_panel(ui);
                                ui.add_space(4.0);
                                let (strip, _) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), 8.0),
                                    egui::Sense::hover(),
                                );
                                for (index, color) in
                                    [NAVY, BLUE, CREAM, SAND].into_iter().enumerate()
                                {
                                    let slat = egui::Rect::from_min_size(
                                        strip.min
                                            + egui::vec2(index as f32 * strip.width() / 4.0, 0.0),
                                        egui::vec2(strip.width() / 4.0, strip.height()),
                                    );
                                    ui.painter().rect_filled(slat, 0, color);
                                }
                            });
                        });
                    });
            });
        if self.state.is_busy() {
            ctx.request_repaint_after(Duration::from_millis(40));
        }
        if self.startup_repaints > 0 {
            self.startup_repaints -= 1;
            ctx.request_repaint();
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(WorkerCmd::Stop);
        }
        // Also covers shutdown paths that bypass the viewport close request.
        // The worker finalizes EOS on Stop or command-channel disconnection.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_worker(
    device_name: Option<String>,
    output: PathBuf,
    bitrate: i32,
    format: OutputFormat,
    _extension: Option<String>,
    cmd_rx: Receiver<WorkerCmd>,
    msg_tx: &Sender<RecorderMsg>,
) -> Result<()> {
    let (capture, accum) = start_capture_named(device_name.as_deref(), 0)?;
    let fmt = capture.format.clone();
    let diagnostics = capture.diagnostics.clone();
    run_worker_with_capture(
        fmt,
        accum,
        diagnostics,
        move || drop(capture),
        output,
        bitrate,
        format,
        cmd_rx,
        msg_tx,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_worker_with_capture(
    fmt: CaptureFormat,
    accum: Accum,
    diagnostics: CaptureDiagnostics,
    stop_capture: impl FnOnce(),
    output: PathBuf,
    bitrate: i32,
    format: OutputFormat,
    cmd_rx: Receiver<WorkerCmd>,
    msg_tx: &Sender<RecorderMsg>,
) -> Result<()> {
    let mut stop_capture = Some(stop_capture);
    let mut sink = open_sink(&output, &fmt, format, bitrate)?;
    let _ = msg_tx.send(RecorderMsg::Started {
        path: output.clone(),
    });
    let _ = msg_tx.send(RecorderMsg::Ready);
    let mut paused = false;
    let mut samples = 0u64;
    let mut level_sum = 0.0;
    let mut level_samples = 0u64;
    let mut buffer = Vec::with_capacity(fmt.sample_rate as usize / 100 * usize::from(fmt.channels));
    let mut last_progress = Instant::now();
    let mut last_diagnostic = Instant::now();
    let mut previous = CaptureSnapshot::default();
    let mut backend_error = None;
    loop {
        let mut stopping = false;
        loop {
            match cmd_rx.try_recv() {
                Ok(WorkerCmd::Pause) => {
                    paused = true;
                    discard_partial_blocking(&accum);
                    level_sum = 0.0;
                    level_samples = 0;
                }
                Ok(WorkerCmd::Resume) => {
                    discard_partial_blocking(&accum);
                    paused = false;
                }
                Ok(WorkerCmd::Stop) | Err(TryRecvError::Disconnected) => {
                    stopping = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if let Some(error) = diagnostics.take_error() {
            backend_error = Some(error);
            stopping = true;
        }
        if stopping {
            if let Some(stop) = stop_capture.take() {
                stop();
            }
        }
        if paused {
            discard_partial_blocking(&accum);
        } else {
            let (count, squares) =
                drain_capture(&accum, sink.as_mut(), &fmt, stopping, &mut buffer)?;
            samples += count;
            level_samples += count;
            level_sum += squares;
        }
        if stopping {
            sink.finalize()?;
        }
        if stopping || last_progress.elapsed() >= Duration::from_millis(80) {
            let level = if level_samples == 0 {
                0.0
            } else {
                (level_sum / level_samples as f64).sqrt().clamp(0.0, 1.0) as f32
            };
            let _ = msg_tx.send(RecorderMsg::Progress(ProgressUpdate {
                elapsed_ms: samples * 1000 / u64::from(fmt.channels) / u64::from(fmt.sample_rate),
                bytes: sink.bytes_written(),
                level,
                paused,
            }));
            level_samples = 0;
            level_sum = 0.0;
            last_progress = Instant::now();
        }
        if stopping || last_diagnostic.elapsed() >= Duration::from_secs(1) {
            let current = diagnostics.snapshot();
            let note = capture_diagnostic(current, previous, paused);
            tracing::info!(
                callbacks = current.callbacks,
                samples = current.samples,
                nonzero_samples = current.nonzero_samples,
                "{note}"
            );
            let output_rate = if format == OutputFormat::Opus {
                48_000
            } else {
                fmt.sample_rate
            };
            let _ = msg_tx.send(RecorderMsg::Diagnostic(format!(
                "Input: {} ch @ {} Hz / output: {} Hz. {note}",
                fmt.channels, fmt.sample_rate, output_rate
            )));
            previous = current;
            last_diagnostic = Instant::now();
        }
        if stopping {
            if let Some(error) = backend_error.or_else(|| diagnostics.take_error()) {
                anyhow::bail!(
                    "capture backend failed: {error}; partial recording finalized at {}",
                    output.display()
                );
            }
            let _ = msg_tx.send(RecorderMsg::Finished {
                path: output,
                bytes: sink.bytes_written(),
            });
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn main() -> eframe::Result<()> {
    let log_note = match initialize_logging() {
        Ok(path) => format!("Local diagnostics: {}", path.display()),
        Err(error) => format!("Local diagnostics unavailable: {error:#}"),
    };
    let result = eframe::run_native(
        "Kagantic Voice Recorder",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([840.0, 860.0])
                .with_min_inner_size([340.0, 540.0])
                .with_resizable(true),
            ..Default::default()
        },
        Box::new(move |cc| {
            let mut app = App::new(cc);
            app.log_note = log_note;
            if app.cfg.output_path.is_empty() {
                app.error = Some(
                    "No writable default output directory. Choose a destination before recording."
                        .into(),
                );
            }
            Ok(Box::new(app))
        }),
    );
    if let Err(error) = &result {
        tracing::error!("GUI startup/runtime failed: {error}");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headless_app(rx: Receiver<RecorderMsg>) -> App {
        let ctx = egui::Context::default();
        configure_style(&ctx);
        App {
            state: State::Idle,
            cfg: AppConfig {
                selected_device: None,
                output_path: String::new(),
                bitrate_kbps: 96,
                format: OutputFormat::Opus,
                extension: None,
            },
            last_progress: ProgressUpdate::default(),
            level_history: VecDeque::new(),
            status: String::new(),
            error: None,
            device_error: None,
            capture_note: String::new(),
            log_note: String::new(),
            saved_path: None,
            last_resolved: None,
            input_devices: Vec::new(),
            cmd_tx: None,
            msg_rx: Some(rx),
            worker: None,
            closing: false,
            startup_repaints: 0,
            logo: load_logo(&ctx),
            microphone_icon: load_asset(
                &ctx,
                "microphone",
                include_bytes!("../../assets/icons/microphone.png"),
                egui::TextureOptions::LINEAR,
            ),
            folder_icon: load_asset(
                &ctx,
                "folder",
                include_bytes!("../../assets/icons/folder.png"),
                egui::TextureOptions::LINEAR,
            ),
        }
    }

    #[test]
    fn level_history_freezes_during_pause_and_resumes_without_a_gap() {
        let (tx, rx) = mpsc::channel();
        let history: VecDeque<f32> = (0..HISTORY_SAMPLES)
            .map(|index| index as f32 / HISTORY_SAMPLES as f32)
            .collect();
        let mut app = headless_app(rx);
        app.state = State::Paused;
        app.level_history = history.clone();

        // Both an in-flight recording update and paused heartbeats must leave
        // every bar in place after the user presses Pause.
        for paused in [false, true, true] {
            tx.send(RecorderMsg::Progress(ProgressUpdate {
                elapsed_ms: 1000,
                level: 0.5,
                paused,
                ..Default::default()
            }))
            .unwrap();
        }
        app.receive_messages();
        assert_eq!(app.level_history, history);
        assert_eq!(app.last_progress.elapsed_ms, 1000);

        app.state = State::Recording;
        // A queued paused heartbeat after Resume must not add a silence bar.
        tx.send(RecorderMsg::Progress(ProgressUpdate {
            paused: true,
            ..Default::default()
        }))
        .unwrap();
        app.receive_messages();
        assert_eq!(app.level_history, history);

        tx.send(RecorderMsg::Progress(ProgressUpdate {
            level: 0.75,
            ..Default::default()
        }))
        .unwrap();
        app.receive_messages();
        let mut expected = history;
        expected.pop_front();
        expected.push_back(0.75);
        assert_eq!(app.level_history, expected);
    }

    #[test]
    fn writable_destination_selection_skips_invalid_candidates_without_cwd_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("not-a-directory");
        std::fs::write(&blocked, b"keep").unwrap();
        let writable = dir.path().join("user-data").join("Recordings");
        let selected = choose_writable_directory(&[
            PathBuf::from("relative"),
            blocked.clone(),
            writable.clone(),
        ])
        .unwrap();
        assert_eq!(selected, writable);
        assert!(selected.is_absolute());
        assert_eq!(std::fs::read(&blocked).unwrap(), b"keep");
        assert!(choose_writable_directory(&[PathBuf::from("relative"), blocked]).is_err());
    }

    #[test]
    fn final_size_and_capture_failure_have_distinct_gui_states() {
        let (tx, rx) = mpsc::channel();
        let mut app = headless_app(rx);
        let path = PathBuf::from("take.wav");
        tx.send(RecorderMsg::Started { path: path.clone() })
            .unwrap();
        tx.send(RecorderMsg::Diagnostic("all-zero samples / silence".into()))
            .unwrap();
        tx.send(RecorderMsg::Finished {
            path: path.clone(),
            bytes: 1234,
        })
        .unwrap();
        app.receive_messages();
        assert_eq!(app.state, State::Stopped);
        assert_eq!(app.last_progress.bytes, 1234);
        assert_eq!(app.saved_path, Some(path.clone()));
        assert_eq!(app.last_resolved, Some(path));
        assert!(app.error.is_none());

        let (tx, rx) = mpsc::channel();
        app.msg_rx = Some(rx);
        app.saved_path = None;
        app.state = State::Recording;
        tx.send(RecorderMsg::Error(
            "device disconnected; partial take finalized".into(),
        ))
        .unwrap();
        app.receive_messages();
        assert_eq!(app.state, State::Idle);
        assert!(app.saved_path.is_none());
        assert!(app
            .error
            .as_deref()
            .unwrap()
            .contains("device disconnected"));
    }

    #[test]
    fn worker_mock_capture_reports_live_and_final_actual_bytes_for_all_formats() {
        for format in [
            OutputFormat::Opus,
            OutputFormat::WavPcm16le,
            OutputFormat::RawF32le,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let output = dir.path().join(format!("take.{}", format.default_ext()));
            let fmt = CaptureFormat {
                sample_rate: 44_100,
                channels: 2,
            };
            let samples: Vec<f32> = (0..22_050)
                .flat_map(|index| {
                    let value =
                        0.25 * (index as f32 * std::f32::consts::TAU * 440.0 / 44_100.0).sin();
                    [value, -value]
                })
                .collect();
            let accum = std::sync::Arc::new(parking_lot::Mutex::new(samples.clone()));
            let diagnostics = CaptureDiagnostics::default();
            diagnostics.record_samples(&samples);
            let (cmd_tx, cmd_rx) = mpsc::channel();
            let (msg_tx, msg_rx) = mpsc::channel();
            let path = output.clone();
            let worker = std::thread::spawn(move || {
                run_worker_with_capture(
                    fmt,
                    accum,
                    diagnostics,
                    || {},
                    path,
                    96_000,
                    format,
                    cmd_rx,
                    &msg_tx,
                )
            });
            let live = loop {
                match msg_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    RecorderMsg::Progress(update) => break update,
                    RecorderMsg::Error(error) => panic!("{error}"),
                    _ => {}
                }
            };
            assert_eq!(live.elapsed_ms, 500);
            assert_eq!(live.bytes, std::fs::metadata(&output).unwrap().len());
            assert!(live.level > 0.1);
            cmd_tx.send(WorkerCmd::Stop).unwrap();
            let mut final_progress = None;
            let final_bytes = loop {
                match msg_rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                    RecorderMsg::Progress(update) => final_progress = Some(update),
                    RecorderMsg::Finished { bytes, .. } => break bytes,
                    _ => {}
                }
            };
            worker.join().unwrap().unwrap();
            assert_eq!(final_bytes, std::fs::metadata(&output).unwrap().len());
            assert_eq!(final_progress.unwrap().bytes, final_bytes);
            assert!(final_bytes >= live.bytes);
            let mut decoder = std::process::Command::new("ffmpeg");
            decoder.args(["-v", "error"]);
            if format == OutputFormat::RawF32le {
                decoder.args(["-f", "f32le", "-ar", "44100", "-ac", "2"]);
            }
            let decoded = decoder
                .arg("-i")
                .arg(&output)
                .args(["-f", "f32le", "-acodec", "pcm_f32le", "pipe:1"])
                .output()
                .unwrap();
            assert!(
                decoded.status.success(),
                "{}",
                String::from_utf8_lossy(&decoded.stderr)
            );
            let rate = if format == OutputFormat::Opus {
                48_000
            } else {
                44_100
            };
            assert_eq!(
                decoded.stdout.len(),
                rate / 2 * 2 * 4,
                "worker timer and decoded duration must agree"
            );
            let energy = decoded
                .stdout
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f64::from(f32::from_le_bytes(*bytes)).powi(2))
                .sum::<f64>();
            assert!(
                energy / (decoded.stdout.len() / 4) as f64 > 0.01,
                "worker output must decode to nonzero audio"
            );
        }
    }

    #[test]
    fn worker_mock_silence_absence_and_backend_failure_are_not_conflated() {
        let empty = CaptureSnapshot::default();
        assert!(capture_diagnostic(empty, empty, false).contains("Waiting"));
        let diagnostics = CaptureDiagnostics::default();
        diagnostics.record_samples(&[0.0; 441]);
        assert!(capture_diagnostic(diagnostics.snapshot(), empty, false).contains("all-zero"));
        diagnostics.record_samples(&[0.25; 441]);
        assert!(capture_diagnostic(diagnostics.snapshot(), empty, false).contains("nonzero"));

        for paused in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let output = dir.path().join("partial.wav");
            let accum = std::sync::Arc::new(parking_lot::Mutex::new(vec![0.0; 441]));
            let diagnostics = CaptureDiagnostics::default();
            diagnostics.record_samples(&[0.0; 441]);
            diagnostics.report_error("mock backend disconnected".into());
            let (cmd_tx, cmd_rx) = mpsc::channel();
            if paused {
                cmd_tx.send(WorkerCmd::Pause).unwrap();
            }
            let (msg_tx, msg_rx) = mpsc::channel();
            let error = run_worker_with_capture(
                CaptureFormat {
                    sample_rate: 44_100,
                    channels: 1,
                },
                accum,
                diagnostics,
                || {},
                output.clone(),
                96_000,
                OutputFormat::WavPcm16le,
                cmd_rx,
                &msg_tx,
            )
            .unwrap_err();
            assert!(error.to_string().contains("mock backend disconnected"));
            let bytes = std::fs::read(&output).unwrap();
            assert_eq!(&bytes[..4], b"RIFF");
            assert_eq!(
                u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize,
                bytes.len() - 44
            );
            let messages: Vec<_> = msg_rx.try_iter().collect();
            assert!(!messages
                .iter()
                .any(|message| matches!(message, RecorderMsg::Finished { .. })));
            assert!(messages.iter().any(|message| matches!(message, RecorderMsg::Progress(update) if update.bytes == bytes.len() as u64 && update.elapsed_ms == if paused { 0 } else { 10 })));
        }
        // All-zero samples remain a normal, successful recording.
        let dir = tempfile::tempdir().unwrap();
        let accum = std::sync::Arc::new(parking_lot::Mutex::new(vec![0.0; 441]));
        let diagnostics = CaptureDiagnostics::default();
        diagnostics.record_samples(&[0.0; 441]);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        cmd_tx.send(WorkerCmd::Stop).unwrap();
        let (msg_tx, msg_rx) = mpsc::channel();
        run_worker_with_capture(
            CaptureFormat {
                sample_rate: 44_100,
                channels: 1,
            },
            accum,
            diagnostics,
            || {},
            dir.path().join("silent.wav"),
            96_000,
            OutputFormat::WavPcm16le,
            cmd_rx,
            &msg_tx,
        )
        .unwrap();
        assert!(msg_rx
            .try_iter()
            .any(|message| matches!(message, RecorderMsg::Finished { .. })));
    }

    #[test]
    fn gui_format_selection_changes_sink_and_suffix_and_preserves_custom_extensions() {
        let (_, rx) = mpsc::channel();
        let mut app = headless_app(rx);
        app.cfg.output_path = "recording.opus".into();
        for format in [
            OutputFormat::WavPcm16le,
            OutputFormat::RawF32le,
            OutputFormat::Opus,
        ] {
            app.cfg.select_format(format);
            assert_eq!(app.cfg.output_format(), format);
            assert_eq!(
                PathBuf::from(&app.cfg.output_path).extension().unwrap(),
                format.default_ext()
            );
            assert_eq!(app.cfg.effective_ext(), format.default_ext());
        }
        app.cfg.output_path = "take.custom".into();
        app.cfg.extension = Some("custom".into());
        app.cfg.select_format(OutputFormat::WavPcm16le);
        assert_eq!(app.cfg.output_format(), OutputFormat::WavPcm16le);
        assert_eq!(app.cfg.effective_ext(), "custom");
        assert_eq!(app.cfg.output_path, "take.custom");
        app.cfg.extension = Some("raw".into());
        assert_eq!(app.cfg.output_format(), OutputFormat::RawF32le);
        app.cfg.select_format(OutputFormat::Opus);
        assert_eq!(app.cfg.output_format(), OutputFormat::Opus);
    }

    #[test]
    fn gui_extension_keyboard_edit_survives_multiple_repaint_frames() {
        let (_, rx) = mpsc::channel();
        let mut app = headless_app(rx);
        let ctx = egui::Context::default();
        configure_style(&ctx);
        let frame = |app: &mut App, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(840.0, 860.0),
                )),
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.output_settings(ui));
            });
        };
        frame(&mut app, Vec::new());
        let id = egui::Id::new("extension_override");
        ctx.memory_mut(|memory| memory.request_focus(id));
        frame(
            &mut app,
            vec![
                egui::Event::Key {
                    key: egui::Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers {
                        ctrl: true,
                        command: true,
                        ..Default::default()
                    },
                },
                egui::Event::Text("o".into()),
            ],
        );
        assert_eq!(app.cfg.extension.as_deref(), Some("o"));
        frame(&mut app, vec![egui::Event::Text("gg".into())]);
        assert_eq!(app.cfg.extension.as_deref(), Some("ogg"));
        ctx.memory_mut(|memory| memory.surrender_focus(id));
        frame(&mut app, Vec::new());
        assert_eq!(app.cfg.extension.as_deref(), Some("ogg"));
        assert_eq!(app.cfg.output_format(), OutputFormat::Opus);
    }

    #[test]
    fn worker_late_backend_failure_while_paused_finalizes_only_recorded_audio() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("partial.wav");
        let accum = std::sync::Arc::new(parking_lot::Mutex::new(vec![0.25; 4410]));
        let diagnostics = CaptureDiagnostics::default();
        diagnostics.record_samples(&[0.25; 4410]);
        let errors = diagnostics.clone();
        let paused_input = accum.clone();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (msg_tx, msg_rx) = mpsc::channel();
        let path = output.clone();
        let worker = std::thread::spawn(move || {
            run_worker_with_capture(
                CaptureFormat {
                    sample_rate: 44_100,
                    channels: 1,
                },
                accum,
                diagnostics,
                || {},
                path,
                96_000,
                OutputFormat::WavPcm16le,
                cmd_rx,
                &msg_tx,
            )
        });
        let receive_progress = |paused| loop {
            if let RecorderMsg::Progress(progress) =
                msg_rx.recv_timeout(Duration::from_secs(10)).unwrap()
            {
                if progress.paused == paused {
                    break progress;
                }
            }
        };
        assert_eq!(receive_progress(false).elapsed_ms, 100);
        cmd_tx.send(WorkerCmd::Pause).unwrap();
        assert_eq!(receive_progress(true).elapsed_ms, 100);
        paused_input.lock().extend_from_slice(&[0.5; 4410]);
        errors.report_error("device unplugged after pause".into());
        let error = worker.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("device unplugged after pause"));
        let messages: Vec<_> = msg_rx.try_iter().collect();
        assert!(!messages
            .iter()
            .any(|message| matches!(message, RecorderMsg::Finished { .. })));
        let bytes = std::fs::read(output).unwrap();
        assert_eq!(bytes.len(), 44 + 4410 * 2);
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            4410 * 2
        );
    }

    #[test]
    fn worker_without_callbacks_can_stop_cleanly_and_diagnoses_missing_input() {
        let directory = tempfile::tempdir().unwrap();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        cmd_tx.send(WorkerCmd::Stop).unwrap();
        let (msg_tx, msg_rx) = mpsc::channel();
        run_worker_with_capture(
            CaptureFormat {
                sample_rate: 44_100,
                channels: 1,
            },
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
            CaptureDiagnostics::default(),
            || {},
            directory.path().join("empty.wav"),
            96_000,
            OutputFormat::WavPcm16le,
            cmd_rx,
            &msg_tx,
        )
        .unwrap();
        let messages: Vec<_> = msg_rx.try_iter().collect();
        assert!(messages.iter().any(
            |message| matches!(message, RecorderMsg::Diagnostic(note) if note.contains("Waiting"))
        ));
        assert!(messages
            .iter()
            .any(|message| matches!(message, RecorderMsg::Finished { bytes: 44, .. })));
    }

    #[test]
    fn gui_committed_extension_keeps_precedence_over_a_different_path_format() {
        let (_, rx) = mpsc::channel();
        let mut app = headless_app(rx);
        app.cfg.output_path = "take.wav".into();
        assert_eq!(app.cfg.output_format(), OutputFormat::WavPcm16le);
        app.cfg.edit_extension("opus".into(), true);
        assert_eq!(app.cfg.output_format(), OutputFormat::Opus);
        assert_eq!(app.cfg.effective_ext(), "opus");
        app.cfg.edit_extension(String::new(), true);
        assert_eq!(app.cfg.output_format(), OutputFormat::WavPcm16le);
    }

    #[test]
    fn paused_diagnostics_still_distinguish_absent_zero_and_signal_input() {
        let previous = CaptureSnapshot::default();
        let zero = CaptureSnapshot {
            callbacks: 1,
            samples: 441,
            nonzero_samples: 0,
        };
        let signal = CaptureSnapshot {
            nonzero_samples: 1,
            ..zero
        };
        for (current, expected) in [
            (previous, "Waiting"),
            (zero, "all-zero"),
            (signal, "nonzero"),
        ] {
            let note = capture_diagnostic(current, previous, true);
            assert!(note.contains(expected), "{note}");
            assert!(note.contains("Paused"), "{note}");
        }
    }
}
