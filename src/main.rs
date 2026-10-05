//! Binary entry point for `kvr`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, Notify};

use kvr_recorder::output::OutputFormat;
use kvr_recorder::session::{
    clear_session, is_pid_alive, read_session, session_dir, write_session, Session,
};
use kvr_recorder::{resolve_output, Control, Progress, RunConfig};

#[derive(Debug, Parser)]
#[command(
    name = "kvr",
    version,
    about = "Native audio recorder (Opus/WAV/raw PCM)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Start a new recording. By default runs in the foreground with an
    /// interactive REPL (type `p`/`r`/`s`/`q` + Enter).
    Start {
        /// Output file path (default: ./<YYYYMMDD-HHMMSS>.<ext> with a numeric suffix if even that name already exists).
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Directory to write to when `--output` is omitted.
        #[arg(long, default_value = ".")]
        dir: PathBuf,

        /// Container format. `opus` is the default.
        #[arg(long, value_enum, default_value_t = OutputFormat::Opus)]
        format: OutputFormat,

        /// Optional file extension override (without leading dot). Defaults
        /// to the format's standard extension (`opus` / `wav` / `f32le`).
        #[arg(long)]
        extension: Option<String>,

        /// Opus bitrate in bits per second (default 96k for voice). Only
        /// meaningful when `--format opus`.
        #[arg(short, long, default_value_t = 96_000)]
        bitrate: i32,

        /// Force a channel count (1 or 2). Default: device default.
        #[arg(long)]
        channels: Option<u8>,

        /// Force a sample rate. Default: device default.
        #[arg(long)]
        sample_rate: Option<u32>,

        /// Reserved flag: does NOT detach from the terminal and does NOT
        /// disable the interactive REPL. The recorder always runs in the
        /// foreground; control it with `p`/`r`/`s`/`q` in this shell (or
        /// `kvr pause/resume/stop` from another shell on Unix).
        #[arg(long)]
        daemon: bool,
    },

    /// Pause an active recording (no-op if already paused).
    Pause,

    /// Resume a paused recording (no-op if not paused).
    Resume,

    /// Stop the active recording and close the file.
    Stop,

    /// Show the current session (PID, output path, negotiated format).
    Status,

    /// Print the directory session.json lives in.
    Where,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();
    // The recorder holds a `cpal::Stream` (not Send) and the `OpusEncoder`
    // (also !Send). Run the start subcommand inside a LocalSet so we can
    // `spawn_local` the recorder on this thread.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            match cli.cmd {
                Cmd::Start {
                    output,
                    dir,
                    format,
                    extension,
                    bitrate,
                    channels,
                    sample_rate,
                    daemon,
                } => {
                    cmd_start(
                        output,
                        dir,
                        format,
                        extension,
                        bitrate,
                        channels,
                        sample_rate,
                        daemon,
                    )
                    .await
                }
                Cmd::Pause => cmd_pause().await,
                Cmd::Resume => cmd_resume().await,
                Cmd::Stop => cmd_stop().await,
                Cmd::Status => cmd_status().await,
                Cmd::Where => {
                    println!("{}", session_dir()?.display());
                    Ok(())
                }
            }
        })
        .await
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,kvr_recorder"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

