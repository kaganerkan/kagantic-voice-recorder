//! Native audio recorder built with eframe/egui and the glow renderer.
//! Audio capture and encoding run on a worker thread; closing the window
//! stops and joins that worker after the current take has been finalized.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui::{self, Color32, FontId, RichText, Stroke};
use kvr_recorder::audio::{discard_partial_blocking, start_capture_named, try_take_frame_blocking};
use kvr_recorder::devices::input_devices;
use kvr_recorder::encoder::OpusStreamEncoder;
use kvr_recorder::output::OutputFormat;
use kvr_recorder::session::resolve_recording_path;
use kvr_recorder::writers::{FrameSink, OggOpusSink, RawF32Sink, WavSink};

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
    },
    Error(String),
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
        // The displayed field is a *placeholder stem*, not the final filename.
        // The actual recording name (`<home>/<YYYYMMDD-HHMMSS>.<ext>` or the
        // user-typed stem) is resolved at the moment the user presses RECORD
        // (see `start_recording`), so two consecutive presses always produce
        // two unique takes even within the same second.
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let ext = OutputFormat::Opus.default_ext();
        let placeholder = std::path::Path::new(&home)
            .join(format!("recording.{ext}"))
            .to_string_lossy()
            .into_owned();
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
            _ => self.format.default_ext(),
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
}

