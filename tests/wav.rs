//! Integration test for the new `WavSink` writer:
//!   synthesize a 1 kHz sine wave → WavSink → .wav file
//!
//! Verifies the produced file:
//!   1. Has the canonical RIFF/WAVE header at the expected offsets.
//!   2. After `finalize`, RIFF chunk size = payload + 36 and data chunk
//!      size = payload (no off-by-one, no header padding).
//!   3. ffprobe recognises it as `pcm_s16le`, 48 kHz, mono, ~0.5 s.

use std::io::{Read, Seek, SeekFrom};

use kvr_recorder::audio::CaptureFormat;
use kvr_recorder::writers::{FrameSink, WavSink};

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
fn wav_sink_writes_valid_pcm_wav() {
    // Unique, RAII-cleaned working dir: safe under parallel runs and
    // Windows file-sharing rules.
    let tmpdir = tempfile::tempdir().expect("create temp dir");
    let tmp = tmpdir.path().join("recording.wav");

    let fmt = CaptureFormat {
        sample_rate: 48_000,
        channels: 1,
    };

    // Open the file as a read+write handle and wrap it in a WavSink so we
    // can seek back to verify the finalised RIFF/data chunk sizes.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .expect("create file");
    let mut sink = WavSink::new(file, fmt.clone());

    sink.write_header(&fmt).expect("write_header");

    // Synthesize 0.5 s of 1 kHz mono audio → 24 frames × 960 samples.
    let sr = 48_000u32;
    let samples = synth_sine(0.5, 1000.0, sr);
    assert_eq!(samples.len(), (0.5 * sr as f64) as usize);
    // Payload size in the raw f32 frame buffer: bytes will be packed as
    // i16 little-endian in the WAV (2 bytes per sample per channel).
    let payload_samples = samples.len();
    let payload_bytes = payload_samples * fmt.channels as usize * 2;

    for chunk in samples.chunks(960) {
        sink.write_frames(chunk, &fmt).expect("write_frames");
    }

    sink.finalize().expect("finalize");

    // Re-open for verification (the sink owns its file handle; let it drop
    // before reading back to be safe on Windows).
    let mut f = std::fs::File::open(&tmp).expect("re-open for header check");
    let mut header = [0u8; 44];
    f.read_exact(&mut header).expect("read 44-byte header");

    // 1) Magic numbers and chunk ids at fixed offsets.
    assert_eq!(&header[0..4], b"RIFF", "bytes 0-3 must be RIFF");
    assert_eq!(&header[8..12], b"WAVE", "bytes 8-11 must be WAVE");
    assert_eq!(&header[12..16], b"fmt ", "bytes 12-15 must be fmt ");
    assert_eq!(&header[36..40], b"data", "bytes 36-39 must be data");

    // fmt chunk: PCM=1, channels, sample rate, byte rate, block align, bits.
    let audio_format = u16::from_le_bytes(header[20..22].try_into().unwrap());
    assert_eq!(audio_format, 1, "WAVE_FORMAT_PCM");
    let channels = u16::from_le_bytes(header[22..24].try_into().unwrap());
    assert_eq!(channels, fmt.channels as u16);
    let sample_rate = u32::from_le_bytes(header[24..28].try_into().unwrap());
    assert_eq!(sample_rate, fmt.sample_rate);
    let bits_per_sample = u16::from_le_bytes(header[34..36].try_into().unwrap());
    assert_eq!(bits_per_sample, 16);

    // 2) After finalize, RIFF size = payload + 36 and data size = payload.
    f.seek(SeekFrom::Start(4)).expect("seek RIFF size");
    let mut riff_size_bytes = [0u8; 4];
    f.read_exact(&mut riff_size_bytes).expect("read RIFF size");
    let riff_size = u32::from_le_bytes(riff_size_bytes);
    assert_eq!(
        riff_size as usize,
        payload_bytes + 36,
        "RIFF chunk size must be payload + 36"
    );

    f.seek(SeekFrom::Start(40)).expect("seek data size");
    let mut data_size_bytes = [0u8; 4];
    f.read_exact(&mut data_size_bytes).expect("read data size");
    let data_size = u32::from_le_bytes(data_size_bytes);
    assert_eq!(
        data_size as usize, payload_bytes,
        "data chunk size must equal payload bytes"
    );

    // 3) ffprobe round-trip.
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
            tmp.to_str().unwrap(),
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
        "ffprobe failed to parse the produced .wav"
    );
    assert!(
        stdout.contains("codec_name=pcm_s16le"),
        "ffprobe did not recognise PCM s16le: {}",
        stdout
    );
    assert!(stdout.contains("sample_rate=48000"));
    assert!(stdout.contains("channels=1"));

    // Parse duration from `duration=…` (seconds, decimal). Allow a small
    // jitter band: 0.49 s ≤ duration ≤ 0.51 s.
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