#[allow(clippy::too_many_arguments)]
async fn cmd_start(
    output: Option<PathBuf>,
    dir: PathBuf,
    format: OutputFormat,
    extension: Option<String>,
    bitrate: i32,
    channels: Option<u8>,
    sample_rate: Option<u32>,
    daemon: bool,
) -> Result<()> {
    if let Some(s) = read_session()? {
        if is_pid_alive(s.pid) {
            // External POSIX controls cannot stop a Windows CLI session.
            let hint = if cfg!(windows) {
                "stop it in its original terminal with `s` or `q` + Enter (or Ctrl+C)."
            } else {
                "Run `kvr stop` first."
            };
            anyhow::bail!(
                "an active session already exists (pid={}, output={}). {hint}",
                s.pid,
                s.output_path.display()
            );
        } else {
            // Stale session from a dead process — clear it.
            clear_session()?;
        }
    }

    let cfg = RunConfig {
        output: output.clone(),
        bitrate_bps: bitrate,
        force_channels: channels,
        sample_rate,
        format,
        extension: extension.clone(),
    };

    // Resolve the output path (explicit path wins; otherwise a collision-free
    // default inside `dir` using the format's default extension, or the
    // `--extension` override if provided).
    let out = resolve_output(&cfg, &dir);

    let (ctrl_tx, ctrl_rx) = mpsc::channel::<Control>(8);
    let stop_notify = Arc::new(Notify::new());

    // Forward Ctrl+C into the recorder so the file is finalized cleanly.
    let ctrl_tx_signal = ctrl_tx.clone();
    let stop_notify_signal = stop_notify.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = ctrl_tx_signal.send(Control::Stop).await;
        stop_notify_signal.notify_waiters();
        tokio::time::sleep(Duration::from_millis(150)).await;
        std::process::exit(0);
    });

    // Translate SIGUSR1 → Pause, SIGUSR2 → Resume into the control channel so
    // external `kvr pause/resume` subcommands can drive the running recorder
    // from another shell.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut usr1 = signal(SignalKind::user_defined1()).context("install SIGUSR1 handler")?;
        let mut usr2 = signal(SignalKind::user_defined2()).context("install SIGUSR2 handler")?;
        let ctrl_tx_sig = ctrl_tx.clone();
        tokio::spawn(async move {
            // Loop until ctrl_tx_sig is dropped by the outer task on Stop.
            loop {
                tokio::select! {
                    biased;
                    _ = usr1.recv() => {
                        if ctrl_tx_sig.send(Control::Pause).await.is_err() { break; }
                    }
                    _ = usr2.recv() => {
                        if ctrl_tx_sig.send(Control::Resume).await.is_err() { break; }
                    }
                }
            }
        });
    }

    let session = Session {
        pid: std::process::id(),
        output_path: out.clone(),
        sample_rate: sample_rate.unwrap_or(0),
        channels: channels.unwrap_or(0),
        bitrate_bps: bitrate,
        started_unix_ms: now_ms(),
    };
    write_session(&session).context("persist session.json")?;

    println!("recording → {}", out.display());
    println!(
        "codec: {}   bitrate: {bitrate} bps   pid: {}",
        format, session.pid
    );
    println!("controls: p=pause  r=resume  s=stop  q=quit (saves file)");

    if daemon {
        // --daemon is a reserved compatibility flag: it does NOT detach the
        // process from the terminal, and the REPL below still owns the
        // session — the run behaves exactly like a plain `start`.
        println!("note: --daemon does not detach; the foreground REPL (p/r/s/q) stays active in this shell.");
    }

    let progress_printer = make_progress_printer();
    let mut rec_handle = tokio::task::spawn_local(run_recorder_with_cleanup(
        cfg,
        ctrl_rx,
        stop_notify,
        progress_printer,
    ));

    // REPL on stdin.
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    loop {
        tokio::select! {
            r = lines.next_line() => {
                match r {
                    Ok(Some(line)) => {
                        let line = line.trim().to_ascii_lowercase();
                        match line.as_str() {
                            "p" | "pause"    => { let _ = ctrl_tx.send(Control::Pause).await; }
                            "r" | "resume"   => { let _ = ctrl_tx.send(Control::Resume).await; }
                            "s" | "stop"     => { let _ = ctrl_tx.send(Control::Stop).await; break; }
                            "q" | "quit"     => { let _ = ctrl_tx.send(Control::Stop).await; break; }
                            "" => {}
                            _  => println!("unknown: p/r/s/q"),
                        }
                    }
                    Ok(None) => break, // stdin closed → stop.
                    Err(e) => { tracing::warn!("stdin error: {e}"); break; }
                }
            }
            res = &mut rec_handle => {
                match res {
                    Ok(Ok(file)) => println!(
                        "saved {} ({} frames, {} ms)",
                        file.path.display(),
                        file.frames_encoded,
                        file.duration_ms
                    ),
                    Ok(Err(e)) => eprintln!("recorder failed: {e:?}"),
                    Err(e) => eprintln!("recorder task panicked: {e}"),
                }
                clear_session().ok();
                return Ok(());
            }
        }
    }

    // Wait for recorder to finish finalizing.
    match rec_handle.await {
        Ok(Ok(file)) => println!(
            "saved {} ({} frames, {} ms)",
            file.path.display(),
            file.frames_encoded,
            file.duration_ms
        ),
        Ok(Err(e)) => eprintln!("recorder failed: {e:?}"),
        Err(e) => eprintln!("recorder task panicked: {e}"),
    }
    clear_session().ok();
    Ok(())
}