struct App {
    state: State,
    cfg: AppConfig,
    last_progress: ProgressUpdate,
    level_history: VecDeque<f32>,
    status: String,
    error: Option<String>,
    device_error: Option<String>,
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
                Ok(RecorderMsg::Finished { path }) => {
                    if self.join_worker() {
                        self.state = State::Stopped;
                        self.last_progress.level = 0.0;
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
                    "{:.1} KB encoded audio",
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
                // Format dropdown. Detect a change so the extension field can
                // snap to the new default (when the user hasn't typed a
                // custom value yet).
                let previous_default = self.cfg.format.default_ext().to_string();
                ui.horizontal(|ui| {
                    ui.label("FORMAT");
                    egui::ComboBox::from_id_salt("output_format")
                        .selected_text(format_label(self.cfg.format))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.cfg.format,
                                OutputFormat::Opus,
                                format_label(OutputFormat::Opus),
                            );
                            ui.selectable_value(
                                &mut self.cfg.format,
                                OutputFormat::WavPcm16le,
                                format_label(OutputFormat::WavPcm16le),
                            );
                            ui.selectable_value(
                                &mut self.cfg.format,
                                OutputFormat::RawF32le,
                                format_label(OutputFormat::RawF32le),
                            );
                        });
                });

                // Extension override (auto-tracks the format default if blank
                // or matching the previous default).
                let current_default = self.cfg.format.default_ext().to_string();
                // When the dropdown moved to a new format and the user hasn't
                // typed a custom override, snap the extension to the new
                // format's default.
                if current_default != previous_default
                    && self.cfg.extension.as_deref() == Some(previous_default.as_str())
                {
                    self.cfg.extension = None;
                }
                let ext_text = self
                    .cfg
                    .extension
                    .clone()
                    .unwrap_or_else(|| current_default.clone());
                let mut ext_buffer = ext_text.clone();
                ui.horizontal(|ui| {
                    ui.label("EXTENSION");
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut ext_buffer)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(120.0),
                    );
                    if response.lost_focus() {
                        let trimmed = ext_buffer.trim().to_string();
                        self.cfg.extension = if trimmed.is_empty() || trimmed == current_default {
                            None
                        } else {
                            Some(trimmed)
                        };
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
    let capture_fmt = capture.format.clone();
    let mut capture = Some(capture);

    // Build the writer based on the chosen format. The Opus path keeps the
    // pre-existing behaviour (encoder + OggWriter) and is wrapped in the
    // shared `OggOpusSink`; the WAV and raw paths pull from the accumulator
    // directly.
    enum SinkMode {
        Opus {
            run: Box<dyn FrameSink + Send>,
            encoder: OpusStreamEncoder,
        },
        Pcm {
            run: Box<dyn FrameSink + Send>,
            block_samples: usize,
            channels: usize,
        },
    }
    let mut mode = match format {
        OutputFormat::Opus => {
            let mut ogg_sink = OggOpusSink::new_file(&output, &capture_fmt, bitrate)?;
            let encoder = OpusStreamEncoder::new(&capture_fmt, bitrate)?;
            ogg_sink.write_header(&capture_fmt)?;
            SinkMode::Opus {
                run: Box::new(ogg_sink),
                encoder,
            }
        }
        OutputFormat::WavPcm16le => {
            let mut wav = WavSink::new_file(&output, &capture_fmt)?;
            wav.write_header(&capture_fmt)?;
            SinkMode::Pcm {
                run: Box::new(wav),
                block_samples: 480,
                channels: capture_fmt.channels as usize,
            }
        }
        OutputFormat::RawF32le => {
            let mut raw = RawF32Sink::new_file(&output, &capture_fmt)?;
            raw.write_header(&capture_fmt)?;
            SinkMode::Pcm {
                run: Box::new(raw),
                block_samples: 480,
                channels: capture_fmt.channels as usize,
            }
        }
    };

    // The output file is now open; tell the GUI the actual filename so it
    // can update the visible field (and so a same-second click produces a
    // distinct, user-visible name).
    let _ = msg_tx.send(RecorderMsg::Started {
        path: output.clone(),
    });

    let _ = msg_tx.send(RecorderMsg::Ready);

    let mut paused = false;
    let mut stopping = false;
    let mut frames = 0u64;
    let mut level_sum = 0.0f32;
    let mut level_samples = 0usize;
    let mut last_progress = Instant::now();

    loop {
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

        if stopping {
            // Stop callbacks before draining the remaining complete frames.
            // Paused input is discarded, never appended to the saved take.
            drop(capture.take());
        }
        if paused {
            discard_partial_blocking(&accum);
        } else {
            match &mut mode {
                SinkMode::Opus { run, encoder } => {
                    while let Some(frame) = try_take_frame_blocking(&accum, encoder) {
                        // Measure real samples before encoding consumes the
                        // frame, rather than inspecting an already-drained
                        // accumulator.
                        level_sum += frame.iter().map(|s| s * s).sum::<f32>();
                        level_samples += frame.len();
                        run.write_frames(&frame, &capture_fmt)?;
                        frames += 1;
                    }
                }
                SinkMode::Pcm {
                    run,
                    block_samples,
                    channels,
                } => {
                    let block_samples = *block_samples;
                    let channels = *channels;
                    let need = block_samples * channels;
                    loop {
                        let drained = {
                            let mut buf = accum.lock();
                            if buf.len() < need {
                                None
                            } else {
                                Some(buf.drain(..need).collect::<Vec<f32>>())
                            }
                        };
                        let Some(block) = drained else { break };
                        let resampled = if capture_fmt.sample_rate == 48_000 {
                            block
                        } else {
                            kvr_recorder::output::resample_linear(
                                &block,
                                capture_fmt.sample_rate,
                                48_000,
                            )
                        };
                        level_sum += resampled.iter().map(|s| s * s).sum::<f32>();
                        level_samples += resampled.len();
                        run.write_frames(&resampled, &capture_fmt)?;
                        frames += 1;
                    }
                }
            }
        }

        if stopping || last_progress.elapsed() >= Duration::from_millis(80) {
            let level = if level_samples == 0 {
                0.0
            } else {
                (level_sum / level_samples as f32).sqrt().clamp(0.0, 1.0)
            };
            // The Opus path exposes the encoder's frame size + sample rate;
            // PCM paths use 10ms blocks at the negotiated capture rate.
            let elapsed_ms = match &mode {
                SinkMode::Opus { encoder, .. } => {
                    frames * encoder.frame_size() as u64 * 1000
                        / u64::from(encoder.sample_rate()).max(1)
                }
                SinkMode::Pcm { .. } => {
                    frames * 480 * 1000 / u64::from(capture_fmt.sample_rate).max(1)
                }
            };
            let _ = msg_tx.send(RecorderMsg::Progress(ProgressUpdate {
                elapsed_ms,
                bytes: 0,
                level,
                paused,
            }));
            level_sum = 0.0;
            level_samples = 0;
            last_progress = Instant::now();
        }
        if stopping {
            // Finalize the sink before dropping it so WAV / raw files get
            // their size headers back-patched and the Opus sink emits its
            // EOS page.
            match &mut mode {
                SinkMode::Opus { run, .. } => {
                    run.finalize()?;
                }
                SinkMode::Pcm { run, .. } => {
                    run.finalize()?;
                }
            }
            let _ = msg_tx.send(RecorderMsg::Finished { path: output });
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn main() -> eframe::Result<()> {
    eframe::run_native(
        "Kagantic Voice Recorder",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([840.0, 860.0])
                .with_min_inner_size([340.0, 540.0])
                .with_resizable(true),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_history_freezes_during_pause_and_resumes_without_a_gap() {
        let (tx, rx) = mpsc::channel();
        let history: VecDeque<f32> = (0..HISTORY_SAMPLES)
            .map(|index| index as f32 / HISTORY_SAMPLES as f32)
            .collect();
        let mut app = App {
            state: State::Paused,
            cfg: AppConfig::default(),
            last_progress: ProgressUpdate::default(),
            level_history: history.clone(),
            status: String::new(),
            error: None,
            device_error: None,
            saved_path: None,
            last_resolved: None,
            input_devices: Vec::new(),
            cmd_tx: None,
            msg_rx: Some(rx),
            worker: None,
            closing: false,
            startup_repaints: 0,
            logo: load_logo(&egui::Context::default()),
            microphone_icon: load_asset(
                &egui::Context::default(),
                "microphone",
                include_bytes!("../../assets/icons/microphone.png"),
                egui::TextureOptions::LINEAR,
            ),
            folder_icon: load_asset(
                &egui::Context::default(),
                "folder",
                include_bytes!("../../assets/icons/folder.png"),
                egui::TextureOptions::LINEAR,
            ),
        };

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
}
