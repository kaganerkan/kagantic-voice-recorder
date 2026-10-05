//! Integration test for the new `RawF32Sink` writer:
//!   synthesize a 1 kHz sine wave → RawF32Sink → .f32le file
//!
//! Verifies the produced file:
//!   1. Plays back correctly through ffmpeg's `f32le` demuxer.
//!   2. ffprobe on the resulting WAV reports `codec_name=pcm_s16le`,
//!      `sample_rate=48000`, `channels=1`, duration ≈ 0.5 s.

use kvr_recorder::audio::CaptureFormat;
use kvr_recorder::writers::{FrameSink, RawF32Sink};

fn synth_sine(seconds: f64, freq: f64, sr: u32) -> Vec<f32> {
    let n = (seconds * sr as f64) as usize;
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / sr as f64;
        v.push(0.3 * (2.0 * std::f64::consts::PI * freq * t).sin() as f32);
    }
    v
}

#[test]
fn raw_f32_sink_round_trips_through_ffmpeg() {
    // Unique, RAII-cleaned working dir: safe under parallel runs and
    // Windows file-sharing rules.
    let tmpdir = tempfile::tempdir().expect("create temp dir");
    let raw = tmpdir.path().join("recording.f32le");
    let wav = tmpdir.path().join("recording.wav");

    let fmt = CaptureFormat {
        sample_rate: 48_000,
        channels: 1,
    };

    let file = std::fs::File::create(&raw).expect("create file");
    let mut sink = RawF32Sink::new(file, fmt.clone());

    sink.write_header(&fmt)
        .expect("write_header (no-op for raw)");

    // Synthesize 0.5 s of 1 kHz mono audio → 24 frames × 960 samples.
    let sr = 48_000u32;
    let samples = synth_sine(0.5, 1000.0, sr);
    assert_eq!(samples.len(), (0.5 * sr as f64) as usize);

    for chunk in samples.chunks(960) {
        sink.write_frames(chunk, &fmt).expect("write_frames");
    }

    sink.finalize().expect("finalize");

    // Convert the raw f32le stream into a WAV container via ffmpeg so we
    // can ffprobe it with a real codec/format.
    let conv = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "f32le",
            "-ar",
            "48000",
            "-ac",
            "1",
            "-i",
            raw.to_str().unwrap(),
            "-f",
            "wav",
            wav.to_str().unwrap(),
        ])
        .output()
        .expect("ffmpeg");
    assert!(
        conv.status.success(),
        "ffmpeg f32le → wav failed: {}",
        String::from_utf8_lossy(&conv.stderr)
    );

    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_name,codec_type,sample_rate,channels:format=duration",
            "-of",
            "default=nw=1",
            wav.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("ffprobe stdout:\n{}", stdout);
    if !out.status.success() {
        eprintln!("ffprobe stderr:\n{}", stderr);
    }
    assert!(
        out.status.success(),
        "ffprobe failed to parse ffmpeg-produced .wav"
    );
    assert!(
        stdout.contains("codec_name=pcm_s16le"),
        "ffprobe did not recognise PCM s16le: {}",
        stdout
    );
    assert!(stdout.contains("sample_rate=48000"));
    assert!(stdout.contains("channels=1"));

    // Parse `duration=…` and require 0.49 s ≤ duration ≤ 0.51 s.
    let duration_line = stdout
        .lines()
        .find(|l| l.starts_with("duration="))
        .expect("ffprobe must emit duration");
    let duration: f64 = duration_line
        .trim_start_matches("duration=")
        .parse()
        .expect("duration parse");
    assert!(
        (0.49..=0.51).contains(&duration),
        "duration {duration} s outside 0.5 s ± 0.01 s band"
    );

    // Cleanup is handled by `tempdir`'s Drop.
}