fn make_progress_printer() -> impl FnMut(&Progress) + Send + 'static {
    let mut last_print_ms: u64 = 0;
    let start = std::time::Instant::now();
    move |p: &Progress| {
        let now = start.elapsed().as_millis() as u64;
        if now - last_print_ms < 250 {
            return;
        }
        last_print_ms = now;
        let elapsed_ms = now;
        let mins = elapsed_ms / 60_000;
        let secs = (elapsed_ms / 1000) % 60;
        let dec = (elapsed_ms / 100) % 10;
        let pad = "          ";
        print!(
            "\r[{:02}:{:02}.{}] frames={} granule={} rate={}ch/{}Hz {}{}{pad}",
            mins,
            secs,
            dec,
            p.frames_encoded,
            p.granule,
            p.format.channels,
            p.format.sample_rate,
            p.format_name,
            if p.paused { " [PAUSED]" } else { "" },
        );
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
}

// Spawned by `cmd_start`: owns the progress closure and calls `run_recorder`.
async fn run_recorder_with_cleanup(
    cfg: RunConfig,
    ctrl_rx: mpsc::Receiver<Control>,
    stop_notify: Arc<Notify>,
    progress: impl FnMut(&Progress) + Send + 'static,
) -> Result<kvr_recorder::RecordedFile> {
    kvr_recorder::run_recorder(cfg, ctrl_rx, stop_notify, progress).await
}

async fn cmd_pause() -> Result<()> {
    signal_session(Control::Pause, "paused").await
}
async fn cmd_resume() -> Result<()> {
    signal_session(Control::Resume, "resumed").await
}
async fn cmd_stop() -> Result<()> {
    signal_session(Control::Stop, "stopped").await
}

/// Send an external control command to the running recorder.
#[cfg(unix)]
async fn signal_session(cmd: Control, label: &str) -> Result<()> {
    let s = match read_session()? {
        Some(s) => s,
        None => {
            println!("no active recording session");
            return Ok(());
        }
    };
    if !is_pid_alive(s.pid) {
        eprintln!("session points at dead pid {}; clearing.", s.pid);
        clear_session()?;
        return Ok(());
    }
    // The running recorder installs SIGUSR1/SIGUSR2 handlers that translate
    // them into Pause/Resume; SIGINT triggers a clean Stop.
    let sig = match cmd {
        Control::Pause => libc::SIGUSR1,
        Control::Resume => libc::SIGUSR2,
        Control::Stop => libc::SIGINT,
    };
    let res = unsafe { libc::kill(s.pid as i32, sig) };
    if res != 0 {
        let e = std::io::Error::last_os_error();
        anyhow::bail!("signal pid {} failed: {e}", s.pid);
    }
    println!("{label} → pid {} ({})", s.pid, s.output_path.display());
    Ok(())
}

/// Non-Unix platforms (e.g. Windows) have no portable external-control
/// mechanism, so bail with guidance before reading or clearing the session.
#[cfg(not(unix))]
async fn signal_session(_cmd: Control, _label: &str) -> Result<()> {
    anyhow::bail!(
        "external pause/resume/stop requires Unix/POSIX signals. \
         Use the foreground recorder's REPL (`p`/`r`/`s`/`q` + Enter) instead."
    );
}

async fn cmd_status() -> Result<()> {
    match read_session()? {
        None => println!("no active recording session."),
        Some(s) => {
            let alive = is_pid_alive(s.pid);
            // The CLI no longer persists the chosen format (only the resolved
            // output path). Surface "unknown" for sessions started by older
            // builds that didn't yet record it.
            let ext = s
                .output_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let format_name = match ext.to_ascii_lowercase().as_str() {
                "opus" => "opus",
                "wav" => "wav",
                "f32le" => "raw",
                _ => "unknown",
            };
            println!(
                "pid     : {}\nalive   : {}\noutput  : {}\nformat  : {}\nsample  : {}/{}Hz\nbitrate : {} bps\nstarted : unix_ms={}",
                s.pid,
                alive,
                s.output_path.display(),
                format_name,
                if s.channels == 0 { "default".into() } else { s.channels.to_string() },
                if s.sample_rate == 0 { "default".into() } else { s.sample_rate.to_string() },
                s.bitrate_bps,
                s.started_unix_ms
            );
        }
    }
    Ok(())
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
